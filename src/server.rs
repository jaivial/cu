//! Loopback HTTP server for `cu`: request parsing, routing and the browser bridge.
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub token: String,
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

pub fn launch_browser(data: &Path) -> Result<Child, String> {
    let binary = env::var("CU_BROWSER").unwrap_or_else(|_| "chromium".into());
    Command::new(binary)
        .args([
            "--remote-debugging-port=9222",
            "--remote-allow-origins=*",
            "--no-first-run",
            "--no-default-browser-check",
            "--user-data-dir",
        ])
        .arg(data.join("profiles/default"))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start Chromium (set CU_BROWSER): {e}"))
}

pub fn handle(mut stream: TcpStream, state: Arc<AppState>) {
    let mut buf = [0; 16384];
    let Ok(size) = stream.read(&mut buf) else {
        return;
    };
    let request = String::from_utf8_lossy(&buf[..size]);
    let mut first = request.lines().next().unwrap_or("").split_whitespace();let method = first.next().unwrap_or("");
    let path = first.next().unwrap_or("");
    let authorized = request
        .lines()
        .find_map(|l| l.strip_prefix("Authorization: Bearer "))
        .map(|v| v.trim() == state.token)
        .unwrap_or(false);
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
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
            match cdp_command("Page.captureScreenshot", "{\"format\":\"png\"}") {
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

pub fn cdp_command(method: &str, params: &str) -> Result<String, String> {
    let target = http_get("127.0.0.1:9222", "/json")?;
    let ws = target
        .split("\"webSocketDebuggerUrl\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
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
    let mut header = [0; 2];
    stream.read_exact(&mut header).map_err(|e| e.to_string())?;
    let mut len = (header[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0; 2];
        stream.read_exact(&mut b).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(b) as usize;
    }
    if len == 127 {
        return Err("CDP response is too large".into());
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    String::from_utf8(payload).map_err(|e| e.to_string())
}
pub fn http_get(address: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    Ok(response.split("\r\n\r\n").nth(1).unwrap_or("").into())
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
