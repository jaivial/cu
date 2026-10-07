//! Loopback HTTP server for `cu`: request parsing, routing and the browser bridge.
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
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
    // Both loopback stacks: with the IPv4 port taken by a browser that is
    // still shutting down, Chromium binds the DevTools port on `::1` only,
    // and an IPv4-only probe then never sees its own browser.
    let address = format!("127.0.0.1:{cdp_port}");
    let address_v6 = format!("[::1]:{cdp_port}");
    loop {
        if http_get(&address, "/json/version").is_ok()
            || http_get(&address_v6, "/json/version").is_ok()
        {
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
            // Recorded before readiness: a launch that times out (or a
            // browser that comes up late) used to leave the whole Chromium
            // tree running for ever, because only a successful launch stored
            // the pid that shutdown closes.
            BROWSER_PID.store(child.id(), Ordering::SeqCst);
            // One window is not enough when the predecessor browser is still
            // letting go of the profile and the port (a restart under load):
            // the new Chromium then takes longer than the window to answer,
            // and declaring the daemon browserless for ever is worse than
            // waiting. While the child lives it is still coming up, so the
            // watch is repeated -- up to five windows, then it is a failure.
            let mut result = wait_for_browser(cdp_port, BROWSER_START_TIMEOUT, Some(&mut child));
            let mut windows = 1;
            while result.is_err() && windows < 5 {
                match child.try_wait() {
                    // A browser that exited is the real error, reported as-is.
                    Ok(Some(status)) => {
                        result = Err(format!(
                            "Chromium exited immediately ({status}); set CU_BROWSER to a working binary"
                        ));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        result = Err(format!("could not check on Chromium: {e}"));
                        break;
                    }
                }
                windows += 1;
                result = wait_for_browser(cdp_port, BROWSER_START_TIMEOUT, Some(&mut child));
            }
            match result {
                Ok(()) => {
                    // Downloads land in the session's own directory (see
                    // `spawn_browser_control`), before readiness so the first
                    // click is already covered.
                    spawn_browser_control(cdp_port, &data);
                    browser.mark_ready()
                }
                Err(e) => browser.mark_failed(e),
            }
        }
        Err(e) => browser.mark_failed(e),
    })
}

/// One browser-level DevTools connection held for the daemon's whole life.
///
/// It exists for downloads. `Browser.setDownloadBehavior` with a custom
/// directory is only honoured while a session with `eventsEnabled` stays
/// attached (Chromium 145: set from a socket that then closes, and the
/// browser keeps its own default -- the user's real Downloads folder, where
/// an agent can neither see nor manage the files). The daemon therefore
/// connects once, enables the download events and drains them forever; the
/// page sessions still see `Page.downloadWillBegin` too, which is what turns
/// a click into a `"download"` result.
fn spawn_browser_control(cdp_port: u16, data: &Path) {
    let download_path = data.join("downloads").to_string_lossy().into_owned();
    thread::spawn(move || {
        let Some(connection) = browser_control_connection(cdp_port) else {
            return;
        };
        let mut connection = connection;
        if let Err(error) = connection.call(
            "Browser.setDownloadBehavior",
            &format!(
                "{{\"behavior\":\"allow\",\"downloadPath\":{},\"eventsEnabled\":true}}",
                json_string(&download_path)
            ),
        ) {
            // Not fatal: downloads fall back to the browser's own default
            // directory, which the agent then cannot see.
            eprintln!("cu: could not redirect downloads: {error}");
            return;
        }
        // Drain browser events (download progress, target changes) so the
        // socket buffer never fills up. A blocking read with no timeout: the
        // session must outlive hours between downloads.
        let _ = connection.stream.set_read_timeout(None);
        while connection.next_message(MAX_CDP_MESSAGE).is_ok() {}
    });
}

/// A browser-level websocket from the discovery document.
fn browser_control_connection(cdp_port: u16) -> Option<CdpConnection> {
    let version = http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version").ok()?;
    let ws = json_string_value(&version, "webSocketDebuggerUrl")?;
    CdpConnection::connect(&ws).ok()
}

/// Pid of the browser this daemon launched, 0 while there is none.
static BROWSER_PID: AtomicU32 = AtomicU32::new(0);

