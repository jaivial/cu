//! Loopback HTTP server for `cu`: request parsing, routing and the browser bridge.
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::pagecache;
use crate::policy::BrowserPolicy;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub token: String,
    /// Port Chromium listens on for the DevTools protocol (default 9222).
    pub cdp_port: u16,
    /// Readiness of the persistent browser, shared with the launch thread.
    pub browser: Arc<BrowserState>,
    /// Host (lower-case, no port) of the public URL a reverse proxy serves
    /// `/login` under (`CU_PUBLIC_URL` / `--public-url`), if any.
    pub public_host: Option<String>,
    /// Host the vault UI is served under (`cu-vault.menustudioai.com`,
    /// `CU_VAULT_HOST`), when a reverse proxy forwards it. Only `/vault`
    /// paths answer that host; the API stays loopback-named.
    pub vault_host: Option<String>,
    /// This process is the canonical daemon (`cu daemon`): the CLI may open
    /// a tab per invocation and the tab garbage collector runs.
    pub daemon: bool,
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
    policy: BrowserPolicy,
) -> JoinHandle<()> {
    thread::spawn(move || match launch_browser(&data, cdp_port, &policy) {
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
            let launched = Instant::now();
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
                    browser.mark_ready();
                    // After readiness: the record is for the log and
                    // `/v1/diagnostics`, and must not delay the first action.
                    let version = http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version").ok();
                    let info = cdp_browser_command(cdp_port, "SystemInfo.getInfo", "{}").ok();
                    crate::diagnostics::record_ready(
                        &data,
                        version.as_deref(),
                        info.as_deref(),
                        launched.elapsed(),
                    );
                    restore_cookies(cdp_port, &data.join("profiles/default"));
                    observe_identity(cdp_port);
                    harden(cdp_port, &policy);
                }
                Err(e) => {
                    crate::diagnostics::record_failure(&e);
                    browser.mark_failed(e)
                }
            }
        }
        Err(e) => {
            crate::diagnostics::record_failure(&e);
            browser.mark_failed(e)
        }
    })
}

/// Put the cookies a loaded session captured back into the browser.
fn restore_cookies(cdp_port: u16, profile: &Path) {
    let Some(cookies) = crate::session::take_pending_cookies(profile) else {
        return;
    };
    if let Err(e) = cdp_browser_command(
        cdp_port,
        "Storage.setCookies",
        &format!("{{\"cookies\":{cookies}}}"),
    )
    .and_then(|reply| match reply.contains("\"error\":{") {
        true => Err(json_value(&reply, "message").unwrap_or(reply)),
        false => Ok(()),
    }) {
        eprintln!("cu: could not restore the saved session's cookies: {e}");
    }
}

/// Give the browser the fingerprint hardening [`crate::stealth`] describes,
/// before the first action, and note what happened.
///
/// Best effort by design: a browser that could not be hardened still browses
/// (`cu navigate` must never fail because a defence script did not apply),
/// and `/v1/diagnostics` reports what did or did not happen. The window the
/// profile asks for is the size the window is set to, grown onto the whole
/// virtual screen when the display's size is known, so `screen` reads a real
/// desktop rather than an 800x600 window floating in one.
fn harden(cdp_port: u16, policy: &BrowserPolicy) {
    if !policy.stealth {
        eprintln!("cu: stealth off (CU_STEALTH=0)");
        return;
    }
    let window = policy.display.as_deref().and_then(|display| {
        let (width, height) = crate::session::identity()
            .and_then(|i| i.window)
            .unwrap_or((1440, 900));
        match crate::stealth::display_size(display) {
            Some((screen_w, screen_h)) => Some((
                width.max(screen_w).min(4096),
                height.max(screen_h).min(2160),
            )),
            None => Some((width, height)),
        }
    });
    match crate::stealth::apply(cdp_port, window) {
        Ok(notes) => {
            for note in notes {
                eprintln!("cu: stealth: {note}");
            }
            if let Some(display) = &policy.display {
                if let Some((width, height)) = window {
                    eprintln!("cu: stealth: window {width}x{height} on {display}");
                }
            }
        }
        Err(e) => eprintln!("cu: stealth could not be applied: {e}"),
    }
}

/// Read back, from cu's isolated world of the default tab, what a page sees
/// of the identity (language, zone, platform, viewport), and note any
/// disagreement with what the profile asked for.
fn observe_identity(cdp_port: u16) {
    let tab = Tab::default_for(cdp_port);
    let mut cmd = |m: &str, p: &str| tab.command(m, p);
    let observed = crate::execution::main_frame(&mut cmd)
        .and_then(|frame| {
            crate::execution::context_fragment(&mut cmd, crate::execution::World::Isolated, &frame)
        })
        .and_then(|fragment| {
            cmd(
                "Runtime.evaluate",
                &crate::execution::evaluate_params(crate::session::OBSERVE_JS, &fragment, false),
            )
        })
        .ok()
        .and_then(|reply| evaluated_string(&reply));
    if let Some(observed) = observed {
        crate::session::record_observed(&observed);
    }
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
        // Target discovery on the same connection: every popup, iframe
        // target and worker is announced here (observation only; nothing is
        // attached), for `GET /v1/targets` (see `crate::targets`).
        if let Err(error) = connection.call_observed(
            "Target.setDiscoverTargets",
            "{\"discover\":true}",
            &mut |e| crate::targets::see(e),
        ) {
            eprintln!("cu: target discovery is off: {error}");
        }
        // Drain browser events (download progress, target changes) so the
        // socket buffer never fills up. A blocking read with no timeout: the
        // session must outlive hours between downloads.
        let _ = connection.stream.set_read_timeout(None);
        while let Ok(event) = connection.next_message(MAX_CDP_MESSAGE) {
            crate::targets::see(&event);
            crate::challenge::see_target_event(cdp_port, &event);
        }
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
    // The X server this daemon started goes with the browser: it has no other
    // user, and a stale `:99` would keep the next launch headful when it
    // should be headless.
    crate::stealth::stop_display();
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

/// Launch the persistent browser that owns `data/profiles/default`, with the
/// executable, flags and headless mode `policy` decided (see
/// [`crate::policy`]).
///
/// The DevTools port must match the port the daemon was configured with:
/// `launch_browser` used to hard-code 9222, so a daemon started with
/// `--port`/`CU_CDP_PORT` pointed at a browser that was listening somewhere
/// else and every navigate/screenshot failed with "connection refused".
pub fn launch_browser(data: &Path, cdp_port: u16, policy: &BrowserPolicy) -> Result<Child, String> {
    clear_stale_locks(data);
    let profile = data.join("profiles/default");
    let mut args = policy.args(&profile, cdp_port);
    // The profile's identity (locale, time zone, window), from the host and
    // stable across restarts; flags go before the start URL.
    let (identity, notes) = crate::session::Identity::load_or_create(&profile);
    let start_url = args.pop();
    args.extend(identity.args(policy.headless));
    args.extend(start_url);
    for note in &notes {
        eprintln!("cu: identity: {note}");
    }
    crate::session::set_current(&identity, notes);
    crate::diagnostics::record_launch(policy, &args);
    eprintln!(
        "cu: launching {} -> {} ({} mode, {})",
        policy.executable.display(),
        policy.resolved.display(),
        policy.mode.name(),
        if policy.headless_shell {
            "headless-shell"
        } else if policy.headless {
            "headless=new"
        } else if policy.display.is_some() {
            "headful"
        } else {
            "headful asked for, no X display: headless=new"
        }
    );
    if let Some(display) = &policy.display {
        eprintln!("cu: browser window on DISPLAY={display}");
    }
    let child = Command::new(&policy.executable)
        .args(&args)
        .envs(identity.env())
        .envs(
            policy
                .display
                .iter()
                .map(|display| ("DISPLAY", display.as_str())),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!(
                "could not start {} (set CU_BROWSER): {e}",
                policy.executable.display()
            )
        })?;
    // A browser that exits immediately (no display, bad binary) is reported by
    // `wait_for_browser` on the first poll instead of after a fixed sleep here,
    // which used to cost every start 300 ms whether or not it was needed.
    Ok(child)
}

/// How often DevTools is probed while a browser is coming up. A loopback
/// connect is a few microseconds, so this is pure responsiveness, not load.
const CDP_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Longest a loopback DevTools connect may take.
const CDP_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
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
    let span = crate::server::trace_start();
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
    let head = request.split(sep).next().unwrap_or("");
    let host_ok = host_allowed(
        header_value(head, "host"),
        path,
        state.public_host.as_deref(),
        state.vault_host.as_deref(),
    );
    // The vault's bundled typeface (OFL Manrope) is binary: it never
    // passes through the String response type. Served on page-route terms
    // (the login page needs it before any session exists).
    if host_ok && method == "GET" && path == "/vault/manrope.ttf" {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n",
            MANROPE.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(MANROPE);
        return;
    }
    // The live stream is a WebSocket: it takes the connection over instead of
    // answering one request, and gates itself (vault session or bearer).
    if host_ok && crate::live_stream::wants(method, path, head) {
        crate::live_stream::serve(stream, head, path, authorized, &state);
        return;
    }
    let mut set_cookie: Option<String> = None;
    let mut tab_used: Option<(String, bool)> = None;
    let route_span = crate::server::trace_start();
    let (status, content_type, response) = if !host_ok {
        (
            "421 Misdirected Request",
            "application/json",
            "{\"error\":\"host not served here\"}".into(),
        )
    } else if path == "/login" && method == "GET" {
        ("200 OK", "text/html", login_page())
    } else if path == "/login" && method == "POST" {
        ("200 OK", "text/html", login_submit(body, &state))
    } else if method == "POST" && is_vault_path(path) && is_vault_ui_only(path) && !origin_ok(head)
    {
        (
            "403 Forbidden",
            "application/json",
            "{\"error\":\"cross-origin POST refused\"}".into(),
        )
    } else if is_vault_path(path) && (!authorized || is_vault_ui_only(path)) {
        // The vault UI authenticates with its own cookie (master password
        // behind nginx basic auth); the local CLI keeps using the bearer.
        // Approve/deny are UI-only even for a bearer caller: every agent on
        // this machine holds the same bearer token, so an agent allowed to
        // answer its own secret request would make the approval theatre.
        let (status, kind, response, cookie) = vault_route_with(
            method,
            path,
            body,
            &state,
            header_value(head, "cookie"),
            header_value(head, "x-cu-otp-collect-token"),
        );
        set_cookie = cookie;
        (status, kind, response)
    } else if !authorized {
        (
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"missing or invalid bearer token\"}".into(),
        )
    } else {
        route_with(
            method,
            path,
            body,
            &state,
            header_value(head, "x-cu-otp-collect-token"),
            session_of(head),
            &mut tab_used,
        )
    };
    crate::server::trace("http.route.total", route_span);
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n",
        response.len()
    );
    if let Some(cookie) = set_cookie {
        head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    // The tab the request was served on, for the CLI to name it.
    if let Some((tab, is_new)) = &tab_used {
        head.push_str(&format!("X-Cu-Tab: {tab}\r\n"));
        if *is_new {
            head.push_str("X-Cu-Tab-New: 1\r\n");
        }
    }
    // Only the read-only viewer page may be framed, and only by the one origin
    // named in CU_LIVE_FRAME_ORIGIN (an exact `https://host`); without it
    // nothing may frame it. The page with controls, /vault/live2, may never be
    // framed. Other routes keep their current headers.
    match path.split('?').next() {
        Some("/vault/live2/view") => head.push_str(&format!(
            "Content-Security-Policy: frame-ancestors {}\r\n",
            crate::live_stream::frame_ancestors()
        )),
        Some("/vault/live2") => {
            head.push_str("Content-Security-Policy: frame-ancestors 'none'\r\n")
        }
        _ => {}
    }
    head.push_str("\r\n");
    let _ = write!(stream, "{head}{response}");
    crate::server::trace("http.handle.total", span);
}

/// The session a request belongs to: the process it works in, and whether it
/// asked for a tab of its own.
///
/// The CLI names its session in a header, and the daemon resolves the tab
/// that session works in, in the same request.
fn session_of(head: &str) -> Option<(u32, bool)> {
    let pid = header_value(head, "x-cu-session")?.trim().parse::<u32>().ok()?;
    let fresh = header_value(head, "x-cu-session-fresh")
        .map(|v| !v.trim().eq_ignore_ascii_case("0") && !v.trim().is_empty())
        .unwrap_or(false);
    Some((pid, fresh))
}

/// Whether a request's `Host` header is one this daemon answers.
///
/// The listener is loopback-only, so loopback names (any port) are accepted for
/// every route; a missing header is too, for raw local clients. The public
/// host a reverse proxy forwards is accepted for `/login` only, which is all
/// the documented proxy exposes. Any other host is refused, which also keeps a
/// DNS-rebinding page from reaching the API through a browser.
pub fn host_allowed(
    host: Option<&str>,
    path: &str,
    public_host: Option<&str>,
    vault_host: Option<&str>,
) -> bool {
    let Some(host) = host else { return true };
    let host = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':')
            .map_or(host.as_str(), |(name, _)| name)
    };
    if matches!(name, "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    if path == "/login" && public_host == Some(name) {
        return true;
    }
    is_vault_path(path) && vault_host == Some(name)
}

/// Whether a path belongs to the vault UI/API (cookie- or bearer-gated, and
/// the only paths the vault host answers).
fn is_vault_path(path: &str) -> bool {
    path == "/vault" || path.starts_with("/vault/") || path.starts_with("/v1/vault")
}

/// Whether a vault route may be answered only to a live UI session, never to
/// the bearer token: deciding a secret request. The bearer token sits in
/// `server.json`, readable by every process of this user — every agent this
/// daemon serves included — so an agent that could answer its own request
/// would reduce the human's approval to a formality. Approving or denying is
/// for the person in front of the vault UI.
fn is_vault_ui_only(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path.ends_with("/approve")
        || path.ends_with("/deny")
        // The one-time-code hand-off: looking up, answering or viewing the
        // setup QR of a request is the person's side, exactly like deciding
        // a secret request. An agent holds the bearer token; it must not
        // answer its own hand-off.
        || matches!(
            path,
            "/v1/vault/otp/answer" | "/v1/vault/otp/lookup" | "/v1/vault/otp/qr"
        )
}

/// Normalise a public base URL (`https://login.example.com[:port][/]`) and
/// return it without the trailing slash, plus its lower-case host.
pub fn parse_public_url(url: &str) -> Result<(String, String), String> {
    let url = url.trim().trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("public URL must start with https:// or http://")?;
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@', '"', '\\'])
        || rest.contains(char::is_whitespace)
    {
        return Err("public URL must be a bare origin like https://login.example.com".into());
    }
    let host = rest
        .rsplit_once(':')
        .map_or(rest, |(name, _)| name)
        .to_ascii_lowercase();
    Ok((url.to_string(), host))
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
    let mut tab_used = None;
    route_with(method, full_path, body, state, None, None, &mut tab_used)
}

