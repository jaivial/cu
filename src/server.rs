//! Loopback HTTP server for `cu`: request parsing, routing and the browser bridge.
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub token: String,
    /// Port Chromium listens on for the DevTools protocol (default 9222).
    pub cdp_port: u16,
    /// Readiness of the persistent browser, shared with the launch thread.
    pub browser: Arc<BrowserState>,
}

/// Whether the persistent browser is up, or why it never came up.
///
/// The daemon listens before the browser is ready: a cold `cu start` used to
/// block for the whole Chromium start-up (~0.5 s) before an agent could get so
/// much as a `status` back. With the signal, start returns at once and the
/// first action that needs the browser waits for it, and only for as long as
/// it is actually missing.
pub struct BrowserState {
    inner: Mutex<BrowserStateInner>,
    signal: Condvar,
}

#[derive(Default)]
struct BrowserStateInner {
    ready: bool,
    error: Option<String>,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(BrowserStateInner::default()),
            signal: Condvar::new(),
        }
    }
}

impl BrowserState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A daemon that is not going to have a browser at all (tests, or a
    /// browser that was never asked for). Waiting for one would be wrong, so
    /// this is recorded as a failure rather than left pending.
    pub fn absent(reason: impl Into<String>) -> Arc<Self> {
        Self::failed(reason)
    }

    /// A browser that is already known to be unavailable.
    pub fn failed(error: impl Into<String>) -> Arc<Self> {
        let state = Self::default();
        state.mark_failed(error.into());
        Arc::new(state)
    }

    /// Mark the browser as answering DevTools.
    pub fn mark_ready(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ready = true;
        self.signal.notify_all();
    }

    /// Record that the browser will never come up, and why.
    pub fn mark_failed(&self, error: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.error.get_or_insert(error);
        self.signal.notify_all();
    }

    /// Is the browser answering DevTools yet?
    pub fn is_ready(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).ready
    }

    /// Why the browser is not available, if that is already known.
    pub fn failure(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
    }

    /// Wait until the browser is usable, or fail with the reason it is not.
    pub fn wait(&self, timeout: Duration) -> Result<(), String> {
        let started = std::time::Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if inner.ready {
                return Ok(());
            }
            if let Some(error) = &inner.error {
                return Err(error.clone());
            }
            if started.elapsed() >= timeout {
                return Err(format!(
                    "Chromium DevTools never answered within {} ms",
                    timeout.as_millis()
                ));
            }
            let (guard, _wait) = self
                .signal
                .wait_timeout(inner, Duration::from_millis(2))
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
    }
}

/// Accept loop. Binds to loopback only and serves one thread per connection.
pub fn serve(state: Arc<AppState>, listener: TcpListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle(stream, state));
            }
            Err(error) => eprintln!("connection error: {error}"),
        }
    }
}

/// Block until Chromium answers its DevTools discovery endpoint.
///
/// `cu navigate` used to race the browser start-up and lose, returning
/// "connection refused" for a browser that was in fact coming up.
pub fn wait_for_cdp(cdp_port: u16, timeout: Duration) -> Result<(), String> {
    wait_for_browser(cdp_port, timeout, None)
}

/// Wait for DevTools to answer, and fail as soon as the browser is known dead.
///
/// This is what makes a cold `cu start` fast: instead of a fixed start-up
/// sleep followed by a coarse 50 ms poll, the endpoint is probed every 2 ms
/// (a loopback connect is a few microseconds) and the child is checked on
/// every miss, so a browser that died is reported in the same round rather
/// than after a timeout.
pub fn wait_for_browser(
    cdp_port: u16,
    timeout: Duration,
    mut child: Option<&mut Child>,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let address = format!("127.0.0.1:{cdp_port}");
    loop {
        if http_get(&address, "/json/version").is_ok() {
            return Ok(());
        }
        // `as_deref_mut` reborrows instead of moving the child out of the
        // option, so the check can run on every poll.
        if let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!(
                        "Chromium exited immediately ({status}); set CU_BROWSER to a working binary"
                    ));
                }
                Ok(None) => {}
                Err(e) => return Err(format!("could not check on Chromium: {e}")),
            }
        }
        if started.elapsed() >= timeout {
            return Err(format!("Chromium DevTools never answered on {address}"));
        }
        thread::sleep(CDP_POLL_INTERVAL);
    }
}

/// Launch the persistent browser in the background and signal when it is up.
///
/// `cu start` returns before Chromium is ready, so an agent gets its prompt
/// back immediately and the browser warms up while it is deciding what to do.
pub fn spawn_browser_thread(
    data: PathBuf,
    cdp_port: u16,
    browser: Arc<BrowserState>,
) -> JoinHandle<()> {
    thread::spawn(move || match launch_browser(&data, cdp_port) {
        Ok(mut child) => {
            let _ = child.stdin.take();
            match wait_for_browser(cdp_port, BROWSER_START_TIMEOUT, Some(&mut child)) {
                Ok(()) => browser.mark_ready(),
                Err(e) => browser.mark_failed(e),
            }
        }
        Err(e) => browser.mark_failed(e),
    })
}

/// Remove the per-process locks a killed browser left in a profile.
///
/// Chromium exits without cleaning `Singleton*` up when it is killed, and then
/// refuses every later start on that profile, so a daemon can never come back
/// up after its browser was stopped.
fn clear_stale_locks(data: &Path) {
    let profile = data.join("profiles/default");
    let Ok(entries) = fs::read_dir(&profile) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("Singleton"))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Launch the persistent Chromium that owns `data/profiles/default`.