/// Close the browser this daemon launched.
///
/// A daemon that exits used to leave its whole Chromium tree behind -- eight
/// processes and a quarter of a gigabyte each time `cu start` was re-run.
/// `Browser.close` lets Chromium flush the profile (cookies, local storage)
/// before it goes; SIGTERM is the fallback for a browser that does not answer.
pub fn shutdown_browser(cdp_port: u16) {
    let pid = BROWSER_PID.swap(0, Ordering::SeqCst);
    if pid == 0 {
        return;
    }
    let _ = cdp_browser_command(cdp_port, "Browser.close", "{}");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && process_alive(pid) {
        thread::sleep(Duration::from_millis(10));
    }
    if process_alive(pid) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

/// Close the browser and exit when the daemon is asked to stop.
///
/// SIGTERM and SIGINT are blocked in every thread and collected by one thread
/// with `sigwait`, so shutdown runs as ordinary code rather than inside a
/// signal handler. Must be called before any other thread is spawned so they
/// all inherit the mask.
pub fn exit_on_signal(cdp_port: u16) {
    unsafe extern "C" {
        fn sigemptyset(set: *mut SigSet) -> i32;
        fn sigaddset(set: *mut SigSet, sig: i32) -> i32;
        fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
        fn sigwait(set: *const SigSet, sig: *mut i32) -> i32;
    }
    /// `sigset_t` is 128 bytes on Linux/glibc and musl.
    #[repr(C)]
    struct SigSet([u64; 16]);
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIG_BLOCK: i32 = 0;
    let mut set = SigSet([0; 16]);
    // SAFETY: plain libc calls on a correctly sized, owned sigset.
    unsafe {
        sigemptyset(&mut set);
        sigaddset(&mut set, SIGINT);
        sigaddset(&mut set, SIGTERM);
        pthread_sigmask(SIG_BLOCK, &set, std::ptr::null_mut());
    }
    thread::spawn(move || {
        let mut sig = 0;
        // SAFETY: `set` was initialised above and lives in this closure.
        unsafe { sigwait(&set, &mut sig) };
        shutdown_browser(cdp_port);
        std::process::exit(0);
    });
}

fn process_alive(pid: u32) -> bool {
    // A zombie still has a /proc entry; it is gone in every way that matters.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| {
            !s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
        .unwrap_or(false)
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

/// Flags every browser is launched with.
///
/// An agent's browser needs pages, DevTools and the profile -- not updates,
/// sync, translation, crash upload, audio or a GPU process. Each of those is a
/// process or a background timer, and on a loaded machine they compete with
/// the page for CPU. Site isolation is relaxed so same-site frames share one
/// renderer, which is most of the memory saving.
pub const BROWSER_ARGS: &[&str] = &[
    "--remote-allow-origins=*",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-dev-shm-usage",
    // Background work an agent never asked for.
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-sync",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-breakpad",
    "--disable-crash-reporter",
    "--metrics-recording-only",
    "--no-pings",
    "--mute-audio",
    "--password-store=basic",
    // Rendering: software raster. (`--in-process-gpu` would also drop the GPU
    // process, but it crashes chrome-headless-shell on cross-document
    // navigation, so it is not used.)
    "--disable-gpu",
    // The network service as a thread of the browser, not a process.
    "--enable-features=NetworkServiceInProcess",
    // Fewer processes: same-site frames share a renderer.
    "--disable-site-isolation-trials",
    "--renderer-process-limit=4",
    "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,\
     AutofillServerCommunication,CalculateNativeWinOcclusion,InterestFeedContentSuggestions,\
     CertificateTransparencyComponentUpdater,LensOverlay,PaintHolding,\
     SpareRendererForSitePerProcess,BackForwardCache",
];

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
        .args(BROWSER_ARGS)
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
            | ("POST", "/v1/act")
            | ("POST", "/v1/click")
            | ("POST", "/v1/type")
            | ("GET", "/v1/contexts")
            | ("GET", "/v1/tabs")
            | ("GET", "/v1/text")
    ) || (method == "DELETE"
        && (path.starts_with("/v1/contexts/") || path.starts_with("/v1/tabs/")));
    if needs_browser {
        if let Err(error) = wait_until_ready(state) {
            return (
                "503 Service Unavailable",
                "application/json",
                format!("{{\"error\":\"{}\"}}", json_escape(&error)),
            );
        }
    }
    // `?context=NAME` runs a page action in an isolated browser context of
    // the same browser; `?tab=ID` runs it in another real tab (a popup, a
    // window a click opened). They are two ways of picking a page, so giving
    // both is an error rather than a silent preference.
    let tab = if needs_browser && !path.starts_with("/v1/contexts") && !path.starts_with("/v1/tabs")
    {
        let context = form_value(query(full_path), "context");
        let wanted = form_value(query(full_path), "tab");
        if !context.is_empty() && !wanted.is_empty() {
            return (
                "400 Bad Request",
                "application/json",
                "{\"error\":\"give either ?context= or ?tab=, not both\"}".into(),
            );
        }
        if !context.is_empty() {
            match context_tab(state.cdp_port, &context, &state.data_dir) {
                Ok(tab) => tab,
                Err(e) => {
                    return (
                        "400 Bad Request",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                    );
                }
            }
        } else if !wanted.is_empty() {
            match named_tab(state.cdp_port, &wanted) {
                Ok(tab) => tab,
                Err(e) => {
                    return (
                        "400 Bad Request",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                    );
                }
            }
        } else {
            Tab::default_for(state.cdp_port)
        }
    } else {
        Tab::default_for(state.cdp_port)
    };
    match (method, path) {
        ("GET", "/v1/tabs") => match tabs_json(state.cdp_port) {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => (
                "502 Bad Gateway",
                "application/json",
                format!("{{\"error\":{}}}", json_string(&e)),
            ),
        },
        ("DELETE", p) if p.starts_with("/v1/tabs/") => {
            match close_tab(state.cdp_port, &p["/v1/tabs/".len()..]) {
                Ok(()) => ("200 OK", "application/json", "{\"closed\":true}".into()),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                ),
            }
        }
        ("GET", "/v1/contexts") => (
            "200 OK",
            "application/json",
            format!(
                "{{\"contexts\":[{}]}}",
                context_names(state.cdp_port)
                    .iter()
                    .map(|n| json_string(n))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("DELETE", p) if p.starts_with("/v1/contexts/") => {
            match close_context(state.cdp_port, &p["/v1/contexts/".len()..]) {
                Ok(closed) => (
                    "200 OK",
                    "application/json",
                    format!("{{\"closed\":{closed}}}"),
                ),
                Err(e) => (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                ),
            }
        }
        ("GET", "/v1/status") => (
            "200 OK",
            "application/json",
            "{\"running\":true,\"browser\":\"chromium\"}".into(),
        ),
        // Visible text of the page, for the reading a snapshot deliberately
        // does not do: prose, API responses, error messages. Every frame is
        // read, each one named, capped in the page so a huge document cannot
        // flood the socket.
        ("GET", "/v1/text") => text_response(|method, params| tab.command(method, params)),
        // Pure file listing: works whether or not the browser is up, because
        // the files outlive the session that downloaded them.
        ("GET", "/v1/downloads") => (
            "200 OK",
            "application/json",
            downloads_json(&state.data_dir.join("downloads")),
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
            match tab.command(
                "Page.navigate",
                &format!("{{\"url\":\"{}\"}}", json_escape(&url)),
            ) {
                Ok(result) => {
                    // A navigation the browser could not perform answers with
                    // `errorText` (`net::ERR_NAME_NOT_RESOLVED` and friends);
                    // passing that on as a 200 hid a failed navigation from
                    // the agent.
                    if let Some(error) =
                        json_string_value(&result, "errorText").filter(|e| !e.is_empty())
                    {
                        return (
                            "502 Bad Gateway",
                            "application/json",
                            format!(
                                "{{\"error\":{}}}",
                                json_string(&format!("navigation failed: {error}"))
                            ),
                        );
                    }
                    // Do not hand back a half-loaded page, but never hang on a
                    // stream that stays open for ever. Whether the wait ended
                    // on a ready page or on the budget says `settled`.
                    let (waited, settled) = wait_for_page_on(&tab, SETTLE_MAX);
                    (
                        "200 OK",
                        "application/json",
                        format!(
                            "{{\"result\":{},\"settled_ms\":{},\"settled\":{}}}",
                            result.trim(),
                            waited.as_millis(),
                            settled
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
        ("GET", "/v1/screenshot") => screenshot(&tab, query(full_path)),
        // Act on snapshot refs. `/v1/act` takes a batch; `/v1/click` and
        // `/v1/type` are the same thing for a single action.
        ("POST", "/v1/act") => act(&tab, crate::actions::parse_batch(body)),
        ("POST", "/v1/click") => act(&tab, single("click", body)),
        ("POST", "/v1/type") => act(&tab, single("type", body)),
        ("GET", "/v1/snapshot") => match snapshot_tab(&tab) {
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

/// A one-action batch from a `/v1/click` or `/v1/type` body.
fn single(kind: &str, body: &str) -> Result<crate::actions::Batch, String> {
    let object = body.trim();
    let inner = object
        .strip_prefix('{')
        .and_then(|o| o.strip_suffix('}'))
        .ok_or("body must be a JSON object")?;
    let action = format!("{{\"do\":{},{inner}}}", json_string(kind));
    Ok(crate::actions::Batch {
        actions: vec![crate::actions::parse_action(&action)?],
        snapshot: crate::actions::flag(object, "snapshot").unwrap_or(false),
    })
}

fn act(
    tab: &Tab,
    batch: Result<crate::actions::Batch, String>,
) -> (&'static str, &'static str, String) {
    let error = |status, e: &str| {
        (
            status,
            "application/json",
            format!("{{\"error\":{}}}", json_string(e)),
        )
    };
    match batch {
        Err(e) => error("400 Bad Request", &e),
        Ok(batch) => match crate::actions::run_batch(tab, &batch) {
            // A failed action is the agent's to handle, so it is a 200 with
            // `ok:false` and the index that failed, not a transport error.
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => error("502 Bad Gateway", &e),
        },
    }
}

/// Every open page tab, default first, as the `/v1/tabs` body.
pub fn tabs_json(cdp_port: u16) -> Result<String, String> {
    let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
    let pages = page_target_ids_and_sockets(&discovery, cdp_port);
    let pinned = pinned_target(cdp_port);
    let default_id = pinned
        .filter(|p| pages.iter().any(|(id, _)| id == p))
        .or_else(|| pages.first().map(|(id, _)| id.clone()));
    let listed: Vec<String> = json_objects(&discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter_map(|o| {
            let id = json_string_value(&o, "id")?;
            Some(format!(
                "{{\"id\":{},\"url\":{},\"title\":{},\"default\":{}}}",
                json_string(&id),
                json_string(&json_string_value(&o, "url").unwrap_or_default()),
                json_string(&json_string_value(&o, "title").unwrap_or_default()),
                default_id.as_deref() == Some(id.as_str())
            ))
        })
        .collect();
    Ok(format!("{{\"tabs\":[{}]}}", listed.join(",")))
}

/// Close a tab by id. The last page is refused: Chromium exits with it.
pub fn close_tab(cdp_port: u16, id: &str) -> Result<(), String> {
    let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
    // Every page target counts here, context tabs included: any of them keeps
    // Chromium alive, and a context's tab may be closed directly (the context
    // then reports its tab as gone).
    let pages: Vec<String> = json_objects(&discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter_map(|o| json_string_value(&o, "id"))
        .collect();
    if !pages.iter().any(|page| page == id) {
        return Err("no such tab; see GET /v1/tabs".into());
    }
    if pages.len() <= 1 {
        return Err(
            "refusing to close the last tab; stopping cu closes the browser instead".into(),
        );
    }
    let reply = cdp_browser_command(
        cdp_port,
        "Target.closeTarget",
        &format!("{{\"targetId\":{}}}", json_string(id)),
    )?;
    if json_value(&reply, "success").as_deref() == Some("false") {
        return Err("Chromium refused to close that tab".into());
    }
    // The reply arrives before the target leaves `/json`; wait a beat so the
    // `GET /v1/tabs` that follows a close agrees with it.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
        let gone = json_objects(&discovery)
            .into_iter()
            .all(|o| json_string_value(&o, "id").as_deref() != Some(id));
        if gone {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // The pool of a closed tab is dead weight, and if it was the default the
    // next action must pick a live page instead of a vanished id.
    forget_tab(&Tab {
        cdp_port,
        target: Some(id.to_string()),
    });
    unpin_target(cdp_port, id);
    Ok(())
}

/// Characters `GET /v1/text` returns before truncating: enough for an API
/// response or several screens of prose, small enough that one reply stays
/// a tool-sized payload.
const TEXT_LIMIT: usize = 16_000;

/// Visible text of one document, capped in the page so a huge document cannot
/// flood the socket however big it is.
///
/// `body.innerText` is what the user would read (layout-aware, no scripts or
/// styles). A page that hands its content to a web component shows nothing
/// there, so every open shadow root is read as well. A `ShadowRoot` has no
/// `innerText` of its own in Chromium, so its words are collected from the
/// visible leaves inside it; that is also what keeps a `display:none` control
/// out of the answer, because `innerText` degrades to `textContent` for a
/// subtree that is not rendered. A closed shadow root is invisible to every
/// script, so it is not read -- exactly what a page itself would see.
/// `n` is how many characters came back and `full` how many there were, which
/// is what `truncated` is computed from.
const READ_TEXT_JS: &str = r#"JSON.stringify((function(){
  const shown = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 1 && r.height < 1) return false;
    const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none';
  };
  // Chromium gives a ShadowRoot no innerText at all, so its words are read
  // from the visible leaves inside it -- which is also what keeps a
  // display:none control out of the answer, since innerText degrades to
  // textContent for a subtree that is not rendered.
  const root = (r) => {
    const out = [];
    for (const el of r.querySelectorAll('*')) {
      // A host renders its shadow root rather than its own children: read
      // that instead, or the same words come back twice.
      if (el.shadowRoot) { out.push(root(el.shadowRoot)); continue; }
      if (el.children.length) continue;
      if (!shown(el)) continue;
      const t = (el.innerText || '').trim();
      if (t) out.push(t);
    }
    return out.filter(Boolean).join('\n');
  };
  // Only shadow hosts are collected. Everything else is in body.innerText
  // already, and walking the document for leaves as well would double it.
  let shadow = '';
  for (const el of document.querySelectorAll('*')) if (el.shadowRoot) shadow += (shadow ? '\n' : '') + root(el.shadowRoot);
  let t = document.body ? document.body.innerText : '';
  if (shadow) t += (t ? '\n' : '') + shadow;
  const s = t.slice(0,16001);
  return {n:s.length,full:t.length,text:s};
})())"#;

/// Line that names a frame whose text could not be read.
///
/// Same wording the snapshot uses for a frame it could not walk: the words are
/// missing because the frame moved or the browser would not expose it, which
/// a new snapshot may fix, and the rest of the page is still worth reading.
fn unreadable_frame(url: &str) -> String {
    format!("- frame: {url} (unreadable right now; take a new snapshot)")
}

/// The `GET /v1/text` body: the visible text of every frame of the page.
///
/// The frames are the ones a snapshot walks, so text and snapshot agree on
/// what a page is made of. The main document comes first and each iframe under
/// a `- frame: <url>` line, so the words never claim to be the page's own.
/// A frame that cannot be read is named, not fatal: the answer an agent wants
/// is the prose, and half of it beats an error.
fn text_response<F>(mut cmd: F) -> (&'static str, &'static str, String)
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    // The main document first: it is the frame an action lands on, and a page
    // that fails here is a page that cannot be read at all.
    let tree = cmd("Page.getFrameTree", "{}");
    let tree = match tree {
        Ok(tree) => tree,
        Err(e) => {
            return (
                "502 Bad Gateway",
                "application/json",
                format!("{{\"error\":{}}}", json_string(&e)),
            );
        }
    };
    let mut frames = flatten_frame_tree(&tree);
    if frames.is_empty() {
        frames.push(Frame {
            id: String::new(),
            parent: None,
            url: String::new(),
        });
    }
    let mut text = String::new();
    let mut truncated = false;
    for (index, frame) in frames.iter().enumerate() {
        // Subframes are read in an isolated world of their own, so a
        // cross-origin frame is read without touching its scripts -- the same
        // move the snapshot makes.
        let context = if index == 0 {
            String::new()
        } else {
            let isolated = cmd(
                "Page.createIsolatedWorld",
                &format!(
                    "{{\"frameId\":{},\"worldName\":\"cu-text\"}}",
                    json_string(&frame.id)
                ),
            );
            match isolated
                .as_deref()
                .ok()
                .and_then(|r| json_value(r, "executionContextId"))
                .and_then(|v| v.parse::<i64>().ok())
            {
                // Trailing comma: the format below puts one comma between the
                // expression and this fragment and none after it.
                Some(id) => format!("\"contextId\":{id},"),
                // The frame went away between the tree walk and here.
                None => {
                    text.push('\n');
                    text.push_str(&unreadable_frame(&frame.url));
                    continue;
                }
            }
        };
        let params = format!(
            "{{\"expression\":{},{}\"returnByValue\":true}}",
            json_string(READ_TEXT_JS),
            context
        );
        let value = match cmd("Runtime.evaluate", &params) {
            Ok(reply) => evaluated_string(&reply),
            Err(_) => None,
        };
        let Some(value) = value else {
            if index == 0 {
                return (
                    "502 Bad Gateway",
                    "application/json",
                    "{\"error\":\"page did not return its text\"}".into(),
                );
            }
            text.push('\n');
            text.push_str(&unreadable_frame(&frame.url));
            continue;
        };
        let (full, n) = (
            json_string_value(&value, "full").and_then(|f| f.parse::<usize>().ok()),
            json_string_value(&value, "n").and_then(|n| n.parse::<usize>().ok()),
        );
        truncated |= full.zip(n).is_some_and(|(full, n)| full > n);
        if index > 0 {
            text.push('\n');
            text.push_str(&format!("- frame: {}", frame.url));
        }
        text.push('\n');
        text.push_str(&json_string_value(&value, "text").unwrap_or_default());
    }
    let text: String = text.chars().take(TEXT_LIMIT).collect();
    (
        "200 OK",
        "application/json",
        format!(
            "{{\"text\":{},\"truncated\":{}}}",
            json_string(&text),
            truncated
        ),
    )
}

/// Finished downloads in `dir`, newest first, as the `/v1/downloads` body.
///
/// A file Chromium is still writing carries the `.crdownload` suffix, so an
/// entry here is complete and safe for the agent to read. A file that belongs
/// to a named context also carries its `"context"`, because that context
/// downloads into a subdirectory of its own.
pub fn downloads_json(dir: &Path) -> String {
    let mut files: Vec<(SystemTime, String)> = Vec::new();
    collect_downloads(dir, None, &mut files, 0);
    files.sort_by(|a, b| b.0.cmp(&a.0));
    format!(
        "{{\"downloads\":[{}]}}",
        files
            .iter()
            .map(|(_, item)| item.as_str())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// One level of the download tree, newest file first.
///
/// A named context downloads into its own subdirectory, so the listing walks
/// those too and each entry says which context it came from -- two contexts
/// downloading `report.pdf` are two different files, and an agent has to be
/// able to tell them apart. `context` is `None` for the browser-wide
/// directory. The depth guard keeps a stray nested directory from turning one
/// listing into a filesystem crawl.
fn collect_downloads(
    dir: &Path,
    context: Option<&str>,
    files: &mut Vec<(SystemTime, String)>,
    depth: usize,
) {
    const MAX_DEPTH: usize = 2;
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            // One more level is a context's directory; deeper is not ours.
            if depth < MAX_DEPTH {
                let name = path.file_name().and_then(|n| n.to_str());
                if let Some(name) = name {
                    collect_downloads(&path, Some(name), files, depth + 1);
                }
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".crdownload") {
            continue;
        }
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        files.push((
            mtime,
            match context {
                Some(context) => format!(
                    "{{\"name\":{},\"bytes\":{},\"context\":{}}}",
                    json_string(name),
                    meta.len(),
                    json_string(context)
                ),
                None => format!(
                    "{{\"name\":{},\"bytes\":{}}}",
                    json_string(name),
                    meta.len()
                ),
            },
        ));
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
fn screenshot(tab: &Tab, query: &str) -> (&'static str, &'static str, String) {
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
    match tab.command(
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

/// One frame of the page as `Page.getFrameTree` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub id: String,
    /// The frame this one is embedded in; `None` for the main document.
    pub parent: Option<String>,
    pub url: String,
}

/// The object bound to `key` at the top of `input`, as a string slice.
/// The mirror of [`json_array`] for objects.
fn json_object<'a>(input: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = input.find(&needle)?.checked_add(needle.len())?;
    let rest = input[start..].trim_start().strip_prefix(':')?.trim_start();
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
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
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Depth-first flatten of a `Page.getFrameTree` reply: the main document
/// first, then each child under its parent.
pub fn flatten_frame_tree(tree: &str) -> Vec<Frame> {
    let mut out = Vec::new();
    if let Some(root) = json_object(tree, "frameTree") {
        flatten_frame_node(root, None, &mut out);
    }
    out
}

fn flatten_frame_node(node: &str, parent: Option<String>, out: &mut Vec<Frame>) {
    let Some(frame) = json_object(node, "frame") else {
        return;
    };
    let id = json_string_value(frame, "id").unwrap_or_default();
    let url = json_string_value(frame, "url").unwrap_or_default();
    // Pre-order: the main document must come first, because position zero is
    // what marks the frame walked in its own default world.
    out.push(Frame {
        id: id.clone(),
        parent,
        url,
    });
    if let Some(children) = json_array(node, "childFrames") {
        for child in json_objects(&children) {
            flatten_frame_node(&child, Some(id.clone()), out);
        }
    }
}

/// The ref -> frameId map of each tab's last snapshot, so an action can walk
/// straight to the frame that owns its ref instead of probing frames one by
/// one. Rebuilt wholesale by every snapshot.
fn ref_frames() -> &'static Mutex<HashMap<Tab, HashMap<String, String>>> {
    static REF_FRAMES: OnceLock<Mutex<HashMap<Tab, HashMap<String, String>>>> = OnceLock::new();
    REF_FRAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record which frame each ref of this snapshot lives in.
pub fn note_ref_frames(tab: &Tab, pairs: &[(String, String)]) {
    let map: HashMap<String, String> = pairs.iter().cloned().collect();
    ref_frames()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), map);
}

/// The frame a snapshot ref lives in, if the last snapshot named one.
pub fn frame_for_ref(tab: &Tab, reference: &str) -> Option<String> {
    ref_frames()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .and_then(|map| map.get(reference))
        .cloned()
}

/// Build a `GET /v1/snapshot` response body.
///
/// The walk runs in the page: a few hundred bytes instead of a DOM dump the
/// daemon then has to shrink.
pub fn snapshot_pages(state: &AppState) -> Result<String, String> {
    snapshot_tab(&Tab::default_for(state.cdp_port))
}

fn snapshot_tab(tab: &Tab) -> Result<String, String> {
    let (text, pairs) = walk_snapshot(tab, |method, params| tab.command(method, params))?;
    note_ref_frames(tab, &pairs);
    Ok(format!("{{\"snapshot\":{}}}", json_string(&text)))
}

/// Walk every frame of the page, fetching the frame tree first.
pub fn walk_snapshot<F>(tab: &Tab, cmd: F) -> Result<(String, Vec<(String, String)>), String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let mut cmd = cmd;
    let tree = cmd("Page.getFrameTree", "{}")?;
    let frames = flatten_frame_tree(&tree);
    walk_frames(tab, frames, cmd)
}

/// Walk every frame of the page and merge the result.
///
/// Returns the snapshot text and the (ref, frameId) of every element. The
/// main document is walked in its default world -- that is where the actions
/// look for the registry -- and each iframe in an isolated world of its own,
/// so a cross-origin frame is read without ever touching its scripts.
/// Isolated worlds persist per frame (created with the fixed name `cu`), so
/// the registry the walk installs is the one the later action finds.
pub fn walk_frames<F>(
    tab: &Tab,
    frames: Vec<Frame>,
    mut cmd: F,
) -> Result<(String, Vec<(String, String)>), String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let mut text = String::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let expression = snapshot::script_with_base(snapshot::base_for(tab.cdp_port, &frame.id));
        let context = if index == 0 {
            String::new()
        } else {
            let reply = cmd(
                "Page.createIsolatedWorld",
                &format!(
                    "{{\"frameId\":{},\"worldName\":\"cu\"}}",
                    json_string(&frame.id)
                ),
            )?;
            match json_value(&reply, "executionContextId").and_then(|v| v.parse::<i64>().ok()) {
                // Trailing comma: the format below puts one comma between the
                // expression and this fragment and none after it.
                Some(id) => format!("\"contextId\":{id},"),
                // The frame went away between the tree walk and here; its
                // section says so rather than failing the whole snapshot.
                None => {
                    text.push_str(&format!(
                        "- frame: {} (unreadable right now; take a new snapshot)\n",
                        frame.url
                    ));
                    continue;
                }
            }
        };
        let params = format!(
            "{{\"expression\":{},{}\"returnByValue\":true}}",
            json_string(&expression),
            context
        );
        let Ok(result) = cmd("Runtime.evaluate", &params) else {
            if index == 0 {
                return Err("page did not return a snapshot".into());
            }
            text.push_str(&format!(
                "- frame: {} (unreadable right now; take a new snapshot)\n",
                frame.url
            ));
            continue;
        };
        let Some(value) = evaluated_string(&result) else {
            if index == 0 {
                return Err("page did not return a snapshot".into());
            }
            text.push_str(&format!(
                "- frame: {} (unreadable right now; take a new snapshot)\n",
                frame.url
            ));
            continue;
        };
        snapshot::saw_refs_up_to(&value);
        let Ok(snap) = parse_snapshot(&value) else {
            if index == 0 {
                return Err("page did not return a snapshot".into());
            }
            text.push_str(&format!(
                "- frame: {} (unreadable right now; take a new snapshot)\n",
                frame.url
            ));
            continue;
        };
        if index == 0 {
            text.push_str(&snap.render());
        } else {
            text.push_str(&format!("- frame: {}\n", frame.url));
            text.push_str(&snap.render_content());
        }
        for node in &snap.nodes {
            pairs.push((node.ref_id.clone(), frame.id.clone()));
        }
    }
    Ok((text, pairs))
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
    let headings = json_array(json, "headings")
        .map(|block| json_strings(&block))
        .unwrap_or_default();
    Ok(snapshot::Snapshot {
        url,
        title,
        headings,
        nodes,
    })
}

/// The string members of a JSON array of strings, unescaped.
pub fn json_strings(array: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = array;
    while let Some(start) = rest.find('"') {
        let member = &rest[start..];
        // End of this member: the first quote that is not escaped.
        let mut escaped = false;
        let Some(len) = member.char_indices().skip(1).find_map(|(i, c)| {
            let end = !escaped && c == '"';
            escaped = !escaped && c == '\\';
            end.then_some(i + 1)
        }) else {
            break;
        };
        if let Some(value) = json_string_value(&format!("{{\"s\":{}}}", &member[..len]), "s") {
            out.push(value);
        }
        rest = &member[len..];
    }
    out
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

/// Wait in the page for the DOM to be usable, in one CDP round trip.
///
/// The first smart wait polled `document.readyState` every 25 ms, so a page
/// that became interactive just after a poll paid most of a tick for nothing:
/// that was the navigate p95 regression (~25 ms -> ~67 ms). Here the page
/// itself resolves a promise on `DOMContentLoaded` -- or at once when it is
/// already past `loading` -- and the daemon awaits it, so the wait ends the
/// moment the page is usable. An SSE stream or websocket cannot hold it open:
/// the in-page timer caps it at `budget`.
fn settle_script(budget: Duration) -> String {
    format!(
        "new Promise(function(r){{\
           if(document.readyState!=='loading'){{r(document.readyState);return;}}\
           document.addEventListener('DOMContentLoaded',function(){{r(document.readyState);}},{{once:true}});\
           setTimeout(function(){{r(document.readyState);}},{});\
         }})",
        budget.as_millis()
    )
}

/// Wait until the page is usable, or give up and let the agent have what is
/// there. Returns how long the wait took and whether the page settled.
///
/// A navigation that swaps the execution context under the evaluate makes it
/// fail; that is retried until the budget is spent, so a cross-document
/// redirect still ends on a settled page rather than an error.
pub fn wait_for_page(cdp_port: u16, budget: Duration) -> (Duration, bool) {
    wait_for_page_on(&Tab::default_for(cdp_port), budget)
}

/// The settle script resolves with the document's `readyState`, so the reply
/// says whether the wait ended on a usable page (`settled`) or ran out of
/// budget against a page still loading -- the difference between "here is
/// your page" and "the page is still coming; take a snapshot before acting".
fn wait_for_page_on(tab: &Tab, budget: Duration) -> (Duration, bool) {
    let started = std::time::Instant::now();
    let budget = budget.max(SETTLE_TIMEOUT);
    let mut settled = false;
    while started.elapsed() < budget {
        let left = budget.saturating_sub(started.elapsed());
        let params = format!(
            "{{\"expression\":{},\"awaitPromise\":true,\"returnByValue\":true}}",
            json_string(&settle_script(left))
        );
        match tab.command("Runtime.evaluate", &params) {
            Ok(reply) if !reply.contains("\"exceptionDetails\"") => {
                settled = evaluated_string(&reply).is_some_and(|state| state != "loading");
                break;
            }
            _ => thread::sleep(Duration::from_millis(2)),
        }
    }
    (started.elapsed(), settled)
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

/// The page a command is for: the default tab, or the tab of a named context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tab {
    pub cdp_port: u16,
    /// DevTools target id; `None` is the persistent profile's own tab.
    pub target: Option<String>,
}

impl Tab {
    pub fn default_for(cdp_port: u16) -> Self {
        Self {
            cdp_port,
            target: None,
        }
    }

    /// Run one command on this tab over a pooled connection.
    pub fn command(&self, method: &str, params: &str) -> Result<String, String> {
        tab_command(self, method, params)
    }
}

/// A browser context: an isolated cookie jar and cache inside the one browser.
struct Context {
    browser_context: String,
    target: String,
    /// Where this context's downloads are directed, once `cu` has told the
    /// browser. `None` until then, and left `None` if the browser refused.
    downloads: Option<PathBuf>,
}

/// Named contexts per browser.
///
/// A second agent (or a second account) used to mean a second `cu` and a
/// second Chromium: ~180 MB and half a second to a second of start-up. A
/// context is ~20 MB and ~35 ms, and shares the browser's processes.
fn contexts() -> &'static Mutex<HashMap<(u16, String), Context>> {
    static CONTEXTS: OnceLock<Mutex<HashMap<(u16, String), Context>>> = OnceLock::new();
    CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The id of the page target the default tab resolves to, per DevTools port.
fn pinned_target(cdp_port: u16) -> Option<String> {
    pinned_targets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cdp_port)
        .cloned()
}

/// Remember which page target the default tab is, so popups and `/json`
/// ordering cannot move it under the agent's feet.
fn pin_target(cdp_port: u16, id: &str) {
    pinned_targets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cdp_port, id.to_string());
}

/// Forget the pinned default (after it was closed); the next action that
/// needs the default picks a live page and pins that one.
fn unpin_target(cdp_port: u16, id: &str) {
    let mut pinned = pinned_targets().lock().unwrap_or_else(|e| e.into_inner());
    if pinned.get(&cdp_port).is_some_and(|p| p == id) {
        pinned.remove(&cdp_port);
    }
}

fn pinned_targets() -> &'static Mutex<HashMap<u16, String>> {
    static PINNED: OnceLock<Mutex<HashMap<u16, String>>> = OnceLock::new();
    PINNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `(id, websocket url)` of every real page target the default tab may use:
/// pages that do not belong to a named context.
fn page_target_ids_and_sockets(discovery: &str, cdp_port: u16) -> Vec<(String, String)> {
    let taken = context_targets(cdp_port);
    json_objects(discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter(|o| json_string_value(o, "id").is_none_or(|id| !taken.contains(&id)))
        .filter_map(|o| {
            match (
                json_string_value(&o, "id"),
                json_string_value(&o, "webSocketDebuggerUrl"),
            ) {
                (Some(id), Some(ws)) => Some((id, ws)),
                _ => None,
            }
        })
        .collect()
}

/// The tab of context `name`, created on first use.
pub fn context_tab(cdp_port: u16, name: &str, data: &Path) -> Result<Tab, String> {
    if !valid_name(name) {
        return Err("invalid context name".into());
    }
    let mut all = contexts().lock().unwrap_or_else(|e| e.into_inner());
    let key = (cdp_port, name.to_string());
    if let Some(context) = all.get(&key) {
        return Ok(Tab {
            cdp_port,
            target: Some(context.target.clone()),
        });
    }
    let created = cdp_browser_command(
        cdp_port,
        "Target.createBrowserContext",
        "{\"disposeOnDetach\":false}",
    )?;
    let browser_context = json_string_value(&created, "browserContextId")
        .ok_or("Chromium did not create a browser context")?;
    let target = cdp_browser_command(
        cdp_port,
        "Target.createTarget",
        &format!(
            "{{\"url\":\"about:blank\",\"browserContextId\":{}}}",
            json_string(&browser_context)
        ),
    )
    .ok()
    .and_then(|reply| json_string_value(&reply, "targetId"));
    let Some(target) = target else {
        let _ = dispose_context(cdp_port, &browser_context);
        return Err("Chromium did not open a tab in the new context".into());
    };
    all.insert(
        key.clone(),
        Context {
            browser_context,
            target: target.clone(),
            downloads: None,
        },
    );
    // Drop the map lock before the CDP round trip below: it is a browser-level
    // command, and holding the lock would serialise every other context.
    drop(all);
    if let Err(error) = direct_context_downloads(cdp_port, &key, data) {
        // Not fatal: the context still works, and its downloads fall back to
        // the browser-wide directory, which `GET /v1/downloads` still lists.
        eprintln!("cu: downloads for context {name} stay browser-wide: {error}");
    }
    Ok(Tab {
        cdp_port,
        target: Some(target),
    })
}

/// Point a named context's downloads at `<data>/downloads/<name>`.
///
/// Without a `browserContextId`, `Browser.setDownloadBehavior` only governs
/// the default context, so a download started from `?context=NAME` landed in
/// the browser-wide directory and was indistinguishable from the persistent
/// profile's own files. A context that has its own behaviour gets its own
/// directory, and the browser-wide listing picks the subdirectories up, so
/// `GET /v1/downloads` reports them under the context's name.
fn direct_context_downloads(cdp_port: u16, key: &(u16, String), data: &Path) -> Result<(), String> {
    let dir = context_download_dir(data, &key.1);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let browser_context = {
        let all = contexts().lock().unwrap_or_else(|e| e.into_inner());
        all.get(key).map(|c| c.browser_context.clone())
    };
    let Some(browser_context) = browser_context else {
        return Err("the context went away".into());
    };
    cdp_browser_command(
        cdp_port,
        "Browser.setDownloadBehavior",
        &format!(
            "{{\"behavior\":\"allow\",\"downloadPath\":{},\"eventsEnabled\":true,\"browserContextId\":{}}}",
            json_string(&dir.to_string_lossy()),
            json_string(&browser_context)
        ),
    )?;
    let mut all = contexts().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(context) = all.get_mut(key) {
        context.downloads = Some(dir);
    }
    Ok(())
}

/// Where the downloads of context `name` are kept.
fn context_download_dir(data: &Path, name: &str) -> PathBuf {
    data.join("downloads").join(name)
}

/// A tab another tab named by id (`?tab=ID`), validated against discovery so
/// a stale id is an error an agent can act on rather than a CDP failure.
pub fn named_tab(cdp_port: u16, id: &str) -> Result<Tab, String> {
    let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
    let known = json_objects(&discovery).into_iter().any(|o| {
        json_string_value(&o, "type").as_deref() == Some("page")
            && json_string_value(&o, "id").as_deref() == Some(id)
    });
    if !known {
        return Err("no such tab; see GET /v1/tabs".into());
    }
    Ok(Tab {
        cdp_port,
        target: Some(id.to_string()),
    })
}

/// Close context `name`: its tab, cookies and cache go with it.
pub fn close_context(cdp_port: u16, name: &str) -> Result<bool, String> {
    let removed = contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cdp_port, name.to_string()));
    let Some(context) = removed else {
        return Ok(false);
    };
    forget_tab(&Tab {
        cdp_port,
        target: Some(context.target),
    });
    dispose_context(cdp_port, &context.browser_context)?;
    Ok(true)
}

/// Names of the open contexts.
pub fn context_names(cdp_port: u16) -> Vec<String> {
    let mut names: Vec<String> = contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|(port, _)| *port == cdp_port)
        .map(|(_, name)| name.clone())
        .collect();
    names.sort();
    names
}

fn dispose_context(cdp_port: u16, browser_context: &str) -> Result<(), String> {
    cdp_browser_command(
        cdp_port,
        "Target.disposeBrowserContext",
        &format!("{{\"browserContextId\":{}}}", json_string(browser_context)),
    )
    .map(|_| ())
}

/// Target ids that belong to a named context, so the default tab is never
/// mistaken for one of them.
fn context_targets(cdp_port: u16) -> Vec<String> {
    contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((port, _), _)| *port == cdp_port)
        .map(|(_, c)| c.target.clone())
        .collect()
}

/// A DevTools websocket kept open between commands.
///
/// Opening one cost a `/json` discovery request, a TCP connect and a websocket
/// handshake per action -- all of it pure overhead repeated for every navigate,
/// screenshot and snapshot an agent takes. Keeping the connection means an
/// action is one write and one read.
pub struct CdpConnection {
    stream: TcpStream,
    next_id: u64,
}

impl CdpConnection {
    /// Discover the tab's target and complete the websocket handshake.
    fn open(tab: &Tab) -> Result<Self, String> {
        let discovery = http_get(&format!("127.0.0.1:{}", tab.cdp_port), "/json")?;
        let ws = match &tab.target {
            Some(id) => json_objects(&discovery)
                .into_iter()
                .find(|o| json_string_value(o, "id").as_deref() == Some(id))
                .and_then(|o| json_string_value(&o, "webSocketDebuggerUrl"))
                .ok_or("no DevTools target with that id any more (see GET /v1/tabs)")?,
            None => {
                let pages = page_target_ids_and_sockets(&discovery, tab.cdp_port);
                // Pin the default tab. `/json` lists a freshly opened popup
                // ahead of the tab the agent is working in, so without a pin
                // the next connection made with an empty pool would silently
                // drive a different page.
                let pinned = pinned_target(tab.cdp_port);
                let chosen = pinned
                    .as_ref()
                    .and_then(|p| pages.iter().find(|(id, _)| id == p))
                    .or_else(|| pages.first())
                    .ok_or("Chromium did not expose a page target")?;
                pin_target(tab.cdp_port, &chosen.0);
                chosen.1.clone()
            }
        };
        Self::connect(&ws)
    }

    /// Complete the websocket handshake with one DevTools endpoint.
    fn connect(ws: &str) -> Result<Self, String> {
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
        // Read until the blank line that ends the response head. A single
        // `read()` can return the status line alone when the machine is
        // loaded, which used to be reported as a rejected handshake.
        let mut handshake: Vec<u8> = Vec::with_capacity(256);
        let mut buf = [0; 512];
        loop {
            if head_end(&handshake).is_some() {
                break;
            }
            if handshake.len() > 8192 {
                return Err("oversized CDP websocket handshake".into());
            }
            match stream.read(&mut buf) {
                Ok(0) => return Err("Chromium closed during the CDP websocket handshake".into()),
                Ok(n) => handshake.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e.to_string()),
            }
        }
        if !handshake.starts_with(b"HTTP/1.1 101") {
            return Err("Chromium rejected CDP websocket handshake".into());
        }
        Ok(Self { stream, next_id: 1 })
    }

    /// Send one command and read its reply, skipping events meant for nobody.
    pub(crate) fn call(&mut self, method: &str, params: &str) -> Result<String, String> {
        self.call_observed(method, params, &mut |_| {})
    }

    /// Like [`call`](Self::call), but hands every event that arrives before
    /// the reply to `observe`. That is how an action learns it started a
    /// navigation without a second round trip: Chromium announces the
    /// navigation before it answers the input event that caused it.
    pub(crate) fn call_observed(
        &mut self,
        method: &str,
        params: &str,
        observe: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let message = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
        self.stream
            .write_all(&encode_client_frame(message.as_bytes()))
            .map_err(|e| e.to_string())?;
        loop {
            let text = self.next_message(MAX_CDP_MESSAGE)?;
            // Events carry no `id`; a reply for an older, abandoned command is
            // not ours either. Both are skipped rather than misreported.
            match message_id(&text) {
                Some(got) if got == id => return Ok(text),
                Some(_) => continue,
                None => observe(&text),
            }
        }
    }

    /// Read events until `done` says stop or `deadline` passes.
    ///
    /// Returns `false` if the deadline cut a read short: the stream may then
    /// hold half a frame, so the caller must not pool the connection again.
    pub(crate) fn pump_events(
        &mut self,
        deadline: std::time::Instant,
        observe: &mut dyn FnMut(&str) -> bool,
    ) -> Result<bool, String> {
        let clean = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break false;
            }
            let _ = self.stream.set_read_timeout(Some(left));
            match self.next_message(MAX_CDP_MESSAGE) {
                Ok(text) => {
                    if message_id(&text).is_none() && observe(&text) {
                        break true;
                    }
                }
                Err(_) => break false,
            }
        };
        let _ = self.stream.set_read_timeout(Some(CDP_HTTP_TIMEOUT));
        Ok(clean)
    }

    fn next_message(&mut self, limit: usize) -> Result<String, String> {
        let payload = read_frame(&mut self.stream, limit)?;
        String::from_utf8(payload).map_err(|e| e.to_string())
    }
}

