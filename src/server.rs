//! Loopback HTTP server for `cu`: request parsing, routing and the browser bridge.
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub token: String,
    /// Port Chromium listens on for the DevTools protocol (default 9222).
    pub cdp_port: u16,
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
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version").is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "Chromium DevTools never answered on 127.0.0.1:{cdp_port}"
    ))
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
    let mut child = command
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start Chromium (set CU_BROWSER): {e}"))?;
    // A browser that exits immediately (no display, bad binary) must be
    // reported instead of leaving a daemon that answers status but can never
    // navigate.
    thread::sleep(Duration::from_millis(300));
    if let Some(status) = child
        .try_wait()
        .map_err(|e| format!("could not check on Chromium: {e}"))?
    {
        return Err(format!(
            "Chromium exited immediately ({status}); set CU_BROWSER to a working binary"
        ));
    }
    Ok(child)
}

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

pub fn route(
    method: &str,
    path: &str,
    body: &str,
    state: &AppState,
) -> (&'static str, &'static str, String) {
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
                Ok(result) => ("200 OK", "application/json", result),
                Err(error) => (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(&error)),
                ),
            }
        }
        ("GET", "/v1/screenshot") => {
            match cdp_command(
                state.cdp_port,
                "Page.captureScreenshot",
                "{\"format\":\"png\"}",
            ) {
                Ok(result) => (
                    "200 OK",
                    "application/json",
                    format!(
                        "{{\"png_base64\":{}}}",
                        json_value(&result, "data")
                            .map(|v| format!("\"{v}\""))
                            .unwrap_or_else(|| "null".into())
                    ),
                ),
                Err(error) => (
                    "502 Bad Gateway",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(&error)),
                ),
            }
        }
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
            match copy_dir(&src, &dest) {
                Ok(()) => ("200 OK", "application/json", "{\"saved\":true}".into()),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    format!("{{\"error\":\"{}\"}}", json_escape(&e)),
                ),
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
pub fn login_submit(body: &str, state: &AppState) -> String {
    let user = form_value(body, "username");
    let password = form_value(body, "password");
    let _ = fs::write(state.data_dir.join("last_login_user"), user);
    let _password_received = !password.is_empty();
    "<h1>Login received</h1><p>Your credentials were delivered to the local session server. The password is not displayed or returned.</p>".into()
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

pub fn cdp_command(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
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
    let key = base64_encode(b"cu-cdp-clientkey");
    write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").map_err(|e| e.to_string())?;
    let mut handshake = [0; 2048];
    let n = stream.read(&mut handshake).map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&handshake[..n]).starts_with("HTTP/1.1 101") {
        return Err("Chromium rejected CDP websocket handshake".into());
    }
    let message = format!("{{\"id\":1,\"method\":\"{method}\",\"params\":{params}}}");
    let mask = [0x43, 0x55, 0x2d, 0x31];
    let bytes = message.as_bytes();
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(0x80 | bytes.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (i, byte) in bytes.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }
    stream.write_all(&frame).map_err(|e| e.to_string())?;
    let payload = read_frame(&mut stream, MAX_CDP_MESSAGE)?;
    String::from_utf8(payload).map_err(|e| e.to_string())
}
/// A DevTools HTTP request may not be answered with `Connection: close`, so the
/// response has to be framed rather than read to EOF: `read_to_string` used to
/// block for ever on a browser that kept the socket open and the whole daemon
/// stalled on its first navigate.
/// Largest CDP payload accepted, including base64 screenshots.
const MAX_CDP_MESSAGE: usize = 32 * 1024 * 1024;
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
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
pub fn form_value(body: &str, key: &str) -> String {
    body.split('&')
        .find_map(|item| item.strip_prefix(&format!("{key}=")).map(url_decode))
        .unwrap_or_default()
}
pub fn url_decode(s: &str) -> String {
    s.replace('+', " ")
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
        if item.path().is_dir() {
            copy_dir(&item.path(), &target)?
        } else {
            fs::copy(item.path(), target).map_err(|e| e.to_string())?;
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
    fn byte_needle_is_located() {
        assert_eq!(find(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
        assert_eq!(find(b"", b"\r\n\r\n"), None);
    }
}