/// `route` plus the OTP collect token header, threaded to the vault API.
fn route_with(
    method: &str,
    full_path: &str,
    body: &str,
    state: &AppState,
    collect_token: Option<&str>,
    session: Option<(u32, bool)>,
    tab_used: &mut Option<(String, bool)>,
) -> (&'static str, &'static str, String) {
    // A query string selects options (`?format=png`), never a different
    // resource, so the match below is on the path alone.
    let path = full_path.split('?').next().unwrap_or("");
    // A CLI-created tab stays owned as long as its agent keeps using it.
    let wanted_tab = form_value(query(full_path), "tab");
    if !wanted_tab.is_empty() {
        touch_tab_lease(&wanted_tab);
    }
    // Only the routes that drive the page wait for the browser: `status`, the
    // login form and the 401/404 paths answer instantly whatever Chromium is
    // doing, which is what makes `cu start` return at once.
    // Session save/load is a profile copy, not a page action: it must work
    // while the browser is down, so it is deliberately not gated.
    let needs_browser = matches!(
        (method, path),
        ("POST", "/v1/tabs")
            | ("POST", "/v1/navigate")
            | ("GET", "/v1/screenshot")
            | ("GET", "/v1/snapshot")
            | ("POST", "/v1/navigate-and-snapshot")
            | ("POST", "/v1/act")
            | ("POST", "/v1/click")
            | ("POST", "/v1/type")
            | ("GET", "/v1/contexts")
            | ("GET", "/v1/tabs")
            | ("GET", "/v1/text")
            | ("GET", "/v1/challenge")
            | ("POST", "/v1/challenge/release")
            | ("GET", "/v1/handoff")
            | ("GET", "/v1/inspect")
            | ("GET", "/v1/marks")
            | ("GET", "/v1/console")
            | ("POST", "/v1/marks")
            | ("DELETE", "/v1/marks")
            | ("POST", "/v1/vault/sessions/save")
    ) || (method == "DELETE"
        && (path.starts_with("/v1/contexts/") || path.starts_with("/v1/tabs/")));
    if needs_browser && let Err(error) = wait_until_ready(state) {
        return (
            "503 Service Unavailable",
            "application/json",
            format!("{{\"error\":\"{}\"}}", json_escape(&error)),
        );
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
        } else if let Some((pid, fresh)) = session {
            // The tab of the session that sent the request: the one it had,
            // or a new one. The CLI names its session in a header, so the page
            // it works in is resolved here, in the same request -- a round
            // trip of its own cost about as much as the action.
            match session_tab(state, Some(pid), fresh, "about:blank") {
                Ok((id, reused)) => {
                    *tab_used = Some((id.clone(), !reused));
                    Tab {
                        cdp_port: state.cdp_port,
                        target: Some(id),
                    }
                }
                Err(e) => {
                    return (
                        "502 Bad Gateway",
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
    // A person holds this tab in the live viewer: no automatic interaction
    // until they give it back (reads stay allowed).
    if matches!(
        (method, path),
        ("POST", "/v1/navigate")
            | ("POST", "/v1/act")
            | ("POST", "/v1/click")
            | ("POST", "/v1/type")
            | ("POST", "/v1/upload")
    ) && crate::live_stream::taken(&tab)
    {
        return (
            "409 Conflict",
            "application/json",
            "{\"error\":\"a person has taken control of this tab in the live viewer; automatic actions are paused until they hand it back\"}".into(),
        );
    }
    // A person is completing a challenge in this tab: no automatic
    // interaction until a re-check shows it cleared (reads stay allowed, so
    // the agent can still look).
    if matches!(
        (method, path),
        ("POST", "/v1/navigate")
            | ("POST", "/v1/act")
            | ("POST", "/v1/click")
            | ("POST", "/v1/type")
    ) && let Some(blocking) = crate::challenge::gate(&tab)
    {
        return (
            "409 Conflict",
            "application/json",
            format!(
                "{{\"error\":\"human verification in progress on this tab; automatic actions are paused\",\"challenge\":{}}}",
                blocking.json()
            ),
        );
    }
    match (method, path) {
        ("GET", "/v1/challenge") => {
            // `?wait=S` parks here instead of answering at once: this is
            // the call an agent makes instead of retrying its navigation in
            // a loop while a person is away. The budget is clamped inside
            // the wait, so a large `S` cannot hold the daemon open.
            let wait_for = seconds_param(query(full_path), "wait", 0.0);
            if wait_for > 0.0 {
                let timeout = Duration::from_secs_f64(wait_for.min(wait_ceiling_s()));
                let outcome = crate::challenge::handoff::wait(&tab, timeout);
                return (
                    "200 OK",
                    "application/json",
                    crate::challenge::handoff::outcome_json(&outcome),
                );
            }
            // A paused tab is re-checked the way the gate does it, so an
            // interstitial read without a wait cannot quietly end the
            // hand-off; any other tab gets a fresh, counted check.
            let assessment = match crate::challenge::gate(&tab) {
                Some(held) => held,
                None => {
                    let fresh =
                        crate::challenge::assess(&tab, |m, p| tab.command(m, p), Duration::ZERO);
                    crate::challenge::record(&tab, &fresh);
                    fresh
                }
            };
            ("200 OK", "application/json", assessment.json())
        }
        ("POST", "/v1/challenge/release") => (
            "200 OK",
            "application/json",
            format!("{{\"released\":{}}}", crate::challenge::release(&tab)),
        ),
        // The hand-off state itself: is a person needed on this tab, for how
        // long, and what the last passes looked like. A single `?tab=`-
        // scoped object when the tab is held, plus the history, so an agent
        // can tell "still waiting" from "gave up" without a probe.
        ("GET", "/v1/handoff") => {
            let held = crate::challenge::handoff::open_for(&tab);
            let assessment = crate::challenge::last(&tab);
            let mut body = format!(
                "{{\"tab\":{}",
                json_string(tab.target.as_deref().unwrap_or("default"))
            );
            body.push_str(&format!(
                ",\"waiting\":{},\"open\":{}",
                held.is_some(),
                held.unwrap_or_else(|| "null".into())
            ));
            if let Some(a) = assessment {
                body.push_str(&format!(",\"assessment\":{}", a.json()));
            }
            body.push_str(&format!(
                ",\"budget\":{},\"history\":{}}}",
                json_string(&crate::challenge::handoff::outcome_hint()),
                crate::challenge::handoff::json()
            ));
            ("200 OK", "application/json", body)
        }
        ("GET", "/v1/tabs") => match tabs_json(state.cdp_port) {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => (
                "502 Bad Gateway",
                "application/json",
                format!("{{\"error\":{}}}", json_string(&e)),
            ),
        },
        // The page this session works in: the tab it already holds when there
        // is one (so `cu navigate` and the `cu snapshot` after it read the same
        // page), and a fresh tab otherwise -- or always, with `"fresh":true`
        // (`--new-tab`), which is what a test that wants a page of its own
        // asks for. A different session is a different owner and never lands
        // in another agent's tab.
        ("POST", "/v1/tabs") => {
            let url = {
                let asked = json_value(body, "url").unwrap_or_default();
                if asked.is_empty() {
                    "about:blank".to_string()
                } else {
                    asked
                }
            };
            let owner = json_value(body, "owner_pid").and_then(|p| p.parse::<u32>().ok());
            // The string-only JSON reader cannot see a bare boolean, which is
            // what `--new-tab` sends (`"fresh":true`).
            let fresh = body.contains("\"fresh\":true") || body.contains("\"fresh\": true");
            match session_tab(state, owner, fresh, &url) {
                Ok((id, reused)) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"id\":{},\"tab\":{},\"reused\":{reused}}}",
                        json_string(&id),
                        json_string(&id)
                    ),
                ),
                Err(e) => (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                ),
            }
        }
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
        // Which browser really runs, in which mode, and how the protocol is
        // used. Answers before the browser is up too (`browser` is then the
        // launch decision without a version yet).
        ("GET", "/v1/diagnostics") => ("200 OK", "application/json", crate::diagnostics::json()),
        // Popups, iframe targets and workers the browser is running, as
        // target discovery reported them.
        ("GET", "/v1/targets") => ("200 OK", "application/json", crate::targets::json()),
        ("GET", "/v1/status") => (
            "200 OK",
            "application/json",
            "{\"running\":true,\"browser\":\"chromium\"}".into(),
        ),
        // Visible text of the page, for the reading a snapshot deliberately
        // does not do: prose, API responses, error messages. Every frame is
        // read, each one named, capped in the page so a huge document cannot
        // flood the socket.
        ("GET", "/v1/text") => {
            let (status, kind, body) =
                text_response(&tab, |method, params| tab.command(method, params));
            (status, kind, with_oopif_text(&tab, body))
        }
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
            // Vault injection is on unless the caller asked for a clean page
            // (`--inject off`: the body carries `"inject":false`, which the
            // string-only JSON reader cannot see).
            let inject = !body.contains("\"inject\":false")
                && !body.contains("\"inject\": \"off\"")
                && !body.contains("\"inject\":\"off\"");
            // The basic-auth responder is on like the injector, and off with
            // `"auth":false` for a caller that wants the raw challenge.
            let auth_enabled = !body.contains("\"auth\":false")
                && !body.contains("\"auth\": \"off\"")
                && !body.contains("\"auth\":\"off\"");
            navigate_with_vault(state, &tab, &url, inject, auth_enabled)
        }
        // Everything the page wrote to its console, by level (see
        // `crate::console`): `?level=warn+`, `grep=`, `since=`, `limit=`,
        // `clear=1`, `format=text`.
        ("GET", "/v1/console") => {
            let q = query(full_path);
            let levels = match crate::console::expand_levels(&form_value(q, "level")) {
                Ok(l) => l,
                Err(e) => {
                    return (
                        "400 Bad Request",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                    );
                }
            };
            let limit = form_value(q, "limit").parse::<usize>().unwrap_or(200);
            let since = form_value(q, "since").parse::<u64>().unwrap_or(0);
            crate::console::read(
                &tab,
                &levels,
                &form_value(q, "grep"),
                since,
                limit,
                form_value(q, "clear") == "1",
                form_value(q, "format") == "text",
            )
        }
        ("GET", "/v1/screenshot") => screenshot(&tab, query(full_path)),
        // Visual QA references (see `crate::visual`): what an element is,
        // how to find it again and which code renders it; and QA marks
        // pinned to elements, as JSON or a Markdown report.
        ("GET", "/v1/inspect") => {
            let q = query(full_path);
            let (reference, selector) = (form_value(q, "ref"), form_value(q, "selector"));
            if reference.is_empty() == selector.is_empty() {
                return (
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"give ref or selector, one of them\"}".into(),
                );
            }
            let found = crate::visual::inspect(
                &tab,
                Some(reference.as_str()).filter(|r| !r.is_empty()),
                Some(selector.as_str()).filter(|s| !s.is_empty()),
            );
            match found {
                Ok(info) => ("200 OK", "application/json", info),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                ),
            }
        }
        ("POST", "/v1/marks") => {
            let field = |k: &str| json_string_value(body, k).filter(|v| !v.is_empty());
            let (reference, selector) = (field("ref"), field("selector"));
            if reference.is_some() == selector.is_some() {
                return (
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"give ref or selector, one of them\"}".into(),
                );
            }
            match crate::visual::add_mark(
                &tab,
                reference.as_deref(),
                selector.as_deref(),
                &field("note").unwrap_or_default(),
                field("severity").as_deref(),
            ) {
                Ok(mark) => ("200 OK", "application/json", mark),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                ),
            }
        }
        ("GET", "/v1/marks") => {
            let marks = crate::visual::marks_of(&tab);
            if form_value(query(full_path), "format") == "md" {
                (
                    "200 OK",
                    "text/markdown; charset=utf-8",
                    crate::visual::marks_markdown(&marks),
                )
            } else {
                (
                    "200 OK",
                    "application/json",
                    crate::visual::marks_json(&marks),
                )
            }
        }
        ("DELETE", "/v1/marks") => (
            "200 OK",
            "application/json",
            format!("{{\"cleared\":{}}}", crate::visual::clear_marks(&tab)),
        ),
        // Act on snapshot refs. `/v1/act` takes a batch; `/v1/click` and
        // `/v1/type` are the same thing for a single action.
        ("POST", "/v1/act") => act(&tab, crate::actions::parse_batch(body)),
        ("POST", "/v1/click") => act(&tab, single("click", body)),
        ("POST", "/v1/type") => act(&tab, single("type", body)),
        ("POST", "/v1/upload") => act(&tab, single("upload", body)),
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
            let sessions = state.data_dir.join("sessions");
            let context = form_value(query(full_path), "context");
            if !context.is_empty() {
                return context_session(state, &sessions, name, &context, load);
            }
            let profile = state.data_dir.join("profiles/default");
            // The browser owns the live profile and rewrites it constantly, so
            // step away from the page first and let the copy settle.
            let browser_up = state.browser.is_ready();
            let mut live_cookies = None;
            if !load && browser_up {
                let _ = cdp_command(state.cdp_port, "Page.navigate", "{\"url\":\"about:blank\"}");
                live_cookies = cdp_browser_command(state.cdp_port, "Storage.getCookies", "{}")
                    .ok()
                    .and_then(|reply| json_array(&reply, "cookies"));
            }
            // Encrypted at rest (see `crate::session`); a plain directory
            // saved before encryption still loads.
            let outcome = if load {
                crate::session::load(&profile, &sessions, name)
            } else {
                crate::session::save(&profile, &sessions, name, live_cookies.as_deref())
            };
            // A load under a running browser: its cookie jar is live, so the
            // saved cookies go straight into it (otherwise at the next launch).
            if load && browser_up && outcome.is_ok() {
                restore_cookies(state.cdp_port, &profile);
            }
            let storage = match outcome {
                Ok(storage) => storage,
                Err(e) => {
                    return (
                        "400 Bad Request",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                    );
                }
            };
            // A note for the agent, without any secret: which session is live.
            let _ = fs::write(
                sessions.join(format!("{name}.current")),
                if load { "loaded\n" } else { "saved\n" },
            );
            let verb = if load { "loaded" } else { "saved" };
            (
                "200 OK",
                "application/json",
                format!("{{\"{verb}\":true,\"storage\":\"{storage}\"}}"),
            )
        }
        // Bearer-authorized vault access (the local CLI, an agent). The
        // full path: the vault API reads its own query (`?tab=`, `?query=`).
        (_, p) if p.starts_with("/v1/vault") => {
            vault_api(method, full_path, body, state, collect_token)
        }
        _ => (
            "404 Not Found",
            "application/json",
            "{\"error\":\"not found\"}".into(),
        ),
    }
}

/// `POST /v1/session/NAME[/load]?context=CTX`: save the cookie jar of the
/// named context `CTX` as session `NAME`, encrypted, or load such a session
/// into `CTX` (created if it does not exist yet), live.
fn context_session(
    state: &AppState,
    sessions: &Path,
    name: &str,
    context: &str,
    load: bool,
) -> (&'static str, &'static str, String) {
    let error = |status, e: &str| {
        (
            status,
            "application/json",
            format!("{{\"error\":{}}}", json_string(e)),
        )
    };
    if !valid_name(context) {
        return error("400 Bad Request", "invalid context name");
    }
    if let Err(e) = wait_until_ready(state) {
        return error("503 Service Unavailable", &e);
    }
    let browser_context = |cdp_port: u16| {
        contexts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(cdp_port, context.to_string()))
            .map(|c| c.browser_context.clone())
    };
    if load {
        let cookies = match crate::session::load_context(sessions, name) {
            Ok(c) => c,
            Err(e) => return error("400 Bad Request", &e),
        };
        if let Err(e) = context_tab(state.cdp_port, context, &state.data_dir) {
            return error("502 Bad Gateway", &e);
        }
        let Some(id) = browser_context(state.cdp_port) else {
            return error("502 Bad Gateway", "the context went away");
        };
        let set = cdp_browser_command(
            state.cdp_port,
            "Storage.setCookies",
            &format!(
                "{{\"cookies\":{cookies},\"browserContextId\":{}}}",
                json_string(&id)
            ),
        )
        .and_then(|reply| match reply.contains("\"error\":{") {
            true => Err(json_value(&reply, "message").unwrap_or(reply)),
            false => Ok(()),
        });
        return match set {
            Ok(()) => (
                "200 OK",
                "application/json",
                format!(
                    "{{\"loaded\":true,\"storage\":\"encrypted\",\"context\":{},\"cookies\":{}}}",
                    json_string(context),
                    json_objects(&cookies).len()
                ),
            ),
            Err(e) => error(
                "502 Bad Gateway",
                &format!("could not set the cookies: {e}"),
            ),
        };
    }
    let Some(id) = browser_context(state.cdp_port) else {
        return error(
            "400 Bad Request",
            &format!("no context named {context} is open in this daemon"),
        );
    };
    let cookies = cdp_browser_command(
        state.cdp_port,
        "Storage.getCookies",
        &format!("{{\"browserContextId\":{}}}", json_string(&id)),
    )
    .ok()
    .and_then(|reply| json_array(&reply, "cookies"));
    let Some(cookies) = cookies else {
        return error("502 Bad Gateway", "could not read the context's cookies");
    };
    match crate::session::save_context(sessions, name, &cookies) {
        Ok(count) => (
            "200 OK",
            "application/json",
            format!(
                "{{\"saved\":true,\"storage\":\"encrypted\",\"context\":{},\"cookies\":{count}}}",
                json_string(context)
            ),
        ),
        Err(e) => error("400 Bad Request", &e),
    }
}

/// `POST /v1/navigate`: wait for the origin's budget, navigate, settle,
/// assess, and retry a 429/503 or a transient network error within the
/// budget (see [`crate::scheduler`]). A challenge or a block is never
/// retried.
fn navigate_scheduled(
    tab: &Tab,
    url: &str,
    auth: Option<(String, String)>,
) -> (&'static str, &'static str, String) {
    let refused =
        |r: crate::scheduler::Refused| ("429 Too Many Requests", "application/json", r.json());
    let retries = crate::scheduler::budget().retries;
    let mut attempt = 0;
    let span = crate::server::trace_start();
    loop {
        let permit = match crate::scheduler::acquire(url) {
            Ok(permit) => permit,
            Err(r) => return refused(r),
        };
        let queued = permit.as_ref().map(|p| p.waited).unwrap_or_default();
        let (result, document, auth_handled) = match navigate_observed(tab, url, auth.clone()) {
            Ok(pair) => pair,
            Err(error) => {
                return (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(&error)),
                );
            }
        };
        // A navigation the browser could not perform answers with
        // `errorText` (`net::ERR_NAME_NOT_RESOLVED` and friends); passing
        // that on as a 200 hid a failed navigation from the agent.
        if let Some(error) = json_string_value(&result, "errorText").filter(|e| !e.is_empty()) {
            if attempt < retries && crate::scheduler::transient(&error) {
                attempt += 1;
                drop(permit);
                thread::sleep(crate::scheduler::retry_pause(attempt));
                continue;
            }
            // An HTTP-level refusal is not a transport failure: the origin
            // answered (403, 4xx), Chromium refused to render it and left its
            // own error page on the tab. `ERR_HTTP_RESPONSE_CODE_FAILURE` and
            // friends are exactly what X/Twitter's edge produces today, and
            // reporting them as a bare `502 navigation failed` hid a block
            // the agent could have understood. Assess the page the browser is
            // on: if it is a refusal, say so with the state, the vendor, the
            // status the network saw and the signals, plus the browser error.
            let http_level = crate::scheduler::http_level_failure(&error);
            let assessment = (http_level && crate::challenge::enabled()).then(|| {
                let assessment = crate::challenge::assess_response(
                    tab,
                    |m, p| tab.command(m, p),
                    crate::challenge::wait_budget(),
                    document.as_ref(),
                );
                crate::challenge::record(tab, &assessment);
                assessment
            });
            let refusal = assessment
                .as_ref()
                .filter(|a| {
                    matches!(
                        a.state,
                        crate::challenge::ChallengeState::Blocked
                            | crate::challenge::ChallengeState::RateLimited
                    )
                })
                .cloned();
            if let Some(refusal) = refusal {
                crate::scheduler::report(&crate::scheduler::Outcome {
                    url,
                    status: document
                        .as_ref()
                        .map(|d| d.status)
                        .unwrap_or(refusal.status),
                    retry_after: document.as_ref().and_then(|d| d.retry_after),
                    state: refusal.state,
                });
                let status = document.as_ref().map(|d| d.status).unwrap_or(0);
                let mut body = format!(
                    "{{\"navigated\":false,\"error\":{},\"challenge\":{}",
                    json_string(&format!("navigation failed: {error}")),
                    refusal.json()
                );
                if status != 0 {
                    body.push_str(&format!(",\"status\":{status}"));
                }
                if attempt > 0 {
                    body.push_str(&format!(",\"retries\":{attempt}"));
                }
                if !queued.is_zero() {
                    body.push_str(&format!(",\"queued_ms\":{}", queued.as_millis()));
                }
                body.push('}');
                return ("200 OK", "application/json", body);
            }
            return (
                "502 Bad Gateway",
                "application/json",
                format!(
                    "{{\"error\":{}}}",
                    json_string(&format!("navigation failed: {error}"))
                ),
            );
        }
        // Do not hand back a half-loaded page, but never hang on a stream
        // that stays open for ever. Whether the wait ended on a ready page or
        // on the budget says `settled`.
        // The document behind the page is a new one: every world cu
        // remembered for this tab died with it, and the frame list is stale.
        pagecache::forget_tab(tab);
        pagecache::forget_frames(tab);
        crate::server::trace("navigate.before_settle", span);
        let (waited, settled) = wait_for_page_on(tab, SETTLE_MAX);
        crate::server::trace("navigate.after_settle", span);
        // A page that loaded is not necessarily the page asked for: say what
        // it is, and never call a defence page (or an undecided one) a plain
        // success.
        let assessment = crate::challenge::enabled().then(|| {
            let assessment = crate::challenge::assess_response(
                tab,
                |m, p| tab.command(m, p),
                crate::challenge::wait_budget(),
                document.as_ref(),
            );
            crate::challenge::record(tab, &assessment);
            assessment
        });
        crate::server::trace("navigate.after_assess", span);
        let status = document.as_ref().map(|d| d.status).unwrap_or(0);
        let state = assessment
            .as_ref()
            .map(|a| a.state)
            .unwrap_or(if matches!(status, 429) {
                crate::challenge::ChallengeState::RateLimited
            } else {
                crate::challenge::ChallengeState::Unknown
            });
        let closed = crate::scheduler::report(&crate::scheduler::Outcome {
            url,
            status,
            retry_after: document.as_ref().and_then(|d| d.retry_after),
            state,
        });
        drop(permit);
        // Only a plain "slow down" is retried, and only if the wait fits the
        // budget; `acquire` waits out the origin's closed window.
        let retryable = matches!(status, 429 | 503)
            && matches!(
                state,
                crate::challenge::ChallengeState::RateLimited
                    | crate::challenge::ChallengeState::Unknown
            );
        if retryable && attempt < retries && closed <= crate::scheduler::budget().max_wait {
            attempt += 1;
            continue;
        }
        let mut extra = String::new();
        if auth_handled {
            // A challenge was met with the vault's credentials; the status
            // above is the post-auth response, not the bare 401.
            extra.push_str(",\"basic_auth\":\"responded\"");
        }
        if let Some(a) = &assessment {
            extra.push_str(&format!(",\"challenge\":{}", a.json()));
        }
        if status != 0 {
            extra.push_str(&format!(",\"status\":{status}"));
        }
        if attempt > 0 {
            extra.push_str(&format!(",\"retries\":{attempt}"));
        }
        if !queued.is_zero() {
            extra.push_str(&format!(",\"queued_ms\":{}", queued.as_millis()));
        }
        if !closed.is_zero() {
            extra.push_str(&format!(",\"origin_closed_ms\":{}", closed.as_millis()));
        }
        return (
            "200 OK",
            "application/json",
            format!(
                "{{\"result\":{},\"settled_ms\":{},\"settled\":{}{extra}}}",
                result.trim(),
                waited.as_millis(),
                settled
            ),
        );
    }
}

