//! `cu mcp`: the daemon's browser tools for any Model Context Protocol client
//! (pi, Claude Code, Codex, Cursor...), over stdio.
//!
//! The client launches `cu mcp` and speaks JSON-RPC 2.0 to it, one message per
//! line on stdin and stdout. Every tool is one request to the running daemon,
//! on the same routes the CLI uses, so the browser, its tabs, the vault and the
//! login form stay where they are: this process only translates. Logs go to
//! stderr; stdout carries protocol messages and nothing else.
//!
//! The session is this process. Page tools without `tab` or `context` work in
//! the tab the daemon leases to this pid, so one MCP connection is one page,
//! two connections never share one, and the tabs a connection opened are
//! closed when it ends (the daemon's collector is the backstop when the
//! process is killed instead).
//!
//! Credentials never pass through here: no tool takes or returns a password,
//! a vault secret or a one-time code. `login_url` hands the person the local
//! form instead, exactly like `cu login`.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread;

use crate::server::json_string;

/// Protocol revisions this server speaks, newest first. A client asking for
/// one of them gets it back; any other request gets the newest.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// What the client reads at `initialize`. pi lists the first line next to the
/// server's name in its system prompt, so it says what `cu` is in one breath.
const INSTRUCTIONS: &str = "cu drives one persistent local Chrome: navigate, read compact ref-addressable snapshots, act by ref, screenshots, console and QA marks.

- This MCP connection is one browser session: page tools without `tab` or `context` work in its own tab, opened on first use and closed when the connection ends. Another agent never lands in it.
- Read with `snapshot` (one line per element, interactive ones carry a ref such as e3) or `text` when the answer is words. Use `screenshot` only when pixels matter.
- Act by ref with `click` and `type`, or `act` for several steps in one call; `act` answers the resulting snapshot (`click` and `type` do with `snapshot: true`). A stale ref means: take a new snapshot.
- Popups and extra tabs: `tabs` lists them, any page tool takes `tab`, `close_tab` dismisses one. `context` gives an isolated cookie jar.
- Credentials: never ask for, type or pass a password or a one-time code. Call `login_url` and send the person that link; the daemon types what they enter.
- A human-verification page pauses actions on its tab: `handoff` shows the state, `handoff` with `wait` parks until the person is done.";

/// One daemon request a tool call turns into.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub method: &'static str,
    /// The route with its query string.
    pub path: String,
    pub body: Option<String>,
    /// A page command with neither `tab` nor `context`: the daemon works in
    /// this session's own tab, so the host names the session in a header.
    pub session: bool,
    /// Ask the daemon for a fresh tab for this session (`new_tab`).
    pub fresh: bool,
}

impl Call {
    fn plain(method: &'static str, path: impl Into<String>, body: Option<&str>) -> Call {
        Call {
            method,
            path: path.into(),
            body: body.map(str::to_string),
            session: false,
            fresh: false,
        }
    }
}

/// What the server needs from the `cu` binary: the daemon, reached the way the
/// CLI reaches it (and started the way the CLI starts it), and the address of
/// the login form.
pub trait Host: Send + Sync {
    /// One daemon request: the reply's head and body on a 2xx, the reply's
    /// body (or the transport error) otherwise.
    fn request(&self, call: &Call) -> Result<(String, String), String>;
    /// Where a person enters credentials.
    fn login_url(&self) -> String;
}

/// Serve MCP on `input`/`output` until the client closes `input`.
///
/// Calls on this session's own page run one at a time, in the order they
/// arrived, on a worker of their own: they act on one page, and two of them
/// racing would open two tabs, or read the page before the navigation sent
/// ahead of them. Calls that name a `tab` or a `context`, and those that touch
/// no page, run on threads of their own, so a hand-off `wait` in one tab does
/// not hold up a snapshot in another. When `input` ends, the calls still
/// running are answered before the session's tabs are closed.
pub fn serve<R, W>(input: R, output: W, host: Arc<dyn Host>) -> Result<(), String>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let (page, queue) = mpsc::channel::<Job>();
    thread::spawn(move || {
        for job in queue {
            job();
        }
    });
    let server = Arc::new(Server {
        host,
        out: Mutex::new(Box::new(output)),
        page,
        opened: Mutex::new(Vec::new()),
        running: Mutex::new(0),
        settled: Condvar::new(),
    });
    for line in input.lines() {
        match line {
            Ok(line) => server.handle(&line),
            Err(e) => {
                eprintln!("cu mcp: reading stdin: {e}");
                break;
            }
        }
    }
    let mut running = lock(&server.running);
    while *running > 0 {
        running = server
            .settled
            .wait(running)
            .unwrap_or_else(|e| e.into_inner());
    }
    drop(running);
    server.close_opened();
    Ok(())
}

/// A tool call, ready to run.
type Job = Box<dyn FnOnce() + Send>;

struct Server {
    host: Arc<dyn Host>,
    out: Mutex<Box<dyn Write + Send>>,
    /// The worker that runs the calls on this session's own page, in order.
    page: mpsc::Sender<Job>,
    /// The tabs the daemon opened for this session, closed when it ends.
    opened: Mutex<Vec<String>>,
    /// Tool calls not answered yet, and the signal that one was.
    running: Mutex<usize>,
    settled: Condvar,
}

/// One tool call not answered yet, counted until it is dropped (a panicking
/// call included, so the end of the session never waits for it forever).
struct Running<'a>(&'a Server);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *lock(&self.0.running) -= 1;
        self.0.settled.notify_all();
    }
}