/// The `id` of a CDP message: a reply has one at the top level, an event has
/// none. Nested objects (a frame tree, a node) have ids of their own, so the
/// first `"id"` in the text is not necessarily the message's.
fn message_id(message: &str) -> Option<u64> {
    let bytes = message.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &c) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'"' if depth == 1 && message[i..].starts_with("\"id\"") => {
                return json_value(&message[i..], "id")?.parse().ok();
            }
            b'"' => in_string = true,
            _ => {}
        }
    }
    None
}

/// Take a warm DevTools connection for a sequence of commands.
///
/// A batch of actions runs on one connection so its commands are ordered and
/// pay no hand-off between them. Give it back with [`checkin`] only if every
/// command on it completed.
pub fn checkout(tab: &Tab) -> Result<CdpConnection, String> {
    let pool = cdp_pool(tab);
    let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
    guard.take(tab)
}

/// A fresh connection, for when a pooled one turned out to be dead.
pub fn open_connection(tab: &Tab) -> Result<CdpConnection, String> {
    CdpConnection::open(tab)
}

pub fn checkin(tab: &Tab, connection: CdpConnection) {
    let pool = cdp_pool(tab);
    let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
    guard.put(connection);
}

/// The string a `Runtime.evaluate` with `returnByValue` produced.
///
/// The reply is `{"result":{"result":{"type":"string","value":"..."}}}`; look
/// one level in and fall back to the raw reply so both shapes work.
pub fn evaluated_string(reply: &str) -> Option<String> {
    json_objects(reply)
        .iter()
        .filter_map(|o| json_string_value(o, "value"))
        .next()
        .or_else(|| json_string_value(reply, "value"))
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
    fn take(&mut self, tab: &Tab) -> Result<CdpConnection, String> {
        self.idle
            .pop()
            .map(Ok)
            .unwrap_or_else(|| CdpConnection::open(tab))
    }

    /// Give a still-usable connection back, up to the idle limit.
    fn put(&mut self, connection: CdpConnection) {
        if self.idle.len() < CDP_POOL_IDLE {
            self.idle.push(connection);
        }
    }
}