/// `Page.navigate` on a connection that has `Network` on just for the
/// navigation, so the main document's status and `Retry-After` are seen.
/// Only those two values are kept from the response.
///
/// With `auth` set (the vault has a user/password pair for this host), the
/// connection also has `Fetch` interception on, scoped to the host: an
/// `authRequired` event — the 401 + `WWW-Authenticate` challenge a headless
/// browser would otherwise sit on with no dialog to answer — is answered
/// with `Fetch.continueWithAuth` and the vault's pair, and every paused
/// request is continued, all without ever stopping to read a command's
/// reply inline (a reply read for would swallow concurrent events and leave
/// a request paused for ever). The third element of the result says whether
/// a challenge was answered.
fn navigate_observed(
    tab: &Tab,
    url: &str,
    auth: Option<(String, String)>,
) -> Result<(String, Option<crate::scheduler::DocumentResponse>, bool), String> {
    let span = crate::server::trace_start();
    let mut connection = checkout(tab)?;
    crate::server::trace("navigate.after_checkout", span);
    // Small buffers: the bodies are never fetched, only headers are needed.
    let enabled = connection
        .call(
            "Network.enable",
            "{\"maxTotalBufferSize\":0,\"maxResourceBufferSize\":0}",
        )
        .or_else(|_| {
            connection = open_connection(tab)?;
            connection.call(
                "Network.enable",
                "{\"maxTotalBufferSize\":0,\"maxResourceBufferSize\":0}",
            )
        });
    let auth_on = auth.is_some() && enabled.is_ok();
    crate::server::trace("navigate.after_network_enable", span);
    if auth_on {
        // This navigation arms `Fetch` itself, so the tab's background
        // responder has to let go first: two sessions intercepting the same
        // host would both have to answer every request.
        stop_auth_keeper(tab);
    }
    if auth_on
        && connection
            .call("Fetch.enable", &fetch_enable_params(&url_host(url)))
            .is_err()
    {
        // The Fetch domain refused (an interception is somehow already armed
        // on this pooled connection): a fresh connection does not carry it.
        connection = open_connection(tab)?;
        let _ = connection.call(
            "Network.enable",
            "{\"maxTotalBufferSize\":0,\"maxResourceBufferSize\":0}",
        );
        let _ = connection.call("Fetch.enable", &fetch_enable_params(&url_host(url)));
    }
    // Every document response seen; the main document's is the one whose
    // request id is the navigation's loader id. Taking the first one was
    // wrong: a frame of the previous page still loading (DataDome's CAPTCHA
    // iframe answers 200) could arrive first and stand in for a 403.
    let documents = std::cell::RefCell::new(Vec::new());
    let mut see = |event: &str| {
        if let Some(d) = crate::scheduler::document_response(event) {
            documents.borrow_mut().push(d);
        }
    };
    // One interception event, answered as it is read: `send` never stops to
    // read a reply, so the stream keeps flowing and no event is lost.
    let answer = |text: &str, connection: &mut CdpConnection, auth_handled: &mut bool| {
        if let Some(rid) = auth_required_id(text) {
            let _unused = connection.send(
                "Fetch.continueWithAuth",
                &continue_with_auth_params(&rid, &auth),
            );
            *auth_handled = true;
        } else if let Some(rid) = paused_request_id(text) {
            let _unused = connection.send(
                "Fetch.continueRequest",
                &format!("{{\"requestId\":{}}}", json_string(&rid)),
            );
        }
    };
    let (reply, auth_handled) = if !auth_on {
        let r = connection.call_observed(
            "Page.navigate",
            &format!("{{\"url\":{}}}", json_string(url)),
            &mut see,
        );
        crate::server::trace("navigate.after_page_navigate", span);
        (r, false)
    } else {
        // With interception armed, the challenge must be answered while the
        // navigation is in flight: Chrome does not deliver the
        // `Page.navigate` reply while the document request sits paused, so
        // reading the reply first would starve it (five seconds, then
        // EAGAIN). The navigation is sent unread instead, and this loop
        // takes everything the stream offers — its reply as well as the
        // interception events, each answered as it is read.
        let _unused = connection.send(
            "Page.navigate",
            &format!("{{\"url\":{}}}", json_string(url)),
        );
        let deadline = std::time::Instant::now() + AUTH_WINDOW;
        let mut auth_handled = false;
        let mut nav_reply: Option<String> = None;
        let mut frame: Option<String> = None;
        loop {
            // Done: the reply is in and the document this navigation is for
            // has landed — without a challenge, any response for its frame;
            // with one, the response that followed the credentials (the
            // pre-auth 401 shares the request id, so the last one is the
            // real one).
            let settled = match (&nav_reply, frame.as_deref()) {
                (Some(_), Some(f)) => {
                    match documents.borrow().iter().rev().find(|d| d.frame_id == f) {
                        Some(d) => !auth_handled || d.status != 401,
                        None => false,
                    }
                }
                _ => false,
            };
            if settled {
                break;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            connection.set_read_timeout(left);
            let Ok(text) = connection.next_message(MAX_CDP_MESSAGE) else {
                break;
            };
            if message_id(&text).is_some() {
                // The navigation's reply (the only command outstanding).
                frame = json_string_value(&text, "frameId");
                nav_reply = Some(text);
                continue;
            }
            if let Some(d) = crate::scheduler::document_response(&text) {
                documents.borrow_mut().push(d);
            }
            answer(&text, &mut connection, &mut auth_handled);
        }
        // A short grace: requests paused when the loop ended get continued
        // before interception is torn down, so the settle wait that follows
        // sees an unintercepted browser.
        let grace = std::time::Instant::now() + Duration::from_millis(400);
        loop {
            let left = grace.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            connection.set_read_timeout(left);
            let Ok(text) = connection.next_message(MAX_CDP_MESSAGE) else {
                break;
            };
            if message_id(&text).is_some() {
                if nav_reply.is_none() {
                    nav_reply = Some(text);
                }
                continue;
            }
            answer(&text, &mut connection, &mut auth_handled);
        }
        // Interception is torn down once the document has settled: Chrome
        // caches the credential it was handed for the realm, and answers the
        // page's own `fetch()` calls with it from then on.
        //
        // Leaving it armed is what the first cut did, and it wedges pages: an
        // intercepted request stays paused until a CDP client reads the event
        // and answers it, and between commands nobody is reading the tab's
        // connection. A page whose first request comes after the navigation
        // (every SPA boot) then hangs until some later command happens to
        // drain the stream. Verified against the live daemon: with the
        // responder left armed, `fetch()` to a protected path never resolved
        // while the daemon sat idle, and the pending request stayed paused
        // across commands.
        //
        // A short late window still answers a challenge that lands just after
        // the document — a redirect chain finishing, or a page whose very
        // first request beats the credential cache — so the handshake that
        // populates that cache is not lost by a hair.
        let late = std::time::Instant::now() + AUTH_LATE_WINDOW;
        loop {
            let left = late.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            connection.set_read_timeout(left);
            let Ok(text) = connection.next_message(MAX_CDP_MESSAGE) else {
                break;
            };
            if message_id(&text).is_some() {
                continue;
            }
            answer(&text, &mut connection, &mut auth_handled);
        }
        let _ = connection.call("Fetch.disable", "{}");
        connection.set_read_timeout(CDP_HTTP_TIMEOUT);
        // Hand the host to a responder of its own: from here the page's
        // requests are answered even while no command is running (see
        // [`AuthKeeper`]).
        if let Some(pair) = auth.clone() {
            start_auth_keeper(tab, &url_host(url), pair);
        }
        (
            nav_reply.ok_or_else(|| "navigation did not answer within the auth window".to_string()),
            auth_handled,
        )
    };
    if reply.is_ok() && enabled.is_ok() {
        // The document's response can trail the navigation's reply by a
        // hair; one round trip drains what is already queued. The round trip
        // asks for the frame tree, not `Network.disable`: the browser answers
        // the disable only after the navigation has committed *and* the
        // network stack has gone quiet, which was most of a navigation's
        // time. The frame tree is the browser's own answer, so it comes back
        // as soon as the new document has committed -- and it is the frame
        // list cu remembers.
        let tree = connection.call_observed("Page.getFrameTree", "{}", &mut see);
        if let Ok(tree) = tree {
            let frames = flatten_frame_tree(&tree);
            if !frames.is_empty() {
                pagecache::remember_frames(tab, frames);
            }
        }
        crate::server::trace("navigate.after_drain", span);
        // Stopping the network events is fire-and-forget: the browser does
        // it when it is ready, and nothing waits for the answer.
        let _unused = connection.send("Network.disable", "{}");
        checkin(tab, connection);
    }
    let reply = reply?;
    let documents = documents.into_inner();
    let loader = json_string_value(&reply, "loaderId");
    let frame = json_string_value(&reply, "frameId");
    let mut document = documents
        .iter()
        .find(|d| loader.as_deref() == Some(d.request_id.as_str()))
        .or_else(|| {
            documents
                .iter()
                .find(|d| frame.as_deref() == Some(d.frame_id.as_str()))
        })
        .cloned();
    if auth_handled {
        // The challenge replays the request: the 401 and the response after
        // the credentials share a request id, and the report — like the done
        // condition above — must speak for the one that answered.
        let replayed = documents
            .iter()
            .rev()
            .find(|d| {
                loader.as_deref() == Some(d.request_id.as_str())
                    || frame.as_deref().is_some_and(|f| !f.is_empty())
                        && frame.as_deref() == Some(d.frame_id.as_str())
            })
            .cloned();
        if let Some(replayed) = replayed {
            document = Some(replayed);
        }
    }
    Ok((reply, document, auth_handled))
}

/// How long a navigation may keep answering auth challenges and continuing
/// intercepted requests before the page is handed back to the normal settle
/// wait (which then reports `settled:false` if it is still coming).
const AUTH_WINDOW: Duration = Duration::from_secs(20);

/// How long after the document settles a challenge may still arrive and be
/// answered before interception is torn down: a redirect chain finishing, or
/// a page whose first request beats the credential cache. Keeping
/// interception on past this point is what wedges an idle page (see the
/// comment in `navigate_observed`).
const AUTH_LATE_WINDOW: Duration = Duration::from_millis(250);

/// `Fetch.authRequired`: the request id of an HTTP auth challenge.
fn auth_required_id(event: &str) -> Option<String> {
    if !event.contains("\"Fetch.authRequired\"") {
        return None;
    }
    json_string_value(event, "requestId")
}

/// `Fetch.requestPaused`: the request id of an intercepted request.
fn paused_request_id(event: &str) -> Option<String> {
    if !event.contains("\"Fetch.requestPaused\"") {
        return None;
    }
    json_string_value(event, "requestId")
}

/// `Fetch.enable` scoped to one host: only its URLs (and subdomains') are
/// intercepted, so the rest of the page's origins load untouched. Auth
/// challenges are reported for those requests too.
fn fetch_enable_params(host: &str) -> String {
    let mut patterns = Vec::new();
    for scheme in ["http", "https"] {
        // A pattern without an explicit port only matches the scheme's
        // default port (URLPattern semantics — `http://host/*` never sees
        // `host:8893/`), so each host gets a bare and a `:*` pattern.
        for host_part in [host.to_string(), format!("*.{host}")] {
            for port in ["", ":*"] {
                patterns.push(format!(
                    "{{\"urlPattern\":{}}}",
                    json_string(&format!("{scheme}://{host_part}{port}/*"))
                ));
            }
        }
    }
    format!(
        "{{\"handleAuthRequests\":true,\"patterns\":[{}]}}",
        patterns.join(",")
    )
}

/// The `Fetch.continueWithAuth` answer to a challenge: the vault's pair when
/// there is one, a silent cancel otherwise (headless has no dialog to offer,
/// and `Default` would leave the page waiting for exactly that). The answer
/// rides in `authChallengeResponse` — the flat spelling this protocol once
/// used is refused by current Chrome with a deserialisation error.
fn continue_with_auth_params(request_id: &str, auth: &Option<(String, String)>) -> String {
    let (response, extra) = match auth {
        Some((user, password)) => (
            "ProvideCredentials",
            format!(
                ",\"username\":{},\"password\":{}",
                json_string(user),
                json_string(password)
            ),
        ),
        None => ("CancelAuth", String::new()),
    };
    format!(
        "{{\"requestId\":{},\"authChallengeResponse\":{{\"response\":\"{response}\"{extra}}}}}",
        json_string(request_id)
    )
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
                "{{\"id\":{},\"url\":{},\"title\":{},\"default\":{},\"opener\":{}}}",
                json_string(&id),
                json_string(&json_string_value(&o, "url").unwrap_or_default()),
                json_string(&json_string_value(&o, "title").unwrap_or_default()),
                default_id.as_deref() == Some(id.as_str()),
                // A popup names the tab that opened it (target discovery).
                crate::targets::opener_of(&id)
                    .map(|o| json_string(&o))
                    .unwrap_or_else(|| "null".into())
            ))
        })
        .collect();
    Ok(format!("{{\"tabs\":[{}]}}", listed.join(",")))
}