/// A lock whose holder panicked is still a usable lock here: the data behind
/// it is a writer, a counter or a list of tab ids, never half-updated state.
fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Server {
    fn handle(self: &Arc<Self>, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let message = match Json::parse(line) {
            Ok(message) => message,
            Err(e) => {
                return self.send(&error(
                    &Json::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                ));
            }
        };
        if !matches!(message, Json::Object(_)) {
            return self.send(&error(
                &Json::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC object per line (batches are not supported)",
            ));
        }
        let Some(method) = message.get("method").and_then(Json::as_str) else {
            // A response to a request: this server never sends any.
            return;
        };
        // Without an id it is a notification (`notifications/initialized`,
        // `notifications/cancelled`, ...): nothing to answer.
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let params = message.get("params").cloned().unwrap_or(Json::Null);
        match method {
            "initialize" => self.send(&reply(&id, &initialize(&params))),
            "ping" => self.send(&reply(&id, "{}")),
            "tools/list" => match tools_list() {
                Ok(tools) => self.send(&reply(&id, &tools)),
                Err(e) => self.send(&error(&id, INTERNAL_ERROR, &e)),
            },
            "tools/call" => self.dispatch(id, &params),
            _ => self.send(&error(
                &id,
                METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            )),
        }
    }

    /// One message, one line. Valid JSON holds no raw line break (string
    /// contents are escaped), so the line is the message.
    fn send(&self, message: &str) {
        let mut out = lock(&self.out);
        let written = out
            .write_all(message.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .and_then(|_| out.flush());
        if let Err(e) = written {
            eprintln!("cu mcp: writing stdout: {e}");
        }
    }

    /// `tools/call`: checked and planned here, in arrival order, then run on
    /// the page worker or on a thread of its own. A call that names no known
    /// tool is a protocol error; a tool that fails answers `isError` with the
    /// reason, so the model can read it and correct the call.
    fn dispatch(self: &Arc<Self>, id: Json, params: &Json) {
        let failed = |reason: String| reply(&id, &tool_result(&[Content::Text(reason)], true));
        let Some(name) = params.get("name").and_then(Json::as_str) else {
            return self.send(&error(
                &id,
                INVALID_PARAMS,
                "tools/call needs the tool's name",
            ));
        };
        if !TOOLS.iter().any(|tool| tool.name == name) {
            return self.send(&error(
                &id,
                INVALID_PARAMS,
                &format!("unknown tool: {name}"),
            ));
        }
        let args = match params.get("arguments") {
            None | Some(Json::Null) => Json::Object(Vec::new()),
            Some(object @ Json::Object(_)) => object.clone(),
            Some(_) => return self.send(&failed("`arguments` must be an object".into())),
        };
        // `login_url` is the one tool that asks the daemon nothing.
        let call = match name {
            "login_url" => None,
            _ => match plan(name, &Args(&args)) {
                Ok(call) => Some(call),
                Err(e) => return self.send(&failed(e)),
            },
        };
        let on_page = call.as_ref().is_some_and(|call| call.session);
        let name = name.to_string();
        *lock(&self.running) += 1;
        let server = Arc::clone(self);
        let job: Job = Box::new(move || {
            let _answered = Running(&server);
            let result = match server.run(&name, &Args(&args), call.as_ref()) {
                Ok(content) => tool_result(&content, false),
                Err(e) => tool_result(&[Content::Text(e)], true),
            };
            server.send(&reply(&id, &result));
        });
        if !on_page {
            thread::spawn(job);
        } else if let Err(mpsc::SendError(job)) = self.page.send(job) {
            // The page worker is gone (it never stops on its own): run the
            // call here rather than drop it.
            job();
        }
    }

    fn run(&self, name: &str, args: &Args, call: Option<&Call>) -> Result<Vec<Content>, String> {
        let Some(call) = call else {
            return Ok(vec![Content::Text(format!(
                "Send the person this link to sign in: {}\nThey type their credentials into that local form and the daemon enters them into the browser. Navigate to the site's sign-in page first. Never ask for a password or a one-time code in chat and never pass one to a tool.",
                self.host.login_url()
            ))]);
        };
        let (head, body) = self.host.request(call).map_err(|e| e.trim().to_string())?;
        let mut content = match name {
            "screenshot" => shot_content(&body, args)?,
            "text" => vec![Content::Text(readable_text(&body))],
            _ => vec![Content::Text(readable(&body))],
        };
        if name == "close_tab"
            && let Some(id) = args.str("id")
        {
            lock(&self.opened).retain(|open| open != id);
        }
        if call.session
            && header(&head, "x-cu-tab-new").is_some()
            && let Some(tab) = header(&head, "x-cu-tab")
        {
            lock(&self.opened).push(tab.to_string());
            content.push(Content::Text(format!(
                "cu: this session works in a fresh tab ({tab}); page tools without `tab` use it, and it is closed when this MCP session ends"
            )));
        }
        Ok(content)
    }

    /// Close the tabs this session opened: the connection is over and nobody
    /// else can reach them by session. A killed process never gets here; the
    /// daemon collects its tabs once they have been idle long enough.
    fn close_opened(&self) {
        let tabs = std::mem::take(&mut *lock(&self.opened));
        for id in tabs {
            let _ = self
                .host
                .request(&Call::plain("DELETE", format!("/v1/tabs/{id}"), Some("{}")));
        }
    }
}

/// `initialize`: the protocol version, what this server offers and how to use it.
fn initialize(params: &Json) -> String {
    let asked = params.get("protocolVersion").and_then(Json::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    format!(
        "{{\"protocolVersion\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"cu\",\"title\":\"cu browser\",\"version\":{}}},\"instructions\":{}}}",
        json_string(version),
        json_string(env!("CARGO_PKG_VERSION")),
        json_string(INSTRUCTIONS)
    )
}

fn reply(id: &Json, result: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}",
        id.to_json()
    )
}

fn error(id: &Json, code: i32, message: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        id.to_json(),
        json_string(message)
    )
}