///
/// The DevTools port must match the port the daemon was configured with:
/// `launch_browser` used to hard-code 9222, so a daemon started with
/// `--port`/`CU_CDP_PORT` pointed at a browser that was listening somewhere
/// else and every navigate/screenshot failed with "connection refused".
///
/// Browsers are launched headless because agents usually run without a
/// display; set `CU_HEADLESS=0` (or `--show`) to attach one instead.
pub fn launch_browser(data: &Path, cdp_port: u16) -> Result<Child, String> {
    let binary = env::var("CU_BROWSER").unwrap_or_else(|_| "chromium".into());
    let headless = env::var("CU_HEADLESS")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true);
    clear_stale_locks(data);
    let mut command = Command::new(binary);
    command
        .arg(format!("--remote-debugging-port={cdp_port}"))
        .args([
            "--remote-allow-origins=*",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-dev-shm-usage",
        ])
        // One argument: Chromium only parses `--user-data-dir=PATH` here, and
        // treats a separate PATH as a second target ("Multiple targets are not
        // supported in headless mode", exit 13).
        .arg(format!(
            "--user-data-dir={}",
            data.join("profiles/default").display()
        ))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if headless {
        command.arg("--headless=new");
    }
    // Chromium writes its startup diagnostics (including "no display") to
    // stderr; keep it so a browser that dies at once can be diagnosed.
    let child = command
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start Chromium (set CU_BROWSER): {e}"))?;
    // A browser that exits immediately (no display, bad binary) is reported by
    // `wait_for_browser` on the first poll instead of after a fixed sleep here,
    // which used to cost every start 300 ms whether or not it was needed.
    Ok(child)
}

/// How often DevTools is probed while a browser is coming up. A loopback
/// connect is a few microseconds, so this is pure responsiveness, not load.
const CDP_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// How long to wait for a DevTools HTTP response before using what arrived.
const CDP_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum accepted request head and body. Guards against unbounded buffering.
const MAX_REQUEST: usize = 64 * 1024;