/// Close a tab by id. The last page is refused: Chromium exits with it.
pub fn close_tab(cdp_port: u16, id: &str) -> Result<(), String> {
    // The tab's background auth responder goes with it.
    stop_auth_keeper(&Tab {
        cdp_port,
        target: Some(id.to_string()),
    });
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
    // The lease goes with the tab: a session must not be handed a closed id.
    tab_leases()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
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
fn text_response<F>(tab: &Tab, mut cmd: F) -> (&'static str, &'static str, String)
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    // The main document first: it is the frame an action lands on, and a page
    // that fails here is a page that cannot be read at all. The frame list is
    // the one the last snapshot read, when there is one: the browser answers
    // the same question until the page navigates.
    let mut frames = match pagecache::page_frames(tab) {
        Some(frames) if !frames.is_empty() => frames,
        _ => {
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
            let frames = flatten_frame_tree(&tree);
            pagecache::remember_frames(tab, frames.clone());
            frames
        }
    };
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
        // Every frame is read in cu's isolated world -- the main document
        // too, unless `CU_HELPER_WORLD=main` -- so a cross-origin frame is
        // read without touching its scripts, the same move the snapshot makes.
        let world = if index == 0 {
            crate::execution::helper_world()
        } else {
            crate::execution::World::Isolated
        };
        let context = if frame.id.is_empty() {
            String::new()
        } else {
            match cached_context_fragment(tab, &frame.id, world, &mut cmd) {
                Ok(fragment) => fragment,
                // The frame went away between the tree walk and here.
                Err(_) => {
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
        // The world belongs to the document: when the document was replaced
        // the remembered one is gone, and the browser says so. The world is
        // then made again, and the text is read in the new document.
        let mut value = None;
        for attempt in 0..2 {
            value = match cmd("Runtime.evaluate", &params) {
                Ok(reply) => evaluated_string(&reply),
                Err(_) => None,
            };
            if value.is_some() {
                break;
            }
            if attempt == 0 && !frame.id.is_empty() {
                pagecache::forget(tab, &frame.id);
                let Ok(fresh) = cached_context_fragment(tab, &frame.id, world, &mut cmd) else {
                    break;
                };
                let params = format!(
                    "{{\"expression\":{},{}\"returnByValue\":true}}",
                    json_string(READ_TEXT_JS),
                    fresh
                );
                value = match cmd("Runtime.evaluate", &params) {
                    Ok(reply) => evaluated_string(&reply),
                    Err(_) => None,
                };
                break;
            }
        }
        let value = match value {
            Some(value) => value,
            None => {
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
            }
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
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
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
///
/// Pixels for people (slides, docs, social posts) need more control:
/// - `width`, `height`: the viewport in CSS pixels for this capture;
/// - `scale`: the device scale factor (`2` gives a retina-sharp image);
/// - `ref` (a snapshot ref) or `selector` (CSS): clip to that element, with
///   `padding` CSS pixels around it.
///
/// The viewport is overridden through `Emulation.setDeviceMetricsOverride`
/// for this one capture and cleared afterwards, so later actions see the page
/// as before.
/// The metrics a tab is being shown at right now, as far as CDP will say.
///
/// CDP has **no getter** for `Emulation.setDeviceMetricsOverride` -- asking a
/// browser what override is in force is not a command that exists -- so the
/// only way to put a tab back the way it was is to measure what it is being
/// *shown* at and re-apply that. Everything [`Viewport::read`] recovers comes
/// from one `Runtime.evaluate`: the CSS viewport the page is laid out at and
/// the device pixel ratio it is rendered at.
///
/// What is recovered is the size and the scale, which is what an override
/// that another tool set (a QA harness emulating a phone, a second agent's
/// `cu shot --width`) is for. What is not recovered is the `mobile` flag and
/// an explicit `screenWidth`/`screenHeight`: neither is observable from the
/// page, and guessing them would be worse than the browser's own values. For
/// a `mobile:false` override -- the ordinary one, and the only one `cu`
/// itself sets -- the restore is exact; for a mobile emulation it puts the
/// tab back at the right CSS size and scale rather than at the window's.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Viewport {
    width: u32,
    height: u32,
    scale: f64,
}

impl Viewport {
    /// The metrics this tab is currently shown at.
    fn read(tab: &Tab) -> Option<Self> {
        let reply = tab.command(
            "Runtime.evaluate",
            "{\"expression\":\"JSON.stringify({w:innerWidth,h:innerHeight,d:devicePixelRatio})\",\"returnByValue\":true}",
        ).ok()?;
        let value = evaluated_string(&reply)?;
        let number = |key: &str| json_number(&value, key);
        let (width, height) = (number("w")?, number("h")?);
        // A page mid-navigation can report 0x0; there is nothing to restore
        // from that, and re-applying it would pin the tab to nothing.
        if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
            return None;
        }
        let scale = number("d")
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(1.0);
        Some(Self {
            width: width.round() as u32,
            height: height.round() as u32,
            scale,
        })
    }
}

/// The viewport's size in CSS pixels, from the browser itself.
///
/// `Page.getLayoutMetrics` answers it without running a script in the page,
/// which is what a plain capture needs (the old way measured the page in
/// JavaScript, and the animation frames it waited for were most of the
/// capture's time).
fn viewport_size(tab: &Tab) -> Result<(f64, f64), String> {
    let reply = tab.command("Page.getLayoutMetrics", "{}")?;
    // `cssVisualViewport` is the part of the page the agent sees; the layout
    // viewport is the whole document, which can be wider than the window.
    let visual = json_object(&reply, "cssVisualViewport")
        .or_else(|| json_object(&reply, "cssLayoutViewport"))
        .ok_or("the browser did not report the viewport size")?;
    let width = json_number(visual, "clientWidth").filter(|w| *w > 0.0);
    let height = json_number(visual, "clientHeight").filter(|h| *h > 0.0);
    match (width, height) {
        (Some(width), Some(height)) => Ok((width, height)),
        _ => Err("the browser did not report the viewport size".into()),
    }
}

/// Put the tab back the way it was before this capture.
///
/// Handing the tab to the browser is right when there was nothing to keep --
/// but it is wrong when something else had emulated the tab, which is what
/// clearing unconditionally used to do. With no getter for the override, the
/// saved metrics are compared against what the browser falls back to: the same
/// answer means nothing was emulated and the clear was the whole job, and a
/// different answer means the saved metrics were an override the caller set,
/// so they go back on.
fn restore_viewport(was: Option<Viewport>, tab: &Tab) {
    let _ = tab.command("Emulation.clearDeviceMetricsOverride", "{}");
    let Some(was) = was else { return };
    if Viewport::read(tab) == Some(was) {
        return;
    }
    let _ = tab.command(
        "Emulation.setDeviceMetricsOverride",
        &format!(
            "{{\"width\":{},\"height\":{},\"deviceScaleFactor\":{},\"mobile\":false}}",
            was.width, was.height, was.scale
        ),
    );
}

fn screenshot(tab: &Tab, query: &str) -> (&'static str, &'static str, String) {
    let options = match ShotOptions::parse(query) {
        Ok(options) => options,
        Err(error) => {
            return (
                "400 Bad Request",
                "application/json",
                format!("{{\"error\":{}}}", json_string(&error)),
            );
        }
    };
    let png = options.png;
    let span = crate::server::trace_start();
    let format = if png { "png" } else { "jpeg" };
    // Read BEFORE the capture overrides anything: what the tab is shown at
    // now is the caller's, not the viewport this capture is about to set.
    // Only an emulated capture changes the viewport, so only it has to
    // remember the old one.
    let was = if options.emulates() {
        Viewport::read(tab)
    } else {
        None
    };
    crate::server::trace("shot.after_viewport_read", span);
    let result = capture_annotated(tab, &options);
    crate::server::trace("shot.after_capture", span);
    if options.emulates() {
        // Put the tab back the way this capture found it, even if it failed.
        restore_viewport(was, tab);
    }
    match result {
        Ok((result, size)) => {
            let field = if png { "png_base64" } else { "jpeg_base64" };
            (
                "200 OK",
                "application/json",
                format!(
                    "{{\"format\":\"{format}\",{size}\"{field}\":{}}}",
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

/// What `GET /v1/screenshot` was asked for.
#[derive(Debug, Default, PartialEq)]
pub struct ShotOptions {
    pub png: bool,
    pub quality: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub scale: Option<f64>,
    pub reference: Option<String>,
    pub selector: Option<String>,
    pub padding: f64,
    /// Snapshot refs to box and label on the image (`annotate=e1,e4`), or
    /// every ref of the last snapshot (`annotate=all`).
    pub annotate: Option<Vec<String>>,
    /// Box this tab's QA marks on the page, numbered like the report.
    pub marks: bool,
}

/// Largest viewport side accepted, in CSS pixels.
const MAX_VIEWPORT: u32 = 8192;
/// Largest device scale factor accepted.
const MAX_SCALE: f64 = 4.0;

/// How long the screenshot's measure step waits for two painted frames, in
/// milliseconds, before measuring the page anyway.
///
/// Two frames at 60 Hz are about 33 ms, so a page that is painting pays a
/// third of this and nothing; the budget only matters on a browser that has
/// stopped producing frames for the tab, where the wait ends by itself
/// instead of hanging the capture until the socket read times out.
const FRAME_WAIT_MS: u32 = 250;

impl ShotOptions {
    pub fn parse(query: &str) -> Result<Self, String> {
        let get = |key: &str| Some(form_value(query, key)).filter(|v| !v.is_empty());
        let side = |key: &str| -> Result<Option<u32>, String> {
            get(key)
                .map(|v| {
                    v.parse::<u32>()
                        .ok()
                        .filter(|n| (1..=MAX_VIEWPORT).contains(n))
                        .ok_or_else(|| {
                            format!("{key} must be 1..{MAX_VIEWPORT} CSS pixels, got {v}")
                        })
                })
                .transpose()
        };
        let scale = get("scale")
            .map(|v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|s| *s > 0.0 && *s <= MAX_SCALE)
                    .ok_or_else(|| {
                        format!("scale must be a number above 0 and up to {MAX_SCALE}, got {v}")
                    })
            })
            .transpose()?;
        let padding = get("padding")
            .map(|v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|p| *p >= 0.0 && *p <= MAX_VIEWPORT as f64)
                    .ok_or_else(|| format!("padding must be 0..{MAX_VIEWPORT} CSS pixels, got {v}"))
            })
            .transpose()?
            .unwrap_or(0.0);
        let options = Self {
            png: form_value(query, "format") == "png",
            quality: form_value(query, "quality")
                .parse::<u32>()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .unwrap_or(60),
            width: side("width")?,
            height: side("height")?,
            scale,
            reference: get("ref"),
            selector: get("selector"),
            padding,
            annotate: get("annotate")
                .map(|v| {
                    let refs: Vec<String> = v
                        .split(',')
                        .map(|r| r.trim().to_string())
                        .filter(|r| !r.is_empty())
                        .collect();
                    match refs
                        .iter()
                        .find(|r| *r != "all" && !crate::actions::valid_ref(r))
                    {
                        Some(bad) => Err(format!(
                            "annotate takes snapshot refs (e1,e4) or all, got {bad}"
                        )),
                        None => Ok(refs),
                    }
                })
                .transpose()?,
            marks: matches!(form_value(query, "marks").as_str(), "1" | "true"),
        };
        if options.reference.is_some() && options.selector.is_some() {
            return Err("give ref or selector, not both".into());
        }
        Ok(options)
    }

    /// Whether the capture changes the viewport (and must restore it).
    pub fn emulates(&self) -> bool {
        self.width.is_some() || self.height.is_some() || self.scale.is_some()
    }
}

/// [`capture`] with the overlays the options ask for drawn on the page for
/// this one image and removed right after it, whatever the capture did.
fn capture_annotated(tab: &Tab, options: &ShotOptions) -> Result<(String, String), String> {
    let mut items = Vec::new();
    if let Some(refs) = &options.annotate {
        let refs = if refs.iter().any(|r| r == "all") {
            snapshot_refs(tab)
        } else {
            refs.clone()
        };
        if refs.is_empty() {
            return Err("no refs to annotate; take a snapshot first".into());
        }
        items.extend(crate::visual::ref_items(&refs));
    }
    if options.marks {
        let url = crate::visual::current_url(tab);
        items.extend(crate::visual::mark_items(
            &crate::visual::marks_of(tab),
            &url,
        ));
    }
    if items.is_empty() {
        return capture(tab, options);
    }
    // The viewport changes before the boxes are measured, or they would sit
    // where the elements were at the old size.
    if options.emulates() {
        tab.command(
            "Emulation.setDeviceMetricsOverride",
            &format!(
                "{{\"width\":{},\"height\":{},\"deviceScaleFactor\":{},\"mobile\":false}}",
                options.width.unwrap_or(0),
                options.height.unwrap_or(0),
                options.scale.unwrap_or(0.0)
            ),
        )?;
    }
    let drawn = crate::visual::draw(tab, &items);
    let result = capture(tab, options);
    crate::visual::clear(&drawn);
    let (reply, size) = result?;
    let labels: Vec<String> = drawn.labels.iter().map(|l| json_string(l)).collect();
    Ok((
        reply,
        format!("{size}\"annotated\":[{}],", labels.join(",")),
    ))
}

/// The refs of `tab`'s last snapshot, in the order they were handed out.
pub fn snapshot_refs(tab: &Tab) -> Vec<String> {
    let mut refs: Vec<String> = ref_frames()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    refs.sort_by_key(|r| r[1..].parse::<u64>().unwrap_or(u64::MAX));
    refs
}

/// Run the capture: override the viewport, measure the clip, take the shot.
/// Returns the CDP reply and a `"width":..,"height":..,` fragment with the
/// size of the image in pixels.
fn capture(tab: &Tab, options: &ShotOptions) -> Result<(String, String), String> {
    let span = crate::server::trace_start();
    let scale = options.scale.unwrap_or(1.0);
    // A plain capture of the viewport needs nothing measured: there is no
    // clip to work out, and the page is laid out as it is. Measuring it
    // anyway cost two animation frames -- and the frame the measure waited
    // for was the frame the capture then had to wait *after* for, which is
    // most of a capture's time.
    if options.reference.is_none() && options.selector.is_none() && !options.emulates() {
        let params = if options.png {
            "{\"format\":\"png\"}".to_string()
        } else {
            format!(
                "{{\"format\":\"jpeg\",\"quality\":{},\"optimizeForSpeed\":true}}",
                options.quality
            )
        };
        let result = tab.command("Page.captureScreenshot", &params)?;
        if let Some(error) = json_object(&result, "error") {
            return Err(json_string_value(&error, "message")
                .or_else(|| {
                    json_string_value(&error, "code").map(|c| format!("capture failed ({c})"))
                })
                .unwrap_or_else(|| "the browser returned no image".into()));
        }
        // Without an override the real device scale factor is unknown here,
        // so the size is only reported when it is known.
        let size = if options.scale.is_some() {
            let (width, height) = viewport_size(tab)?;
            format!(
                "\"width\":{},\"height\":{},\"scale\":{scale},",
                (width * scale).round() as u64,
                (height * scale).round() as u64
            )
        } else {
            String::new()
        };
        return Ok((result, size));
    }
    if options.emulates() {
        // 0 keeps the browser's own value for a side that was not given.
        tab.command(
            "Emulation.setDeviceMetricsOverride",
            &format!(
                "{{\"width\":{},\"height\":{},\"deviceScaleFactor\":{},\"mobile\":false}}",
                options.width.unwrap_or(0),
                options.height.unwrap_or(0),
                options.scale.unwrap_or(0.0)
            ),
        )?;
    }
    let target = match (&options.reference, &options.selector) {
        (Some(reference), _) => Some(format!(
            "window.__cu&&window.__cu.element?window.__cu.element({}):undefined",
            json_string(reference)
        )),
        (None, Some(selector)) => {
            Some(format!("document.querySelector({})", json_string(selector)))
        }
        (None, None) => None,
    };
    // Measuring also waits two frames, so the new viewport has been laid out
    // and painted (responsive images, layout transitions) before the capture.
    //
    // The frame wait is bounded, and it must be: `requestAnimationFrame` only
    // fires while the browser is producing frames for the tab, and on a host
    // with no GPU a tab that is not visible (occluded, another window on top,
    // a window the window manager never mapped) produces none at all. An
    // unbounded wait there never resolves, `Runtime.evaluate` with
    // `awaitPromise` never answers, the read times out and the whole capture
    // dies with "Resource temporarily unavailable" -- a screenshot that fails
    // on *every* page of such a browser. Racing the frames against a timer
    // keeps the paint wait on a page that is painting and gives up on one
    // that is not: `Page.captureScreenshot` below reads the frame that is
    // there, which on a still frame is the page.
    let what = options
        .reference
        .clone()
        .or_else(|| options.selector.clone())
        .unwrap_or_default();
    let expression = format!(
        "(async()=>{{const err=(m)=>JSON.stringify({{error:m}});{}\
         await new Promise(r=>{{let done=false;const go=()=>{{if(!done){{done=true;r();}}}};\
         requestAnimationFrame(()=>requestAnimationFrame(go));setTimeout(go,{FRAME_WAIT_MS});}});\
         {}}})()",
        match &target {
            Some(target) => format!(
                "const el=({target});\
                 if(el===undefined)return err('this page has no refs yet; take a snapshot first');\
                 if(!el)return err({}+' is not on the page');\
                 el.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}});",
                json_string(&what)
            ),
            None => String::new(),
        },
        match &target {
            Some(_) => format!(
                "const r=el.getBoundingClientRect();\
                 if(r.width<1||r.height<1)return err({}+' is not visible');\
                 return JSON.stringify({{x:r.left+scrollX,y:r.top+scrollY,w:r.width,h:r.height,vw:innerWidth,vh:innerHeight}});",
                json_string(&what)
            ),
            None => "return JSON.stringify({vw:innerWidth,vh:innerHeight});".into(),
        }
    );
    // In cu's isolated world: that is where the ref registry lives, and the
    // page does not see the measure run.
    let reply = tab.command(
        "Runtime.evaluate",
        &crate::execution::evaluate_params(&expression, &helper_context(tab)?, true),
    )?;
    crate::server::trace("shot.after_measure", span);
    if reply.contains("\"exceptionDetails\"") {
        return Err(json_string_value(&reply, "description")
            .unwrap_or_else(|| "measuring the page failed".into()));
    }
    let measured = evaluated_string(&reply).unwrap_or_default();
    if let Some(error) = json_string_value(&measured, "error") {
        return Err(error);
    }
    let number = |key: &str| json_number(&measured, key);
    let mut params = if options.png {
        "{\"format\":\"png\"".to_string()
    } else {
        format!(
            "{{\"format\":\"jpeg\",\"quality\":{},\"optimizeForSpeed\":true",
            options.quality
        )
    };
    let (css_w, css_h) = if target.is_some() {
        let (x, y, w, h) = match (number("x"), number("y"), number("w"), number("h")) {
            (Some(x), Some(y), Some(w), Some(h)) => (x, y, w, h),
            _ => return Err(format!("could not measure {what}")),
        };
        let pad = options.padding;
        let (x, y) = ((x - pad).max(0.0), (y - pad).max(0.0));
        let (w, h) = (w + 2.0 * pad, h + 2.0 * pad);
        // `captureBeyondViewport` would relayout the whole page to its full
        // height and the clip measured here would land somewhere else; the
        // element was scrolled into view instead, and one taller or wider
        // than the viewport is captured from the full page only then.
        let fits = w <= number("vw").unwrap_or(0.0) && h <= number("vh").unwrap_or(0.0);
        params.push_str(&format!(
            ",\"clip\":{{\"x\":{x},\"y\":{y},\"width\":{w},\"height\":{h},\"scale\":1}},\"captureBeyondViewport\":{}",
            !fits
        ));
        (w, h)
    } else {
        (number("vw").unwrap_or(0.0), number("vh").unwrap_or(0.0))
    };
    params.push('}');
    let result = tab.command("Page.captureScreenshot", &params)?;
    crate::server::trace("shot.after_cdp_capture", span);
    // A capture the browser refused has no `data`, and reporting that as a
    // 200 with a null image is what made a broken capture look like a working
    // one: the agent got `jpeg_base64: null` instead of the reason.
    if let Some(error) = json_object(&result, "error") {
        return Err(json_string_value(&error, "message")
            .or_else(|| json_string_value(&error, "code").map(|c| format!("capture failed ({c})")))
            .unwrap_or_else(|| "the browser returned no image".into()));
    }
    // Without an override the real device scale factor is unknown here, so
    // the size is only reported when it is known.
    let size = if options.scale.is_some() {
        format!(
            "\"width\":{},\"height\":{},\"scale\":{scale},",
            (css_w * scale).round() as u64,
            (css_h * scale).round() as u64
        )
    } else {
        String::new()
    };
    Ok((result, size))
}

/// A number field of a flat JSON object, fractions and sign included
/// ([`json_value`] keeps only the leading digits).
pub fn json_number(input: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let start = input.find(&needle)? + needle.len();
    let rest = input[start..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The query string of a request path, without the `?`.
fn query(path: &str) -> &str {
    path.split_once('?').map(|(_, q)| q).unwrap_or("")
}

/// A numeric query parameter, or `fallback` when it is absent, empty or not
/// a number. A negative or absurd value is the caller's problem to clamp, not
/// a reason to answer `400`: these are durations, and the ceiling that
/// applies to them lives with the feature.
fn seconds_param(query: &str, key: &str, fallback: f64) -> f64 {
    form_value(query, key).parse().unwrap_or(fallback)
}

/// The longest a hand-off wait may be parked, whatever the request asks for.
fn wait_ceiling_s() -> f64 {
    crate::challenge::handoff::wait_ceiling().as_secs_f64()
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
pub fn json_object<'a>(input: &'a str, key: &str) -> Option<&'a str> {
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
    // what marks the frame that follows the main-document helper policy.
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

/// Out-of-process iframes of `tab`'s page, outermost first: with site
/// isolation (the compatibility profile) a cross-site iframe is rendered by
/// another process and is missing from the page's `Page.getFrameTree`; it is
/// a target of its own (`type: iframe`, `parentId` = the target that embeds
/// it) with its own DevTools endpoint. Each comes back as its root frame
/// (whose id is the target id, parented to the embedding frame) and a
/// [`Tab`] that drives that target.
pub fn oopif_targets(tab: &Tab) -> Vec<(Frame, Tab)> {
    let Some(page) = tab.target.clone().or_else(|| pinned_target(tab.cdp_port)) else {
        return Vec::new();
    };
    let Ok(discovery) = http_get(&format!("127.0.0.1:{}", tab.cdp_port), "/json") else {
        return Vec::new();
    };
    let iframes: Vec<(String, String, String)> = json_objects(&discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("iframe"))
        .filter_map(|o| {
            Some((
                json_string_value(&o, "id")?,
                json_string_value(&o, "parentId")?,
                json_string_value(&o, "url").unwrap_or_default(),
            ))
        })
        .collect();
    // Breadth-first from the page, so a nested one follows its parent.
    let mut owners = vec![page];
    let mut out = Vec::new();
    let mut i = 0;
    while i < owners.len() {
        for (id, parent, url) in &iframes {
            if *parent == owners[i] && !owners.contains(id) {
                owners.push(id.clone());
                out.push((
                    Frame {
                        id: id.clone(),
                        parent: Some(parent.clone()),
                        url: url.clone(),
                    },
                    Tab {
                        cdp_port: tab.cdp_port,
                        target: Some(id.clone()),
                    },
                ));
            }
        }
        i += 1;
    }
    out
}

/// Which out-of-process iframe target each frame of each tab's last
/// snapshot lives in (frames in the page's own process are absent).
fn frame_targets() -> &'static Mutex<HashMap<Tab, HashMap<String, Tab>>> {
    static FRAME_TARGETS: OnceLock<Mutex<HashMap<Tab, HashMap<String, Tab>>>> = OnceLock::new();
    FRAME_TARGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The out-of-process iframe target that renders `frame` of `tab`, if the
/// last snapshot found it in one.
pub fn target_for_frame(tab: &Tab, frame: &str) -> Option<Tab> {
    frame_targets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .and_then(|m| m.get(frame))
        .cloned()
}

/// Every frame the last snapshot of `tab` found in out-of-process iframes,
/// with its parent, for actions that need the whole frame chain.
pub fn oopif_frames(tab: &Tab) -> Vec<Frame> {
    oopif_frame_list()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
        .unwrap_or_default()
}

fn oopif_frame_list() -> &'static Mutex<HashMap<Tab, Vec<Frame>>> {
    static LIST: OnceLock<Mutex<HashMap<Tab, Vec<Frame>>>> = OnceLock::new();
    LIST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Walk every out-of-process iframe of `tab` in its own isolated world, the
/// way in-process frames are walked, and record where each frame lives.
/// A frame that cannot be read is named, not fatal.
pub fn walk_oopifs(tab: &Tab, text: &mut String, pairs: &mut Vec<(String, String)>) {
    let mut owners: HashMap<String, Tab> = HashMap::new();
    let mut frames_seen: Vec<Frame> = Vec::new();
    for (root, target) in oopif_targets(tab) {
        let tree = target.command("Page.getFrameTree", "{}");
        let mut frames = tree.map(|t| flatten_frame_tree(&t)).unwrap_or_default();
        if frames.is_empty() {
            text.push_str(&format!(
                "- frame: {} (unreadable right now; take a new snapshot)\n",
                root.url
            ));
            continue;
        }
        // The root of an OOPIF's own tree has no parent there; it is the
        // child of the frame that embeds it.
        frames[0].parent = root.parent.clone();
        for frame in &frames {
            owners.insert(frame.id.clone(), target.clone());
        }
        frames_seen.extend(frames.iter().cloned());
        match walk_frames_nested(tab, frames, |m, p| target.command(m, p), true) {
            Ok((t, p)) => {
                text.push_str(&t);
                pairs.extend(p);
            }
            Err(_) => text.push_str(&format!(
                "- frame: {} (unreadable right now; take a new snapshot)\n",
                root.url
            )),
        }
    }
    frame_targets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), owners);
    oopif_frame_list()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), frames_seen);
}

/// Add the text of `tab`'s out-of-process iframes to a `/v1/text` body,
/// each under its `- frame:` line, within the same limit.
fn with_oopif_text(tab: &Tab, body: String) -> String {
    let Some(mut text) = json_string_value(&body, "text") else {
        return body;
    };
    let mut truncated = body.contains("\"truncated\":true");
    for (root, target) in oopif_targets(tab) {
        let (_, _, part) = text_response(&target, |m, p| target.command(m, p));
        match json_string_value(&part, "text") {
            Some(t) => {
                truncated |= part.contains("\"truncated\":true");
                text.push_str(&format!("\n- frame: {}", root.url));
                text.push_str(&t);
            }
            None => {
                text.push('\n');
                text.push_str(&unreadable_frame(&root.url));
            }
        }
    }
    let text: String = text.chars().take(TEXT_LIMIT).collect();
    format!(
        "{{\"text\":{},\"truncated\":{}}}",
        json_string(&text),
        truncated
    )
}

/// Build a `GET /v1/snapshot` response body.
///
/// The walk runs in the page: a few hundred bytes instead of a DOM dump the
/// daemon then has to shrink.
pub fn snapshot_pages(state: &AppState) -> Result<String, String> {
    snapshot_tab(&Tab::default_for(state.cdp_port))
}

fn snapshot_tab(tab: &Tab) -> Result<String, String> {
    let (mut text, mut pairs) = walk_snapshot(tab, |commands| call_batch(tab, commands))?;
    walk_oopifs(tab, &mut text, &mut pairs);
    note_ref_frames(tab, &pairs);
    Ok(format!("{{\"snapshot\":{}}}", json_string(&text)))
}

/// Walk every frame of the page, fetching the frame tree first.
pub fn walk_snapshot<F>(tab: &Tab, cmd: F) -> Result<(String, Vec<(String, String)>), String>
where
    F: FnMut(&[(&str, &str)]) -> Vec<Result<String, String>>,
{
    let mut cmd = cmd;
    // The fast path: the main frame's world is known from the last snapshot,
    // so the frame tree and the walk go out together and cost one round
    // trip instead of two.
    if let Some(main) = pagecache::main_frame(tab)
        && let Some(state) = pagecache::get(tab, &main)
    {
        let expression = if state.installed {
            snapshot::call_script().to_string()
        } else {
            snapshot::install_script(snapshot::base_for(tab.cdp_port, &main))
        };
        let params = format!(
            "{{\"expression\":{},\"contextId\":{},\"returnByValue\":true}}",
            json_string(&expression),
            state.context
        );
        let replies = cmd(&[
            ("Page.getFrameTree", "{}"),
            ("Runtime.evaluate", params.as_str()),
        ]);
        if let [Ok(tree), Ok(reply)] = replies.as_slice()
            && let Some(value) = evaluated_string(reply)
        {
            let frames = flatten_frame_tree(tree);
            // The frame list is the browser's answer to "what is this page
            // made of"; the actions that follow need the same answer.
            pagecache::remember_frames(tab, frames.clone());
            if !state.installed {
                pagecache::mark_installed(tab, &main);
            }
            snapshot::saw_refs_up_to(&value);
            let snap = parse_snapshot(&value)?;
            let mut text = snap.render();
            let mut pairs: Vec<(String, String)> = snap
                .nodes
                .iter()
                .map(|node| (node.ref_id.clone(), main.clone()))
                .collect();
            // The frames below the main document are walked on their own.
            let others: Vec<Frame> = frames.iter().filter(|f| f.id != main).cloned().collect();
            if !others.is_empty() {
                // `nested`: these are frames below the main document, so each
                // one is a section of its own.
                let (more, more_pairs) = walk_frames_nested(
                    tab,
                    others,
                    |method, params| one(&mut cmd, method, params),
                    true,
                )?;
                text.push_str(&more);
                pairs.extend(more_pairs);
            }
            return Ok((text, pairs));
        }
    }
    // The slow path: the frame tree first, then every frame in it.
    let replies = cmd(&[("Page.getFrameTree", "{}")]);
    let tree = match replies.into_iter().next() {
        Some(Ok(tree)) => tree,
        Some(Err(error)) => return Err(error),
        None => return Err("the browser did not answer with a frame tree".into()),
    };
    let frames = flatten_frame_tree(&tree);
    pagecache::remember_frames(tab, frames.clone());
    walk_frames(tab, frames, |method, params| one(&mut cmd, method, params))
}

/// One command, through a batch-capable runner.
fn one<F>(cmd: &mut F, method: &str, params: &str) -> Result<String, String>
where
    F: FnMut(&[(&str, &str)]) -> Vec<Result<String, String>>,
{
    cmd(&[(method, params)])
        .into_iter()
        .next()
        .unwrap_or_else(|| Err("the browser did not answer".into()))
}

/// Walk every frame of the page and merge the result.
///
/// Returns the snapshot text and the (ref, frameId) of every element. Every
/// frame, the main document included, is walked in cu's isolated world (see
/// [`crate::execution`]), so the page's scripts never see the walk or the
/// registry it installs.
/// Isolated worlds persist per frame (created with the fixed name `cu`), so
/// the registry the walk installs is the one the later action finds.
pub fn walk_frames<F>(
    tab: &Tab,
    frames: Vec<Frame>,
    cmd: F,
) -> Result<(String, Vec<(String, String)>), String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    walk_frames_nested(tab, frames, cmd, false)
}

/// [`walk_frames`]; `nested` walks an out-of-process iframe's own tree,
/// whose first frame is an iframe (a `- frame:` section, always in the
/// isolated world) rather than the page.
pub fn walk_frames_nested<F>(
    tab: &Tab,
    frames: Vec<Frame>,
    mut cmd: F,
    nested: bool,
) -> Result<(String, Vec<(String, String)>), String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let mut text = String::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        // The main document follows the helper policy (cu's isolated world
        // unless `CU_HELPER_WORLD=main`); an iframe is always read in its
        // isolated world, so a cross-origin frame's scripts are never touched.
        let world = if index == 0 && !nested {
            crate::execution::helper_world()
        } else {
            crate::execution::World::Isolated
        };
        let value = match walk_frame(tab, frame, world, &mut cmd) {
            Ok(value) => value,
            Err(e) if index == 0 => return Err(e),
            // The frame went away between the tree walk and here; its
            // section says so rather than failing the whole snapshot.
            Err(_) => {
                text.push_str(&format!(
                    "- frame: {} (unreadable right now; take a new snapshot)\n",
                    frame.url
                ));
                continue;
            }
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
        if index == 0 && !nested {
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

/// Walk one frame and return the JSON the walk produced.
///
/// The walk runs in cu's isolated world of the frame. That world belongs to
/// the document, so it is made once and remembered (see
/// [`crate::pagecache`]), and the helper script is sent once per document
/// too: the walk is left in `window.__cuSnapshot` and the next snapshot
/// only has to call it.
///
/// A world that is gone -- the document was replaced by a navigation the
/// daemon did not see -- is made again, and the walk installed again.
fn walk_frame<F>(
    tab: &Tab,
    frame: &Frame,
    world: crate::execution::World,
    cmd: &mut F,
) -> Result<String, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    for attempt in 0..2 {
        let context = match world {
            crate::execution::World::Main => None,
            crate::execution::World::Isolated => Some(world_for_with(tab, &frame.id, cmd)?),
        };
        let installed =
            context.is_some() && pagecache::get(tab, &frame.id).is_some_and(|f| f.installed);
        let expression = if installed {
            snapshot::call_script().to_string()
        } else {
            snapshot::install_script(snapshot::base_for(tab.cdp_port, &frame.id))
        };
        let params = match context {
            Some(id) => format!(
                "{{\"expression\":{},\"contextId\":{id},\"returnByValue\":true}}",
                json_string(&expression)
            ),
            None => format!(
                "{{\"expression\":{},\"returnByValue\":true}}",
                json_string(&expression)
            ),
        };
        let value = match cmd("Runtime.evaluate", &params) {
            Ok(reply) => evaluated_string(&reply),
            Err(error) => {
                // The world is gone: the document behind it was replaced.
                if attempt == 0 && stale_world(&error) {
                    pagecache::forget(tab, &frame.id);
                    continue;
                }
                return Err(error);
            }
        };
        let Some(value) = value else {
            // No value: either the page did not answer, or the helpers were
            // not installed there after all (a document that replaced the one
            // the cache remembered). Install them and walk again.
            if attempt == 0 {
                pagecache::forget(tab, &frame.id);
                continue;
            }
            return Err("page did not return a snapshot".into());
        };
        if !installed {
            pagecache::mark_installed(tab, &frame.id);
        }
        return Ok(value);
    }
    Err("page did not return a snapshot".into())
}

/// The execution context id of cu's isolated world in `frame_id`, made once
/// per document and remembered until the document is replaced.
fn world_for_with<F>(tab: &Tab, frame_id: &str, cmd: &mut F) -> Result<i64, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    if let Some(frame) = pagecache::get(tab, frame_id) {
        return Ok(frame.context);
    }
    let context = crate::execution::isolated_context(cmd, frame_id)?;
    pagecache::remember(tab, frame_id, context, false);
    Ok(context)
}

/// The `"contextId":N,` fragment for cu's isolated world of `frame_id`.
///
/// The world belongs to the document: it is made once and remembered until
/// the document is replaced (see [`crate::pagecache`]).
fn cached_context_fragment<F>(
    tab: &Tab,
    frame_id: &str,
    world: crate::execution::World,
    cmd: &mut F,
) -> Result<String, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    match world {
        crate::execution::World::Main => Ok(String::new()),
        crate::execution::World::Isolated => {
            let context = world_for_with(tab, frame_id, cmd)?;
            Ok(format!("\"contextId\":{context},"))
        }
    }
}