/// One block of a tool's answer.
enum Content {
    Text(String),
    Image { data: String, mime: &'static str },
}

fn tool_result(content: &[Content], is_error: bool) -> String {
    let blocks: Vec<String> = content
        .iter()
        .map(|block| match block {
            Content::Text(text) => {
                format!("{{\"type\":\"text\",\"text\":{}}}", json_string(text))
            }
            Content::Image { data, mime } => format!(
                "{{\"type\":\"image\",\"data\":{},\"mimeType\":\"{mime}\"}}",
                json_string(data)
            ),
        })
        .collect();
    format!(
        "{{\"content\":[{}],\"isError\":{is_error}}}",
        blocks.join(",")
    )
}

/// How a page tool picks its page.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    /// Not a page command.
    None,
    /// `tab` or `context`, else this session's page.
    Page,
    /// The same, plus `new_tab`.
    PageOrNew,
}

/// One tool: what the model reads in `tools/list`.
struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// The members of the input schema's `properties` (the page-targeting
    /// ones are added from `target`).
    properties: &'static str,
    required: &'static [&'static str],
    target: Target,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
}

const TAB_PROPERTY: &str = r#""tab":{"type":"string","description":"Work in this tab id (from `tabs`, e.g. a popup a click opened) instead of this session's page."}"#;
const CONTEXT_PROPERTY: &str = r#""context":{"type":"string","description":"Work in this isolated browser context (its own cookie jar), created on first use, instead of this session's page."}"#;
const NEW_TAB_PROPERTY: &str = r#""new_tab":{"type":"boolean","description":"Open a fresh page for this session instead of reusing the one it has."}"#;