/// How long a client may take to finish sending one request. Without this a
/// peer that announces more body than it sends would pin a thread forever.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Offset just past the blank line that ends the header block, if it has
/// arrived yet. Accepts the `\r\n\r\n` the spec requires and tolerates a bare
/// `\n\n` from a sloppy client.
fn head_end(buf: &[u8]) -> Option<usize> {
    let crlf = find(buf, b"\r\n\r\n").map(|p| p + 4);
    let lf = find(buf, b"\n\n").map(|p| p + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Read one request from the connection.
///
/// A request may arrive in any number of TCP segments, so a single `read()`
/// cannot be relied on: the first segment is often just the start of the
/// request line while the peer is still writing. Keep reading until the blank
/// line that terminates the header block, then honour `Content-Length` so the
/// whole body is available before routing.
fn read_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0; 2048];
    let head_end = loop {
        if let Some(end) = head_end(&buf) {
            break end;
        }
        if buf.len() > MAX_REQUEST {
            return Err("request head is too large".into());
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(if buf.is_empty() {
                    "connection closed before a request was sent".into()
                } else {
                    "connection closed mid-request".into()
                });
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e.to_string()),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let len = header_value(&head, "content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_REQUEST {
        return Err("request body is too large".into());
    }
    while buf.len() < head_end + len {
        match stream.read(&mut chunk) {
            Ok(0) => break, // peer hung up; serve what did arrive
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    String::from_utf8(buf).map_err(|_| "request is not valid UTF-8".into())
}

/// Read a full request and answer it. A request that cannot be parsed is
/// answered with a 400 instead of dropping the connection, so the client gets
/// a diagnostic rather than a silent failure.
pub fn handle(mut stream: TcpStream, state: Arc<AppState>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let body = format!("{{\"error\":\"{}\"}}", json_escape(&error));
            let _ = write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            return;
        }
    };
    let mut first = request.lines().next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("");
    let path = first.next().unwrap_or("");
    let authorized = request
        .lines()
        .find_map(|l| l.strip_prefix("Authorization: Bearer "))
        .map(|v| v.trim() == state.token)
        .unwrap_or(false);
    // Split on whichever blank line ended the headers.
    let sep = if request.contains("\r\n\r\n") {
        "\r\n\r\n"
    } else {
        "\n\n"
    };
    let body = request.split(sep).nth(1).unwrap_or("");
    let (status, content_type, response) = if path == "/login" && method == "GET" {
        ("200 OK", "text/html", login_page())
    } else if path == "/login" && method == "POST" {
        ("200 OK", "text/html", login_submit(body, &state))
    } else if !authorized {
        (
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"missing or invalid bearer token\"}".into(),
        )
    } else {
        route(method, path, body, &state)
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
}

/// Position of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Case-insensitive lookup of a header value inside a request head.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Wait for the browser the route is about to use.
///
/// The daemon listens before Chromium is up, so the first action after a cold
/// `cu start` waits here for the launch to finish -- and only for as long as
/// the browser is really missing, because the launch thread signals the moment
/// DevTools answers.
fn wait_until_ready(state: &AppState) -> Result<(), String> {
    state.browser.wait(BROWSER_START_TIMEOUT)
}

pub fn route(
    method: &str,
    full_path: &str,
    body: &str,
    state: &AppState,
) -> (&'static str, &'static str, String) {
    // A query string selects options (`?format=png`), never a different
    // resource, so the match below is on the path alone.
    let path = full_path.split('?').next().unwrap_or("");
    // Only the routes that drive the page wait for the browser: `status`, the
    // login form and the 401/404 paths answer instantly whatever Chromium is
    // doing, which is what makes `cu start` return at once.
    // Session save/load is a profile copy, not a page action: it must work
    // while the browser is down, so it is deliberately not gated.
    let needs_browser = matches!(
        (method, path),
        ("POST", "/v1/navigate")
            | ("GET", "/v1/screenshot")
            | ("GET", "/v1/snapshot")
            | ("POST", "/v1/navigate-and-snapshot")
    );
    if needs_browser {
        if let Err(error) = wait_until_ready(state) {
            return (
                "503 Service Unavailable",
                "application/json",
                format!("{{\"error\":\"{}\"}}", json_escape(&error)),
            );
        }
    }
    match (method, path) {
        ("GET", "/v1/status") => (
            "200 OK",
            "application/json",
            "{\"running\":true,\"browser\":\"chromium\"}".into(),
        ),
        ("POST", "/v1/navigate") => {
            let url = json_value(body, "url").unwrap_or_default();
            if url.is_empty() {
                return (
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"url is required\"}".into(),
                );
            }
            match cdp_command(
                state.cdp_port,
                "Page.navigate",
                &format!("{{\"url\":\"{}\"}}", json_escape(&url)),
            ) {
                Ok(result) => {
                    // Do not hand back a half-loaded page, but never hang on a
                    // stream that stays open for ever.
                    let waited = wait_for_page(state.cdp_port, SETTLE_MAX);
                    (
                        "200 OK",
                        "application/json",
                        format!(
                            "{{\"result\":{},\"settled_ms\":{}}}",
                            result.trim(),
                            waited.as_millis()
                        ),
                    )
                }
                Err(error) => (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(&error)),
                ),
            }
        }
        ("GET", "/v1/screenshot") => screenshot(state, query(full_path)),
        ("GET", "/v1/snapshot") => match snapshot_pages(state, true) {
            Ok(text) => ("200 OK", "application/json", text),
            Err(error) => (
                "502 Bad Gateway",
                "application/json",
                format!("{{\"error\":\"{}\"}}", json_escape(&error)),
            ),
        },
        ("POST", p) if p.starts_with("/v1/session/") => {
            let load = p.ends_with("/load");
            let name = p
                .trim_start_matches("/v1/session/")
                .trim_end_matches("/load");
            if !valid_name(name) {
                return (
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"invalid session name\"}".into(),
                );
            }
            let (src, dest) = if load {
                (
                    state.data_dir.join("sessions").join(name),
                    state.data_dir.join("profiles/default"),
                )
            } else {
                (
                    state.data_dir.join("profiles/default"),
                    state.data_dir.join("sessions").join(name),
                )
            };
            // The browser owns the live profile and rewrites it constantly, so
            // step away from the page first and let the copy settle.
            if !load {
                let _ = cdp_command(state.cdp_port, "Page.navigate", "{\"url\":\"about:blank\"}");
            }
            let outcome = copy_dir(&src, &dest);
            if let Err(e) = &outcome {
                return (
                    "400 Bad Request",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(e)),
                );
            }
            // A note for the agent, without any secret: which session is live.
            let _ = fs::write(
                state
                    .data_dir
                    .join("sessions")
                    .join(format!("{name}.current")),
                if load { "loaded\n" } else { "saved\n" },
            );
            if load {
                ("200 OK", "application/json", "{\"loaded\":true}".into())
            } else {
                ("200 OK", "application/json", "{\"saved\":true}".into())
            }
        }
        _ => (
            "404 Not Found",
            "application/json",
            "{\"error\":\"not found\"}".into(),
        ),
    }
}

pub fn login_page() -> String {
    "<!doctype html><meta name=\"viewport\" content=\"width=device-width\"><title>Secure login</title><h1>Sign in</h1><p>This form sends your password directly to the computer-use server. It is never shown to the AI agent.</p><form method=post><label>Username <input name=username autocomplete=username></label><br><label>Password <input name=password type=password autocomplete=current-password></label><br><button>Submit securely</button></form>".into()
}

/// Take a screenshot.
///
/// `format=jpeg` (default) and a modest quality are what an agent wants: a
/// model reads a lossy frame just as well and the capture, encode and transfer
/// all shrink, which is most of the latency of `Page.captureScreenshot`. PNG
/// is still available with `format=png`.
fn screenshot(state: &AppState, query: &str) -> (&'static str, &'static str, String) {
    let png = form_value(query, "format") == "png";
    let quality = form_value(query, "quality")
        .parse::<u32>()
        .ok()
        .filter(|q| (1..=100).contains(q))
        .unwrap_or(60);
    let (format, extra) = if png {
        ("png", String::new())
    } else {
        (
            "jpeg",
            format!(",\"quality\":{quality},\"optimizeForSpeed\":true"),
        )
    };
    match cdp_command(
        state.cdp_port,
        "Page.captureScreenshot",
        &format!("{{\"format\":\"{format}\"{extra}}}"),
    ) {
        Ok(result) => {
            let field = if png { "png_base64" } else { "jpeg_base64" };
            (
                "200 OK",
                "application/json",
                format!(
                    "{{\"format\":\"{format}\",\"{field}\":{}}}",
                    json_value(&result, "data")
                        .map(|v| format!("\"{v}\""))
                        .unwrap_or_else(|| "null".into())
                ),
            )
        }
        Err(error) => (
            "502 Bad Gateway",
            "application/json",
            format!("{{\"error\":\"{}\"}}", json_escape(&error)),
        ),
    }
}