/// The execution context id of cu's isolated world in `frame_id` of `tab`,
/// made once per document and remembered until the document is replaced.
pub fn world_for(tab: &Tab, frame_id: &str) -> Result<i64, String> {
    if let Some(frame) = pagecache::get(tab, frame_id) {
        return Ok(frame.context);
    }
    let context = crate::execution::isolated_context(&mut |m, p| tab.command(m, p), frame_id)?;
    pagecache::remember(tab, frame_id, context, false);
    Ok(context)
}

/// Whether an error says the world cu remembered is gone: the document was
/// replaced, and the world with it.
pub fn stale_world(error: &str) -> bool {
    error.contains("Cannot find context")
        || error.contains("Execution context was destroyed")
        || error.contains("Inspected target navigated or closed")
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
        // A new document may not have its isolated world yet; that is the
        // same "not ready, try again" as an evaluate that lost its context.
        let Ok(context) = helper_context(tab) else {
            thread::sleep(Duration::from_millis(2));
            continue;
        };
        let params = crate::execution::evaluate_params(&settle_script(left), &context, true);
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
    // The form of a modern app is a React form: its inputs are controlled,
    // so writing `value` into them is not enough -- the framework only sees
    // the change if the value goes through its own setter and the input
    // event follows. The submit is a button click, not `form.submit()`,
    // because the framework listens for the submit event.
    let script = format!(
        "(function(){{\
         var p=document.querySelector('input[type=password]');\
         if(!p)return 'no password field';\
         var u=document.querySelector('input[name=username],input[name=email],input[type=text]');\
         var set=function(el,value){{\
           if(!el)return;\
           var proto=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;\
           var desc=Object.getOwnPropertyDescriptor(proto,'value');\
           if(desc&&desc.set)desc.set.call(el,value);else el.value=value;\
           el.dispatchEvent(new Event('input',{{bubbles:true}}));\
           el.dispatchEvent(new Event('change',{{bubbles:true}}));\
         }};\
         if(u){{if(u.disabled)u.disabled=false;set(u,{user});}}\
         if(p.disabled)p.disabled=false;\
         set(p,{password});\
         var f=p.closest('form');\
         if(f){{\
           var btn=f.querySelector('button[type=submit]')||f.querySelector('button');\
           if(btn){{btn.click();return 'clicked the submit button';}}\
           f.submit();return 'submitted';\
         }}\
         return 'filled';}})()",
        user = json_string(user),
        password = json_string(password)
    );
    let tab = Tab::default_for(state.cdp_port);
    tab.command(
        "Runtime.evaluate",
        &crate::execution::evaluate_params(&script, &helper_context(&tab)?, false),
    )?;
    Ok(())
}

