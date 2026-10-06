//! Integration tests: drive the real loopback server end to end.
//!
//! The daemon used to answer from a single fixed-size `read()`, so a client
//! that wrote its request in more than one TCP segment was cut off mid-request.
//! These tests reproduce that pattern on purpose by splitting every request
//! into small fragments and pausing between them.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use cu::server::{self, AppState};

/// Writes `data` to `addr` in chunks with a pause in between, so the request
/// arrives in several TCP segments, then reads the whole response.
fn send_fragmented(
    addr: &str,
    data: &[u8],
    pieces: usize,
    pause: Duration,
) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    let size = (data.len() / pieces).max(1);
    for chunk in data.chunks(size) {
        stream
            .write_all(chunk)
            .map_err(|e| format!("write failed: {e}"))?;
        stream.flush().map_err(|e| format!("flush failed: {e}"))?;
        std::thread::sleep(pause);
    }
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(response)
}

fn status_code(response: &str) -> String {
    response
        .lines()
        .next()
        .unwrap_or("")
        .split(' ')
        .nth(1)
        .unwrap_or("")
        .to_string()
}

fn body_of(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

/// A running server: its own loopback port, data dir and fake Chromium, so the
/// tests never interfere with each other or with a real session.
struct TestServer {
    addr: String,
    token: String,
    data_dir: PathBuf,
    browser: Option<Child>,
}

impl TestServer {
    fn start(name: &str, with_browser: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr").to_string();
        let data_dir = std::env::temp_dir().join(format!("cu-it-{name}-{}", std::process::id()));
        let _ = remove_dir(&data_dir);
        std::fs::create_dir_all(data_dir.join("profiles/default")).expect("profile dir");
        std::fs::create_dir_all(data_dir.join("sessions")).expect("sessions dir");
        let token = server::random_token();
        let cdp_port = if with_browser { free_port() } else { 9222 };
        let browser = if with_browser {
            Some(spawn_fake_chromium(cdp_port).expect("spawn fake chromium"))
        } else {
            None
        };
        if with_browser {
            wait_for_port(cdp_port);
        }
        let state = Arc::new(AppState {
            data_dir: data_dir.clone(),
            token: token.clone(),
            cdp_port,
        });
        std::thread::spawn(move || server::serve(state, listener));
        Self {
            addr,
            token,
            data_dir,
            browser,
        }
    }

    /// Sends a request split into 8-byte fragments: the pattern that used to
    /// make the server reply before the client had finished sending.
    fn fragmented(&self, request: &str) -> String {
        send_fragmented(
            &self.addr,
            request.as_bytes(),
            request.len().max(2),
            Duration::from_millis(10),
        )
        .expect("fragmented request should get a response")
    }

    /// A JSON POST in four fragments, returning the response body.
    fn post(&self, path: &str, body: &str) -> String {
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.token,
            body.len()
        );
        send_fragmented(&self.addr, request.as_bytes(), 4, Duration::from_millis(5))
            .expect("POST should get a response")
    }

    fn get(&self, path: &str) -> String {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
            self.token
        );
        send_fragmented(&self.addr, request.as_bytes(), 4, Duration::from_millis(5))
            .expect("GET should get a response")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.browser {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = remove_dir(&self.data_dir);
    }
}

fn remove_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
    } else {
        Ok(())
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind for port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Spawns the fake browser: HTTP/CDP discovery on `port`, WebSocket on `port + 1`.
fn spawn_fake_chromium(port: u16) -> std::io::Result<Child> {
    Command::new("python3")
        .arg(env!("CARGO_MANIFEST_DIR").to_owned() + "/tests/fake_chromium.py")
        .arg(port.to_string())
        .arg((port + 1).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn wait_for_port(port: u16) {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("fake chromium never listened on {port}");
}

#[test]
fn status_answers_a_request_sent_in_fragments() {
    let server = TestServer::start("status", false);
    let response = server.fragmented(&format!(
        "GET /v1/status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    ));
    assert_eq!(status_code(&response), "200");
    assert_eq!(
        body_of(&response),
        "{\"running\":true,\"browser\":\"chromium\"}"
    );
}

#[test]
fn status_answers_a_one_byte_at_a_time_request() {
    let server = TestServer::start("status-byte", false);
    let request = format!(
        "GET /v1/status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    let response = send_fragmented(
        &server.addr,
        request.as_bytes(),
        1,
        Duration::from_millis(1),
    )
    .expect("byte-at-a-time request should get a response");
    assert_eq!(status_code(&response), "200");
}

#[test]
fn a_fragmented_body_is_reassembled_before_routing() {
    let server = TestServer::start("body", false);
    // The URL is spread across the last fragments, so the server must have read
    // the whole body before it could parse it.
    let body = format!("{{\"url\":\"https://example.test/{}\"}}", "x".repeat(300));
    let request = format!(
        "POST /v1/navigate HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
        server.token,
        body.len()
    );
    let response = send_fragmented(
        &server.addr,
        request.as_bytes(),
        40,
        Duration::from_millis(5),
    )
    .expect("fragmented POST should get a response");
    assert_eq!(status_code(&response), "502");
    assert!(
        body_of(&response).contains("error"),
        "expected a CDP error, got {response}"
    );
}

#[test]
fn navigate_returns_the_cdp_result() {
    let server = TestServer::start("navigate", true);
    let response = server.post("/v1/navigate", "{\"url\":\"https://example.test\"}");
    assert_eq!(status_code(&response), "200");
    let body = body_of(&response);
    assert!(
        body.contains("\"url\":\"https://example.test\""),
        "got {body}"
    );
}

#[test]
fn navigate_without_a_url_is_rejected() {
    let server = TestServer::start("navigate-empty", true);
    let response = server.post("/v1/navigate", "{\"url\":\"\"}");
    assert_eq!(status_code(&response), "400");
    assert_eq!(body_of(&response), "{\"error\":\"url is required\"}");
}

#[test]
fn screenshot_returns_a_png() {
    let server = TestServer::start("shot", true);
    let response = server.get("/v1/screenshot");
    assert_eq!(status_code(&response), "200");
    let body = body_of(&response);
    let encoded = body
        .trim_start_matches("{\"png_base64\":\"")
        .trim_end_matches("\"}")
        .to_string();
    let decoded = server::decode_base64(&encoded).expect("valid base64");
    assert!(decoded.starts_with(b"\x89PNG\r\n\x1a\n"), "got {decoded:?}");
}

#[test]
fn a_request_without_a_token_is_unauthorized() {
    let server = TestServer::start("auth", false);
    let response = server
        .fragmented("GET /v1/status HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(status_code(&response), "401");
}

#[test]
fn an_unknown_path_is_not_found() {
    let server = TestServer::start("notfound", false);
    let response = server.get("/v1/nope");
    assert_eq!(status_code(&response), "404");
}

#[test]
fn the_login_page_is_served_without_a_token() {
    let server = TestServer::start("login", false);
    let response = server.fragmented("GET /login HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(status_code(&response), "200");
    assert!(body_of(&response).contains("type=password"));
}
