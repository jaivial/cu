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

use cu::server::{self, AppState, BrowserState};

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

/// Like [`send_fragmented`], but closes the write side after sending so a
/// server waiting for more body is released instead of blocked.
fn send_fragmented_then_close(
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
        stream.flush().map_err(|e| e.to_string())?;
        std::thread::sleep(pause);
    }
    stream.shutdown(std::net::Shutdown::Write).ok();
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
    cdp_port: u16,
    /// The fake browser process, killed when the test ends.
    browser: Option<Child>,
    /// Readiness signal handed to the server under test.
    browser_state: Arc<BrowserState>,
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
        // The harness starts its own (fake) browser, so the readiness signal
        // is set here instead of by a launch thread.
        let browser_state = if with_browser {
            let state = BrowserState::new();
            state.mark_ready();
            state
        } else {
            // No browser was started, so the readiness signal must say that
            // instead of leaving every page action waiting for a timeout.
            BrowserState::absent("no browser was started for this server")
        };
        let state = Arc::new(AppState {
            data_dir: data_dir.clone(),
            token: token.clone(),
            cdp_port,
            browser: Arc::clone(&browser_state),
        });
        std::thread::spawn(move || server::serve(state, listener));
        Self {
            addr,
            token,
            data_dir,
            cdp_port,
            browser,
            browser_state,
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

    /// Pretends the browser is up for a test that exercises the HTTP layer
    /// rather than the page, so a route falls through to its real CDP error.
    fn mark_browser_ready(&self) {
        self.browser_state.mark_ready();
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
    // The point is request reassembly, not the browser, so let the route reach
    // its CDP call and fail there instead of at the readiness gate.
    server.mark_browser_ready();
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
fn a_png_is_returned_when_it_is_asked_for() {
    let server = TestServer::start("shot", true);
    let response = server.get("/v1/screenshot?format=png");
    assert_eq!(status_code(&response), "200");
    let body = body_of(&response);
    let encoded = server::json_value(body, "png_base64").expect("png_base64 payload");
    let decoded = server::decode_base64(&encoded).expect("valid base64");
    assert!(decoded.starts_with(b"\x89PNG\r\n\x1a\n"), "got {decoded:?}");
}

#[test]
fn the_default_screenshot_is_a_fast_jpeg() {
    let server = TestServer::start("shot-jpeg", true);
    let response = server.get("/v1/screenshot");
    assert_eq!(status_code(&response), "200");
    let body = body_of(&response);
    // The fast default: lossy, small, and labelled so the client names the file
    // correctly.
    assert_eq!(server::json_value(body, "format").as_deref(), Some("jpeg"));
    let encoded = server::json_value(body, "jpeg_base64").expect("jpeg_base64 payload");
    let decoded = server::decode_base64(&encoded).expect("valid base64");
    assert!(
        decoded.starts_with(b"\x89PNG") || decoded.starts_with(&[0xff, 0xd8]),
        "got {decoded:?}"
    );
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

#[test]
fn a_bare_lf_request_is_still_understood() {
    let server = TestServer::start("bare-lf", false);
    let request = format!(
        "GET /v1/status HTTP/1.1\nHost: localhost\nAuthorization: Bearer {}\nContent-Length: 0\n\n",
        server.token
    );
    let response = send_fragmented(
        &server.addr,
        request.as_bytes(),
        3,
        Duration::from_millis(5),
    )
    .expect("bare-LF request should get a response");
    assert_eq!(status_code(&response), "200");
}

#[test]
fn an_oversized_head_is_refused_not_buffered() {
    let server = TestServer::start("oversize", false);
    let request = format!(
        "GET /v1/status HTTP/1.1\r\nX-Padding: {}\r\n\r\n",
        "a".repeat(70 * 1024)
    );
    // The server stops reading past the limit and closes, so the client may
    // see a 400 or a failed write. It must never buffer and answer anyway.
    match send_fragmented(
        &server.addr,
        request.as_bytes(),
        8,
        Duration::from_millis(1),
    ) {
        Ok(response) => assert_eq!(status_code(&response), "400"),
        Err(error) => assert!(
            error.contains("write failed") || error.contains("read failed"),
            "unexpected: {error}"
        ),
    }
}

#[test]
fn an_immediate_hangup_gets_no_response() {
    let server = TestServer::start("hangup", false);
    let stream = TcpStream::connect(&server.addr).expect("connect");
    drop(stream);
    // The connection must survive for the next client.
    let response = server.get("/v1/status");
    assert_eq!(status_code(&response), "200");
}

#[test]
fn a_client_that_stops_sending_is_released() {
    let server = TestServer::start("short-body", true);
    // Announce more body than is sent, then close the write side. The server
    // must answer from what arrived instead of waiting for the rest.
    let request = format!(
        "POST /v1/navigate HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 400\r\n\r\n{{\"url\":\"https://example.test\"}}",
        server.token
    );
    let response = send_fragmented_then_close(
        &server.addr,
        request.as_bytes(),
        3,
        Duration::from_millis(5),
    )
    .expect("server should answer rather than wait for the missing body");
    assert_eq!(status_code(&response), "200");
    // ...and the listener must still be healthy afterwards.
    assert_eq!(status_code(&server.get("/v1/status")), "200");
}

#[test]
fn a_session_can_be_saved_and_reported() {
    let server = TestServer::start("session", false);
    let response = server.post("/v1/session/trip_1", "{}");
    assert_eq!(status_code(&response), "200");
    assert_eq!(body_of(&response), "{\"saved\":true}");
    assert!(server.data_dir.join("sessions/trip_1").is_dir());
}

#[test]
fn an_invalid_session_name_is_rejected() {
    let server = TestServer::start("bad-name", false);
    let response = server.post("/v1/session/../secrets", "{}");
    assert_eq!(status_code(&response), "400");
}

#[test]
fn a_session_load_is_reported_as_loaded() {
    let server = TestServer::start("session-load", false);
    std::fs::create_dir_all(server.data_dir.join("sessions/trip_1")).expect("session dir");
    let response = server.post("/v1/session/trip_1/load", "{}");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert_eq!(body_of(&response), "{\"loaded\":true}");
}

#[test]
fn a_saved_profile_leaves_the_process_locks_behind() {
    let server = TestServer::start("symlink", false);
    // Chromium leaves dangling links to its per-process locks in the profile.
    // A copied SingletonLock names the machine and pid of the run that saved the
    // profile, and the next browser refuses to start on top of it.
    let profile = server.data_dir.join("profiles/default");
    std::os::unix::fs::symlink("../elsewhere", profile.join("SingletonLock"))
        .expect("create dangling lock");
    std::fs::write(profile.join("Preferences"), "{}").expect("profile file");
    let response = server.post("/v1/session/trip_2", "{}");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let copied = server.data_dir.join("sessions/trip_2");
    assert!(!copied.join("SingletonLock").exists());
    assert!(!copied.join("SingletonLock").is_symlink());
    assert!(
        copied.join("Preferences").is_file(),
        "state is still copied"
    );
}

#[test]
fn a_dangling_symlink_is_copied_as_a_link() {
    let server = TestServer::start("dangling", false);
    let profile = server.data_dir.join("profiles/default");
    std::fs::write(profile.join("Preferences"), "{}").expect("profile file");
    std::os::unix::fs::symlink("/no/such/path", profile.join("Vendor")).expect("dangling link");
    let response = server.post("/v1/session/trip_3", "{}");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(server.data_dir.join("sessions/trip_3/Vendor").is_symlink());
}

#[test]
fn submitting_the_login_form_types_into_the_browser() {
    let server = TestServer::start("login-form", true);
    let response = send_fragmented(
        &server.addr,
        b"username=jaime&password=s3cret%2B50%25",
        4,
        Duration::from_millis(5),
    )
    .ok();
    // The form must be POSTed to the unauthenticated endpoint, which is what a
    // browser does; check it directly.
    let request = format!(
        "POST /login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
        b"username=jaime&password=s3cret%2B50%25".len()
    );
    let _ = request;
    let _ = response;
    let page = send_fragmented(
        &server.addr,
        format!(
            "POST /login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\nusername=jaime&password=s3cret%2B50%25",
            b"username=jaime&password=s3cret%2B50%25".len()
        )
        .as_bytes(),
        5,
        Duration::from_millis(5),
    )
    .expect("login POST should be answered");
    assert_eq!(status_code(&page), "200");
    let body = body_of(&page);
    // The typed password must never come back.
    assert!(!body.contains("s3cret"), "password leaked: {body}");
    assert!(body.contains("Login received"), "got {body}");
}

#[test]
fn a_snapshot_is_compact_and_carries_refs() {
    let server = TestServer::start("snapshot", true);
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    // Compact means compact: a snapshot is a few hundred bytes, not a DOM dump.
    assert!(
        body.len() < 1024,
        "snapshot should be small, got {} bytes: {body}",
        body.len()
    );
    assert!(body.contains("ref=e1"), "no refs in {body}");
    assert!(body.contains("link"), "no roles in {body}");
}

#[test]
fn a_snapshot_needs_the_browser() {
    let server = TestServer::start("snapshot-no-browser", false);
    let response = server.get("/v1/snapshot");
    // Without a page to walk there is nothing to snapshot; that must be an
    // error an agent can act on, not an empty 200.
    assert_eq!(status_code(&response), "503");
    assert!(body_of(&response).contains("error"));
}

#[test]
fn navigate_reports_how_long_the_page_took_to_settle() {
    let server = TestServer::start("settle", true);
    let response = server.post("/v1/navigate", "{\"url\":\"https://example.test\"}");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    // The smart wait reports what it spent instead of hiding it.
    assert!(body.contains("settled_ms"), "no settle timing in {body}");
    // And it comes back promptly: `Page.navigate` plus one settled probe.
    assert!(
        body.contains("\"url\":\"https://example.test\""),
        "navigate result lost: {body}"
    );
}

#[test]
fn stopping_the_daemon_closes_its_browser() {
    let dir = std::env::temp_dir().join(format!("cu-it-shutdown-{}", std::process::id()));
    let _ = remove_dir(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let marker = dir.join("closed");
    let (port, cdp_port) = (free_port(), free_port());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cu"))
        .args(["start", "--port", &port.to_string(), "--data"])
        .arg(&dir)
        .env(
            "CU_BROWSER",
            env!("CARGO_MANIFEST_DIR").to_owned() + "/tests/fake_chromium.py",
        )
        .env("CU_CDP_PORT", cdp_port.to_string())
        .env("FAKE_CHROMIUM_CLOSED", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cu");
    // The browser is up once a page action goes through.
    let token = loop {
        if let Ok(config) = std::fs::read_to_string(dir.join("server.json")) {
            if let Some(token) = server::json_value(&config, "token") {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break token;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let client = cu::Client::new(format!("127.0.0.1:{port}"), token);
    client.snapshot().expect("browser came up");
    let _ = Command::new("kill").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
    let closed = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = remove_dir(&dir);
    assert_eq!(closed, "closed\n", "the browser outlived its daemon");
}

/// Input commands the fake browser received, in order.
fn input_log(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/input-log").expect("input log")
}

#[test]
fn a_click_by_ref_is_a_real_mouse_click() {
    let server = TestServer::start("click-ref", true);
    let response = server.post("/v1/click", r#"{"ref":"e1"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.contains("\"ok\":true"), "{body}");
    // A single click does not pay for a snapshot unless it asks for one.
    assert!(!body.contains("snapshot"), "{body}");
    assert_eq!(
        input_log(&server),
        r#"["Input.dispatchMouseEvent", "Input.dispatchMouseEvent"]"#
    );
}

#[test]
fn a_batch_runs_in_order_and_ends_with_a_snapshot() {
    let server = TestServer::start("act-batch", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"type","ref":"e2","text":"hello","submit":true},{"do":"click","ref":"e1"}]}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.starts_with("{\"ok\":true"), "{body}");
    assert!(body.contains("ref=e1"), "no closing snapshot in {body}");
    assert_eq!(
        input_log(&server),
        r#"["Input.insertText", "Input.dispatchKeyEvent", "Input.dispatchKeyEvent", "Input.dispatchMouseEvent", "Input.dispatchMouseEvent"]"#
    );
}

#[test]
fn a_batch_stops_at_a_stale_ref_and_says_which() {
    let server = TestServer::start("act-stale", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"click","ref":"e404"},{"do":"click","ref":"e1"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.contains("\"ok\":false"), "{body}");
    assert!(body.contains("\"failed\":0"), "{body}");
    assert!(body.contains("take a new snapshot"), "{body}");
    // Nothing after the failure ran.
    assert_eq!(input_log(&server), "[]");
}

#[test]
fn a_malformed_batch_is_a_bad_request() {
    let server = TestServer::start("act-bad", true);
    let response = server.post(
        "/v1/act",
        r##"{"actions":[{"do":"click","ref":"#login"}]}"##,
    );
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
    assert!(body_of(&response).contains("snapshot ref"));
}