/// The context fragment for a helper on `tab`'s main frame, per the helper
/// world policy (see [`crate::execution`]).
fn helper_context(tab: &Tab) -> Result<String, String> {
    let world = crate::execution::helper_world();
    if world == crate::execution::World::Main {
        return Ok(String::new());
    }
    // The main frame's id is known from the last time the page was read, and
    // the frame outlives the documents in it; only the world has to be made
    // again, and only when the document behind it was replaced.
    let frame = match pagecache::main_frame(tab) {
        Some(frame) => frame,
        None => {
            let mut cmd = |m: &str, p: &str| tab.command(m, p);
            crate::execution::main_frame(&mut cmd)?
        }
    };
    let context = world_for(tab, &frame)?;
    Ok(format!("\"contextId\":{context},"))
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
                    let code = hex4(&mut chars)?;
                    // Characters outside the BMP (emoji) arrive as a UTF-16
                    // surrogate pair, `\ud83e\udd16`: join the two halves.
                    // Decoding each half alone gave two U+FFFD.
                    let code = if (0xD800..0xDC00).contains(&code) {
                        let mut ahead = chars.clone();
                        match (ahead.next(), ahead.next()) {
                            (Some('\\'), Some('u')) => match hex4(&mut ahead) {
                                Some(low) if (0xDC00..0xE000).contains(&low) => {
                                    chars = ahead;
                                    0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
                                }
                                _ => code,
                            },
                            _ => code,
                        }
                    } else {
                        code
                    };
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// Four hex digits of a `\u` escape.
fn hex4(chars: &mut std::str::Chars) -> Option<u32> {
    let hex: String = chars.take(4).collect();
    u32::from_str_radix(&hex, 16).ok()
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

/// The tabs a DevTools page target stands for: itself by id (`?tab=`, a
/// named context's page) and the default tab when it is pinned to it.
pub fn tabs_for_target(cdp_port: u16, id: &str) -> Vec<Tab> {
    let mut tabs = vec![Tab {
        cdp_port,
        target: Some(id.to_string()),
    }];
    if pinned_target(cdp_port).is_some_and(|p| p == id) {
        tabs.push(Tab::default_for(cdp_port));
    }
    tabs
}

/// Remember which page target the default tab is, so popups and `/json`
/// ordering cannot move it under the agent's feet.
fn pin_target(cdp_port: u16, id: &str) {
    let mut pinned = pinned_targets().lock().unwrap_or_else(|e| e.into_inner());
    let moved = pinned
        .get(&cdp_port)
        .map(|p| p != id)
        .unwrap_or(false);
    pinned.insert(cdp_port, id.to_string());
    drop(pinned);
    // The default tab now points at another page: what cu remembered about
    // the old one (its frames, its isolated world) is about a document that
    // is gone. Execution context ids are per session, not global, so a stale
    // one can name a frame of the new page.
    if moved {
        pagecache::forget_tab(&Tab::default_for(cdp_port));
        pagecache::forget_frames(&Tab::default_for(cdp_port));
    }
}

/// Forget the pinned default (after it was closed); the next action that
/// needs the default picks a live page and pins that one.
fn unpin_target(cdp_port: u16, id: &str) {
    let mut pinned = pinned_targets().lock().unwrap_or_else(|e| e.into_inner());
    if pinned.get(&cdp_port).is_some_and(|p| p == id) {
        pinned.remove(&cdp_port);
        drop(pinned);
        // The default tab has no page any more; what cu remembered about the
        // closed one is about a document that is gone.
        pagecache::forget_tab(&Tab::default_for(cdp_port));
        pagecache::forget_frames(&Tab::default_for(cdp_port));
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

/// The name of the context `tab` is the tab of, if it is one.
pub fn context_of(tab: &Tab) -> Option<String> {
    let target = tab.target.as_ref()?;
    contexts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|((port, _), c)| *port == tab.cdp_port && &c.target == target)
        .map(|((_, name), _)| name.clone())
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
    /// Open a DevTools websocket to one target of `cdp_port`, by id.
    ///
    /// The browser-level commands do not name a target, so the stealth pass
    /// ([`crate::stealth`]) reaches the page ones through here.
    pub fn target(cdp_port: u16, target: &str) -> Result<Self, String> {
        let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json/list")?;
        let ws = json_objects(&discovery)
            .into_iter()
            .find(|o| json_string_value(o, "id").as_deref() == Some(target))
            .and_then(|o| json_string_value(&o, "webSocketDebuggerUrl"))
            .ok_or_else(|| format!("no DevTools target {target} any more"))?;
        Self::connect(&ws)
    }

    /// Discover the tab's target and complete the websocket handshake.
    pub(crate) fn open(tab: &Tab) -> Result<Self, String> {
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
    ///
    /// Public because cu connects to endpoints other than a tab's own: the
    /// browser-level control connection and the per-target pass of
    /// [`crate::stealth`].
    pub fn connect(ws: &str) -> Result<Self, String> {
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

    /// Send several commands and read their replies in one round trip.
    ///
    /// The commands are written before the first reply is read, so the
    /// browser answers them without a round trip each: a click's press,
    /// release and barrier cost one round trip instead of three. The
    /// replies come back in the order the commands were sent, and the events
    /// that arrive in between are handed to `observe`, exactly as they are
    /// for a single command.
    pub(crate) fn call_many(
        &mut self,
        commands: &[(&str, &str)],
        observe: &mut dyn FnMut(&str),
    ) -> Vec<Result<String, String>> {
        let started = Instant::now();
        let mut ids = Vec::with_capacity(commands.len());
        for (method, params) in commands {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            ids.push(id);
            let message = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
            if let Err(error) = self
                .stream
                .write_all(&encode_client_frame(message.as_bytes()))
            {
                let error = error.to_string();
                return commands.iter().map(|_| Err(error.clone())).collect();
            }
        }
        let mut replies: Vec<Option<Result<String, String>>> =
            (0..commands.len()).map(|_| None).collect();
        let mut left = commands.len();
        while left > 0 {
            let text = match self.next_message(MAX_CDP_MESSAGE) {
                Ok(text) => text,
                Err(error) => {
                    for slot in replies.iter_mut() {
                        if slot.is_none() {
                            *slot = Some(Err(error.clone()));
                        }
                    }
                    break;
                }
            };
            match message_id(&text) {
                Some(id) => {
                    if let Some(position) = ids.iter().position(|sent| *sent == id)
                        && replies[position].is_none()
                    {
                        replies[position] = Some(Ok(text));
                        left -= 1;
                    }
                }
                None => {
                    crate::diagnostics::record_event();
                    observe(&text);
                }
            }
        }
        let elapsed = started.elapsed();
        replies
            .into_iter()
            .enumerate()
            .map(|(index, slot)| {
                let reply = slot.unwrap_or_else(|| Err("the browser did not answer".into()));
                crate::diagnostics::record_command(
                    commands[index].0,
                    commands[index].1,
                    elapsed,
                    reply.as_ref().is_ok_and(|r| !r.contains("\"error\":{")),
                );
                reply
            })
            .collect()
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
        let started = Instant::now();
        let reply = self.exchange(id, &message, observe);
        crate::server::trace(&format!("cdp {method}"), Some(started));
        crate::diagnostics::record_command(
            method,
            params,
            started.elapsed(),
            reply.as_ref().is_ok_and(|r| !r.contains("\"error\":{")),
        );
        reply
    }

    fn exchange(
        &mut self,
        id: u64,
        message: &str,
        observe: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        self.stream
            .write_all(&encode_client_frame(message.as_bytes()))
            .map_err(|e| e.to_string())?;
        loop {
            let text = self.next_message(MAX_CDP_MESSAGE)?;
            // Events carry no `id`; a reply for an older, abandoned command is
            // not ours either. Both are skipped rather than misreported.
            match message_id(&text) {
                Some(got) if got == id => {
                    return Ok(text);
                }
                Some(_) => continue,
                None => {
                    crate::diagnostics::record_event();
                    observe(&text)
                }
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

    /// Fire one command without reading for its reply: the reply comes back
    /// through the event loop that is calling us. Answering interception
    /// events this way never stops reading the stream — a command read for
    /// inline would swallow the events arriving while it waited, and a
    /// request paused for ever is a page hung for ever.
    pub(crate) fn send(&mut self, method: &str, params: &str) -> Result<(), String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let message = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
        self.stream
            .write_all(&encode_client_frame(message.as_bytes()))
            .map_err(|e| e.to_string())
    }

    /// Throw away the frames that are already waiting on this connection.
    ///
    /// A batch starts with whatever happened before it (a navigation the page
    /// made on its own, say): those events are not this batch's business, and
    /// a stale one would make it wait for a page that is not coming.
    ///
    /// Only whole frames are read. A frame that is half in the socket is
    /// left alone: the connection cannot carry it any more, and the batch
    /// that needs it opens a fresh one.
    ///
    /// Returns whether the connection is still usable.
    pub(crate) fn drain_events(&mut self) -> bool {
        loop {
            let mut head = [0u8; 10];
            // The peek must not wait: a blocking peek would sit out the
            // read timeout on every batch with nothing to drain.
            let _ = self.stream.set_nonblocking(true);
            let peeked = self.stream.peek(&mut head);
            let _ = self.stream.set_nonblocking(false);
            let available = match peeked {
                Ok(0) => return false, // the browser closed the connection
                Ok(available) => available,
                // Nothing is waiting: the socket had nothing to give.
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return true;
                }
                // The connection is broken (the browser went away): it cannot
                // carry a command any more.
                Err(_) => return false,
            };
            if available < 2 {
                return true; // a header still on its way
            }
            let mut length = (head[1] & 0x7f) as usize;
            let mut header = 2usize;
            if length == 126 {
                if available < 4 {
                    return true;
                }
                length = u16::from_be_bytes([head[2], head[3]]) as usize;
                header = 4;
            } else if length == 127 {
                if available < 10 {
                    return true;
                }
                let mut wide = [0u8; 8];
                wide.copy_from_slice(&head[2..10]);
                length = u64::from_be_bytes(wide) as usize;
                header = 10;
            }
            let frame = header + length;
            if frame > MAX_CDP_MESSAGE {
                return false;
            }
            // The whole frame has to be here before it is read: a frame read
            // in two pieces leaves the connection out of step.
            let mut whole = vec![0u8; frame];
            match self.stream.peek(&mut whole) {
                Ok(available) if available >= frame => {}
                _ => return true,
            }
            if read_frame(&mut self.stream, MAX_CDP_MESSAGE).is_err() {
                return false;
            }
        }
    }

    fn next_message(&mut self, limit: usize) -> Result<String, String> {
        let payload = read_frame(&mut self.stream, limit).map_err(|e| e.to_string())?;
        String::from_utf8(payload).map_err(|e| e.to_string())
    }

    /// Like [`Self::next_message`], but a read that timed out is `Ok(None)`
    /// instead of an error: the background auth responder polls the stream
    /// between answers and must tell "nothing to answer yet" from "this
    /// connection is gone, stop".
    fn next_message_optional(&mut self, limit: usize) -> Result<Option<String>, String> {
        match read_frame(&mut self.stream, limit) {
            Ok(payload) => String::from_utf8(payload)
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// The next message, or `Ok(None)` when nothing arrived within `idle`.
    ///
    /// The idle wait is a `peek`, which consumes nothing, so a timeout can
    /// never leave half a frame in the stream. Once the first byte is there
    /// the whole frame is read with `frame_limit` as the bound; a failure
    /// after that point is an error, and the connection must be dropped, not
    /// read again.
    pub(crate) fn wait_event(
        &mut self,
        idle: Duration,
        frame_limit: Duration,
    ) -> Result<Option<String>, String> {
        let _ = self
            .stream
            .set_read_timeout(Some(idle.max(Duration::from_millis(1))));
        let mut one = [0u8; 1];
        match self.stream.peek(&mut one) {
            Ok(0) => return Err("Chromium closed the DevTools connection".into()),
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.to_string()),
        }
        let _ = self.stream.set_read_timeout(Some(frame_limit));
        let message = self.next_message(MAX_CDP_MESSAGE);
        let _ = self.stream.set_read_timeout(Some(CDP_HTTP_TIMEOUT));
        message.map(Some)
    }

    /// How long the next read may wait: the auth window pumps the stream
    /// directly and bounds each read by the time it has left.
    pub(crate) fn set_read_timeout(&self, timeout: Duration) {
        let _ = self.stream.set_read_timeout(Some(timeout));
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

/// How long the auth responder's read waits before it looks at its stop flag
/// again.
const AUTH_KEEPER_POLL: Duration = Duration::from_millis(500);

/// One tab's background basic-auth responder.
///
/// Why it exists: a credential handed to the browser through
/// `Fetch.continueWithAuth` is **not** reliably put in Chrome's HTTP auth
/// cache. Measured on this machine against a real nginx-fronted origin over
/// TLS: the realm was challenged again for the page's very next request.
/// With the cache empty and no dialog in headless, that request then waits
/// for ever. Answering every challenge means somebody has to be reading the
/// tab's event stream at all times, which a task-driven agent cannot promise:
/// between its commands nobody is reading, and a paused request only arrives
/// when somebody is.
///
/// So the tab gets a reader of its own — a dedicated DevTools connection with
/// `Fetch` armed for the credentialled host, answering challenges with the
/// vault pair and continuing every other intercepted request. Its own
/// connection keeps the pool free for commands, and a connection that dies
/// (tab closed, browser restarted) takes its interception with it.
struct AuthKeeper {
    host: String,
    pair: (String, String),
    stop: Arc<AtomicBool>,
    /// Set once the thread has let the interception go: whoever arms `Fetch`
    /// next must not race a responder still holding it.
    done: Arc<AtomicBool>,
}

fn auth_keepers() -> &'static Mutex<HashMap<String, Arc<AuthKeeper>>> {
    static KEEPERS: OnceLock<Mutex<HashMap<String, Arc<AuthKeeper>>>> = OnceLock::new();
    KEEPERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The key a tab's keeper is filed under.
fn keeper_key(tab: &Tab) -> String {
    tab.target
        .clone()
        .unwrap_or_else(|| format!("default:{}", tab.cdp_port))
}

/// Stop a tab's keeper, waiting (bounded) for it to give the interception
/// back, so a navigation's own `Fetch.enable` is never a second interceptor
/// on the same host.
fn stop_auth_keeper(tab: &Tab) {
    let key = keeper_key(tab);
    let Some(keeper) = auth_keepers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key)
    else {
        return;
    };
    keeper.stop.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !keeper.done.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

/// Arm (or re-arm) the tab's responder for `host` with `pair`. A keeper
/// already running for the tab is stopped first, so re-navigating to the same
/// host replaces its credentials instead of racing them.
fn start_auth_keeper(tab: &Tab, host: &str, pair: (String, String)) {
    if host.is_empty() {
        return;
    }
    stop_auth_keeper(tab);
    let keeper = Arc::new(AuthKeeper {
        host: host.to_string(),
        pair,
        stop: Arc::new(AtomicBool::new(false)),
        done: Arc::new(AtomicBool::new(false)),
    });
    auth_keepers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(keeper_key(tab), Arc::clone(&keeper));
    let tab = tab.clone();
    thread::spawn(move || {
        let mut connection = match CdpConnection::open(&tab) {
            Ok(connection) => connection,
            Err(_) => {
                keeper.done.store(true, Ordering::SeqCst);
                return;
            }
        };
        if connection
            .call("Fetch.enable", &fetch_enable_params(&keeper.host))
            .is_err()
        {
            // Something else already answers for this host; leave it be.
            keeper.done.store(true, Ordering::SeqCst);
            return;
        }
        while !keeper.stop.load(Ordering::SeqCst) {
            connection.set_read_timeout(AUTH_KEEPER_POLL);
            match connection.next_message_optional(MAX_CDP_MESSAGE) {
                Ok(Some(text)) => {
                    if message_id(&text).is_some() {
                        continue;
                    }
                    if let Some(rid) = auth_required_id(&text) {
                        let _unused = connection.send(
                            "Fetch.continueWithAuth",
                            &continue_with_auth_params(&rid, &Some(keeper.pair.clone())),
                        );
                    } else if let Some(rid) = paused_request_id(&text) {
                        let _unused = connection.send(
                            "Fetch.continueRequest",
                            &format!("{{\"requestId\":{}}}", json_string(&rid)),
                        );
                    }
                }
                // Nothing to answer yet.
                Ok(None) => continue,
                // The stream is gone: the interception dies with it.
                Err(_) => break,
            }
        }
        let _ = connection.call("Fetch.disable", "{}");
        keeper.done.store(true, Ordering::SeqCst);
    });
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

/// Run several DevTools commands on `tab` and return their replies, in the
/// order the commands were sent.
///
/// The commands travel together, so the browser answers them without a round
/// trip each: a snapshot's frame tree and its walk cost one round trip
/// instead of two.
pub fn call_batch(tab: &Tab, commands: &[(&str, &str)]) -> Vec<Result<String, String>> {
    for attempt in 0..2 {
        let Ok(mut connection) = checkout(tab) else {
            return commands
                .iter()
                .map(|_| Err("no DevTools connection".into()))
                .collect();
        };
        let replies = connection.call_many(commands, &mut |_| {});
        if replies.iter().all(|reply| reply.is_ok()) {
            let mut guard = cdp_pool(tab).lock().unwrap_or_else(|e| e.into_inner());
            guard.put(connection);
            return replies;
        }
        // A broken connection is only worth retrying once: the second try
        // has just opened a fresh one.
        if attempt == 0 {
            continue;
        }
        return replies;
    }
    unreachable!("the retry loop returns on its second attempt")
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
/// [`cdp_browser_command`] for other modules.
pub fn browser_command(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
    cdp_browser_command(cdp_port, method, params)
}

/// The page target the default tab is pinned to, if any.
pub fn pinned_default_target(cdp_port: u16) -> Option<String> {
    pinned_target(cdp_port)
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
fn read_frame(stream: &mut TcpStream, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut head = [0; 2];
    stream.read_exact(&mut head)?;
    let opcode = head[0] & 0x0f;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0; 2];
        stream.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0; 8];
        stream.read_exact(&mut b)?;
        len = usize::try_from(u64::from_be_bytes(b))
            .map_err(|_| std::io::Error::other("CDP response is too large"))?;
    }
    if len > limit {
        return Err(std::io::Error::other("CDP response is too large"));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload)?;
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
    // A bounded connect: a SYN dropped while the browser's small DevTools
    // backlog is full (the start-up poll) otherwise sits in the kernel's
    // retransmit backoff for up to two minutes, and the launch thread with it.
    let mut stream = match address.parse::<std::net::SocketAddr>() {
        Ok(addr) => TcpStream::connect_timeout(&addr, CDP_CONNECT_TIMEOUT),
        Err(_) => TcpStream::connect(address),
    }
    .map_err(|e| e.to_string())?;
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
                && buf.len() >= end + len
            {
                break;
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

/// Start a timing span. Enabled with `CU_TRACE=1`; the daemon prints one
/// `TRACE <label> <ms>` line per span on stderr.
pub fn trace_start() -> Option<std::time::Instant> {
    if std::env::var_os("CU_TRACE").is_some() {
        Some(std::time::Instant::now())
    } else {
        None
    }
}

/// Print a timing span started by [`trace_start`].
pub fn trace(label: &str, start: Option<std::time::Instant>) {
    if let Some(start) = start {
        eprintln!(
            "TRACE {label} {:.2} @{}",
            start.elapsed().as_secs_f64() * 1000.0,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
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
/// Whether the JSON object `input` carries `key: true`. `json_value` reads
/// strings and numbers only — on a bare boolean it hands back the empty
/// string, which is indistinguishable from `false`, so a flag like the login
/// page's "remember this session" needs its own reader.
pub fn json_bool(input: &str, key: &str) -> bool {
    let needle = format!("\"{key}\"");
    let Some(start) = input.find(&needle) else {
        return false;
    };
    let rest = input[start + needle.len()..].trim_start();
    let Some(rest) = rest.strip_prefix(':') else {
        return false;
    };
    rest.trim_start().starts_with("true")
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
/// The vault UI routes, for a request that arrived without the bearer token
/// (a browser). Page routes answer whatever the cookie says (the pages
/// themselves decide by fetching data); data routes 401 without a live UI
/// session. `cookie` is the request's `Cookie:` header value if it had one.
// The signature the vault-route tests were written against; production
// goes through `vault_route_with`, which also carries the OTP collect
// token header.
#[cfg(test)]
fn vault_route(
    method: &str,
    full_path: &str,
    body: &str,
    state: &AppState,
    cookie: Option<&str>,
) -> (&'static str, &'static str, String, Option<String>) {
    vault_route_with(method, full_path, body, state, cookie, None)
}

/// `vault_route` plus the OTP collect token, which arrives as a header -
/// never in the URL query, where it would land in history and logs.
fn vault_route_with(
    method: &str,
    full_path: &str,
    body: &str,
    state: &AppState,
    cookie: Option<&str>,
    collect_token: Option<&str>,
) -> (&'static str, &'static str, String, Option<String>) {
    // The query selects content (the hand-off page's token, the login
    // page's `next`), never a different route — so match without it and
    // hand the whole thing to the API below.
    let path = full_path.split('?').next().unwrap_or(full_path);
    match (method, path) {
        ("GET", "/vault") => (
            "200 OK",
            "text/html; charset=utf-8",
            LOGIN_PAGE.into(),
            None,
        ),
        ("GET", "/vault/app") => ("200 OK", "text/html; charset=utf-8", APP_PAGE.into(), None),
        ("GET", "/vault/live") => (
            "200 OK",
            "text/html; charset=utf-8",
            crate::live::PAGE.into(),
            None,
        ),
        ("GET", "/vault/otp") => ("200 OK", "text/html; charset=utf-8", OTP_PAGE.into(), None),
        // Control page: the one explicit route. If the page cannot be switched to
        // control mode it is not served at all, never served in the wrong mode.
        ("GET", "/vault/live2") => match crate::live_stream::control_page() {
            Some(page) => ("200 OK", "text/html; charset=utf-8", page, None),
            None => (
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                "viewer page template is broken".into(),
                None,
            ),
        },
        // The page as shipped is read-only: the one a frame may show.
        ("GET", "/vault/live2/view") => (
            "200 OK",
            "text/html; charset=utf-8",
            crate::live_stream::PAGE.into(),
            None,
        ),
        ("POST", "/v1/vault/ui/login") => {
            if !crate::vault::login_allowed() {
                return (
                    "429 Too Many Requests",
                    "application/json",
                    "{\"error\":\"too many attempts; wait five minutes\"}".into(),
                    None,
                );
            }
            let password = json_value(body, "password").unwrap_or_default();
            if password.is_empty() {
                return (
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"password is required\"}".into(),
                    None,
                );
            }
            let vault = match crate::vault::Vault::open(&state.data_dir) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        "500 Internal Server Error",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                        None,
                    );
                }
            };
            // First run: the first password a person types becomes it.
            let outcome = if vault.has_password() {
                vault.verify_password(&password)
            } else {
                vault.set_password(None, &password).is_ok()
            };
            if !outcome {
                return (
                    "401 Unauthorized",
                    "application/json",
                    "{\"error\":\"wrong master password\"}".into(),
                    None,
                );
            }
            // "Remember this session" is a server-side property of the
            // session, never a client-side one: the cookie is HttpOnly, so
            // nothing in the page (and nothing in localStorage) can extend a
            // session's life. It arrives here in the login body and lives in
            // `ui_sessions` until the session ends.
            let remember = json_bool(body, "remember");
            vault.ui_sessions_reap();
            let token = match vault.ui_session_create(remember) {
                Ok(token) => token,
                Err(e) => {
                    return (
                        "500 Internal Server Error",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                        None,
                    );
                }
            };
            (
                "200 OK",
                "application/json",
                "{\"ok\":true}".into(),
                Some(session_cookie(
                    &token,
                    crate::vault::ui_session_ttl(remember),
                )),
            )
        }
        ("POST", "/v1/vault/ui/logout") => {
            if let Ok(vault) = crate::vault::Vault::open(&state.data_dir) {
                vault.ui_session_drop(cookie.and_then(crate::vault::cookie_token).as_deref());
            }
            (
                "200 OK",
                "application/json",
                "{\"ok\":true}".into(),
                Some(CLEARED_COOKIE.into()),
            )
        }
        ("GET", "/v1/vault/ui/session") => {
            // What the login page asks before showing a form: is there already
            // a live session, and how long has it left? The renewal is
            // deliberate here — the person is demonstrably at the vault — and
            // the answer carries no secret, only its own expiry.
            let vault = match crate::vault::Vault::open(&state.data_dir) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        "500 Internal Server Error",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                        None,
                    );
                }
            };
            match vault.ui_session_check(cookie.and_then(crate::vault::cookie_token).as_deref()) {
                Some(session) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"authenticated\":true,\"remember\":{},\"expires_in\":{}}}",
                        session.remember,
                        session.max_age()
                    ),
                    Some(session_cookie(
                        &cookie
                            .and_then(crate::vault::cookie_token)
                            .unwrap_or_default(),
                        session.max_age(),
                    )),
                ),
                None => (
                    "200 OK",
                    "application/json",
                    "{\"authenticated\":false}".into(),
                    None,
                ),
            }
        }
        ("GET", "/v1/vault/ui/state") => {
            // The login page reads this before any session exists: whether a
            // master password is set decides first-run (repeat field) vs
            // plain login. Nothing secret in the answer.
            match crate::vault::Vault::open(&state.data_dir) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    format!("{{\"has_password\":{}}}", v.has_password()),
                    None,
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    "application/json",
                    format!("{{\"error\":{}}}", json_string(&e)),
                    None,
                ),
            }
        }
        _ => {
            // Everything below this line is the vault's data API and needs a
            // live UI session. With nginx's `auth_basic` gone (the browser's
            // native dialog was the thing being removed), this branch is the
            // *only* thing standing between the public internet and every
            // stored cookie, credential and secret request. It is therefore
            // deliberately deny-by-default: a path that is not matched above
            // gets a session check, and a request without one never reaches
            // `vault_api`, whatever the path, method or query say.
            let vault = match crate::vault::Vault::open(&state.data_dir) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        "500 Internal Server Error",
                        "application/json",
                        format!("{{\"error\":{}}}", json_string(&e)),
                        None,
                    );
                }
            };
            let token = cookie.and_then(crate::vault::cookie_token);
            let Some(session) = vault.ui_session_check(token.as_deref()) else {
                return (
                    "401 Unauthorized",
                    "application/json",
                    "{\"error\":\"vault: log in first\"}".into(),
                    None,
                );
            };
            // Sliding renewal: the browser's copy of `Max-Age` is re-issued on
            // every authenticated request, so an active session never expires
            // under the person working, and an abandoned one still dies after
            // its whole lifetime of silence.
            let (status, kind, response) = vault_api(method, full_path, body, state, collect_token);
            (
                status,
                kind,
                response,
                Some(session_cookie(
                    token.as_deref().unwrap_or_default(),
                    session.max_age(),
                )),
            )
        }
    }
}

const LOGIN_PAGE: &str = include_str!("vault_login.html");
const OTP_PAGE: &str = include_str!("vault_otp.html");
const MANROPE: &[u8] = include_bytes!("manrope.ttf");
const APP_PAGE: &str = include_str!("vault_app.html");

/// The `Set-Cookie` value for a vault UI session.
///
/// Three attributes carry the security here, and the third one is a
/// deliberate loosening:
///
/// - `HttpOnly` — no script can read the session token, so an XSS on this
///   page cannot exfiltrate it.
/// - `Secure` — the vault is served over HTTPS and only over HTTPS. Without
///   the flag the cookie travelled in the clear on any accidental plain-HTTP
///   request (the port-80 vhost answered the vault too), where anything on
///   the path could read it and replay it.
/// - `SameSite=Lax` instead of `Strict` — `Strict` withholds the cookie from
///   every cross-site request, *including a top-level navigation*: arriving at
///   the vault from a link in a mail client or a chat, or through a redirect,
///   dropped the session with no explanation and the person was asked for the
///   master password again. `Lax` sends the cookie on those top-level GETs
///   and withholds it on every cross-site POST/form, which is the case that
///   actually matters for forgery. Since every state-changing vault route is
///   a POST or a DELETE, `Lax` keeps that protection intact.
fn session_cookie(token: &str, max_age: i64) -> String {
    format!("cu_vault_session={token}; Max-Age={max_age}; Path=/; HttpOnly; Secure; SameSite=Lax")
}

/// The same cookie, expired: what logging out sets.
const CLEARED_COOKIE: &str = "cu_vault_session=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Lax";

/// The vault data API. Reached either with the bearer token (the local CLI,
/// an agent) or with a live UI session cookie (the vault UI through nginx).
fn vault_api(
    method: &str,
    path: &str,
    body: &str,
    state: &AppState,
    collect_token: Option<&str>,
) -> (&'static str, &'static str, String) {
    // The live view of a tab (a person finishing a check an agent must not
    // solve) is a vault route: same session, same gate.
    if let Some(reply) = crate::live::api(method, path, body, state) {
        return reply;
    }
    let vault = match crate::vault::Vault::open(&state.data_dir) {
        Ok(v) => v,
        Err(e) => {
            return (
                "500 Internal Server Error",
                "application/json",
                format!("{{\"error\":{}}}", json_string(&e)),
            );
        }
    };
    let path_only = path.split('?').next().unwrap_or(path);
    match (method, path_only) {
        // `/v1/vault/ui/state` is answered in `vault_route`: the login page
        // reads it before any session exists.
        ("GET", "/v1/vault/status") => {
            let sessions = vault.list_sessions().unwrap_or_default();
            let credentials = vault.list_credentials("").unwrap_or_default();
            (
                "200 OK",
                "application/json",
                format!(
                    "{{\"open\":true,\"sessions\":{},\"credentials\":{}}}",
                    sessions.matches("\"domain\"").count(),
                    credentials.matches("\"id\"").count()
                ),
            )
        }
        ("GET", "/v1/vault/sessions") => match vault.list_sessions() {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => error_json(&e),
        },
        ("POST", "/v1/vault/sessions/save") => {
            // The session of one tab: `?tab=ID` says which; without it the
            // tab the default resolves to.
            let id = form_value(query(path), "tab");
            let tab = if id.is_empty() {
                Tab::default_for(state.cdp_port)
            } else {
                Tab {
                    cdp_port: state.cdp_port,
                    target: Some(id.clone()),
                }
            };
            let discovery = match tabs_json(state.cdp_port) {
                Ok(d) => d,
                Err(e) => return error_json(&e),
            };
            // `tabs_json` wraps the array in {"tabs":[...]}; the object reader
            // wants the bare array.
            let listed = json_array(&discovery, "tabs").unwrap_or_default();
            let url = json_objects(&listed)
                .into_iter()
                .find(|o| json_string_value(o, "id").as_deref() == Some(id.as_str()))
                .and_then(|o| json_string_value(&o, "url"))
                .unwrap_or_default();
            let domain = {
                let asked = form_value(query(path), "domain");
                if asked.is_empty() {
                    url_host(&url)
                } else {
                    asked
                }
            };
            if domain.is_empty() {
                return error_json("cannot tell which domain this tab is on");
            }
            let cookies = match cdp_browser_command(state.cdp_port, "Storage.getCookies", "{}") {
                Ok(reply) => json_array(&reply, "cookies").unwrap_or_else(|| "[]".into()),
                Err(e) => return error_json(&e),
            };
            let cookie_list = json_objects(&cookies);
            let cookies = filter_cookies(&cookie_list, &domain);
            let storage = tab
                .command(
                    "Runtime.evaluate",
                    "{\"expression\":\"JSON.stringify(localStorage)\",\"returnByValue\":true}",
                )
                .ok()
                .and_then(|reply| evaluated_string(&reply))
                .unwrap_or_else(|| "{}".into());
            match vault.save_session(&domain, &cookies, &storage, None) {
                Ok(()) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"saved\":true,\"domain\":{},\"cookies\":{},\"local_storage\":{}}}",
                        json_string(&domain),
                        cookies.matches("\"name\"").count(),
                        storage.matches(':').count()
                    ),
                ),
                Err(e) => error_json(&e),
            }
        }
        ("DELETE", p) if p.starts_with("/v1/vault/sessions/") => {
            let domain = url_decode(&p["/v1/vault/sessions/".len()..]);
            match vault.forget_session(&domain) {
                Ok(true) => ("200 OK", "application/json", "{\"forgotten\":true}".into()),
                Ok(false) => error_json("no such session"),
                Err(e) => error_json(&e),
            }
        }
        ("GET", "/v1/vault/credentials") => {
            let q = form_value(query(path), "query");
            match vault.list_credentials(&q) {
                Ok(body) => ("200 OK", "application/json", body),
                Err(e) => error_json(&e),
            }
        }
        ("POST", "/v1/vault/credentials") => {
            let field = |k: &str| json_value(body, k).unwrap_or_default();
            let (domain, label) = (field("domain"), field("label"));
            let fields = json_array(body, "fields").unwrap_or_else(|| "[]".into());
            if domain.is_empty() || label.is_empty() {
                return error_json("domain and label are required");
            }
            let id = json_value(body, "id").and_then(|v| v.parse::<i64>().ok());
            match vault.put_credential(id, &domain, &label, fields.trim(), &field("notes")) {
                Ok(id) => (
                    "200 OK",
                    "application/json",
                    format!("{{\"id\":{id},\"saved\":true}}"),
                ),
                Err(e) => error_json(&e),
            }
        }
        ("GET", p) if p.starts_with("/v1/vault/credentials/") => {
            let Ok(id) = p["/v1/vault/credentials/".len()..].parse::<i64>() else {
                return error_json("credential id must be a number");
            };
            match vault.get_credential(id) {
                Ok(Some(body)) => ("200 OK", "application/json", body),
                Ok(None) => error_json("no such credential"),
                Err(e) => error_json(&e),
            }
        }
        ("DELETE", p) if p.starts_with("/v1/vault/credentials/") => {
            let Ok(id) = p["/v1/vault/credentials/".len()..].parse::<i64>() else {
                return error_json("credential id must be a number");
            };
            match vault.delete_credential(id) {
                Ok(true) => ("200 OK", "application/json", "{\"deleted\":true}".into()),
                Ok(false) => error_json("no such credential"),
                Err(e) => error_json(&e),
            }
        }
        ("POST", "/v1/vault/passwd") => {
            let old = json_value(body, "old").unwrap_or_default();
            let new = json_value(body, "new").unwrap_or_default();
            match vault.set_password((!old.is_empty()).then_some(old.as_str()), &new) {
                Ok(()) => ("200 OK", "application/json", "{\"ok\":true}".into()),
                Err(e) => error_json(&e),
            }
        }
        ("GET", "/v1/vault/requests") => {
            let status = form_value(query(path), "status");
            match vault.list_requests((!status.is_empty()).then_some(status.as_str())) {
                Ok(body) => ("200 OK", "application/json", body),
                Err(e) => error_json(&e),
            }
        }
        ("POST", "/v1/vault/requests") => {
            // An agent asks for a secret: the request records who is asking
            // and why, and reveals nothing until a person approves it in the
            // vault UI (or denies it, or lets it expire).
            let field = |k: &str| json_value(body, k).unwrap_or_default();
            let domain = field("domain");
            if domain.is_empty() {
                return error_json("domain is required");
            }
            match vault.create_request(&domain, &field("reason"), &field("requested_by")) {
                Ok(id) => (
                    "200 OK",
                    "application/json",
                    format!("{{\"id\":{id},\"status\":\"pending\"}}"),
                ),
                Err(e) => error_json(&e),
            }
        }
        ("GET", p) if p.starts_with("/v1/vault/requests/") => {
            let tail = &p["/v1/vault/requests/".len()..];
            let Ok(id) = tail.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            match vault.get_request(id) {
                // An approved request carries the credential with its
                // values — the payoff the flow exists for. Pending, denied
                // and expired requests carry none.
                Ok(Some(body)) => ("200 OK", "application/json", body),
                Ok(None) => error_json("no such request"),
                Err(e) => error_json(&e),
            }
        }
        ("POST", p) if p.starts_with("/v1/vault/requests/") => {
            // Reached only with a live UI session (see `is_vault_ui_only`):
            // this is the person's decision, never the asking agent's.
            let tail = &p["/v1/vault/requests/".len()..];
            let (id_part, action) = match tail.rsplit_once('/') {
                Some((id, action)) => (id, action),
                None => return error_json("POST /v1/vault/requests/{id}/approve|deny"),
            };
            let Ok(id) = id_part.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            let approve = match action {
                "approve" => true,
                "deny" => false,
                _ => return error_json("POST /v1/vault/requests/{id}/approve|deny"),
            };
            // Approval links a credential: the one the UI names, or — when it
            // names none — the only credential the vault holds for the
            // requested domain, so the common case is one click.
            let credential_id =
                json_value(body, "credential_id").and_then(|v| v.parse::<i64>().ok());
            let credential_id = if approve && credential_id.is_none() {
                let domain = vault
                    .get_request(id)
                    .ok()
                    .flatten()
                    .and_then(|r| json_string_value(&r, "domain"))
                    .unwrap_or_default();
                let options = vault.credentials_for_domain(&domain).unwrap_or_default();
                match options.len() {
                    1 => Some(options[0].0),
                    0 => return error_json("no credential for this domain to approve with"),
                    _ => {
                        return error_json(
                            "several credentials cover this domain; pass credential_id",
                        );
                    }
                }
            } else {
                credential_id
            };
            match vault.decide_request(id, approve, credential_id) {
                Ok(true) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"id\":{id},\"status\":\"{}\"}}",
                        if approve { "approved" } else { "denied" }
                    ),
                ),
                Ok(false) => error_json("no such request"),
                Err(e) => error_json(&e),
            }
        }
        // ---- one-time-code hand-offs ----
        // An agent opens a hand-off when a page asks for a code it cannot
        // mint (no stored seed) or when enrollment needs the person's own
        // authenticator. It hands the URL to the person and polls by id.
        ("POST", "/v1/vault/otp/requests") => {
            let field = |k: &str| json_value(body, k).unwrap_or_default();
            let domain = field("domain");
            if domain.is_empty() {
                return error_json("domain is required");
            }
            let kind = {
                let k = field("kind");
                if k.is_empty() { "code".to_string() } else { k }
            };
            let ttl = json_value(body, "ttl_secs")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(crate::vault::OTP_REQUEST_TTL_SECS);
            match vault.create_otp_request(
                &domain,
                &field("reason"),
                &kind,
                &field("requested_by"),
                ttl,
            ) {
                Ok((id, token, expires_at)) => {
                    let path = format!("/vault/otp?t={token}");
                    // The daemon knows the vault's public host when a proxy
                    // forwards one; only then is a full URL meaningful.
                    let url = state
                        .vault_host
                        .as_deref()
                        .map(|host| format!(",\"url\":\"https://{host}{path}\""))
                        .unwrap_or_default();
                    (
                        "200 OK",
                        "application/json",
                        format!(
                            "{{\"id\":{id},\"token\":{},\"path\":{}{url},\"expires_at\":{expires_at}}}",
                            json_string(&token),
                            json_string(&path),
                        ),
                    )
                }
                Err(e) => error_json(&e),
            }
        }
        ("GET", "/v1/vault/otp/requests") => match vault.list_otp_requests() {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => error_json(&e),
        },
        // The agent's poll: status only while pending; the moment the row is
        // answered the response is handed over once and deleted (see
        // `poll_otp_request`). `?wait=S` parks like `cu challenge wait`.
        ("GET", p) if p.starts_with("/v1/vault/otp/requests/") => {
            let tail = &p["/v1/vault/otp/requests/".len()..];
            let Ok(id) = tail.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            let wait_for = seconds_param(query(path), "wait", 0.0);
            let deadline = std::time::Instant::now()
                + Duration::from_secs_f64(wait_for.clamp(0.0, wait_ceiling_s()));
            loop {
                match vault.poll_otp_request(id, true, collect_token) {
                    Ok(Some(body)) => {
                        let done = json_string_value(&body, "status")
                            .map(|s| s != "pending")
                            .unwrap_or(true);
                        if done || wait_for <= 0.0 || std::time::Instant::now() >= deadline {
                            return ("200 OK", "application/json", body);
                        }
                    }
                    Ok(None) => return error_json("no such request"),
                    Err(e) => return error_json(&e),
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        ("DELETE", p) if p.starts_with("/v1/vault/otp/requests/") => {
            let tail = &p["/v1/vault/otp/requests/".len()..];
            let Ok(id) = tail.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            match vault.cancel_otp_request(id) {
                Ok(true) => ("200 OK", "application/json", "{\"cancelled\":true}".into()),
                Ok(false) => error_json("request is not pending any more"),
                Err(e) => error_json(&e),
            }
        }
        // Capture the setup QR off the page. The decoded URI is a seed: it
        // is sealed into the credential (and, for a setup hand-off, into the
        // request so the temporary page can re-render it as a QR). The reply
        // carries the issuer and account for confirmation, never the URI.
        ("POST", "/v1/vault/otp/capture") => otp_capture(state, path, body, &vault, None),
        // The collector's acknowledgement: the code arrived; only now does
        // the vault wipe it (see `poll_otp_request` for the two-step
        // hand-over). Without the owning collect token this changes nothing.
        ("POST", p) if p.starts_with("/v1/vault/otp/requests/") && p.ends_with("/ack") => {
            let tail = &p["/v1/vault/otp/requests/".len()..p.len() - "/ack".len()];
            let Ok(id) = tail.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            let token = json_value(body, "collect_token").unwrap_or_default();
            match vault.ack_otp_request(id, &token) {
                Ok(true) => (
                    "200 OK",
                    "application/json",
                    "{\"acknowledged\":true}".into(),
                ),
                Ok(false) => error_json("not collecting, or the collect token does not match"),
                Err(e) => error_json(&e),
            }
        }
        ("POST", p) if p.starts_with("/v1/vault/otp/requests/") && p.ends_with("/capture") => {
            let tail = &p["/v1/vault/otp/requests/".len()..p.len() - "/capture".len()];
            let Ok(id) = tail.parse::<i64>() else {
                return error_json("request id must be a number");
            };
            otp_capture(state, path, body, &vault, Some(id))
        }
        // The current code from a stored seed — the whole point of capturing
        // one. `{"domain": "..."}` or `{"credential_id": N}`.
        ("POST", "/v1/vault/otp/code") => {
            let credential_id =
                json_value(body, "credential_id").and_then(|v| v.parse::<i64>().ok());
            let found = match credential_id {
                Some(id) => vault
                    .totp_for_credential(id)
                    .map(|opt| opt.map(|(code, left)| (id, code, left))),
                None => {
                    let domain = json_value(body, "domain").unwrap_or_default();
                    if domain.is_empty() {
                        return error_json("domain or credential_id is required");
                    }
                    vault.totp_for_domain(&domain)
                }
            };
            match found {
                Ok(Some((id, code, left))) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"credential_id\":{id},\"code\":{},\"seconds_remaining\":{left}}}",
                        json_string(&code)
                    ),
                ),
                Ok(None) => error_json("no stored TOTP seed covers this"),
                Err(e) => error_json(&e),
            }
        }
        // The person's side. Reached only with a live UI session: these
        // paths are UI-only (see `is_vault_ui_only`).
        ("POST", "/v1/vault/otp/answer") => {
            let token = json_value(body, "token").unwrap_or_default();
            let code = json_value(body, "code").unwrap_or_default();
            match vault.answer_otp_request(&token, &code) {
                Ok(true) => ("200 OK", "application/json", "{\"answered\":true}".into()),
                Ok(false) => error_json("this hand-off is spent, expired or unknown"),
                Err(e) => error_json(&e),
            }
        }
        ("GET", "/v1/vault/otp/lookup") => {
            let token = form_value(query(path), "t");
            match vault.otp_request_by_token(&token) {
                Ok(Some(body)) => ("200 OK", "application/json", body),
                Ok(None) => (
                    "404 Not Found",
                    "application/json",
                    "{\"error\":\"unknown or expired hand-off\"}".into(),
                ),
                Err(e) => error_json(&e),
            }
        }
        ("GET", "/v1/vault/otp/qr") => {
            let token = form_value(query(path), "t");
            match vault.otp_request_seed_by_token(&token) {
                Ok(Some(uri)) => match crate::qr::render_svg(&uri) {
                    Ok(svg) => ("200 OK", "image/svg+xml", svg),
                    Err(e) => error_json(&e),
                },
                Ok(None) => (
                    "404 Not Found",
                    "application/json",
                    "{\"error\":\"no setup code on this hand-off\"}".into(),
                ),
                Err(e) => error_json(&e),
            }
        }
        _ => (
            "404 Not Found",
            "application/json",
            "{\"error\":\"not found\"}".into(),
        ),
    }
}