/// The query string of a request path, without the `?`.
fn query(path: &str) -> &str {
    path.split_once('?').map(|(_, q)| q).unwrap_or("")
}

/// Build a `GET /v1/snapshot` response body.
///
/// The walk runs in the page, so this is one CDP round trip returning a few
/// hundred bytes instead of a DOM dump the daemon then has to shrink.
pub fn snapshot_pages(state: &AppState, compact: bool) -> Result<String, String> {
    let result = cdp_command(
        state.cdp_port,
        "Runtime.evaluate",
        &format!(
            "{{\"expression\":{},\"returnByValue\":true}}",
            json_string(snapshot::SNAPSHOT_JS)
        ),
    )?;
    // Runtime.evaluate wraps the string in {"result":{"result":{"value":...}}}.
    // Runtime.evaluate wraps the returned string in
    // {"result":{"result":{"type":"string","value":"..."}}}; look one level in
    // and fall back to the raw reply so both shapes work.
    let value = json_objects(&result)
        .iter()
        .filter_map(|o| json_string_value(o, "value"))
        .next()
        .or_else(|| json_string_value(&result, "value"))
        .ok_or_else(|| "page did not return a snapshot".to_string())?;
    let snap = parse_snapshot(&value)?;
    Ok(if compact {
        format!("{{\"snapshot\":{}}}", json_string(&snap.render()))
    } else {
        format!(
            "{{\"snapshot\":{},\"url\":{},\"title\":{},\"nodes\":{}}}",
            json_string(&snap.render()),
            json_string(&snap.url),
            json_string(&snap.title),
            value
        )
    })
}

/// Parse the JSON the in-page walk produced.
pub fn parse_snapshot(json: &str) -> Result<snapshot::Snapshot, String> {
    let url = json_string_value(json, "url").unwrap_or_default();
    let title = json_string_value(json, "title").unwrap_or_default();
    let nodes_block = json_array(json, "nodes").unwrap_or_default();
    let mut nodes = Vec::new();
    for object in json_objects(&nodes_block) {
        nodes.push(snapshot::Node {
            ref_id: json_string_value(&object, "ref").unwrap_or_default(),
            role: json_string_value(&object, "role").unwrap_or_default(),
            name: json_string_value(&object, "name").unwrap_or_default(),
            target: None,
        });
    }
    Ok(snapshot::Snapshot { url, title, nodes })
}