const TOOLS: &[Tool] = &[
    Tool {
        name: "status",
        title: "Daemon status",
        description: "Whether the cu daemon and its browser are up.",
        properties: "",
        required: &[],
        target: Target::None,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "navigate",
        title: "Navigate",
        description: "Open a URL and wait up to ~3 s for the page to settle. `\"settled\":false` means it was still loading: take a snapshot before acting. A session saved in the vault for the domain is injected first unless `inject` is false. A navigation that cannot happen is an error, not silence.",
        properties: r#""url":{"type":"string","description":"Absolute URL to open."},"inject":{"type":"boolean","description":"Inject a vault-saved session for the domain (default true)."}"#,
        required: &["url"],
        target: Target::PageOrNew,
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: true,
    },
    Tool {
        name: "snapshot",
        title: "Page snapshot",
        description: "The page in a few hundred bytes: one line per element, interactive ones with a `ref` (e3) that `click`, `type`, `act`, `inspect` and `mark` take. Lines under `- frame:` are inside an iframe; their refs work like any other. Read this rather than a screenshot.",
        properties: "",
        required: &[],
        target: Target::Page,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "text",
        title: "Page text",
        description: "The page's visible words (prose, messages, API responses), iframes and open shadow roots included, capped at 16 000 characters. Use it when the answer is words rather than something to act on.",
        properties: "",
        required: &[],
        target: Target::Page,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "click",
        title: "Click",
        description: "Click the element with this snapshot ref. Answers the outcome: `navigated`, a `download` it started, a `popup` it opened (see `tabs`); `snapshot: true` adds the resulting page.",
        properties: r#""ref":{"type":"string","description":"Snapshot ref, e.g. e3."},"snapshot":{"type":"boolean","description":"Also answer the resulting snapshot (default false)."}"#,
        required: &["ref"],
        target: Target::Page,
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    },
    Tool {
        name: "type",
        title: "Type",
        description: "Replace the value of the field with this snapshot ref with `text`; `submit` presses Enter afterwards. Answers like `click`. Never type a password or a one-time code: use `login_url`.",
        properties: r#""ref":{"type":"string","description":"Snapshot ref of the field, e.g. e2."},"text":{"type":"string","description":"What to type."},"submit":{"type":"boolean","description":"Press Enter after typing (default false)."},"snapshot":{"type":"boolean","description":"Also answer the resulting snapshot (default false)."}"#,
        required: &["ref", "text"],
        target: Target::Page,
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    },
    Tool {
        name: "act",
        title: "Batch of actions",
        description: "Run several steps on one connection and get the resulting page back, e.g. [{\"do\":\"type\",\"ref\":\"e2\",\"text\":\"Ada\"},{\"do\":\"click\",\"ref\":\"e5\"}]. Steps: click (ref); type or fill (ref, text, clear default true, submit); press (key: Enter, Tab, Escape, arrows...); select (ref, value or label); navigate (url); scroll (dy/dx px or to: top|bottom, optional ref); tap, longpress, swipe, pinch (ref or x and y; dx, dy, scale, ms); emulate (mode: mobile|tablet|desktop|custom, width, height); wait (ms); upload (files, optional ref). `\"ok\":false` names the step that failed and why; a stale ref means take a new snapshot.",
        properties: r#""actions":{"type":"array","items":{"type":"object"},"description":"The steps, in order. Each is an object with \"do\" and that step's fields."},"snapshot":{"type":"boolean","description":"End with a fresh snapshot (default true)."}"#,
        required: &["actions"],
        target: Target::Page,
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    },
    Tool {
        name: "screenshot",
        title: "Screenshot",
        description: "Capture the page as an image (JPEG, or PNG with format png). width/height/scale emulate a viewport for the capture (restored afterwards); ref or selector clip one element (with padding px); annotate labels snapshot refs on the image (\"all\" or \"e1,e4\"); marks boxes the QA marks numbered like the report. `path` also saves it to a file; `inline: false` then skips returning the image.",
        properties: r#""format":{"type":"string","enum":["jpeg","png"],"description":"Image format (default jpeg; a path ending in .png picks png)."},"quality":{"type":"number","description":"JPEG quality, 1-100."},"width":{"type":"number","description":"Viewport width in CSS px for this capture."},"height":{"type":"number","description":"Viewport height in CSS px for this capture."},"scale":{"type":"number","description":"Device scale factor, e.g. 2 for a sharp image."},"ref":{"type":"string","description":"Clip to the element with this snapshot ref."},"selector":{"type":"string","description":"Clip to the element this CSS selector finds."},"padding":{"type":"number","description":"Px of context around a clipped element."},"annotate":{"type":"string","description":"Snapshot refs to label on the image: \"all\" or a comma list such as \"e1,e4\"."},"marks":{"type":"boolean","description":"Box this tab's QA marks, numbered like the `marks` report."},"path":{"type":"string","description":"Also save the image to this file (relative to the server's working directory)."},"inline":{"type":"boolean","description":"Return the image in the result (default true; false needs path)."}"#,
        required: &[],
        target: Target::Page,
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "tabs",
        title: "List tabs",
        description: "Every open tab: id, url, title and opener. Pass an id as `tab` to any page tool, e.g. to work in a popup a click opened.",
        properties: "",
        required: &[],
        target: Target::None,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "close_tab",
        title: "Close a tab",
        description: "Close a tab by id: a popup, or a page this session is done with. The browser's last tab is refused.",
        properties: r#""id":{"type":"string","description":"Tab id from `tabs`."}"#,
        required: &["id"],
        target: Target::None,
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "close_context",
        title: "Close a context",
        description: "Close an isolated browser context a page tool opened with `context`, its tabs and its cookie jar with it. Save its logins first with `session_save` if they are needed later.",
        properties: r#""name":{"type":"string","description":"Context name."}"#,
        required: &["name"],
        target: Target::None,
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "console",
        title: "Page console",
        description: "What the page wrote to its console, plus uncaught exceptions, CSP violations and network failures, as text lines (or JSON).",
        properties: r#""level":{"type":"string","description":"debug, log, info, trace, warn, error, exception, csp or network; a comma list; LEVEL+ for that level and above (warn+); or all (default)."},"grep":{"type":"string","description":"Only entries containing this text."},"limit":{"type":"number","description":"At most this many entries (default 200)."},"since":{"type":"number","description":"Only entries after this sequence number."},"clear":{"type":"boolean","description":"Empty the buffer after reading."},"json":{"type":"boolean","description":"Answer JSON instead of text lines."}"#,
        required: &[],
        target: Target::Page,
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    },
    Tool {
        name: "inspect",
        title: "Inspect an element",
        description: "How to find an element again and what renders it: a unique selector (`a >>> b` steps into a shadow root), test id, landmarks, box and key styles, and on a development build the component and its source file.",
        properties: r#""ref":{"type":"string","description":"Snapshot ref, e.g. e3."},"selector":{"type":"string","description":"CSS selector, instead of a ref."}"#,
        required: &[],
        target: Target::Page,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "mark",
        title: "Mark a QA finding",
        description: "Pin a QA note to an element instead of describing it in prose. `marks` collects them into a report a developer can act on. Field values are never recorded.",
        properties: r#""ref":{"type":"string","description":"Snapshot ref, e.g. e7."},"selector":{"type":"string","description":"CSS selector, instead of a ref."},"note":{"type":"string","description":"What is wrong."},"severity":{"type":"string","enum":["blocker","major","minor","nit"],"description":"How bad it is."}"#,
        required: &["note"],
        target: Target::Page,
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    },
    Tool {
        name: "marks",
        title: "QA marks report",
        description: "The QA marks of the page: JSON, or the Markdown report (format md) with per mark the note, a unique selector, test id, landmarks, box, styles and, on a dev build, the component and source file. `clear` removes them.",
        properties: r#""format":{"type":"string","enum":["json","md"],"description":"json (default) or md for the report."},"clear":{"type":"boolean","description":"Remove every mark of the page instead of reading them."}"#,
        required: &[],
        target: Target::Page,
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    },
    Tool {
        name: "upload",
        title: "Upload files",
        description: "Put files into a file input. `ref` is the input or the control that opens the picker (file inputs are usually hidden and have no ref); without it, the page's only file input. Paths are resolved against the server's working directory.",
        properties: r#""files":{"type":"array","items":{"type":"string"},"description":"Paths of the files to upload."},"ref":{"type":"string","description":"The file input, or the button that opens the picker."}"#,
        required: &["files"],
        target: Target::Page,
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    },
    Tool {
        name: "downloads",
        title: "Downloads",
        description: "Files the browser finished downloading, newest first, with their paths. The click that started a download already named it.",
        properties: "",
        required: &[],
        target: Target::None,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "handoff",
        title: "Human hand-off",
        description: "The human-verification state of the page: whether a person is needed, for how long, and the passes so far. `wait` parks until the person is done or the seconds run out (keep it under the client's tool timeout, 60 s in pi by default); `release` gives the tab back to automation once they finished.",
        properties: r#""wait":{"type":"number","description":"Seconds to wait for the person to finish."},"release":{"type":"boolean","description":"Release the pause on the tab instead of reading the state."}"#,
        required: &[],
        target: Target::Page,
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    },
    Tool {
        name: "session_save",
        title: "Save the session",
        description: "Save the browser's logins (the persistent profile, or a context's cookie jar) under a name, encrypted, to restore later with `session_load`.",
        properties: r#""name":{"type":"string","description":"Session name: letters, digits, - and _."},"context":{"type":"string","description":"Save this context's cookie jar instead of the persistent profile."}"#,
        required: &["name"],
        target: Target::None,
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "session_load",
        title: "Load a saved session",
        description: "Restore logins saved with `session_save` into the persistent profile, or live into a context.",
        properties: r#""name":{"type":"string","description":"Session name used when it was saved."},"context":{"type":"string","description":"Load into this context instead of the persistent profile."}"#,
        required: &["name"],
        target: Target::None,
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    },
    Tool {
        name: "login_url",
        title: "Login form link",
        description: "The link to the local login form, for the person to sign in: they type their credentials there and the daemon enters them into the browser. The only way credentials reach a page; never ask for them in chat.",
        properties: "",
        required: &[],
        target: Target::None,
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    },
];