/// Shared by the two capture routes: decode the page's setup QR, seal the
/// URI into the credential it belongs to, and (for a setup hand-off) onto
/// the request, so the temporary page can show the same QR to the person.
fn otp_capture(
    state: &AppState,
    path: &str,
    body: &str,
    vault: &crate::vault::Vault,
    request_id: Option<i64>,
) -> (&'static str, &'static str, String) {
    if let Err(e) = wait_until_ready(state) {
        return error_json(&e);
    }
    let wanted = form_value(query(path), "tab");
    if wanted.is_empty() {
        // A seed capture never falls back to whatever tab happens to be
        // default: the caller names the tab sitting on the service's setup
        // page, so a seed can only come from a deliberately chosen page.
        return error_json(
            "capture needs an explicit --tab naming the tab on the service's setup page",
        );
    }
    let tab = match named_tab(state.cdp_port, &wanted) {
        Ok(tab) => tab,
        Err(e) => return error_json(&e),
    };
    let domain = {
        let asked = json_value(body, "domain").unwrap_or_default();
        if !asked.is_empty() {
            asked
        } else if let Some(id) = request_id {
            vault.otp_request_domain(id).unwrap_or_default()
        } else {
            String::new()
        }
    };
    if domain.is_empty() {
        return error_json("domain is required (or capture through a hand-off that names one)");
    }
    // Identity before decoding: the page this tab actually shows must belong
    // to the domain the capture claims. Sealing a seed read off the wrong
    // site would break a working 2FA setup, so a mismatch refuses instead
    // of guessing - pass an explicit tab when the default one is elsewhere.
    let tab_url = tab
        .command(
            "Runtime.evaluate",
            "{\"expression\":\"location.href\",\"returnByValue\":true}",
        )
        .ok()
        .and_then(|reply| evaluated_string(&reply))
        .unwrap_or_default();
    let host = url_host(&tab_url);
    if host.is_empty() {
        return error_json(
            "could not read the tab's address; pass an explicit --tab on the service's page",
        );
    }
    if !host_covers(&host, &domain) {
        return error_json(&format!(
            "the tab is on {host}, not {domain}; capture on the service's own page (pass --tab to name it)"
        ));
    }
    let uri = match crate::qr::capture_otpauth(&tab) {
        Ok(uri) => uri,
        Err(e) => return error_json(&e),
    };
    let parsed = match crate::otp::parse(&uri) {
        Ok(parsed) => parsed,
        Err(e) => return error_json(&e),
    };
    if !issuer_matches_host(&parsed.issuer, &host) {
        return error_json(&format!(
            "the QR names issuer {} but the tab is on {host}; refusing to seal a seed that does not match its page",
            json_string(&parsed.issuer),
        ));
    }
    let credential_id = json_value(body, "credential_id").and_then(|v| v.parse::<i64>().ok());
    let force = json_value(body, "force").as_deref() == Some("true");
    let label = if parsed.account.is_empty() {
        "2FA".to_string()
    } else {
        parsed.account.clone()
    };
    let (id, replaced) = match vault.put_totp(credential_id, &domain, &label, &uri, force) {
        Ok(done) => done,
        Err(e) => return error_json(&e),
    };
    if let Some(request) = request_id
        && let Err(e) = vault.set_otp_request_seed(request, &uri)
    {
        return error_json(&e);
    }
    let mut reply = format!(
        "{{\"stored\":true,\"credential_id\":{id},\"issuer\":{},\"account\":{}",
        json_string(&parsed.issuer),
        json_string(&parsed.account),
    );
    if let Some(previous) = replaced {
        reply.push_str(&format!(
            ",\"replaced\":true,\"previous_account\":{}",
            json_string(&previous)
        ));
    }
    reply.push('}');
    ("200 OK", "application/json", reply)
}

fn error_json(error: &str) -> (&'static str, &'static str, String) {
    (
        "400 Bad Request",
        "application/json",
        format!("{{\"error\":{}}}", json_string(error)),
    )
}

/// The cookies of a DevTools `Storage.getCookies` array that belong to
/// `domain` (its own or a parent's), as a JSON array string.
fn filter_cookies(cookies: &[String], domain: &str) -> String {
    let kept: Vec<&str> = cookies
        .iter()
        .map(String::as_str)
        .filter(|cookie| {
            json_string_value(cookie, "domain")
                .map(|d| domain_matches_cookie(domain, &d))
                .unwrap_or(false)
        })
        .collect();
    format!("[{}]", kept.join(","))
}

/// `host` is `domain` itself or under it, in either direction of
/// sub-domain: the page a capture reads must live where the caller claims.
fn host_covers(host: &str, domain: &str) -> bool {
    let h = host.trim_start_matches("www.");
    let d = domain.trim_start_matches("www.");
    h == d || h.ends_with(&format!(".{d}")) || d.ends_with(&format!(".{h}"))
}

/// The otpauth issuer should visibly belong to the site showing the QR: a
/// label of the host contains the issuer (or the other way round). An empty
/// issuer cannot contradict anything.
fn issuer_matches_host(issuer: &str, host: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let issuer = norm(issuer);
    if issuer.is_empty() {
        return true;
    }
    // Match against the meaningful labels only: the TLD (or a public
    // suffix piece like `co`) never counts, and containment needs at least
    // three characters on both sides so `co`/`app` cannot vouch for a seed.
    let labels: Vec<&str> = host.split('.').collect();
    let meaningful = &labels[..labels.len().saturating_sub(1)];
    meaningful.iter().any(|label| {
        let label = norm(label);
        !label.is_empty()
            && (label == issuer
                || (label.len() >= 3
                    && issuer.len() >= 3
                    && (label.contains(&issuer) || issuer.contains(&label))))
    })
}

/// A UI-only POST that carries an Origin header must name this host. The
/// SameSite=Lax cookie keeps cross-site posts from authenticating; this
/// refuses the ones a lax client still sends.
fn origin_ok(head: &str) -> bool {
    let Some(origin) = header_value(head, "origin") else {
        return true;
    };
    let host = header_value(head, "host").unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    url_host(origin) == host
}

/// `.example.com` / `example.com` cover `app.example.com` and itself.
fn domain_matches_cookie(host: &str, cookie_domain: &str) -> bool {
    let d = cookie_domain.trim_start_matches('.');
    let h = host.trim_start_matches("www.");
    h == d || h.ends_with(&format!(".{d}"))
}

/// The host of a URL, lower-cased, without port, path or credentials.
pub fn url_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or("");
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    host.rsplit_once(':')
        .map_or(host, |(h, _)| h)
        .to_ascii_lowercase()
}

/// Tabs the CLI created for an invocation, with the pid that owns them.
///
/// A tab is never closed out from under a live owner: collection needs both
/// a dead pid and a long idle time. This is what keeps two agents working in
/// the same daemon from touching each other's pages.
pub struct TabLease {
    pub owner_pid: u32,
    pub used: Instant,
}

/// Idle age after which an orphaned (owner-dead) tab is collected.
const TAB_GC_IDLE: Duration = Duration::from_secs(10 * 60);

fn tab_leases() -> &'static Mutex<HashMap<String, TabLease>> {
    static LEASES: OnceLock<Mutex<HashMap<String, TabLease>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_tab_lease(id: &str, owner_pid: u32) {
    tab_leases()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id.to_string(),
            TabLease {
                owner_pid,
                used: Instant::now(),
            },
        );
}

/// The tab a session works in: the one it had, or a new one.
///
/// `fresh` asks for a new one (the `--new-tab` flag). The owner is the
/// process the tab belongs to: a session's tab is collected when that
/// process is gone.
fn session_tab(
    state: &AppState,
    owner: Option<u32>,
    fresh: bool,
    url: &str,
) -> Result<(String, bool), String> {
    if !fresh
        && let Some(pid) = owner
        && let Some(id) = leased_tab_of(state.cdp_port, pid)
    {
        touch_tab_lease(&id);
        return Ok((id, true));
    }
    let reply = browser_command(
        state.cdp_port,
        "Target.createTarget",
        &format!("{{\"url\":{},\"background\":true}}", json_string(url)),
    )?;
    let id = json_string_value(&reply, "targetId").unwrap_or_default();
    if id.is_empty() {
        return Err(json_string_value(&reply, "error").unwrap_or(reply));
    }
    if let Some(pid) = owner {
        register_tab_lease(&id, pid);
    }
    Ok((id, false))
}