fn pools() -> &'static Mutex<HashMap<Tab, &'static Mutex<CdpPool>>> {
    static POOLS: OnceLock<Mutex<HashMap<Tab, &'static Mutex<CdpPool>>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cdp_pool(tab: &Tab) -> &'static Mutex<CdpPool> {
    let mut guard = pools().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(tab.clone())
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(CdpPool { idle: Vec::new() }))))
}

/// Drop the warm connections of a tab that has been closed.
fn forget_tab(tab: &Tab) {
    let pool = pools()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .copied();
    if let Some(pool) = pool {
        pool.lock().unwrap_or_else(|e| e.into_inner()).idle.clear();
    }
}

/// Run one DevTools command on a pooled connection.
///
/// Reuse is what makes an action cheap: without it every navigate, screenshot
/// and snapshot paid a `/json` discovery, a TCP connect and a websocket
/// handshake before the actual command. A connection the browser has dropped is
/// reopened and retried once, so a target swap is not an error.
pub fn cdp_command(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
    tab_command(&Tab::default_for(cdp_port), method, params)
}

fn tab_command(tab: &Tab, method: &str, params: &str) -> Result<String, String> {
    let pool = cdp_pool(tab);
    for attempt in 0..2 {
        let mut connection = {
            let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
            guard.take(tab)
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
/// Run one command on the browser-level DevTools endpoint (not a page).
fn cdp_browser_command(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
    let version = http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version")?;
    let ws = json_string_value(&version, "webSocketDebuggerUrl")
        .ok_or("Chromium did not expose a browser endpoint")?;
    CdpConnection::connect(&ws)?.call(method, params)
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
    fn the_settle_wait_resolves_in_the_page_and_is_bounded() {
        let script = settle_script(Duration::from_millis(1234));
        assert!(script.contains("DOMContentLoaded"));
        assert!(script.contains("setTimeout"));
        assert!(script.contains("1234"));
    }

    #[test]
    fn the_browser_is_launched_light() {
        for flag in [
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
        ] {
            assert!(BROWSER_ARGS.contains(&flag), "{flag} missing");
        }
        // One --disable-features: Chromium only honours the last one it sees.
        let features: Vec<_> = BROWSER_ARGS
            .iter()
            .filter(|a| a.starts_with("--disable-features="))
            .collect();
        assert_eq!(features.len(), 1);
        assert!(
            !features[0].contains(' '),
            "feature list must not hold spaces"
        );
    }

    #[test]
    fn headings_are_read_back_with_their_escapes() {
        assert_eq!(
            json_strings(r#"["Hello","a \"b\" c","x\\"]"#),
            vec!["Hello", "a \"b\" c", "x\\"]
        );
        assert!(json_strings("[]").is_empty());
        let snap = parse_snapshot(r#"{"url":"u","headings":["Welcome"],"nodes":[]}"#).unwrap();
        assert!(snap.render().contains("- heading \"Welcome\"\n"));
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
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

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
        /// Visible headings: the page's outline, which is what tells an agent
        /// where it landed (a result, an error, a welcome).
        pub headings: Vec<String>,
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
            out.push_str(&self.render_content());
            out
        }

        /// Headings and element lines without the page header: what a
        /// subframe contributes under its own `- frame:` line.
        pub fn render_content(&self) -> String {
            let mut out = String::new();
            for heading in &self.headings {
                out.push_str(&format!("- heading \"{heading}\"\n"));
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

    use std::sync::atomic::{AtomicU64, Ordering};

    /// First ref number a freshly loaded document hands out.
    ///
    /// Refs restart in every document, so `e3` from the page before a
    /// navigation would name some other element on the page after it, and an
    /// agent acting on a stale snapshot would click the wrong thing. Carrying
    /// the counter over from the daemon makes a stale ref miss instead.
    static REF_BASE: AtomicU64 = AtomicU64::new(0);

    /// Refs one frame may use before the next frame's band begins.
    ///
    /// A snapshot walks several frames (the document and its iframes); each
    /// frame numbers its refs from its own band so two frames can never hand
    /// out the same `e7`. The walk caps nodes at 200, so 10 000 leaves room
    /// for documents far bigger than any real page.
    pub const BAND: u64 = 10_000;

    /// First ref number a frame's document starts from: every ref ever handed
    /// out is below `REF_BASE`, so a fresh document in any frame begins above
    /// them all and an old ref misses instead of naming a new element.
    pub fn base_for(cdp_port: u16, frame_id: &str) -> u64 {
        REF_BASE.load(Ordering::Relaxed) + frame_slot(cdp_port, frame_id) * BAND
    }

    /// A stable slot per frame: the band an iframe's refs live in must not
    /// move between snapshots, or its persisted registry and the daemon's
    /// ref-to-frame map would disagree.
    fn frame_slot(cdp_port: u16, frame_id: &str) -> u64 {
        static SLOTS: OnceLock<Mutex<HashMap<u16, HashMap<String, u64>>>> = OnceLock::new();
        let mut all = SLOTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ports = all.entry(cdp_port).or_default();
        let next = ports.len() as u64;
        *ports.entry(frame_id.to_string()).or_insert(next)
    }

    /// The full in-page script: ref registry, then the walk, numbered from
    /// `base`.
    pub fn script_with_base(base: u64) -> String {
        format!(
            "window.__cuBase={base};{}{}",
            crate::actions::REGISTRY_JS,
            SNAPSHOT_JS
        )
    }

    /// Record the highest ref a page has handed out.
    pub fn saw_refs_up_to(snapshot_json: &str) {
        if let Some(next) = super::json_value(snapshot_json, "next").and_then(|n| n.parse().ok()) {
            REF_BASE.fetch_max(next, Ordering::Relaxed);
        }
    }

    /// JavaScript that walks the a11y-relevant tree and returns compact JSON.
    ///
    /// Everything happens in the page, so only the small result crosses CDP --
    /// that is the whole point of a snapshot: one round trip, a few hundred
    /// bytes, instead of a full DOM dump.
    ///
    /// The walk descends into open shadow roots, because a web component
    /// renders its controls there and they are otherwise invisible: a
    /// widget-heavy page would snapshot as a page with nothing on it. A closed
    /// root hides its contents from every script and stays unread, which is
    /// what the page itself sees.
    pub const SNAPSHOT_JS: &str = r#"
/* cuAgentSnapshot */
(() => {
  const INTERACTIVE = 'a[href],button,input,select,textarea,[role=button],[role=link],[role=checkbox],[role=tab],[role=menuitem],[contenteditable]:not([contenteditable=false])';
  const HEADINGS = 'h1,h2,h3,[role=heading]';
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
    if (el.tagName === 'SELECT') return clean(el.selectedOptions[0]?.label);
    const img = el.querySelector('img[alt]');
    if (img) return img.alt;
    return clean(el.textContent) || clean(el.getAttribute('placeholder')) || clean(el.value) || '';
  };
  const roleOf = (el) => el.getAttribute('role')
    || ({A:'link',BUTTON:'button',SELECT:'combobox',TEXTAREA:'textbox'})
      [el.tagName]
    || (el.tagName === 'INPUT'
      ? ({text:'textbox',search:'searchbox',email:'textbox',password:'textbox',
          number:'spinbutton',checkbox:'checkbox',radio:'radio',file:'button'})[el.type] || 'textbox'
      : el.isContentEditable ? 'textbox'
      : 'generic');
  const nodes = [];
  const headings = [];
  // One root at a time: the document, then every open shadow root below it.
  // `querySelectorAll` does not pierce a shadow root, so each one has to be
  // asked for its own -- and a host is skipped in its parent's pass, because
  // the root renders it, not the children it also happens to have.
  const walk = (root) => {
    for (const el of root.querySelectorAll(INTERACTIVE)) {
      if (el.shadowRoot) continue;
      if (!visible(el)) continue;
      if (el.closest('[aria-hidden=true]')) continue;
      nodes.push({ ref: window.__cu.ref(el), role: roleOf(el), name: name(el).slice(0, 120) });
      if (nodes.length >= 200) return;
    }
    for (const el of root.querySelectorAll('*')) if (el.shadowRoot) walk(el.shadowRoot);
  };
  walk(document);
  const outline = (root) => {
    for (const h of root.querySelectorAll(HEADINGS)) {
      if (!visible(h)) continue;
      const t = clean(h.textContent).slice(0, 120);
      if (t) headings.push(t);
      if (headings.length >= 20) return true;
    }
    for (const el of root.querySelectorAll('*')) if (el.shadowRoot && outline(el.shadowRoot)) return true;
    return false;
  };
  outline(document);
  return JSON.stringify({
    url: location.href,
    title: document.title,
    nodes,
    headings,
    next: window.__cu.next(),
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
        let pool = cdp_pool(&Tab::default_for(port));
        // Nothing is cached yet, and the pool is per tab.
        assert!(
            pool.lock()
                .unwrap_or_else(|e| e.into_inner())
                .idle
                .is_empty()
        );
        let other = Tab {
            cdp_port: port,
            target: Some("CTX-TAB".into()),
        };
        assert!(!std::ptr::eq(pool, cdp_pool(&other)));
        assert!(
            cdp_pool(&Tab::default_for(port + 1))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .idle
                .is_empty()
        );
    }

    #[test]
    fn a_reply_is_matched_by_its_own_id_not_a_nested_one() {
        assert_eq!(
            message_id(r#"{"result":{"frame":{"id":"F1"}},"id":7}"#),
            Some(7)
        );
        assert_eq!(message_id(r#"{"id":3,"result":{}}"#), Some(3));
        assert_eq!(
            message_id(r#"{"method":"Page.frameNavigated","params":{"frame":{"id":"F"}}}"#),
            None
        );
        assert_eq!(message_id(r#"{"s":"\"id\":9","method":"x"}"#), None);
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

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn a_frame_tree_flattens_main_first_then_children() {
        let tree = r#"{"id":1,"result":{"frameTree":{"frame":{"id":"M","url":"https://a.test/"},
            "childFrames":[{"frame":{"id":"C1","url":"https://b.test/"},
                "childFrames":[{"frame":{"id":"C2","url":"https://c.test/"}}]}]}}}"#;
        let frames = flatten_frame_tree(tree);
        let ids: Vec<&str> = frames.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["M", "C1", "C2"], "main document must come first");
        assert_eq!(frames[0].parent, None);
        assert_eq!(frames[1].parent.as_deref(), Some("M"));
        assert_eq!(frames[2].parent.as_deref(), Some("C1"));
        assert_eq!(frames[2].url, "https://c.test/");
    }

    #[test]
    fn a_tree_with_no_children_is_one_frame() {
        let tree = r#"{"id":1,"result":{"frameTree":{"frame":{"id":"M","url":"about:blank"}}}}"#;
        assert_eq!(flatten_frame_tree(tree).len(), 1);
        assert_eq!(flatten_frame_tree("not json").len(), 0);
    }

    #[test]
    fn the_walk_descends_into_shadow_roots() {
        let js = snapshot::SNAPSHOT_JS;
        // A document query cannot see a shadow root, so the walk has to ask
        // each root for itself and recurse. Without this the controls of a
        // web component are invisible and the page snapshots as empty.
        assert!(
            js.contains("el.shadowRoot"),
            "the walk must recurse into shadow roots: {js}"
        );
        // ...and a host is skipped in its parent's pass, or the same element
        // would be listed twice.
        assert!(
            js.contains("if (el.shadowRoot) continue;"),
            "a shadow host must not also be listed from its parent: {js}"
        );
        // The heading outline gets the same treatment, or a component's
        // headings never appear.
        assert!(
            js.matches("shadowRoot").count() >= 3,
            "headings must be walked too: {js}"
        );
    }

    #[test]
    fn the_hit_test_sees_past_a_shadow_host() {
        let js = crate::actions::REGISTRY_JS;
        // `document.elementFromPoint` reports the host at the centre of a
        // control inside it, which reads as "covered" and refuses the click.
        assert!(
            js.contains("at.shadowRoot.elementFromPoint"),
            "the hit test must descend into the shadow root: {js}"
        );
    }

    #[test]
    fn the_text_read_reaches_open_shadow_roots_too() {
        let js = READ_TEXT_JS;
        assert!(
            js.contains("el.shadowRoot"),
            "text must include what a web component renders: {js}"
        );
        // Chromium gives a ShadowRoot no innerText, so the words come from
        // the visible leaves inside it rather than from the root itself.
        assert!(
            !js.contains("r.innerText"),
            "a ShadowRoot has no innerText in Chromium: {js}"
        );
        // And a subtree that is not rendered must not leak: innerText
        // degrades to textContent there.
        assert!(js.contains("display"), "visibility is not checked: {js}");
    }

    #[test]
    fn frames_number_their_refs_in_stable_bands() {
        let port = 64_001;
        let first = snapshot::base_for(port, "FRAME-A");
        let second = snapshot::base_for(port, "FRAME-B");
        assert_eq!(first + snapshot::BAND, second, "bands must not overlap");
        // The same frame always gets the same band, or its persisted
        // registry and the daemon's ref map would disagree.
        assert_eq!(snapshot::base_for(port, "FRAME-A"), first);
    }

    #[test]
    fn a_walk_reads_every_frame_and_records_where_each_ref_lives() {
        let tab = Tab::default_for(64_002);
        let frames = vec![
            Frame {
                id: "M".into(),
                parent: None,
                url: "https://main.test/".into(),
            },
            Frame {
                id: "C".into(),
                parent: Some("M".into()),
                url: "https://child.test/".into(),
            },
        ];
        let main_walk = r#"{"url":"https://main.test/","title":"Main","nodes":[{"ref":"e1","role":"link","name":"go"}],"headings":[],"next":1}"#;
        let child_walk = r#"{"url":"https://child.test/","title":"Child","nodes":[{"ref":"e9","role":"textbox","name":"inner"}],"headings":[],"next":9}"#;
        let (text, pairs) = walk_frames(&tab, frames, |method, params| match method {
            "Page.createIsolatedWorld" => {
                assert!(params.contains("\"frameId\":\"C\""), "entered {params}");
                Ok(r#"{"result":{"executionContextId":7}}"#.to_string())
            }
            "Runtime.evaluate" if params.contains("contextId") => Ok(format!(
                "{{\"result\":{{\"result\":{{\"type\":\"string\",\"value\":{}}}}}}}",
                json_string(child_walk)
            )),
            "Runtime.evaluate" => Ok(format!(
                "{{\"result\":{{\"result\":{{\"type\":\"string\",\"value\":{}}}}}}}",
                json_string(main_walk)
            )),
            other => Err(format!("unexpected {other}")),
        })
        .expect("walk");
        assert!(text.contains("- url: https://main.test/"), "{text}");
        assert!(text.contains("- ref=e1 link \"go\""), "{text}");
        assert!(text.contains("- frame: https://child.test/"), "{text}");
        assert!(text.contains("- ref=e9 textbox \"inner\""), "{text}");
        assert_eq!(
            pairs,
            vec![
                ("e1".to_string(), "M".to_string()),
                ("e9".to_string(), "C".to_string())
            ]
        );
        // And the map is what an action will consult later.
        note_ref_frames(&tab, &pairs);
        assert_eq!(frame_for_ref(&tab, "e9").as_deref(), Some("C"));
        assert_eq!(frame_for_ref(&tab, "e1").as_deref(), Some("M"));
        assert_eq!(frame_for_ref(&tab, "e404"), None);
    }

    #[test]
    fn a_child_frame_that_cannot_be_walked_is_named_not_fatal() {
        let tab = Tab::default_for(64_003);
        let frames = vec![
            Frame {
                id: "M".into(),
                parent: None,
                url: "https://main.test/".into(),
            },
            Frame {
                id: "GONE".into(),
                parent: Some("M".into()),
                url: "https://gone.test/".into(),
            },
        ];
        let main_walk =
            r#"{"url":"https://main.test/","title":"Main","nodes":[],"headings":[],"next":0}"#;
        let (text, pairs) = walk_frames(&tab, frames, |method, params| match method {
            "Page.createIsolatedWorld" => Ok(r#"{"result":{}}"#.to_string()),
            "Runtime.evaluate" => Ok(format!(
                "{{\"result\":{{\"result\":{{\"type\":\"string\",\"value\":{}}}}}}}",
                json_string(main_walk)
            )),
            other => Err(format!("unexpected {other} in {params}")),
        })
        .expect("walk");
        assert!(
            text.contains("- frame: https://gone.test/ (unreadable right now"),
            "{text}"
        );
        assert!(pairs.is_empty());
    }
}