/// `tools/list`. Every schema goes through the parser and out again, so what
/// the client gets is checked JSON on one line, whatever the source looks like.
fn tools_list() -> Result<String, String> {
    let mut tools = Vec::new();
    for tool in TOOLS {
        let mut properties: Vec<&str> = Vec::new();
        if !tool.properties.is_empty() {
            properties.push(tool.properties);
        }
        if tool.target != Target::None {
            properties.push(TAB_PROPERTY);
            properties.push(CONTEXT_PROPERTY);
        }
        if tool.target == Target::PageOrNew {
            properties.push(NEW_TAB_PROPERTY);
        }
        let required: Vec<String> = tool.required.iter().map(|r| json_string(r)).collect();
        let raw = format!(
            "{{\"name\":{},\"title\":{},\"description\":{},\"inputSchema\":{{\"type\":\"object\",\"properties\":{{{}}},\"required\":[{}]}},\"annotations\":{{\"title\":{},\"readOnlyHint\":{},\"destructiveHint\":{},\"idempotentHint\":{},\"openWorldHint\":{}}}}}",
            json_string(tool.name),
            json_string(tool.title),
            json_string(tool.description),
            properties.join(","),
            required.join(","),
            json_string(tool.title),
            tool.read_only,
            tool.destructive,
            tool.idempotent,
            tool.open_world,
        );
        let checked = Json::parse(&raw).map_err(|e| format!("tool {}: {e}", tool.name))?;
        tools.push(checked.to_json());
    }
    Ok(format!("{{\"tools\":[{}]}}", tools.join(",")))
}

/// A tool's arguments.
struct Args<'a>(&'a Json);

impl<'a> Args<'a> {
    fn value(&self, key: &str) -> Option<&'a Json> {
        self.0.get(key).filter(|v| !matches!(v, Json::Null))
    }

    /// A non-blank string (a number counts, as written: a session named 1).
    fn str(&self, key: &str) -> Option<&'a str> {
        match self.value(key) {
            Some(Json::String(s)) if !s.trim().is_empty() => Some(s),
            Some(Json::Number(n)) => Some(n),
            _ => None,
        }
    }

    fn required(&self, key: &str) -> Result<&'a str, String> {
        self.str(key).ok_or_else(|| format!("`{key}` is required"))
    }

    /// A string that may be empty but must be there (the text to type).
    fn text(&self, key: &str) -> Result<&'a str, String> {
        match self.value(key) {
            Some(Json::String(s)) => Ok(s),
            Some(Json::Number(n)) => Ok(n),
            _ => Err(format!("`{key}` is required and must be a string")),
        }
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, String> {
        match self.value(key) {
            None => Ok(None),
            Some(Json::Bool(b)) => Ok(Some(*b)),
            Some(Json::String(s)) if s == "true" => Ok(Some(true)),
            Some(Json::String(s)) if s == "false" => Ok(Some(false)),
            Some(_) => Err(format!("`{key}` must be true or false")),
        }
    }

    /// A number, as the text that goes into a query (a numeric string counts).
    fn number(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.value(key) {
            None => Ok(None),
            Some(Json::Number(n)) => Ok(Some(n)),
            Some(Json::String(s)) if s.trim().parse::<f64>().is_ok() => Ok(Some(s.trim())),
            Some(_) => Err(format!("`{key}` must be a number")),
        }
    }

    /// A snapshot ref, checked before it goes anywhere.
    fn reference(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.str(key) {
            Some(r) if crate::actions::valid_ref(r) => Ok(Some(r)),
            Some(r) => Err(format!("\"{r}\" is not a snapshot ref (e.g. e3)")),
            None => Ok(None),
        }
    }
}