/// The live tab a session already works in, if any: the most recently used
/// one leased to `owner_pid`. A leased tab the browser no longer lists (a
/// popup closed by hand, a crashed renderer) is dropped here instead of being
/// handed back.
fn leased_tab_of(cdp_port: u16, owner_pid: u32) -> Option<String> {
    let live: Vec<String> = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")
        .map(|discovery| {
            json_objects(&discovery)
                .into_iter()
                .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
                .filter_map(|o| json_string_value(&o, "id"))
                .collect()
        })
        .unwrap_or_default();
    if !pid_alive(owner_pid) {
        return None;
    }
    let mut leases = tab_leases().lock().unwrap_or_else(|e| e.into_inner());
    leases.retain(|id, lease| lease.owner_pid != owner_pid || live.iter().any(|open| open == id));
    leases
        .iter()
        .filter(|(_, lease)| lease.owner_pid == owner_pid)
        .max_by_key(|(_, lease)| lease.used)
        .map(|(id, _)| id.clone())
}

fn touch_tab_lease(id: &str) {
    if let Some(lease) = tab_leases()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(id)
    {
        lease.used = Instant::now();
    }
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// The daemon's background collector: every minute, close tabs whose owning
/// CLI process is gone and that have been idle past [`TAB_GC_IDLE`]. Tabs of
/// live owners and tabs someone is still using are never touched; a tab the
/// browser already closed just leaves the registry.
pub fn spawn_tab_gc(cdp_port: u16) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(60));
            let leases = tab_leases().lock().unwrap_or_else(|e| e.into_inner());
            let stale: Vec<String> = leases
                .iter()
                .filter(|(_, lease)| {
                    !pid_alive(lease.owner_pid) && lease.used.elapsed() > TAB_GC_IDLE
                })
                .map(|(id, _)| id.clone())
                .collect();
            drop(leases);
            for id in stale {
                match close_tab(cdp_port, &id) {
                    Ok(()) => eprintln!("cu: collected orphaned tab {id}"),
                    Err(e) if e.contains("no such tab") => {}
                    Err(e) => {
                        eprintln!("cu: could not collect tab {id}: {e}");
                        continue;
                    }
                }
                tab_leases()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
        }
    });
}

/// `POST /v1/navigate` with the vault considered: a saved session for the
/// domain is injected before the page loads (cookies) and settled after it
/// (localStorage, then one reload), unless the caller asked for a clean
/// page. The reply gains a `"vault"` note saying what was found — so an
/// agent navigating to a login it has credentials for hears about them
/// without the vault leaking anything into the page itself.
fn navigate_with_vault(
    state: &AppState,
    tab: &Tab,
    url: &str,
    inject: bool,
    auth_enabled: bool,
) -> (&'static str, &'static str, String) {
    crate::console::ensure(tab);
    let host = url_host(url);
    let vault = crate::vault::Vault::open(&state.data_dir).ok();
    let (mut injected, mut session_hint, mut credentials) = (false, "none", 0usize);
    let mut local_storage = String::new();
    // The basic-auth pair the vault holds for this host, if the caller did
    // not turn the responder off: handed to the navigation, which answers an
    // HTTP auth challenge with it instead of leaving the headless browser on
    // a dialog it cannot see.
    let mut auth = None;
    if let Some(vault) = vault.as_ref()
        && !host.is_empty()
    {
        let session = vault.get_session(&host).ok().flatten();
        credentials = vault.count_credentials(&host);
        if auth_enabled {
            for (_id, _domain, fields) in vault.credentials_for_domain(&host).unwrap_or_default() {
                if let Some(pair) = crate::vault::basic_auth_pair(&fields) {
                    auth = Some(pair);
                    break;
                }
            }
        }
        if let Some(session) = session {
            session_hint = "saved";
            local_storage = session.local_storage.clone();
            if inject && !session.cookies.trim().is_empty() && session.cookies != "[]" {
                let set = cdp_browser_command(
                    state.cdp_port,
                    "Storage.setCookies",
                    &format!("{{\"cookies\":{}}}", session.cookies),
                );
                injected = set.is_ok();
            }
        }
    }
    let (status, kind, mut body) = navigate_scheduled(tab, url, auth);
    if injected && !local_storage.is_empty() && local_storage != "{}" {
        // localStorage is origin-bound JavaScript state: write it on the
        // loaded page, then reload once so the site boots with it.
        let script = format!(
            "{{\"expression\":\"(() => {{ const o = JSON.parse(atob('{b64}')); for (const [k, v] of Object.entries(o)) localStorage.setItem(k, v); return 'ok' }})()\",\"returnByValue\":true}}",
            b64 = crate::vault::base64_encode(local_storage.as_bytes())
        );
        if tab.command("Runtime.evaluate", &script).is_ok() {
            let _ = tab.command("Page.reload", "{}");
            wait_for_page_on(tab, SETTLE_MAX);
        }
    }
    if body.ends_with('}') {
        let state_word = match (injected, session_hint) {
            (_, "none") => "none",
            (true, _) => "injected",
            (false, _) => "saved-not-injected",
        };
        let note =
            format!(",\"vault\":{{\"session\":\"{state_word}\",\"credentials\":{credentials}}}");
        body.insert_str(body.len() - 1, &note);
    }
    (status, kind, body)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn a_screenshot_can_ask_for_ref_labels_and_qa_marks() {
        let options = ShotOptions::parse("annotate=e1%2Ce4&marks=1").expect("parses");
        assert_eq!(
            options.annotate,
            Some(vec!["e1".to_string(), "e4".to_string()])
        );
        assert!(options.marks);
        let all = ShotOptions::parse("annotate=all").expect("parses");
        assert_eq!(all.annotate, Some(vec!["all".to_string()]));
        assert!(!all.marks);
        let plain = ShotOptions::parse("").expect("parses");
        assert_eq!(plain.annotate, None);
        let bad = ShotOptions::parse("annotate=e1,button").unwrap_err();
        assert!(bad.contains("got button"), "{bad}");
    }

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
        assert!(json_string_value(&json_string(password), "unused").is_none());
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
            vault_host: None,
            daemon: false,
            data_dir: std::env::temp_dir().join("cu-login-test"),
            token: "t".into(),
            cdp_port: 1,
            browser: BrowserState::new(),
            public_host: None,
        };
        let page = login_submit("username=&password=", &state);
        assert!(!page.contains("Login received"));
        assert!(page.contains("Try again"));
    }

    #[test]
    fn a_session_cannot_be_read_out_of_the_login_response() {
        let state = AppState {
            vault_host: None,
            daemon: false,
            data_dir: std::env::temp_dir().join("cu-login-test"),
            token: "t".into(),
            cdp_port: 1,
            browser: BrowserState::new(),
            public_host: None,
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

    #[test]
    fn vault_ui_only_paths_are_the_decisions() {
        assert!(is_vault_ui_only("/v1/vault/requests/7/approve"));
        assert!(is_vault_ui_only("/v1/vault/requests/7/deny"));
        assert!(!is_vault_ui_only("/v1/vault/requests/7"));
        assert!(!is_vault_ui_only("/v1/vault/requests"));
        assert!(!is_vault_ui_only("/v1/vault/credentials"));
    }

    #[test]
    fn fetch_enable_is_scoped_to_the_host() {
        let params = fetch_enable_params("example.com");
        assert!(params.contains("\"handleAuthRequests\":true"));
        // Bare patterns hit default-port URLs; `:*` wildcards the rest.
        assert!(params.contains("\"https://example.com/*\""));
        assert!(params.contains("\"https://example.com:*/*\""));
        assert!(params.contains("\"https://*.example.com/*\""));
        assert!(params.contains("\"https://*.example.com:*/*\""));
        assert!(params.contains("\"http://example.com:*/*\""));
        assert!(!params.contains("other.com"));
    }

    #[test]
    fn auth_challenge_answers_carry_the_pair_or_cancel() {
        let auth = Some(("jaime".into(), "p@ss\"word".into()));
        let params = continue_with_auth_params("42", &auth);
        // Current Chrome wants the answer nested in `authChallengeResponse`.
        assert!(params.contains("\"authChallengeResponse\":{\"response\":\"ProvideCredentials\""));
        assert!(params.contains("\"requestId\":\"42\""));
        // The pair is JSON-escaped: a quote in a password must survive.
        assert!(params.contains("\"p@ss\\\"word\""));
        assert_eq!(
            continue_with_auth_params("42", &None),
            "{\"requestId\":\"42\",\"authChallengeResponse\":{\"response\":\"CancelAuth\"}}"
        );
    }

    #[test]
    fn interception_events_are_told_apart() {
        let challenge = r#"{"method":"Fetch.authRequired","params":{"requestId":"interception-job-1.0","request":{"url":"https://example.com/"}},"sessionId":"s"}"#;
        let paused = r#"{"method":"Fetch.requestPaused","params":{"requestId":"interception-job-2.0","request":{"url":"https://example.com/x.css"}}}"#;
        let network = r#"{"method":"Network.responseReceived","params":{"requestId":"abc"}}"#;
        assert_eq!(
            auth_required_id(challenge).as_deref(),
            Some("interception-job-1.0")
        );
        assert_eq!(auth_required_id(paused), None);
        assert_eq!(
            paused_request_id(paused).as_deref(),
            Some("interception-job-2.0")
        );
        assert_eq!(paused_request_id(challenge), None);
        assert_eq!(auth_required_id(network), None);
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
            assert!(
                crate::policy::FAST_TEST_ARGS.contains(&flag),
                "{flag} missing"
            );
        }
        // Software WebGL is opt-in (CU_WEBGL / --webgl); the default stays GPU-less.
        for flag in crate::policy::WEBGL_ARGS {
            assert!(
                !crate::policy::FAST_TEST_ARGS.contains(flag),
                "{flag} must not be in the default launch"
            );
        }
        // One --disable-features: Chromium only honours the last one it sees.
        let features: Vec<_> = crate::policy::FAST_TEST_ARGS
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

    /// The script that installs cu's helpers in a frame and walks the page.
    ///
    /// It is sent once per document: the registry it installs is guarded
    /// against a second install, and the walk is left behind in
    /// `window.__cuSnapshot` for the next snapshot to call. Sending it again
    /// for every snapshot made the browser compile ten kilobytes of
    /// JavaScript every time, which was most of a snapshot's cost.
    pub fn install_script(base: u64) -> String {
        format!(
            "window.__cuBase={base};{}{}\nwindow.__cuSnapshot()",
            crate::actions::REGISTRY_JS,
            SNAPSHOT_JS
        )
    }

    /// The walk alone, for a document whose helpers are already installed.
    pub fn call_script() -> &'static str {
        "window.__cuSnapshot()"
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
window.__cuSnapshot = function() {
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
};
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
        // Both documents are read in cu's isolated world of their own frame;
        // the context id tells the fake which one is asking.
        let (text, pairs) = walk_frames(&tab, frames, |method, params| match method {
            "Page.createIsolatedWorld" if params.contains("\"frameId\":\"C\"") => {
                Ok(r#"{"result":{"executionContextId":7}}"#.to_string())
            }
            "Page.createIsolatedWorld" => {
                assert!(params.contains("\"frameId\":\"M\""), "entered {params}");
                Ok(r#"{"result":{"executionContextId":5}}"#.to_string())
            }
            "Runtime.evaluate" if params.contains("\"contextId\":7") => Ok(format!(
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
        // The main document's world exists; the child frame's is gone.
        let (text, pairs) = walk_frames(&tab, frames, |method, params| match method {
            "Page.createIsolatedWorld" if params.contains("\"frameId\":\"M\"") => {
                Ok(r#"{"result":{"executionContextId":5}}"#.to_string())
            }
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

#[cfg(test)]
mod vault_session_tests {
    use super::*;

    /// A throwaway data directory per test, so the real `~/.cu/daemon` vault
    /// is never the thing a test writes to.
    fn tmp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "cu-vault-route-{tag}-{}-{}",
            std::process::id(),
            crate::vault::ui_session_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        AppState {
            vault_host: None,
            daemon: false,
            data_dir: dir,
            token: "t".into(),
            cdp_port: 1,
            browser: BrowserState::new(),
            public_host: None,
        }
    }

    fn cookie_header(token: &str) -> String {
        format!("cu_vault_session={token}")
    }

    #[test]
    fn the_session_cookie_is_httponly_secure_and_lax() {
        let cookie = session_cookie("TOKEN", 43200);
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(!cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Max-Age=43200"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        // Clearing must carry the same attributes, or the browser keeps a
        // cookie that no longer matches and the logout does nothing.
        assert!(CLEARED_COOKIE.contains("Max-Age=0"));
        assert!(CLEARED_COOKIE.contains("Secure"));
        assert!(CLEARED_COOKIE.contains("HttpOnly"));
    }

    /// The deny-by-default guarantee that replaces nginx's `auth_basic`: with
    /// that gone, every route below the page/login/session ones must answer
    /// 401 to a browser with no cookie, whatever the method or query.
    #[test]
    fn no_cookie_means_no_vault_data() {
        let state = tmp_state("nocookie");
        let routes = [
            ("GET", "/v1/vault/status"),
            ("GET", "/v1/vault/sessions"),
            ("POST", "/v1/vault/sessions/save"),
            ("DELETE", "/v1/vault/sessions/example.com"),
            ("GET", "/v1/vault/credentials"),
            ("POST", "/v1/vault/credentials"),
            ("GET", "/v1/vault/credentials/1"),
            ("DELETE", "/v1/vault/credentials/1"),
            ("POST", "/v1/vault/passwd"),
            ("GET", "/v1/vault/requests"),
            ("POST", "/v1/vault/requests"),
            ("GET", "/v1/vault/requests/1"),
            ("POST", "/v1/vault/requests/1/approve"),
            ("POST", "/v1/vault/requests/1/deny"),
        ];
        for (method, path) in routes {
            let (status, _, body, _) = vault_route(method, path, "{}", &state, None);
            assert_eq!(
                status, "401 Unauthorized",
                "{method} {path} answered {body}"
            );
            assert!(body.contains("log in first"), "{method} {path}: {body}");
        }
        // A junk cookie is no better than none.
        for cookie in ["cu_vault_session=", "cu_vault_session=bogus", "other=1"] {
            let (status, _, _, _) =
                vault_route("GET", "/v1/vault/credentials", "{}", &state, Some(cookie));
            assert_eq!(status, "401 Unauthorized", "cookie {cookie:?}");
        }
    }

    #[test]
    fn the_pages_and_the_session_probe_stay_open_but_leak_nothing() {
        let state = tmp_state("open");
        // The login page and the app shell are HTML with no data in them; the
        // login page needs them to render at all.
        for (method, path) in [("GET", "/vault"), ("GET", "/vault/app")] {
            let (status, _, body, _) = vault_route(method, path, "", &state, None);
            assert_eq!(status, "200 OK", "{path}");
            assert!(!body.contains("hunter2"), "{path} must carry no data");
        }
        // `/v1/vault/ui/state` is read before any session exists (it decides
        // first-run vs login) and says only whether a password is set.
        let (status, _, body, _) = vault_route("GET", "/v1/vault/ui/state", "", &state, None);
        assert_eq!(status, "200 OK");
        assert!(body.contains("has_password"), "{body}");
        // `/v1/vault/ui/session` says whether *this* cookie is live, and
        // nothing about anyone else's.
        let (status, _, body, _) = vault_route("GET", "/v1/vault/ui/session", "", &state, None);
        assert_eq!(status, "200 OK");
        assert!(body.contains("\"authenticated\":false"), "{body}");
        assert!(!body.contains("remember"), "{body}");
    }

    #[test]
    fn logging_in_sets_a_cookie_and_logging_out_clears_it() {
        let state = tmp_state("login");
        let vault = crate::vault::Vault::open(&state.data_dir).expect("vault");
        vault.set_password(None, "correct horse").unwrap();
        let (status, _, body, _) = vault_route(
            "POST",
            "/v1/vault/ui/login",
            r#"{"password":"correct horse","remember":true}"#,
            &state,
            None,
        );
        assert_eq!(status, "200 OK", "{body}");
        let cookie = vault_route(
            "POST",
            "/v1/vault/ui/login",
            r#"{"password":"correct horse","remember":true}"#,
            &state,
            None,
        )
        .3
        .expect("cookie");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        let token = cookie
            .split(';')
            .next()
            .and_then(|c| c.split_once('='))
            .map(|(_, t)| t.to_string())
            .expect("token");
        // A remembered login gets the long Max-Age, and reads data back.
        let max_age: i64 = cookie
            .split("Max-Age=")
            .nth(1)
            .and_then(|v| v.split(';').next())
            .and_then(|v| v.trim().parse().ok())
            .expect("max-age");
        assert!(
            max_age > crate::vault::UI_SESSION_TTL,
            "remember got {max_age}s"
        );
        let (status, _, _, _) = vault_route(
            "GET",
            "/v1/vault/credentials",
            "{}",
            &state,
            Some(&cookie_header(&token)),
        );
        assert_eq!(status, "200 OK");
        // Logout ends it.
        let (_, _, _, cleared) = vault_route(
            "POST",
            "/v1/vault/ui/logout",
            "{}",
            &state,
            Some(&cookie_header(&token)),
        );
        assert_eq!(cleared.as_deref(), Some(CLEARED_COOKIE));
        let (status, _, _, _) = vault_route(
            "GET",
            "/v1/vault/credentials",
            "{}",
            &state,
            Some(&cookie_header(&token)),
        );
        assert_eq!(status, "401 Unauthorized", "a logged-out cookie is dead");
    }

    #[test]
    fn an_authenticated_request_renews_the_cookie_and_an_unanswered_one_does_not() {
        let state = tmp_state("renew");
        let vault = crate::vault::Vault::open(&state.data_dir).expect("vault");
        vault.set_password(None, "pw").unwrap();
        let token = vault.ui_session_create(true).expect("session");
        let head = cookie_header(&token);
        let (status, _, _, cookie) =
            vault_route("GET", "/v1/vault/credentials", "{}", &state, Some(&head));
        assert_eq!(status, "200 OK");
        let cookie = cookie.expect("an authenticated request re-issues the cookie");
        assert!(cookie.contains(&token), "the same token, not a new one");
        assert!(cookie.contains("Secure") && cookie.contains("SameSite=Lax"));
        let max_age: i64 = cookie
            .split("Max-Age=")
            .nth(1)
            .and_then(|v| v.split(';').next())
            .and_then(|v| v.trim().parse().ok())
            .expect("max-age");
        // Sliding: the reply hands back a full lifetime, not the remainder.
        assert!(
            max_age > crate::vault::UI_SESSION_TTL_REMEMBER - 120,
            "renewed cookie had {max_age}s left, expected about a month"
        );
    }
}

#[cfg(test)]
mod login_page_tests {
    use super::*;

    /// The iOS-Safari zoom trap is silent and invisible in a screenshot: the
    /// zoom only shows on a real device. Asserting the rule in the served
    /// bytes is the only check that catches a regression here.
    #[test]
    fn every_login_input_is_sixteen_pixels() {
        for (page, name) in [(LOGIN_PAGE, "login"), (APP_PAGE, "app")] {
            assert!(
                page.contains("font-size: max(16px, 1em)"),
                "{name} page lost the 16px input floor: a zoomed iOS viewport \
                 that never zooms back"
            );
            // The old offenders, spelled out so a reintroduction is obvious.
            assert!(
                !page.contains("font-size: 0.95rem"),
                "{name} page went back to 15.2px inputs"
            );
            assert!(
                !page.contains("font-size: 1.2rem"),
                "{name} page went back to a rem-sized input"
            );
        }
    }

    #[test]
    fn the_login_form_meets_the_touch_target_floor() {
        // 44px is the smallest reliably tappable target on a phone; the card's
        // fields, its button and the remember row all carry it.
        for needle in ["min-height: 44px"] {
            assert!(
                LOGIN_PAGE.contains(needle),
                "login page lost its touch target floor ({needle})"
            );
        }
        assert!(LOGIN_PAGE.contains(".remember {"), "remember row styled");
        assert!(
            LOGIN_PAGE.contains("id=\"remember\""),
            "no remember checkbox in the form"
        );
        assert!(
            LOGIN_PAGE.contains("remember: $('remember').checked"),
            "the checkbox never reaches the server, so it would do nothing"
        );
    }

    #[test]
    fn the_login_page_respects_the_colour_scheme_and_focus() {
        assert!(
            LOGIN_PAGE.contains("prefers-color-scheme: dark"),
            "login page is dark-only"
        );
        assert!(
            LOGIN_PAGE.contains(":focus-visible"),
            "no visible focus ring: keyboard users cannot see where they are"
        );
        assert!(
            LOGIN_PAGE.contains("aria-live"),
            "the error state is not announced"
        );
        assert!(
            LOGIN_PAGE.contains("<label for=\"password\">"),
            "the field has a placeholder but no label"
        );
    }
}