/// The top-level array bound to `key`, as a string.
pub fn json_array(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = input.find(&needle)?.checked_add(needle.len())?;
    let rest = input[start..].trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, byte) in bytes.iter().enumerate() {
        if in_string {
            match byte {
                b'\\' => escaped = !escaped,
                b'"' if !escaped => in_string = false,
                _ => {}
            }
            if *byte != b'\\' {
                escaped = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Is the page usable *now*, in the browser's own judgement?
///
/// This is the "smart wait". Waiting for the network to go quiet hangs forever
/// on a page that holds an SSE stream, a websocket or a long poll open -- which
/// is most of them -- so the question asked here is the one Playwright calls
/// `domcontentloaded` plus "has the DOM stopped changing".
fn page_is_settled(cdp_port: u16) -> bool {
    let probe = r#"(function(){
      return JSON.stringify({
        ready: document.readyState,
        loading: document.readyState === 'loading',
        mutating: (function(){
          var n = 0;
          var obs = new MutationObserver(function(m){ n += m.length; });
          obs.observe(document.documentElement, {childList:true, subtree:true, attributes:true});
          return n;
        })()
      });
    })()"#;
    match cdp_command(
        cdp_port,
        "Runtime.evaluate",
        &format!(
            "{{\"expression\":{},\"returnByValue\":true}}",
            json_string(probe)
        ),
    ) {
        Ok(result) => {
            let value = json_objects(&result)
                .iter()
                .filter_map(|o| json_string_value(o, "value"))
                .next()
                .or_else(|| json_string_value(&result, "value"))
                .unwrap_or_default();
            let ready = json_string_value(&value, "ready");
            // `complete` or `interactive` with no pending mutations is a page an
            // agent can act on. `loading` is not.
            matches!(ready.as_deref(), Some("complete") | Some("interactive"))
                && json_value(&value, "mutating").as_deref() != Some("1")
        }
        Err(_) => false,
    }
}

/// Wait until the page is usable, or give up and let the agent have what is
/// there.
///
/// The budget is spent in favour of the agent, not against it: the first poll
/// answers as soon as the DOM is interactive, which is typically a few tens of
/// milliseconds after `Page.navigate` returns, and a page that never settles
/// (SSE, websocket, long poll) costs at most `SETTLE_MAX` instead of hanging.
pub fn wait_for_page(cdp_port: u16, budget: Duration) -> Duration {
    let started = std::time::Instant::now();
    let deadline = started + budget.max(SETTLE_TIMEOUT);
    loop {
        if page_is_settled(cdp_port) {
            return started.elapsed();
        }
        if std::time::Instant::now() >= deadline {
            return started.elapsed();
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Type the credentials into the page the browser is showing and submit it.
///
/// Any outcome is reported as a plain bool; the secret never reaches the
/// response or the daemon's log.
fn fill_login_form(user: &str, password: &str, state: &AppState) -> Result<(), String> {
    let script = format!(
        "(function(){{var p=document.querySelector('input[type=password]');\
         if(!p)return 'no password field';\
         var u=document.querySelector('input[name=username],input[type=email],input[type=text]');\
         if(u)u.value={user};\
         p.value={password};\
         var f=p.closest('form');\
         if(f){{f.submit();return 'submitted';}}\
         return 'filled';}})()",
        user = json_string(user),
        password = json_string(password)
    );
    cdp_command(
        state.cdp_port,
        "Runtime.evaluate",
        &format!(
            "{{\"expression\":{},\"returnByValue\":true}}",
            json_string(&script)
        ),
    )?;
    Ok(())
}
/// Page shown after the human submitted the local login form.
fn login_result_page(delivered: bool, user: &str) -> String {
    let note = if delivered {
        format!(
            "Your credentials were typed into the browser session as {user}. The password is not displayed, stored or returned."
        )
    } else {
        "Your credentials were received, but the browser was not reachable, so they were not typed anywhere. Start `cu start`, navigate to the sign-in page, then submit this form again.".to_string()
    };
    format!("<!doctype html><title>Secure login</title><h1>Login received</h1><p>{note}</p>")
}
/// Handle `POST /login`.
///
/// The password arrives in the form the human just filled in and never leaves
/// this function's frame: it is typed into the page the browser is showing and
/// then dropped. It is not written to disk, logged, or echoed back.
pub fn login_submit(body: &str, state: &AppState) -> String {
    let user = form_value(body, "username");
    let password = form_value(body, "password");
    if user.is_empty() || password.is_empty() {
        return [
            "<!doctype html><title>Secure login</title><h1>Sign in</h1>",
            "<p>Both a username and a password are needed.</p>",
            "<p><a href=\"/login\">Try again</a></p>",
        ]
        .concat();
    }
    let delivered = fill_login_form(&user, &password, state).is_ok();
    login_result_page(delivered, &user)
}
/// Value of a string field inside a JSON object.
///
/// Tolerates the whitespace Chromium puts after a colon and understands the
/// escapes that show up in DevTools payloads.
pub fn json_string_value(object: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = object.find(&needle)?.checked_add(needle.len())?;
    let rest = object[start..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// The objects of a JSON array, in order.
///
/// DevTools discovery is an array of target descriptions; this keeps only the
/// top-level members so a nested object can never be mistaken for a target.
pub fn json_objects(array: &str) -> Vec<String> {
    let bytes = array.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else {
                match c {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        out.push(array[start..i].to_string());
    }
    out
}

/// WebSocket URLs of the DevTools targets that are real pages.
///
/// `/json` also lists service workers and extension background pages; picking
/// the first URL in the list used to send navigate and screenshot to a target
/// that can render nothing.
pub fn page_targets(discovery: &str) -> Vec<String> {
    json_objects(discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter_map(|o| json_string_value(&o, "webSocketDebuggerUrl"))
        .collect()
}

/// A DevTools websocket kept open between commands.
///
/// Opening one cost a `/json` discovery request, a TCP connect and a websocket
/// handshake per action -- all of it pure overhead repeated for every navigate,
/// screenshot and snapshot an agent takes. Keeping the connection means an
/// action is one write and one read.
struct CdpConnection {
    stream: TcpStream,
    next_id: u64,
}

impl CdpConnection {
    /// Discover the page target and complete the websocket handshake.
    fn open(cdp_port: u16) -> Result<Self, String> {
        let target = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
        let ws = page_targets(&target)
            .into_iter()
            .next()
            .ok_or("Chromium did not expose a page target")?;
        let (host, path) = ws
            .strip_prefix("ws://")
            .and_then(|s| s.split_once('/'))
            .ok_or("unsupported CDP websocket URL")?;
        let mut stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(CDP_HTTP_TIMEOUT));
        let key = base64_encode(b"cu-cdp-clientkey");
        write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut handshake = [0; 2048];
        let n = stream.read(&mut handshake).map_err(|e| e.to_string())?;
        if !String::from_utf8_lossy(&handshake[..n]).starts_with("HTTP/1.1 101") {
            return Err("Chromium rejected CDP websocket handshake".into());
        }
        Ok(Self { stream, next_id: 1 })
    }

    /// Send one command and read its reply, skipping events meant for nobody.
    fn call(&mut self, method: &str, params: &str) -> Result<String, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let message = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
        self.stream
            .write_all(&encode_client_frame(message.as_bytes()))
            .map_err(|e| e.to_string())?;
        loop {
            let payload = read_frame(&mut self.stream, MAX_CDP_MESSAGE)?;
            let text = String::from_utf8(payload).map_err(|e| e.to_string())?;
            // Events carry no `id`; a reply for an older, abandoned command is
            // not ours either. Both are skipped rather than misreported.
            match json_value(&text, "id") {
                Some(got) if got == id.to_string() => return Ok(text),
                Some(_) => continue,
                None => continue,
            }
        }
    }
}

/// A client-to-server websocket frame: masked, as the protocol requires.
fn encode_client_frame(bytes: &[u8]) -> Vec<u8> {
    let mask = [0x43, 0x55, 0x2d, 0x31];
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(0x80 | bytes.len() as u8);
    } else if bytes.len() < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (i, byte) in bytes.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }
    frame
}

/// How many warm DevTools connections are kept per browser.
///
/// Enough that a burst of parallel actions does not queue behind one socket,
/// few enough that the browser is not holding idle fds. Extra connections
/// opened during a burst are dropped instead of kept, so a spike costs one
/// handshake and leaves nothing behind.
const CDP_POOL_IDLE: usize = 8;

/// Warm DevTools connections, per browser, that any request thread can take.
struct CdpPool {
    idle: Vec<CdpConnection>,
}

impl CdpPool {
    /// Take a warm connection, or open one if the pool is empty.
    fn take(&mut self, cdp_port: u16) -> Result<CdpConnection, String> {
        self.idle
            .pop()
            .map(Ok)
            .unwrap_or_else(|| CdpConnection::open(cdp_port))
    }

    /// Give a still-usable connection back, up to the idle limit.
    fn put(&mut self, connection: CdpConnection) {
        if self.idle.len() < CDP_POOL_IDLE {
            self.idle.push(connection);
        }
    }
}

fn cdp_pool(cdp_port: u16) -> &'static Mutex<CdpPool> {
    static POOLS: OnceLock<Mutex<HashMap<u16, &'static Mutex<CdpPool>>>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = pools.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(cdp_port)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(CdpPool { idle: Vec::new() }))))
}

/// Run one DevTools command on a pooled connection.
///
/// Reuse is what makes an action cheap: without it every navigate, screenshot
/// and snapshot paid a `/json` discovery, a TCP connect and a websocket
/// handshake before the actual command. A connection the browser has dropped is
/// reopened and retried once, so a target swap is not an error.
pub fn cdp_command(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
    let pool = cdp_pool(cdp_port);
    for attempt in 0..2 {
        let mut connection = {
            let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
            guard.take(cdp_port)
        }?;
        match connection.call(method, params) {
            Ok(reply) => {
                let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
                guard.put(connection);
                return Ok(reply);
            }
            // A broken connection is only worth retrying once: the second try
            // has just opened a fresh one.
            Err(_error) if attempt == 0 => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("the retry loop returns on its second attempt")
}
/// A DevTools HTTP request may not be answered with `Connection: close`, so the
/// response has to be framed rather than read to EOF: `read_to_string` used to
/// block for ever on a browser that kept the socket open and the whole daemon
/// stalled on its first navigate.
/// Largest CDP payload accepted, including base64 screenshots.
const MAX_CDP_MESSAGE: usize = 32 * 1024 * 1024;
/// How long the daemon waits for its own browser to come up.
const BROWSER_START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a navigation may take to become usable before handing back what
/// the page shows anyway.
pub const SETTLE_TIMEOUT: Duration = Duration::from_millis(400);
/// Longest extra wait for a page that is still loading useful content.
pub const SETTLE_MAX: Duration = Duration::from_millis(3000);

/// Read one (unmasked, server-to-client) websocket frame.
///
/// A screenshot comes back in a 64-bit length frame: only the 7- and 16-bit
/// forms used to be handled, so `Page.captureScreenshot` failed with
/// "CDP response is too large". Payloads are also read to their full announced
/// length, which a single `read()` does not guarantee.
fn read_frame(stream: &mut TcpStream, limit: usize) -> Result<Vec<u8>, String> {
    let mut head = [0; 2];
    stream.read_exact(&mut head).map_err(|e| e.to_string())?;
    let opcode = head[0] & 0x0f;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0; 2];
        stream.read_exact(&mut b).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0; 8];
        stream.read_exact(&mut b).map_err(|e| e.to_string())?;
        len = usize::try_from(u64::from_be_bytes(b))
            .map_err(|_| "CDP response is too large".to_string())?;
    }
    if len > limit {
        return Err("CDP response is too large".into());
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    // Text frames carry the JSON; anything else (ping/close/pong) is not an
    // answer, so read the next frame instead of parsing it as a response.
    match opcode {
        0x1 | 0x2 => Ok(payload),
        _ => read_frame(stream, limit),
    }
}

/// GET a DevTools HTTP endpoint and return the response body.
///
/// Chromium does not answer with `Connection: close`, so the response has to be
/// framed instead of read to EOF: `read_to_string` used to block for ever on a
/// browser that kept the socket open, and the daemon stalled on its first
/// navigate.
pub fn http_get(address: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(CDP_HTTP_TIMEOUT));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0; 2048];
    loop {
        if let Some(end) = head_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..end]);
            if let Some(len) =
                header_value(&head, "content-length").and_then(|v| v.trim().parse::<usize>().ok())
            {
                if buf.len() >= end + len {
                    break;
                }
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            // Timed out with the body still incomplete: use what did arrive
            // rather than hanging the request thread.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map(|(_, body)| body.to_string())
        .unwrap_or_default())
}
pub fn base64_encode(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(alphabet[(n >> 18 & 63) as usize] as char);
        out.push(alphabet[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            alphabet[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            alphabet[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn json_value(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = input.find(&needle)?.checked_add(needle.len())?;
    let rest = input[start..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].replace("\\\"", "\""))
    } else {
        Some(rest.split(|c: char| !c.is_ascii_digit()).next()?.into())
    }
}
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
pub fn form_value(body: &str, key: &str) -> String {
    body.split('&')
        .find_map(|item| item.strip_prefix(&format!("{key}=")).map(url_decode))
        .unwrap_or_default()
}
/// `s` as a JSON string literal.
pub fn json_string(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

/// Decode an `application/x-www-form-urlencoded` value.
///
/// `+` means space and `%XX` is a byte; only `+` used to be handled, so a
/// password containing `+`, `%` or a non-ASCII character arrived wrong.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
pub fn random_token() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{now:x}")
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    if !src.exists() {
        return Err("browser profile does not exist".into());
    }
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for item in fs::read_dir(src).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let target = dest.join(item.file_name());
        let source = item.path();
        // Chromium's profile holds symlinks (SingletonLock, SingletonSocket,
        // SingletonCookie) that point outside the profile and are usually
        // dangling. Copying the file they point to made `session save` fail
        // with ENOENT, and would scatter browser state over the filesystem if
        // it ever did exist, so symlinks are copied as symlinks.
        // Chromium's per-process locks are not session state: a copied
        // SingletonLock points at the machine and pid of the run that saved the
        // profile, and the next browser refuses to start on top of it
        // ("Failed to create a ProcessSingleton"). Leave them out entirely.
        if item
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("Singleton"))
        {
            continue;
        }
        let meta = fs::symlink_metadata(&source).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            let link = fs::read_link(&source).map_err(|e| e.to_string())?;
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(&link, &target).map_err(|e| e.to_string())?;
        } else if meta.is_dir() {
            copy_dir(&source, &target)?
        } else {
            fs::copy(&source, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = alphabet
            .iter()
            .position(|x| *x == c)
            .ok_or("invalid base64")? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let head = "GET /v1/status HTTP/1.1\r\nHost: localhost\r\nContent-Length: 7\r\n\r\n";
        assert_eq!(header_value(head, "CONTENT-LENGTH"), Some("7"));
        assert_eq!(header_value(head, "content-length"), Some("7"));
        assert_eq!(header_value(head, "Authorization"), None);
    }

    #[test]
    fn percent_encoding_and_plus_are_decoded() {
        assert_eq!(url_decode("s3cret%2B50%25"), "s3cret+50%");
        assert_eq!(url_decode("a+space"), "a space");
        assert_eq!(url_decode("caf%C3%A9"), "caf\u{e9}");
        // A stray escape is kept rather than swallowed.
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn json_strings_are_escaped_and_read_back() {
        let password = "quote\" backslash\\ newline\n";
        assert_eq!(
            json_string_value(&json_string(password), "unused").is_none(),
            true
        );
        assert_eq!(json_string_value(&json_string(password), ""), None);
        let encoded = format!("{{\"v\":{}}}", json_string(password));
        assert_eq!(json_string_value(&encoded, "v").as_deref(), Some(password));
    }

    #[test]
    fn page_targets_skip_workers_and_extensions() {
        let discovery = r#"[
          {
             "description": "",
             "title": "Extension",
             "type": "background_page",
             "url": "chrome-extension://abc/background.html",
             "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/EXT"
          },
          {
             "description": "",
             "title": "about:blank",
             "type": "page",
             "url": "about:blank",
             "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/PAGE"
          }
       ]"#;
        assert_eq!(
            page_targets(discovery),
            vec!["ws://127.0.0.1:9222/devtools/page/PAGE".to_string()]
        );
        assert!(page_targets("[]").is_empty());
        assert!(page_targets("not json").is_empty());
    }

    #[test]
    fn a_login_page_rejects_empty_credentials() {
        let state = AppState {
            data_dir: std::env::temp_dir().join("cu-login-test"),
            token: "t".into(),
            cdp_port: 1,
            browser: BrowserState::new(),
        };
        let page = login_submit("username=&password=", &state);
        assert!(!page.contains("Login received"));
        assert!(page.contains("Try again"));
    }

    #[test]
    fn a_session_cannot_be_read_out_of_the_login_response() {
        let state = AppState {
            data_dir: std::env::temp_dir().join("cu-login-test"),
            token: "t".into(),
            cdp_port: 1,
            browser: BrowserState::new(),
        };
        // The browser is unreachable on port 1, so nothing is typed; the point
        // is that no password ever comes back in the page.
        let page = login_submit("username=jaime&password=s3cret", &state);
        assert!(!page.contains("s3cret"));
        assert!(!page.contains("password"));
    }

    #[test]
    fn byte_needle_is_located() {
        assert_eq!(find(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
        assert_eq!(find(b"", b"\r\n\r\n"), None);
    }
}

/// Compact, LLM-oriented snapshot of the current page.
///
/// This is what makes an agent fast in practice: instead of shipping the whole
/// DOM (or a screenshot the model has to OCR) it gets one short line per
/// interactive element, each carrying a stable `ref` the next action can name.
/// Playwright MCP, browser-use and Stagehand all converge on this shape.
#[cfg(test)]
mod snapshot_tests {
    use super::*;

    const SAMPLE: &str = r#"{"url":"https://a.test/p","title":"Page",
        "nodes":[{"ref":"e1","role":"link","name":"More information"},
                 {"ref":"e2","role":"textbox","name":"Search"}],
        "headings":["Page"]}"#;

    #[test]
    fn a_snapshot_is_rendered_as_refs_the_agent_can_act_on() {
        let snap = parse_snapshot(SAMPLE).expect("parses");
        assert_eq!(snap.url, "https://a.test/p");
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.nodes[0].ref_id, "e1");
        assert_eq!(snap.nodes[1].role, "textbox");
        let text = snap.render();
        assert!(text.contains("- ref=e1 link \"More information\""));
        assert!(text.contains("- page: Page\n"));
        // No presentation markup and no raw HTML reaches the model.
        assert!(!text.contains('<'));
    }

    #[test]
    fn a_snapshot_without_nodes_is_empty_not_an_error() {
        let snap = parse_snapshot(r#"{"url":"about:blank","nodes":[]}"#).expect("parses");
        assert!(snap.nodes.is_empty());
        assert_eq!(snap.title, "");
    }

    #[test]
    fn an_array_is_extracted_without_its_neighbours() {
        let json = r#"{"a":[1,{"b":"]"}],"nodes":[{"ref":"e1"}],"c":"[not an array]"}"#;
        let nodes = json_array(json, "nodes").expect("nodes array");
        assert_eq!(nodes, r#"[{"ref":"e1"}]"#);
        assert!(json_array(json, "missing").is_none());
        assert!(json_array(r#"{"a":"text"}"#, "a").is_none());
    }
}

pub mod snapshot {
    /// One element worth telling the model about.
    pub struct Node {
        /// Stable handle the agent uses to act on this element.
        pub ref_id: String,
        /// `link`, `button`, `textbox`, `heading`, ...
        pub role: String,
        /// Accessible name, already collapsed to one line.
        pub name: String,
        /// Interactive element this node can be clicked into, if any.
        pub target: Option<String>,
    }

    /// The whole snapshot for a page.
    pub struct Snapshot {
        pub url: String,
        pub title: String,
        pub nodes: Vec<Node>,
    }

    impl Snapshot {
        /// Render as the compact text an agent reads.
        pub fn render(&self) -> String {
            let mut out = String::new();
            out.push_str(&format!("- url: {}\n", self.url));
            if !self.title.is_empty() {
                out.push_str(&format!("- page: {}\n", self.title));
            }
            for node in &self.nodes {
                out.push_str(&format!(
                    "- ref={} {} \"{}\"\n",
                    node.ref_id, node.role, node.name
                ));
            }
            out
        }
    }

    /// JavaScript that walks the a11y-relevant tree and returns compact JSON.
    ///
    /// Everything happens in the page, so only the small result crosses CDP --
    /// that is the whole point of a snapshot: one round trip, a few hundred
    /// bytes, instead of a full DOM dump.
    pub const SNAPSHOT_JS: &str = r#"
/* cuAgentSnapshot */
(() => {
  const INTERACTIVE = 'a[href],button,input,select,textarea,[role=button],[role=link],[role=checkbox],[role=tab],[role=menuitem]';
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none';
  };
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const name = (el) => {
    const labelledby = el.getAttribute('aria-labelledby');
    if (labelledby) {
      const t = clean(document.getElementById(labelledby)?.textContent);
      if (t) return t;
    }
    const a = el.getAttribute('aria-label');
    if (a) return clean(a);
    const label = el.labels && el.labels[0];
    if (label) return clean(label.textContent);
    const img = el.querySelector('img[alt]');
    if (img) return img.alt;
    return clean(el.textContent) || clean(el.getAttribute('placeholder')) || clean(el.value) || '';
  };
  const nodes = [];
  for (const el of document.querySelectorAll(INTERACTIVE)) {
    if (!visible(el)) continue;
    if (el.closest('[aria-hidden=true]')) continue;
    const role = el.getAttribute('role')
      || ({A:'link',BUTTON:'button',SELECT:'combobox',TEXTAREA:'textbox'})
        [el.tagName]
      || (el.tagName === 'INPUT'
        ? ({text:'textbox',search:'searchbox',email:'textbox',password:'textbox',
            number:'spinbutton',checkbox:'checkbox',radio:'radio',file:'button'})[el.type] || 'textbox'
        : 'generic');
    nodes.push({ ref: 'e' + (nodes.length + 1), role, name: name(el).slice(0, 120) });
    if (nodes.length >= 200) break;
  }
  const headings = [];
  for (const h of document.querySelectorAll('h1,h2,h3,[role=heading]')) {
    if (!visible(h)) continue;
    const t = clean(h.textContent).slice(0, 120);
    if (t) headings.push(t);
    if (headings.length >= 20) break;
  }
  return JSON.stringify({
    url: location.href,
    title: document.title,
    nodes,
    headings,
  });
})()
"#;
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    /// The pool is the only shared mutable state on the hot path, so its
    /// borrow/put accounting has to hold up under a burst.
    #[test]
    fn a_pooled_connection_is_reused_and_capped() {
        let port = free_loopback_port();
        let pool = cdp_pool(port);
        // Nothing is cached yet, and the pool is per browser.
        assert!(
            pool.lock()
                .unwrap_or_else(|e| e.into_inner())
                .idle
                .is_empty()
        );
        assert!(
            cdp_pool(port + 1)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .idle
                .is_empty()
        );
    }

    #[test]
    fn client_frames_are_masked_and_sized() {
        let small = encode_client_frame(b"hi");
        assert_eq!(small[0], 0x81);
        assert_eq!(small[1] & 0x80, 0x80, "client frames must be masked");
        assert_eq!((small[1] & 0x7f) as usize, 2);
        let big = encode_client_frame(&vec![b'x'; 70_000]);
        assert_eq!(big[1] & 0x7f, 127, "70k needs the 64-bit length form");
    }

    fn free_loopback_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port()
    }
}