/// A page command: in `tab`, in `context`, or (neither) in this session's page.
fn page(
    args: &Args,
    method: &'static str,
    route: &str,
    mut query: Vec<(&'static str, String)>,
    body: Option<String>,
) -> Result<Call, String> {
    let tab = args.str("tab");
    let context = args.str("context");
    let fresh = args.flag("new_tab")?.unwrap_or(false);
    if tab.is_some() && context.is_some() {
        return Err("give either `tab` or `context`, not both".into());
    }
    if fresh && (tab.is_some() || context.is_some()) {
        return Err(
            "`new_tab` opens a page of this session's own; leave out `tab` and `context`".into(),
        );
    }
    if let Some(tab) = tab {
        query.push(("tab", tab.to_string()));
    }
    if let Some(context) = context {
        query.push(("context", context.to_string()));
    }
    Ok(Call {
        method,
        path: with_query(route, &query),
        body,
        session: tab.is_none() && context.is_none(),
        fresh,
    })
}

fn with_query(route: &str, query: &[(&str, String)]) -> String {
    if query.is_empty() {
        return route.to_string();
    }
    let pairs: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{key}={}", url_encode(value)))
        .collect();
    format!("{route}?{}", pairs.join("&"))
}

/// Percent-encode a query value (a CSS selector has `#`, `&`, spaces...).
fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The daemon request a tool call is.
fn plan(name: &str, args: &Args) -> Result<Call, String> {
    match name {
        "status" => Ok(Call::plain("GET", "/v1/status", None)),
        "tabs" => Ok(Call::plain("GET", "/v1/tabs", None)),
        "downloads" => Ok(Call::plain("GET", "/v1/downloads", None)),
        "close_tab" => {
            let id = args.required("id")?;
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!("\"{id}\" is not a tab id (see `tabs`)"));
            }
            Ok(Call::plain("DELETE", format!("/v1/tabs/{id}"), Some("{}")))
        }
        "close_context" => {
            let context = args.required("name")?;
            if !crate::server::valid_name(context) {
                return Err("a context name is letters, digits, - and _".into());
            }
            Ok(Call::plain(
                "DELETE",
                format!("/v1/contexts/{context}"),
                Some("{}"),
            ))
        }
        "session_save" | "session_load" => {
            let session = args.required("name")?;
            if !crate::server::valid_name(session) {
                return Err("a session name is letters, digits, - and _".into());
            }
            let mut path = format!("/v1/session/{session}");
            if name == "session_load" {
                path.push_str("/load");
            }
            if let Some(context) = args.str("context") {
                path = with_query(&path, &[("context", context.to_string())]);
            }
            Ok(Call::plain("POST", path, Some("{}")))
        }
        "navigate" => {
            let url = args.required("url")?;
            let inject = args.flag("inject")?.unwrap_or(true);
            let body = format!("{{\"url\":{},\"inject\":{inject}}}", json_string(url));
            page(args, "POST", "/v1/navigate", Vec::new(), Some(body))
        }
        "snapshot" => page(args, "GET", "/v1/snapshot", Vec::new(), None),
        "text" => page(args, "GET", "/v1/text", Vec::new(), None),
        "click" => {
            let reference = args.reference("ref")?.ok_or("`ref` is required")?;
            let body = format!(
                "{{\"ref\":{}{}}}",
                json_string(reference),
                snapshot_flag(args)?
            );
            page(args, "POST", "/v1/click", Vec::new(), Some(body))
        }
        "type" => {
            let reference = args.reference("ref")?.ok_or("`ref` is required")?;
            let text = args.text("text")?;
            let submit = args.flag("submit")?.unwrap_or(false);
            let body = format!(
                "{{\"ref\":{},\"text\":{},\"submit\":{submit}{}}}",
                json_string(reference),
                json_string(text),
                snapshot_flag(args)?
            );
            page(args, "POST", "/v1/type", Vec::new(), Some(body))
        }
        "act" => {
            let actions = actions(args.value("actions"))?;
            let snapshot = match args.flag("snapshot")? {
                Some(false) => ",\"snapshot\":false",
                _ => "",
            };
            let body = format!("{{\"actions\":{actions}{snapshot}}}");
            page(args, "POST", "/v1/act", Vec::new(), Some(body))
        }
        "upload" => {
            let files = match args.value("files") {
                Some(Json::Array(items)) => items
                    .iter()
                    .map(|item| item.as_str().ok_or("`files` holds paths (strings)"))
                    .collect::<Result<Vec<_>, _>>()?,
                Some(Json::String(one)) => vec![one.as_str()],
                _ => return Err("`files` is required: a list of paths".into()),
            };
            if files.is_empty() {
                return Err("`files` is empty".into());
            }
            // The browser reads the files on this machine: absolute and
            // existing here, or the page gets an empty File.
            let files = files
                .iter()
                .map(|f| {
                    fs::canonicalize(f)
                        .map(|p| json_string(&p.to_string_lossy()))
                        .map_err(|e| format!("{f}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let reference = match args.reference("ref")? {
                Some(r) => format!(",\"ref\":{}", json_string(r)),
                None => String::new(),
            };
            // Through a batch, which works in the session's page like every
            // other page command.
            let body = format!(
                "{{\"actions\":[{{\"do\":\"upload\"{reference},\"files\":[{}]}}],\"snapshot\":false}}",
                files.join(",")
            );
            page(args, "POST", "/v1/act", Vec::new(), Some(body))
        }
        "screenshot" => {
            let path = args.str("path");
            let png = match args.str("format") {
                Some("png") => true,
                Some("jpeg" | "jpg") => false,
                Some(other) => return Err(format!("format is jpeg or png, not {other}")),
                None => path.is_some_and(|p| p.to_ascii_lowercase().ends_with(".png")),
            };
            if args.flag("inline")? == Some(false) && path.is_none() {
                return Err("`inline: false` needs a `path` to save the image to".into());
            }
            let mut query = vec![("format", (if png { "png" } else { "jpeg" }).to_string())];
            for key in ["quality", "width", "height", "scale", "padding"] {
                if let Some(n) = args.number(key)? {
                    query.push((key, n.to_string()));
                }
            }
            let reference = args.reference("ref")?;
            let selector = args.str("selector");
            if reference.is_some() && selector.is_some() {
                return Err("give either `ref` or `selector`, not both".into());
            }
            if let Some(r) = reference {
                query.push(("ref", r.to_string()));
            }
            if let Some(css) = selector {
                query.push(("selector", css.to_string()));
            }
            let annotate = match args.value("annotate") {
                Some(Json::Array(items)) => {
                    let refs: Vec<&str> = items.iter().filter_map(Json::as_str).collect();
                    (!refs.is_empty()).then(|| refs.join(","))
                }
                _ => args.str("annotate").map(str::to_string),
            };
            if let Some(annotate) = annotate {
                query.push(("annotate", annotate));
            }
            if args.flag("marks")? == Some(true) {
                query.push(("marks", "1".into()));
            }
            page(args, "GET", "/v1/screenshot", query, None)
        }
        "console" => {
            let mut query = Vec::new();
            for key in ["level", "grep"] {
                if let Some(value) = args.str(key) {
                    query.push((key, value.to_string()));
                }
            }
            for key in ["limit", "since"] {
                if let Some(n) = args.number(key)? {
                    query.push((key, n.to_string()));
                }
            }
            if args.flag("clear")? == Some(true) {
                query.push(("clear", "1".into()));
            }
            if args.flag("json")? != Some(true) {
                query.push(("format", "text".into()));
            }
            page(args, "GET", "/v1/console", query, None)
        }
        "inspect" => {
            let query = element(args)?;
            page(args, "GET", "/v1/inspect", vec![query], None)
        }
        "mark" => {
            let (key, value) = element(args)?;
            let note = args.required("note")?;
            let severity = match args.str("severity") {
                Some(s) => format!(",\"severity\":{}", json_string(s)),
                None => String::new(),
            };
            let body = format!(
                "{{\"{key}\":{},\"note\":{}{severity}}}",
                json_string(&value),
                json_string(note)
            );
            page(args, "POST", "/v1/marks", Vec::new(), Some(body))
        }
        "marks" => {
            if args.flag("clear")? == Some(true) {
                return page(args, "DELETE", "/v1/marks", Vec::new(), Some("{}".into()));
            }
            let query = match args.str("format") {
                Some("md") => vec![("format", "md".to_string())],
                Some("json") | None => Vec::new(),
                Some(other) => return Err(format!("format is json or md, not {other}")),
            };
            page(args, "GET", "/v1/marks", query, None)
        }
        "handoff" => {
            if args.flag("release")? == Some(true) {
                return page(
                    args,
                    "POST",
                    "/v1/challenge/release",
                    Vec::new(),
                    Some("{}".into()),
                );
            }
            match args.number("wait")? {
                Some(seconds) if seconds.parse::<f64>().is_ok_and(|s| s > 0.0) => page(
                    args,
                    "GET",
                    "/v1/challenge",
                    vec![("wait", seconds.to_string())],
                    None,
                ),
                _ => page(args, "GET", "/v1/handoff", Vec::new(), None),
            }
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// `,"snapshot":true` when a single action asks for the resulting page (the
/// daemon leaves it out of a click or a type by default).
fn snapshot_flag(args: &Args) -> Result<&'static str, String> {
    Ok(match args.flag("snapshot")? {
        Some(true) => ",\"snapshot\":true",
        _ => "",
    })
}

/// `ref` or `selector` (exactly one), as the query pair the daemon takes.
fn element(args: &Args) -> Result<(&'static str, String), String> {
    match (args.reference("ref")?, args.str("selector")) {
        (Some(_), Some(_)) => Err("give either `ref` or `selector`, not both".into()),
        (Some(r), None) => Ok(("ref", r.to_string())),
        (None, Some(css)) => Ok(("selector", css.to_string())),
        (None, None) => Err("`ref` or `selector` is required".into()),
    }
}

/// The `actions` of `act` as the JSON array the daemon parses: a list of
/// objects, or that list as a JSON string (some models send it that way).
/// `"do"` goes first in every step, ahead of any text that might mention it.
fn actions(value: Option<&Json>) -> Result<String, String> {
    let parsed;
    let list = match value {
        Some(Json::Array(items)) => items,
        Some(Json::String(text)) => {
            parsed = Json::parse(text).map_err(|e| format!("`actions` is not JSON: {e}"))?;
            match &parsed {
                Json::Array(items) => items,
                _ => return Err("`actions` must be a list of steps".into()),
            }
        }
        _ => return Err("`actions` is required: a list of steps".into()),
    };
    if list.is_empty() {
        return Err("`actions` is empty".into());
    }
    let mut steps = Vec::new();
    for step in list {
        let Json::Object(members) = step else {
            return Err("every step is an object such as {\"do\":\"click\",\"ref\":\"e3\"}".into());
        };
        let mut ordered: Vec<(String, Json)> = members
            .iter()
            .filter(|(key, _)| key == "do")
            .cloned()
            .collect();
        ordered.extend(members.iter().filter(|(key, _)| key != "do").cloned());
        steps.push(Json::Object(ordered).to_json());
    }
    Ok(format!("[{}]", steps.join(",")))
}

/// A daemon reply as the model reads it best: a reply carrying a page
/// snapshot answers the rest of the reply on the first line and the snapshot
/// as plain lines below it, instead of one long JSON-escaped string.
fn readable(body: &str) -> String {
    let body = body.trim();
    let Ok(Json::Object(members)) = Json::parse(body) else {
        return body.to_string();
    };
    let Some(snapshot) = members
        .iter()
        .find(|(key, _)| key == "snapshot")
        .and_then(|(_, v)| v.as_str())
    else {
        return body.to_string();
    };
    let rest: Vec<(String, Json)> = members
        .iter()
        .filter(|(key, _)| key != "snapshot")
        .cloned()
        .collect();
    if rest.is_empty() {
        snapshot.trim_end().to_string()
    } else {
        format!("{}\n{}", Json::Object(rest).to_json(), snapshot.trim_end())
    }
}

/// `GET /v1/text` as the words themselves, with a note when they were cut.
fn readable_text(body: &str) -> String {
    let body = body.trim();
    let Ok(Json::Object(members)) = Json::parse(body) else {
        return body.to_string();
    };
    if members
        .iter()
        .any(|(key, _)| key != "text" && key != "truncated")
    {
        return body.to_string();
    }
    let Some(text) = members
        .iter()
        .find(|(key, _)| key == "text")
        .and_then(|(_, v)| v.as_str())
    else {
        return body.to_string();
    };
    let truncated = members
        .iter()
        .any(|(key, v)| key == "truncated" && *v == Json::Bool(true));
    if truncated {
        format!("{}\n[cut at the 16 000-character cap]", text.trim())
    } else {
        text.trim().to_string()
    }
}

/// The screenshot reply as an image block, and its size (and where it was
/// saved) as a line of text.
fn shot_content(body: &str, args: &Args) -> Result<Vec<Content>, String> {
    let reply =
        Json::parse(body.trim()).map_err(|_| "the daemon's screenshot reply is not JSON")?;
    let png = reply.get("format").and_then(Json::as_str) == Some("png");
    let field = if png { "png_base64" } else { "jpeg_base64" };
    let Some(data) = reply.get(field).and_then(Json::as_str) else {
        return Err(reply
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("the daemon returned no image")
            .to_string());
    };
    let Json::Object(members) = &reply else {
        return Err("the daemon's screenshot reply is not an object".into());
    };
    let mut meta: Vec<(String, Json)> = members
        .iter()
        .filter(|(key, _)| key != field)
        .cloned()
        .collect();
    if let Some(path) = args.str("path") {
        let bytes = crate::server::decode_base64(data)?;
        fs::write(path, bytes).map_err(|e| format!("{path}: {e}"))?;
        meta.push(("saved".into(), Json::String(absolute(Path::new(path)))));
    }
    let mut content = Vec::new();
    if args.flag("inline")? != Some(false) {
        content.push(Content::Image {
            data: data.to_string(),
            mime: if png { "image/png" } else { "image/jpeg" },
        });
    }
    content.push(Content::Text(Json::Object(meta).to_json()));
    Ok(content)
}

fn absolute(path: &Path) -> String {
    let full: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    full.to_string_lossy().into_owned()
}

/// A header of a reply head, by name.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A JSON value, read whole. The rest of the crate reads known fields out of
/// known replies; a protocol peer sends arbitrary nesting (a tool's arguments,
/// a list of steps), so this side parses for real.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A number exactly as written, so a request id is echoed back verbatim.
    Number(String),
    String(String),
    Array(Vec<Json>),
    /// Members in the order they came.
    Object(Vec<(String, Json)>),
}

/// Deeper than any message this server needs, shallow enough that a hostile
/// one cannot exhaust the stack.
const MAX_DEPTH: usize = 128;

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
            depth: 0,
        };
        parser.space();
        let value = parser.value()?;
        parser.space();
        if parser.at != parser.bytes.len() {
            return Err(format!("unexpected text at byte {}", parser.at));
        }
        Ok(value)
    }

    /// An object's member, by key.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// Compact JSON, on one line.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(n),
            Json::String(s) => out.push_str(&json_string(s)),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&json_string(key));
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.bytes.get(self.at) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'n') => self.word("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(format!("unexpected character at byte {}", self.at)),
            None => Err("unexpected end of input".into()),
        }
    }

    fn nest(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nested too deep".into());
        }
        self.at += 1;
        self.space();
        Ok(())
    }

    fn object(&mut self) -> Result<Json, String> {
        self.nest()?;
        let mut members = Vec::new();
        if !self.eat(b'}') {
            loop {
                self.space();
                if self.bytes.get(self.at) != Some(&b'"') {
                    return Err(format!("expected a key at byte {}", self.at));
                }
                let key = self.string()?;
                self.space();
                if !self.eat(b':') {
                    return Err(format!("expected ':' at byte {}", self.at));
                }
                self.space();
                members.push((key, self.value()?));
                self.space();
                if self.eat(b',') {
                    continue;
                }
                if self.eat(b'}') {
                    break;
                }
                return Err(format!("expected ',' or '}}' at byte {}", self.at));
            }
        }
        self.depth -= 1;
        Ok(Json::Object(members))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.nest()?;
        let mut items = Vec::new();
        if !self.eat(b']') {
            loop {
                self.space();
                items.push(self.value()?);
                self.space();
                if self.eat(b',') {
                    continue;
                }
                if self.eat(b']') {
                    break;
                }
                return Err(format!("expected ',' or ']' at byte {}", self.at));
            }
        }
        self.depth -= 1;
        Ok(Json::Array(items))
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("unexpected character at byte {}", self.at))
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        self.eat(b'-');
        match self.bytes.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(format!("invalid number at byte {start}")),
        }
        if self.eat(b'.') && self.digits() == 0 {
            return Err(format!("invalid number at byte {start}"));
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(format!("invalid number at byte {start}"));
            }
        }
        // Only ASCII digits and signs were consumed.
        Ok(Json::Number(
            String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned(),
        ))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .filter(|d| d.iter().all(u8::is_ascii_hexdigit))
            .ok_or_else(|| format!("invalid \\u escape at byte {}", self.at))?;
        self.at += 4;
        // Four ASCII hex digits always parse.
        Ok(u32::from_str_radix(&String::from_utf8_lossy(digits), 16).unwrap_or(0xFFFD))
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.at) else {
                return Err("unterminated string".into());
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.at) else {
                        return Err("unterminated string".into());
                    };
                    self.at += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return Err(format!("invalid escape at byte {}", self.at - 1)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0x00..=0x1f => {
                    return Err(format!(
                        "raw control character in a string at byte {}",
                        self.at - 1
                    ));
                }
                _ => out.push(byte),
            }
        }
        // The input was a `&str` and escapes add whole characters, so this
        // is valid UTF-8.
        String::from_utf8(out).map_err(|_| "a string is not valid UTF-8".into())
    }

    /// The character of a `\u` escape. Outside the BMP (emoji) it is a
    /// UTF-16 surrogate pair, `\ud83e\udd16`: the two halves are joined.
    fn unicode(&mut self) -> Result<char, String> {
        let code = self.hex4()?;
        if !(0xD800..0xDC00).contains(&code) {
            return Ok(char::from_u32(code).unwrap_or('\u{fffd}'));
        }
        if !self.bytes[self.at..].starts_with(b"\\u") {
            return Ok('\u{fffd}');
        }
        let resume = self.at;
        self.at += 2;
        let low = self.hex4()?;
        if (0xDC00..0xE000).contains(&low) {
            Ok(
                char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00))
                    .unwrap_or('\u{fffd}'),
            )
        } else {
            // A lone high half: replace it and read the next escape on its own.
            self.at = resume;
            Ok('\u{fffd}')
        }
    }
}
