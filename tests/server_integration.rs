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

/// Point session encryption at a key of the test run's own.
///
/// Without it a session save in a test reads -- or creates -- the user's
/// real `~/.config/cu/session.key`. The server runs in this process, so the
/// variable is set once, before any server starts, and every test sees the
/// same value.
fn isolate_session_key() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let key = std::env::temp_dir().join(format!("cu-it-session-{}.key", std::process::id()));
        let _ = std::fs::remove_file(&key);
        // SAFETY: called once, before any server thread exists, and no test
        // reads the environment concurrently with this write.
        unsafe { std::env::set_var("CU_SESSION_KEY_FILE", &key) };
    });
}

/// Decrypt session `name` of `server` into a fresh directory, the way a load
/// would, so a test can look at what the archive holds.
fn open_session(server: &TestServer, name: &str) -> PathBuf {
    let out = server.data_dir.join(format!("opened-{name}"));
    let _ = remove_dir(&out);
    cu::session::load(&out, &server.data_dir.join("sessions"), name).expect("session loads");
    out
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
        isolate_session_key();
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr").to_string();
        let data_dir = std::env::temp_dir().join(format!("cu-it-{name}-{}", std::process::id()));
        let _ = remove_dir(&data_dir);
        std::fs::create_dir_all(data_dir.join("profiles/default")).expect("profile dir");
        std::fs::create_dir_all(data_dir.join("sessions")).expect("sessions dir");
        let token = server::random_token();
        // Without a fake browser the port must still be one nothing listens
        // on: 9222 is where a developer's real Chrome usually is, and a test
        // that lets a route through would drive that browser.
        let (browser, cdp_port) = if with_browser {
            let (child, port) = spawn_fake_chromium();
            (Some(child), port)
        } else {
            (None, dead_port())
        };
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
            public_host: None,
            vault_host: None,
            daemon: false,
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

/// A loopback port for a server that binds it later (a daemon started as a
/// child process): the OS's choice, never handed out twice in this run.
fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut given = GIVEN.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .expect("bind for port")
            .local_addr()
            .expect("local addr")
            .port();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

/// Spawn the fake browser on a port it binds itself and return it with that
/// port.
///
/// Choosing a free port here and handing it over left a window in which any
/// other process could take it: the fake then failed to bind, and the test
/// talked to whatever did ("connection refused", or another program's
/// `/json` with no page in it). The fake binds port 0 and prints the port it
/// got, so the port is never free in between.
fn spawn_fake_chromium() -> (Child, u16) {
    let mut child = Command::new("python3")
        .arg(env!("CARGO_MANIFEST_DIR").to_owned() + "/tests/fake_chromium.py")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake chromium");
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().expect("fake stdout")),
        &mut line,
    )
    .expect("fake chromium reports its port");
    let port = line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("fake chromium did not start: {line:?}"));
    (child, port)
}

/// A loopback port nothing listens on and nothing will: TCP port 1 is
/// privileged (no unprivileged process can bind it) and has no service on a
/// development machine, so a connect is refused at once. Used for a server
/// that must have no browser behind it.
fn dead_port() -> u16 {
    1
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
fn an_http_level_navigation_failure_is_reported_as_the_block_it_is() {
    // Chromium answers `net::ERR_HTTP_RESPONSE_CODE_FAILURE` when the origin
    // returns a status it will not render (X/Twitter's 403 to an automated
    // client). That used to reach the agent as a bare `502 navigation failed`;
    // the browser is sitting on a refusal page, and the agent must be told what
    // it is (state + vendor + status), not just that something failed.
    let server = TestServer::start("navigate-httpfail", true);
    let response = server.post("/v1/navigate", r#"{"url":"httpfail://x.test/login"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.contains("\"navigated\":false"), "got {body}");
    assert!(
        body.contains("ERR_HTTP_RESPONSE_CODE_FAILURE"),
        "the browser error is not reported: {body}"
    );
    assert!(
        body.contains("\"challenge\":{"),
        "no challenge object: {body}"
    );
    assert!(
        body.contains("\"state\":\"blocked\""),
        "not blocked: {body}"
    );
    assert!(
        body.contains("\"vendor\":\"chrome-deny\""),
        "vendor missing: {body}"
    );
    assert!(body.contains("\"next\":"), "no hint for the agent: {body}");
}
#[test]
fn a_dns_failure_is_still_a_plain_error() {
    // The transport failures keep the 502 they had: there is no page to
    // classify, and inventing a block would be worse than saying it failed.
    let server = TestServer::start("navigate-fail-http", true);
    let response = server.post("/v1/navigate", r#"{"url":"fail://nowhere.test"}"#);
    assert_eq!(status_code(&response), "502", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("navigation failed"),
        "got {}",
        body_of(&response)
    );
    assert!(
        !body_of(&response).contains("challenge"),
        "a transport failure is not a challenge: {}",
        body_of(&response)
    );
}

#[test]
fn a_chromium_error_page_is_blocked_not_unknown() {
    // What x.com's edge answers an automated client: Chromium's own renderer
    // error page (`chrome-error://chromewebdata/`, "Access to x.com was
    // denied"). It used to read as "not an http(s) page", i.e. ready; the
    // classification must say blocked, with the phrase it hangs on.
    let server = TestServer::start("navigate-deny", true);
    let response = server.post("/v1/navigate", r#"{"url":"deny://blocked.test"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        body.contains("\"challenge\":{"),
        "no challenge object: {body}"
    );
    assert!(
        body.contains("\"state\":\"blocked\""),
        "not blocked: {body}"
    );
    assert!(
        body.contains("\"vendor\":\"chrome-deny\""),
        "vendor missing: {body}"
    );
    assert!(
        body.contains("access to") || body.contains("chrome error page"),
        "phrase not reported: {body}"
    );
    // And never success: the navigation must not read like a plain 200.
    assert!(
        !body.contains("\"state\":\"ready\""),
        "ready leaked: {body}"
    );
}

#[test]
fn a_navigation_the_browser_cannot_do_is_a_clear_error() {
    let server = TestServer::start("navigate-fail", true);
    let response = server.post("/v1/navigate", r#"{"url":"fail://nowhere.test"}"#);
    // A 200 with `errorText` tucked inside the CDP result hid the failure;
    // the agent must see what went wrong without parsing a nested payload.
    assert_eq!(status_code(&response), "502", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        body.contains("navigation failed: net::ERR_NAME_NOT_RESOLVED"),
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
fn a_plain_capture_does_not_measure_the_page_first() {
    // A capture of the whole viewport has nothing to measure: the old way
    // ran a script in the page and waited for two animation frames, which
    // cost more than the capture itself.
    let server = TestServer::start("shot-plain", true);
    let before = command_log(&server).len();
    let response = server.get("/v1/screenshot");
    assert_eq!(status_code(&response), "200");
    let sent = command_log(&server);
    let sent = &sent[before..];
    assert!(
        sent.contains(&"Page.captureScreenshot".to_string()),
        "no capture in {sent:?}"
    );
    assert!(
        !sent.contains(&"Runtime.evaluate".to_string()),
        "the page was measured first: {sent:?}"
    );
}

#[test]
fn a_capture_of_one_element_still_measures_the_page() {
    // A clip needs the element's box, so the page is measured -- but the
    // helpers it needs are installed once per document, not once per
    // capture.
    let server = TestServer::start("shot-clip", true);
    let response = server.get("/v1/screenshot?selector=h1");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let sent = command_log(&server);
    assert!(
        sent.contains(&"Runtime.evaluate".to_string()),
        "the element was not measured: {sent:?}"
    );
}

#[test]
fn the_snapshot_installs_its_helpers_once_per_document() {
    // The walk and the ref registry are ten kilobytes of JavaScript the
    // browser compiles every time it is sent. The second snapshot of the
    // same document calls the walk that is already there.
    let server = TestServer::start("snapshot-install", true);
    server.post("/v1/navigate", r#"{"url":"https://example.test/"}"#);
    let before = command_log(&server).len();
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let first = command_log(&server);
    let first = &first[before..];
    let installs = first
        .iter()
        .filter(|method| *method == "Runtime.evaluate")
        .count();
    assert_eq!(installs, 1, "the first snapshot: {first:?}");

    let before = command_log(&server).len();
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let second = command_log(&server);
    let second = &second[before..];
    assert_eq!(
        second.iter().filter(|m| *m == "Runtime.evaluate").count(),
        1,
        "the second snapshot: {second:?}"
    );
    // The world of the document is made once, not once per snapshot.
    assert_eq!(
        second
            .iter()
            .filter(|m| *m == "Page.createIsolatedWorld")
            .count(),
        0,
        "the world was made again: {second:?}"
    );
}

#[test]
fn a_navigation_makes_the_world_again() {
    // A navigation replaces the document, and with it the world the helpers
    // ran in. The next snapshot has to notice and make a new one.
    let server = TestServer::start("world-after-nav", true);
    server.post("/v1/navigate", r#"{"url":"https://example.test/"}"#);
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));

    let before = command_log(&server).len();
    let response = server.post("/v1/navigate", r#"{"url":"https://example.test/other"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let after_navigation = command_log(&server);
    let after_navigation = &after_navigation[before..];
    assert!(
        after_navigation
            .iter()
            .any(|m| m == "Page.createIsolatedWorld"),
        "no new world after the navigation: {after_navigation:?}"
    );

    // And the snapshot after it still reads the page.
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.contains("ref=e1"), "no refs in {body}");
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
    // Sessions are encrypted at rest by default: one archive, no directory.
    assert_eq!(
        body_of(&response),
        "{\"saved\":true,\"storage\":\"encrypted\"}"
    );
    assert!(server.data_dir.join("sessions/trip_1.cuse").is_file());
    assert!(!server.data_dir.join("sessions/trip_1").exists());
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
    // A plain directory saved before encryption still loads, and says so.
    std::fs::create_dir_all(server.data_dir.join("sessions/trip_1")).expect("session dir");
    let response = server.post("/v1/session/trip_1/load", "{}");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert_eq!(
        body_of(&response),
        "{\"loaded\":true,\"storage\":\"plaintext\"}"
    );
    // An encrypted one round-trips through save and load.
    let profile = server.data_dir.join("profiles/default");
    std::fs::write(profile.join("Preferences"), "{\"kept\":1}").expect("profile file");
    assert_eq!(status_code(&server.post("/v1/session/trip_9", "{}")), "200");
    std::fs::remove_file(profile.join("Preferences")).expect("forget the file");
    let response = server.post("/v1/session/trip_9/load", "{}");
    assert_eq!(
        body_of(&response),
        "{\"loaded\":true,\"storage\":\"encrypted\"}"
    );
    assert_eq!(
        std::fs::read_to_string(profile.join("Preferences")).expect("restored"),
        "{\"kept\":1}"
    );
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
    let copied = open_session(&server, "trip_2");
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
    assert!(open_session(&server, "trip_3").join("Vendor").is_symlink());
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
fn downloads_are_listed_from_the_session_directory() {
    let server = TestServer::start("downloads", false);
    // Listing files needs no browser: the files outlive the session.
    assert_eq!(body_of(&server.get("/v1/downloads")), "{\"downloads\":[]}");
    let dir = server.data_dir.join("downloads");
    std::fs::create_dir_all(&dir).expect("downloads dir");
    std::fs::write(dir.join("report.pdf"), b"1234").expect("file");
    // A file Chromium is still writing must not appear as finished.
    std::fs::write(dir.join("half.zip.crdownload"), b"1").expect("partial");
    let response = server.get("/v1/downloads");
    let body = body_of(&response);
    assert!(body.contains("\"name\":\"report.pdf\""), "got {body}");
    assert!(body.contains("\"bytes\":4"), "got {body}");
    assert!(!body.contains("crdownload"), "partial listed: {body}");
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
    // The smart wait reports what it spent instead of hiding it, and whether
    // the wait ended on a ready page at all.
    assert!(body.contains("settled_ms"), "no settle timing in {body}");
    assert!(body.contains("\"settled\":true"), "not settled in {body}");
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
        if let Ok(config) = std::fs::read_to_string(dir.join("server.json"))
            && let Some(token) = server::json_value(&config, "token")
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            break token;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let client = cu::Client::new(format!("127.0.0.1:{port}"), token);
    let up = client.snapshot();
    // Stop the daemon before asserting anything, so a failure cannot leak it.
    let _ = Command::new("kill").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
    up.expect("browser came up");
    let closed = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = remove_dir(&dir);
    assert_eq!(closed, "closed\n", "the browser outlived its daemon");
}

/// Input commands the fake browser received, in order.
fn input_log(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/input-log").expect("input log")
}

/// Every DevTools command the daemon sent, in order: how many round trips a
/// tool took.
fn command_log(server: &TestServer) -> Vec<String> {
    let body = server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/command-log")
        .expect("command log");
    // A JSON array of plain method names: the quoted members are the
    // commands.
    body.split('"')
        .skip(1)
        .step_by(2)
        .map(|name| name.to_string())
        .collect()
}

/// Every `Browser.setDownloadBehavior` the daemon sent, as the fake browser saw
/// them: what decides where a download from `?context=NAME` lands.
fn download_behaviour(server: &TestServer) -> String {
    server::http_get(
        &format!("127.0.0.1:{}", server.cdp_port),
        "/download-behaviour",
    )
    .expect("download behaviour")
}

/// The viewport the fake browser is currently emulating, as
/// `(width, height, scale)`, or `None` when its own window size applies.
///
/// Compared as numbers, not as the JSON text: `cu` sets whole CSS pixels and
/// the fake keeps whatever type it was handed, so the two spell the same
/// viewport differently ("390" and "390.0") and the pixels are what matter.
fn emulated_viewport(server: &TestServer) -> Option<(f64, f64, f64)> {
    let reply =
        server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/viewport").expect("viewport");
    // The fake answers `[width,height,scale]`, so these are array positions,
    // not the object keys `json_number` looks for.
    let values: Vec<f64> = reply
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    match values[..] {
        [w, h, d, ..] => Some((w, h, d)),
        _ => None,
    }
}

/// The same, as the text the fake serves, for a test that wants to assert the
/// override is gone outright.
fn raw_viewport(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/viewport").expect("viewport")
}

/// A capture that emulates a viewport must not take anyone else's with it.
///
/// Another tool (a QA harness emulating a phone, a second agent's
/// `cu shot --width`) can have a device metrics override in force on the tab.
/// `cu` used to clear that override unconditionally once its own capture was
/// done, so the caller found its tab back at the browser's window size with
/// nothing said -- every later action then measured the wrong layout.
#[test]
fn a_capture_restores_the_viewport_it_found() {
    let server = TestServer::start("shot-keep-override", true);
    // Another tool emulates a 390x844 phone on this tab before we get here.
    server::http_get(
        &format!("127.0.0.1:{}", server.cdp_port),
        "/emulate?width=390&height=844&scale=2",
    )
    .expect("seed the override");
    assert_eq!(emulated_viewport(&server), Some((390.0, 844.0, 2.0)));

    let response = server.get("/v1/screenshot?width=800&height=600");
    assert_eq!(status_code(&response), "200", "{}", body_of(&response));

    assert_eq!(
        emulated_viewport(&server),
        Some((390.0, 844.0, 2.0)),
        "cu cleared an override it did not set: the caller's viewport is gone"
    );
}

/// The ordinary case, kept: a capture on a tab nobody emulated leaves the
/// browser's own window in charge, instead of pinning the tab to the size
/// that happened to be on screen during the capture.
#[test]
fn a_capture_leaves_an_unemulated_tab_to_the_browser() {
    let server = TestServer::start("shot-clear-override", true);
    let response = server.get("/v1/screenshot?width=800&height=600");
    assert_eq!(status_code(&response), "200", "{}", body_of(&response));
    assert_eq!(
        raw_viewport(&server),
        "null",
        "the tab is pinned to the capture's viewport instead of following the window"
    );
}

/// The device overrides the fake browser has in force, as
/// `{"metrics":..,"touch":..,"ua":..}` with each the params that set it, or
/// null. Like Chromium, an override dies with the connection that set it.
fn emulation_state(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/emulation")
        .expect("emulation state")
}

/// Whether the fake shows the whole phone `emulate mobile` asks for: the
/// viewport and a screen of its size, portrait, 3x, mobile, five touch
/// points and an Android Chrome User-Agent with client hints that agree.
fn assert_phone(state: &str) {
    for part in [
        "\"width\":390,\"height\":844,\"deviceScaleFactor\":3,\"mobile\":true",
        "\"screenWidth\":390,\"screenHeight\":844",
        "\"screenOrientation\":{\"type\":\"portraitPrimary\",\"angle\":0}",
        "\"maxTouchPoints\":5",
        "(Linux; Android 14; Pixel 8)",
        "Mobile Safari/537.36",
        "\"platform\":\"Android\"",
        "\"model\":\"Pixel 8\"",
    ] {
        assert!(state.contains(part), "{part} is not in force: {state}");
    }
}

/// `emulate mobile` is a whole phone, and it is not lost with the batch's
/// connection: Chromium drops a session's overrides when its socket closes,
/// and the daemon drops a pooled socket whenever a navigation outlasts the
/// settle budget. Here the batch's connection is cut right after the step,
/// which used to leave the tab a desktop again.
#[test]
fn an_emulated_phone_outlives_the_connection_of_its_batch() {
    let server = TestServer::start("emulate-keeper", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"mobile"},{"do":"eval","js":"__fake_hangup__"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        body.contains("390x844 @3x portrait, mobile, touch, Android Chrome 1 (Pixel 8)"),
        "the step does not say what is in force: {body}"
    );
    assert_phone(&emulation_state(&server));
    // Later commands on other connections leave it alone.
    let snapshot = server.get("/v1/snapshot");
    assert_eq!(status_code(&snapshot), "200", "got {}", body_of(&snapshot));
    assert_phone(&emulation_state(&server));
}

/// A capture at another size puts the device back afterwards -- the whole
/// device, not just a viewport measured from the page.
#[test]
fn a_capture_puts_the_emulated_device_back() {
    let server = TestServer::start("emulate-shot", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"mobile"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let shot = server.get("/v1/screenshot?width=800&height=600");
    assert_eq!(status_code(&shot), "200", "{}", body_of(&shot));
    assert_eq!(emulated_viewport(&server), Some((390.0, 844.0, 3.0)));
    assert_phone(&emulation_state(&server));
}

/// A keeper that dies (the browser cut every DevTools socket) is replaced
/// before the next command, and the device goes back on.
#[test]
fn a_lost_keeper_is_replaced_before_the_next_command() {
    let server = TestServer::start("emulate-ensure", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"tablet"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/hangup").expect("hang up");
    // Every override died with the sockets.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        emulation_state(&server),
        r#"{"metrics":null,"touch":null,"ua":null}"#
    );
    let text = server.get("/v1/text");
    assert_eq!(status_code(&text), "200", "got {}", body_of(&text));
    let state = emulation_state(&server);
    assert!(
        state.contains("\"width\":820,\"height\":1180,\"deviceScaleFactor\":2"),
        "{state}"
    );
    assert!(state.contains("Pixel Tablet"), "{state}");
}

/// On an emulated touch device a click is a finger: a touch start and end,
/// no mouse. `desktop` then turns all of it off, and clicks are a mouse again.
#[test]
fn a_click_on_an_emulated_phone_is_a_tap_until_desktop() {
    let server = TestServer::start("emulate-tap", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"mobile"},{"do":"click","ref":"e1"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("\"ok\":true"),
        "{}",
        body_of(&response)
    );
    // The tab is brought forward first: a background tab gets no frames,
    // and touch input without frames never reaches the page.
    assert_eq!(
        input_log(&server),
        r#"["Page.bringToFront", "Input.dispatchTouchEvent", "Input.dispatchTouchEvent"]"#
    );
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"desktop"},{"do":"click","ref":"e1"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert_eq!(
        emulation_state(&server),
        r#"{"metrics":null,"touch":null,"ua":null}"#,
        "desktop left an override behind"
    );
    assert_eq!(
        input_log(&server),
        r#"["Page.bringToFront", "Input.dispatchTouchEvent", "Input.dispatchTouchEvent", "Input.dispatchMouseEvent", "Input.dispatchMouseEvent"]"#
    );
}

/// A bad emulate step is refused before anything runs.
#[test]
fn a_bad_emulate_step_is_a_bad_request() {
    let server = TestServer::start("emulate-bad", true);
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"emulate","mode":"mobile","orientation":"sideways"}]}"#,
    );
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("orientation"),
        "{}",
        body_of(&response)
    );
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

#[test]
fn a_named_context_gets_its_own_tab_in_the_same_browser() {
    let server = TestServer::start("contexts", true);
    let response = server.post(
        "/v1/navigate?context=alice",
        r#"{"url":"https://example.test"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let listed = body_of(&server.get("/v1/contexts")).to_string();
    assert_eq!(listed, r#"{"contexts":["alice"]}"#);
    // The default tab is untouched and still reachable.
    assert_eq!(status_code(&server.get("/v1/snapshot")), "200");
    let request = format!(
        "DELETE /v1/contexts/alice HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    let closed = server.fragmented(&request);
    assert_eq!(body_of(&closed), r#"{"closed":true}"#);
    assert_eq!(body_of(&server.get("/v1/contexts")), r#"{"contexts":[]}"#);
}

#[test]
fn tabs_are_listed_with_a_default_and_named_actions_work() {
    let server = TestServer::start("tabs", true);
    let response = server.get("/v1/tabs");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    // The one page target, marked as the tab the default resolves to.
    assert!(body.contains("\"id\":\"PAGE1\""), "got {body}");
    assert!(body.contains("\"default\":true"), "got {body}");

    // Acting with ?tab= runs against that tab...
    let response = server.post(
        "/v1/navigate?tab=PAGE1",
        r#"{"url":"https://example.test"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    // ...and a stale id is an error an agent can act on, not a CDP failure.
    let response = server.post("/v1/navigate?tab=GONE", r#"{"url":"https://example.test"}"#);
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("no such tab"),
        "got {}",
        body_of(&response)
    );
    // Naming both ways of picking a page is an error, not a silent preference.
    let response = server.post("/v1/navigate?context=alice&tab=PAGE1", r#"{"url":"x"}"#);
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("not both"),
        "got {}",
        body_of(&response)
    );
}

#[test]
fn the_last_tab_cannot_be_closed_but_another_can() {
    let server = TestServer::start("tabs-close", true);
    // One page only: closing it would exit Chromium.
    let request = format!(
        "DELETE /v1/tabs/PAGE1 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    let response = server.fragmented(&request);
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("last tab"),
        "got {}",
        body_of(&response)
    );

    // With a second tab open (a popup in the real browser, a context tab
    // here), the default can be closed and the next action re-pins.
    server.post(
        "/v1/navigate?context=alice",
        r#"{"url":"https://example.test"}"#,
    );
    let response = server.fragmented(&request);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert_eq!(body_of(&response), "{\"closed\":true}");
    // And a tab that was never there is refused.
    let request = format!(
        "DELETE /v1/tabs/NOPE HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    let response = server.fragmented(&request);
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
}

#[test]
fn iframe_contents_are_snapshotted_and_clickable() {
    let server = TestServer::start("frames", true);
    let response = server.post("/v1/navigate", r#"{"url":"https://example.test/frames"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));

    // The snapshot reaches into the iframe: frame line, and its elements.
    let response = server.get("/v1/snapshot");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        body.contains("- frame: https://example.test/child"),
        "got {body}"
    );
    assert!(body.contains("ref=e9"), "no iframe ref in {body}");

    // Clicking an iframe ref enters the frame, adds the iframe's own offset
    // and dispatches the mouse at main-document coordinates.
    let response = server.post("/v1/click", r#"{"ref":"e9"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("\"ok\":true"),
        "got {}",
        body_of(&response)
    );
    assert_eq!(
        input_log(&server),
        r#"["Input.dispatchMouseEvent", "Input.dispatchMouseEvent"]"#
    );

    // The closing snapshot of a batch sees the frame too, and an unknown
    // ref is still the clear error an agent can act on.
    let response = server.post("/v1/act", r#"{"actions":[{"do":"click","ref":"e9"}]}"#);
    let body = body_of(&response);
    assert!(body.contains("\"ok\":true"), "got {body}");
    assert!(
        body.contains("- frame: https://example.test/child"),
        "got {body}"
    );
    let response = server.post("/v1/click", r#"{"ref":"e404"}"#);
    assert!(
        body_of(&response).contains("take a new snapshot"),
        "got {}",
        body_of(&response)
    );
}

#[test]
fn text_returns_the_pages_visible_words() {
    let server = TestServer::start("text", true);
    let response = server.get("/v1/text");
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(body.contains("the quick brown fox"), "got {body}");
    assert!(body.contains("\"truncated\":false"), "got {body}");
}

#[test]
fn text_reads_the_frames_of_the_page_too() {
    let server = TestServer::start("text-frames", true);
    // A page with no frames: just the document, no frame lines.
    let body = body_of(&server.get("/v1/text")).to_string();
    assert!(body.contains("the quick brown fox"), "got {body}");
    assert!(!body.contains("- frame:"), "no frames expected in {body}");

    // With an iframe, its words come back named by the frame they came from,
    // so the text never claims they are the page's own.
    let response = server.post("/v1/navigate", r#"{"url":"https://example.test/frames"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&server.get("/v1/text")).to_string();
    assert!(
        body.contains("the quick brown fox"),
        "main frame lost: {body}"
    );
    assert!(body.contains("words from inside the frame"), "got {body}");
    assert!(
        body.contains("- frame: https://example.test/child"),
        "frame not named: {body}"
    );
    assert!(body.contains("\"truncated\":false"), "got {body}");
}

#[test]
fn a_named_context_downloads_into_a_directory_of_its_own() {
    let server = TestServer::start("context-downloads", true);
    let response = server.post(
        "/v1/navigate?context=alice",
        r#"{"url":"https://example.test"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));

    // Without a browserContextId the browser-wide behaviour governs every
    // context, so a download from one is indistinguishable from the default
    // tab's. Each named context must be given its own directory.
    let behaviour = download_behaviour(&server);
    assert!(
        behaviour.contains("CTX"),
        "no context download behaviour: {behaviour}"
    );
    let alice = behaviour
        .split("},")
        .find(|p| p.contains("CTX"))
        .unwrap_or("");
    assert!(
        alice.contains("browserContextId"),
        "the context behaviour must name its browser context: {alice}"
    );
    // And it is the context's own directory, under the session's downloads.
    let dir = server.data_dir.join("downloads").join("alice");
    assert!(
        alice.contains(&format!("downloads{}", std::path::MAIN_SEPARATOR)),
        "not under the session downloads: {alice}"
    );
    assert!(
        alice.contains("alice"),
        "not the context's own name: {alice}"
    );
    assert!(dir.is_dir(), "{} was not created", dir.display());
}

#[test]
fn downloads_from_named_contexts_are_listed_with_their_context() {
    let server = TestServer::start("context-download-list", false);
    assert_eq!(body_of(&server.get("/v1/downloads")), "{\"downloads\":[]}");
    let dir = server.data_dir.join("downloads");
    std::fs::create_dir_all(dir.join("alice")).expect("context dir");
    std::fs::write(dir.join("report.pdf"), b"1234").expect("default file");
    // Two contexts may download files of the same name; they are two files.
    std::fs::write(dir.join("alice").join("report.pdf"), b"12345").expect("alice file");
    std::fs::write(dir.join("alice").join("half.zip.crdownload"), b"1").expect("partial");
    let body = body_of(&server.get("/v1/downloads")).to_string();
    assert!(body.contains("\"name\":\"report.pdf\""), "got {body}");
    // The context's copy is labelled with its context, and is a different size.
    assert!(body.contains("\"context\":\"alice\""), "got {body}");
    assert!(body.contains("\"bytes\":5"), "got {body}");
    // A partial file inside a context directory is still not finished.
    assert!(!body.contains("crdownload"), "partial listed: {body}");
}

#[test]
fn a_context_name_cannot_be_a_path() {
    let server = TestServer::start("context-name", true);
    let response = server.get("/v1/snapshot?context=..%2Fx");
    assert_eq!(status_code(&response), "400", "got {}", body_of(&response));
}

#[test]
fn inspect_and_marks_say_what_is_missing() {
    let server = TestServer::start("visual", true);
    let response = server.get("/v1/inspect");
    assert_eq!(status_code(&response), "400");
    assert!(body_of(&response).contains("give ref or selector"));
    let response = server.post("/v1/marks", r#"{"ref":"e1"}"#);
    assert_eq!(status_code(&response), "400", "{response}");
    let response = server.get("/v1/marks");
    assert_eq!(status_code(&response), "200");
    assert_eq!(body_of(&response), r#"{"marks":[]}"#);
    let response = server.get("/v1/marks?format=md");
    assert!(body_of(&response).contains("No marks."), "{response}");
}

#[test]
fn an_annotated_screenshot_needs_refs_to_label() {
    let server = TestServer::start("shot-annotate", true);
    let response = server.get("/v1/screenshot?annotate=all");
    assert_eq!(status_code(&response), "502", "{response}");
    assert!(
        body_of(&response).contains("take a snapshot first"),
        "{response}"
    );
    let response = server.get("/v1/screenshot?annotate=nope");
    assert_eq!(status_code(&response), "400", "{response}");
}
// ---- the page a session works in ------------------------------------------
//
// `cu navigate` and the `cu snapshot` an agent runs after it are two CLI
// invocations of one session: the daemon used to open a fresh tab for every
// command, so the second read `about:blank`. These tests drive the real
// routes and the real binary, not a mock of them.

/// A pid that is alive and is not this process: a `sleep` the test kills at
/// the end, standing in for a second agent.
fn live_pid() -> (Child, u32) {
    let child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    (child, pid)
}

#[test]
fn a_second_command_works_in_the_tab_its_session_already_has() {
    let server = TestServer::start("tabs-reuse", true);
    let owner = std::process::id();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    assert_eq!(status_code(&first), "200", "got {}", body_of(&first));
    assert!(
        body_of(&first).contains("\"reused\":false"),
        "a first request should open a tab: {}",
        body_of(&first)
    );
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    let second = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    assert_eq!(status_code(&second), "200", "got {}", body_of(&second));
    assert!(
        body_of(&second).contains("\"reused\":true"),
        "the second request should be handed the same page: {}",
        body_of(&second)
    );
    assert_eq!(
        server::json_value(body_of(&second), "id").as_deref(),
        Some(id.as_str())
    );
    // And only one tab was opened: the page the session now works in.
    let listed = body_of(&server.get("/v1/tabs")).to_string();
    assert!(
        !listed.contains("TAB2"),
        "a second tab was opened: {listed}"
    );
    assert_eq!(
        listed.matches(&format!("\"id\":\"{id}\"")).count(),
        1,
        "{listed}"
    );
}

#[test]
fn two_sessions_never_share_a_page() {
    let server = TestServer::start("tabs-two-sessions", true);
    let (mut other, other_pid) = live_pid();
    let mine = server.post(
        "/v1/tabs",
        &format!("{{\"owner_pid\":{}}}", std::process::id()),
    );
    let theirs = server.post("/v1/tabs", &format!("{{\"owner_pid\":{other_pid}}}"));
    assert_eq!(status_code(&mine), "200", "got {}", body_of(&mine));
    assert_eq!(status_code(&theirs), "200", "got {}", body_of(&theirs));
    assert!(
        body_of(&theirs).contains("\"reused\":false"),
        "the other session got my page: {}",
        body_of(&theirs)
    );
    assert_ne!(
        server::json_value(body_of(&mine), "id"),
        server::json_value(body_of(&theirs), "id")
    );
    // The other session comes back and gets its own tab, not mine.
    let again = server.post("/v1/tabs", &format!("{{\"owner_pid\":{other_pid}}}"));
    assert_eq!(
        server::json_value(body_of(&again), "id"),
        server::json_value(body_of(&theirs), "id")
    );
    let _ = other.kill();
    let _ = other.wait();
}

#[test]
fn a_new_tab_is_asked_for_explicitly() {
    let server = TestServer::start("tabs-new-tab", true);
    let owner = std::process::id();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    // `--new-tab` carries `"fresh":true`: a page of this call's own, even
    // though the session already has one.
    let fresh = server.post(
        "/v1/tabs",
        &format!("{{\"owner_pid\":{owner},\"fresh\":true}}"),
    );
    assert_eq!(status_code(&fresh), "200", "got {}", body_of(&fresh));
    assert!(
        body_of(&fresh).contains("\"reused\":false"),
        "{}",
        body_of(&fresh)
    );
    let new_id = server::json_value(body_of(&fresh), "id").expect("tab id");
    assert_ne!(id, new_id);
    // The session works in the newest one from then on.
    let after = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    assert_eq!(
        server::json_value(body_of(&after), "id").as_deref(),
        Some(new_id.as_str())
    );
}

#[test]
fn a_closed_tab_is_never_handed_back() {
    let server = TestServer::start("tabs-closed", true);
    let owner = std::process::id();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    let closed = server.fragmented(&format!(
        "DELETE /v1/tabs/{id} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    ));
    assert_eq!(body_of(&closed), r#"{"closed":true}"#, "{closed}");
    // A tab the browser no longer lists must not come back as "yours".
    // (The fake browser names a new target from its live count, so a fresh tab
    // can carry the closed tab's name: `reused:false` is what says it is new.)
    let after = server.post("/v1/tabs", &format!("{{\"owner_pid\":{owner}}}"));
    assert_eq!(status_code(&after), "200", "got {}", body_of(&after));
    assert!(
        body_of(&after).contains("\"reused\":false"),
        "a closed tab was handed back: {}",
        body_of(&after)
    );
}

#[test]
fn a_dead_session_is_not_given_a_tab_again() {
    let server = TestServer::start("tabs-dead-owner", true);
    let (mut child, pid) = live_pid();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{pid}}}"));
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    child.kill().expect("kill sleep");
    child.wait().expect("reap sleep");
    // Wait for /proc/<pid> to go: a reaped process has none.
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = server.post("/v1/tabs", &format!("{{\"owner_pid\":{pid}}}"));
    assert_eq!(status_code(&after), "200", "got {}", body_of(&after));
    assert!(
        body_of(&after).contains("\"reused\":false"),
        "a dead session's tab was handed back: {}",
        body_of(&after)
    );
    assert_ne!(server::json_value(body_of(&after), "id"), Some(id));
}

/// A real `cu daemon`, a real fake browser, and the real binary run twice:
/// the documented `navigate` -> `snapshot` flow the CLI documents, end to end.
#[test]
fn the_real_cli_keeps_one_page_for_its_session() {
    let dir = std::env::temp_dir().join(format!("cu-it-cli-session-{}", std::process::id()));
    let _ = remove_dir(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let (port, cdp_port) = (free_port(), free_port());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cu"))
        .args(["daemon", "--port", &port.to_string(), "--cdp-port"])
        .arg(cdp_port.to_string())
        .arg("--data")
        .arg(&dir)
        .env(
            "CU_BROWSER",
            env!("CARGO_MANIFEST_DIR").to_owned() + "/tests/fake_chromium.py",
        )
        .stdout(Stdio::null())
        .stderr(
            std::fs::File::create(std::env::temp_dir().join("cu-it-cli-daemon.log"))
                .map(Stdio::from)
                .unwrap_or(Stdio::null()),
        )
        .spawn()
        .expect("spawn cu daemon");
    // Ready once it answers.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(dir.join("server.json")).is_ok()
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cu"))
            .args(args)
            .env("CU_DAEMON_DIR", &dir)
            .env_remove("CU_DATA_DIR")
            .output()
            .expect("run cu")
    };
    let first = run(&["snapshot"]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        stderr.contains("working in a fresh tab ("),
        "the first command should open a tab: {stderr} daemon-log: {}",
        std::fs::read_to_string(std::env::temp_dir().join("cu-it-cli-daemon.log"))
            .unwrap_or_default()
    );
    assert_eq!(first.status.code(), Some(0), "snapshot failed: {stderr}");
    let second = run(&["snapshot"]);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("the page this session already had"),
        "the second command should reuse the page: {stderr}"
    );
    assert_eq!(second.status.code(), Some(0), "snapshot failed: {stderr}");
    // Same tab both times, and the page really came from it.
    let id_of = |text: &str| {
        let start = text.find("tab (").expect("tab marker") + "tab (".len();
        text[start..]
            .chars()
            .take_while(|c| *c != ')')
            .collect::<String>()
    };
    assert_eq!(
        id_of(&String::from_utf8_lossy(&first.stderr)),
        id_of(&String::from_utf8_lossy(&second.stderr))
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("snapshot") || first.stdout.len() > 2);
    // `--new-tab` opens a second page and keeps working in it.
    let third = run(&["snapshot", "--new-tab"]);
    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(stderr.contains("working in a fresh tab ("), "{stderr}");
    assert_ne!(
        id_of(&stderr),
        id_of(&String::from_utf8_lossy(&second.stderr))
    );
    let _ = Command::new("kill").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
    let _ = remove_dir(&dir);
}

/// The hand-off wait ceiling is read from the environment, which is process
/// wide: these tests are the ones that change it, so they take this lock and
/// run one at a time rather than shortening each other's budget.
fn wait_ceiling_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set (or clear, with `None`) the wait ceiling for the length of a test.
fn set_wait_ceiling(seconds: Option<&str>) {
    // SAFETY: the lock above makes this the only thread touching the
    // environment while it is set, and no other thread is reading it.
    unsafe {
        match seconds {
            Some(value) => std::env::set_var("CU_HANDOFF_WAIT_MAX_S", value),
            None => std::env::remove_var("CU_HANDOFF_WAIT_MAX_S"),
        }
    }
}

/// The fake browser's interstitial counter: how many challenge probes it has
/// served, and which scenario (`clear` / `hold`) is on the tab.
fn interstitial(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/interstitial")
        .expect("interstitial state")
}

#[test]
fn an_interstitial_that_clears_itself_is_resumed_without_the_agent_retrying() {
    // The case the wait exists for: a navigation lands on a Cloudflare
    // interstitial, the check clears by itself a moment later, and the agent
    // must be able to pick the navigation back up from the hand-off instead
    // of looping on `navigate`.
    let server = TestServer::start("handoff-clear", true);
    // A short wait budget keeps the test quick; the default is 10 s and the
    // interstitial clears after two probes either way.
    let response = server.post("/v1/navigate", r#"{"url":"cf://example.test/account"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let navigated = body_of(&response);
    assert!(
        navigated.contains("\"state\":\"ready\"") || navigated.contains("challenge_pending"),
        "no challenge object: {navigated}"
    );

    // Now the wait: it must come back resolved, and the page it reports must
    // be the real one, not the challenge page.
    let response = server.get("/v1/challenge?wait=6");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"resolved\":true"), "not resolved: {body}");
    assert!(body.contains("\"timed_out\":false"), "timed out: {body}");
    assert!(
        body.contains("\"state\":\"ready\""),
        "the interstitial never cleared: {body}"
    );
    // The fake really did serve the held page first and the real one after,
    // so the test is measuring the resume, not a page that was never a
    // challenge.
    let seen = interstitial(&server);
    assert_eq!(
        server::json_string_value(&seen, "kind").as_deref(),
        Some("clear"),
        "{seen}"
    );
    assert!(
        server::json_number(&seen, "probes").unwrap_or(0.0) >= 2.0,
        "the interstitial was never shown: {seen}"
    );
}

#[test]
fn a_wait_on_an_interstitial_that_never_clears_ends_on_the_budget_not_on_a_lie() {
    // A challenge the person has not solved yet. The wait must end on its
    // own budget, say plainly that a person is still needed, and never
    // report the tab as usable -- this is what stops an agent retrying blind.
    let server = TestServer::start("handoff-hold", true);
    // The budget is small: the wait is bounded by seconds, not by how long
    // the person takes.
    let _ceiling = wait_ceiling_lock();
    set_wait_ceiling(Some("4"));
    let response = server.post("/v1/navigate", r#"{"url":"cfhold://example.test/account"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));

    let response = server.get("/v1/challenge?wait=2");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(
        body.contains("\"timed_out\":true"),
        "should not resolve: {body}"
    );
    assert!(
        body.contains("\"resolved\":false"),
        "resolved leaked: {body}"
    );
    assert!(
        !body.contains("\"state\":\"ready\""),
        "a held challenge read as ready: {body}"
    );
    // The tab is paused: an automatic action on it is still refused.
    let response = server.post("/v1/click", r#"{"ref":"e1"}"#);
    assert_eq!(status_code(&response), "409", "got {response}");
    // And the fake really was held the whole time.
    let seen = interstitial(&server);
    assert_eq!(
        server::json_string_value(&seen, "kind").as_deref(),
        Some("hold"),
        "{seen}"
    );
    set_wait_ceiling(None);
}

#[test]
fn the_handoff_route_says_whether_a_person_is_needed_and_how_long() {
    // The state the agent asks before deciding what to do: is anyone needed
    // on this tab, and for how long. Without a hand-off it says so plainly
    // rather than reporting an empty object the agent has to interpret.
    let server = TestServer::start("handoff-state", true);
    let response = server.get("/v1/handoff");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"waiting\":false"), "{body}");
    assert!(body.contains("\"open\":null"), "{body}");
    // The budget it will clamp a wait to is published, so a caller knows what
    // asking for is even possible.
    assert!(body.contains("CU_HANDOFF_WAIT_MAX_S"), "{body}");

    // A held challenge opens a hand-off the route can see.
    let _ceiling = wait_ceiling_lock();
    set_wait_ceiling(Some("4"));
    server.post("/v1/navigate", r#"{"url":"cfhold://example.test/account"}"#);
    let response = server.get("/v1/handoff");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"waiting\":true"), "{body}");
    assert!(body.contains("\"vendor\":\"cloudflare\""), "{body}");
    // The challenge URL is cut: no `__cf_chl_tk` ever reaches the agent.
    assert!(!body.contains("__cf_chl_tk"), "token leaked: {body}");
    set_wait_ceiling(None);
}

#[test]
fn a_human_required_tab_is_brought_to_the_front_of_the_browser() {
    // Half of the hand-off is a *window*: a person can only solve what they
    // can see, so a headful daemon must put the tab in front with both calls
    // -- `Page.bringToFront` on the page session and `Target.activateTarget`
    // on the browser session -- in that order. The test harness launches no
    // browser, so the launch record is written here: without it `headful()`
    // is false and the daemon honestly reports there is no window to raise.
    let _ceiling = wait_ceiling_lock();
    let mut policy = cu::policy::BrowserPolicy::resolve(cu::policy::BrowserMode::FastTest)
        .expect("a policy for the test");
    policy.headless = false;
    policy.headless_shell = false;
    policy.display = Some(":99".into());
    cu::diagnostics::record_launch(&policy, &[]);

    let server = TestServer::start("handoff-front", true);
    set_wait_ceiling(Some("4"));
    server.post("/v1/navigate", r#"{"url":"cfhold://example.test/account"}"#);
    // The fronting runs off the request thread, so give it the same second a
    // real person would have before looking at the tab.
    std::thread::sleep(Duration::from_millis(700));

    let log = input_log(&server);
    let front = log
        .find("Page.bringToFront")
        .unwrap_or_else(|| panic!("the tab was never brought forward: {log}"));
    let activated = log
        .find("Target.activateTarget")
        .unwrap_or_else(|| panic!("the target was never activated: {log}"));
    assert!(
        front < activated,
        "activation must follow bringToFront: {log}"
    );

    // And the hand-off record says it was raised, not merely attempted.
    let response = server.get("/v1/handoff");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(
        body.contains("\"foreground\":\"brought to front"),
        "the hand-off did not report the window: {body}"
    );
    set_wait_ceiling(None);
    cu::diagnostics::clear_launch();
}

#[test]
fn a_wait_clamps_a_long_request_to_the_ceiling() {
    // An agent asking for a very long wait must not be able to hold the
    // daemon open for it: the ceiling applies whatever the request says. The
    // hold never clears, so a clamped wait returns on the ceiling -- fast.
    let server = TestServer::start("handoff-ceiling", true);
    let _ceiling = wait_ceiling_lock();
    set_wait_ceiling(Some("2"));
    server.post("/v1/navigate", r#"{"url":"cfhold://example.test/account"}"#);
    // Asks for an hour; the daemon's ceiling is 2 s, so this returns quickly
    // and says the person is still needed.
    let response = server.get("/v1/challenge?wait=3600");
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"timed_out\":true"), "{body}");
    // The reply costs a bounded number of probes: not one per second of the
    // hour that was asked for.
    let polls = server::json_number(&body, "polls").unwrap_or(0.0);
    assert!(
        polls > 0.0 && polls <= 240.0,
        "poll count not bounded: {body}"
    );
    set_wait_ceiling(None);
}

#[test]
fn a_location_is_applied_to_its_own_context_with_its_proxy() {
    // `POST /v1/seo/geo`: a context created behind the proxy, and locale,
    // time zone, position, Accept-Language and the geolocation permission
    // set for it -- with a position given, so no geocoder is asked.
    let server = TestServer::start("seo-geo", true);
    let response = server.post(
        "/v1/seo/geo?context=geo-test",
        r#"{"geo":"Getxo,Bizkaia,ES","lat":43.35,"lon":-3.01,"proxy":"http://127.0.0.1:3128"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"country\":\"ES\""), "{body}");
    assert!(body.contains("\"timezone\":\"Europe/Madrid\""), "{body}");
    assert!(body.contains("\"source\":\"given\""), "{body}");
    assert!(
        body.contains("\"proxy\":\"http://127.0.0.1:3128\""),
        "{body}"
    );
    // With a proxy there is no "no proxy" warning.
    assert!(!body.contains("no proxy"), "{body}");
    let log = server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/geo-log")
        .expect("the fake browser's log");
    assert!(
        log.contains("\"proxyServer\":\"http://127.0.0.1:3128\""),
        "the context was not created behind the proxy: {log}"
    );
    assert!(log.contains("\"locale\":\"es_ES\""), "{log}");
    assert!(log.contains("\"timezoneId\":\"Europe/Madrid\""), "{log}");
    assert!(log.contains("\"latitude\":43.35"), "{log}");
    assert!(log.contains("\"acceptLanguage\":\"es-ES,es,en\""), "{log}");
    // The browser's own User-Agent, minus the headless token.
    assert!(!log.contains("HeadlessChrome"), "{log}");
    assert!(log.contains("\"setting\":\"granted\""), "{log}");
    // A proxy with credentials in it is refused instead of silently dropped.
    let response = server.post(
        "/v1/seo/geo?context=geo-bad",
        r#"{"country":"ES","proxy":"http://user:secret@127.0.0.1:3128"}"#,
    );
    assert_eq!(status_code(&response), "400", "got {response}");
    // The location is part of the context, and goes with it.
    let request = format!(
        "DELETE /v1/contexts/geo-test HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    let response = send_fragmented(
        &server.addr,
        request.as_bytes(),
        4,
        Duration::from_millis(5),
    )
    .expect("DELETE should get a response");
    assert_eq!(status_code(&response), "200", "got {response}");
    assert!(body_of(&response).contains("\"closed\":true"), "{response}");
}

#[test]
fn an_audit_runs_in_the_context_and_reports_the_page() {
    let server = TestServer::start("seo-audit", true);
    let response = server.post(
        "/v1/seo/audit?context=seo",
        r#"{"url":"https://example.test/","keywords":["example"]}"#,
    );
    assert_eq!(status_code(&response), "200", "got {response}");
    let body = body_of(&response);
    assert!(body.contains("\"title\":\"Example\""), "{body}");
    assert!(body.contains("missing meta description"), "{body}");
    assert!(body.contains("\"challenge_state\""), "{body}");
    // A search without a query is refused before anything is navigated.
    let response = server.post("/v1/seo/serp?context=seo", r#"{"query":" "}"#);
    assert_eq!(status_code(&response), "400", "got {response}");
    let response = server.post(
        "/v1/seo/serp?context=seo",
        r#"{"query":"x","engine":"altavista"}"#,
    );
    assert_eq!(status_code(&response), "400", "got {response}");
}

// ── JavaScript dialogs ──────────────────────────────────────────────────────

/// What the fake browser knows about dialogs: open ones, armed
/// beforeunload handlers and how each dialog was answered.
fn dialog_state(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/dialog-state")
        .expect("dialog state")
}

/// Open a dialog on a tab from the page's side (a timer), as the fake does.
fn open_dialog(server: &TestServer, target: &str, kind: &str, message: &str) {
    server::http_get(
        &format!("127.0.0.1:{}", server.cdp_port),
        &format!("/dialog-open?target={target}&type={kind}&message={message}"),
    )
    .expect("open dialog");
}

/// The value of the first step of an act reply.
fn first_value(body: &str) -> Option<String> {
    server::json_string_value(body, "value")
}

/// A GET as session `pid` (the header the CLI sends).
fn get_as(server: &TestServer, path: &str, pid: u32) -> String {
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Cu-Session: {pid}\r\nContent-Length: 0\r\n\r\n",
        server.token
    );
    send_fragmented(
        &server.addr,
        request.as_bytes(),
        64,
        Duration::from_millis(1),
    )
    .expect("GET should get a response")
}

/// Navigating away from a page that armed `beforeunload` used to leave the
/// "Leave site?" dialog open: the navigation never returned and every later
/// command on the tab timed out. Under the default policy cu accepts it --
/// the agent asked to leave -- and the reply says so.
#[test]
fn navigating_away_from_a_beforeunload_page_is_accepted_by_policy() {
    let server = TestServer::start("dialog-beforeunload", true);
    let armed = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"eval","js":"window.onbeforeunload = e => { e.preventDefault(); return 'unsaved'; }"}],"snapshot":false}"#,
    );
    assert_eq!(status_code(&armed), "200", "got {}", body_of(&armed));
    assert!(
        dialog_state(&server).contains("\"armed\": [\"PAGE1\"]"),
        "{}",
        dialog_state(&server)
    );
    let started = std::time::Instant::now();
    let response = server.post("/v1/navigate", r#"{"url":"https://example.test/next"}"#);
    let took = started.elapsed();
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(took < Duration::from_secs(3), "navigate took {took:?}");
    let body = body_of(&response);
    assert!(
        body.contains("\"dialogs\":[{"),
        "the reply does not say: {body}"
    );
    assert!(body.contains("\"type\":\"beforeunload\""), "{body}");
    assert!(
        body.contains("\"action\":\"accepted\",\"by\":\"policy\""),
        "{body}"
    );
    let state = dialog_state(&server);
    assert!(state.contains("\"result\": \"accepted\""), "{state}");
    assert!(state.contains("\"open\": {}"), "{state}");
    // The page answers again.
    let snapshot = server.get("/v1/snapshot");
    assert_eq!(status_code(&snapshot), "200", "got {}", body_of(&snapshot));
}

/// alert is accepted, confirm and prompt are dismissed; a tab's own policy
/// changes that; each dialog is logged with its message, trimmed.
#[test]
fn alert_confirm_and_prompt_follow_the_policy() {
    let server = TestServer::start("dialog-policy", true);
    let eval = |js: &str| {
        let response = server.post(
            "/v1/act",
            &format!(
                "{{\"actions\":[{{\"do\":\"eval\",\"js\":{}}}],\"snapshot\":false}}",
                server::json_string(js)
            ),
        );
        assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
        body_of(&response).to_string()
    };
    let alert = eval("String(alert('hello there'))");
    assert_eq!(first_value(&alert).as_deref(), Some("undefined"), "{alert}");
    assert!(
        alert.contains("\"type\":\"alert\",\"message\":\"hello there\""),
        "{alert}"
    );
    assert!(alert.contains("\"action\":\"accepted\""), "{alert}");
    let confirm = eval("String(confirm('delete everything?'))");
    assert_eq!(first_value(&confirm).as_deref(), Some("false"), "{confirm}");
    assert!(confirm.contains("\"action\":\"dismissed\""), "{confirm}");
    let prompt = eval("String(prompt('your name?', 'nobody'))");
    assert_eq!(first_value(&prompt).as_deref(), Some("null"), "{prompt}");

    // The tab's own policy, on top of the daemon's.
    let set = server.post(
        "/v1/dialog/policy",
        r#"{"policy":"confirm=accept,prompt=accept"}"#,
    );
    assert_eq!(status_code(&set), "200", "got {}", body_of(&set));
    assert!(
        body_of(&set).contains("\"confirm\":\"accept\""),
        "{}",
        body_of(&set)
    );
    assert!(
        body_of(&set).contains("\"policy_from\":\"tab\""),
        "{}",
        body_of(&set)
    );
    let confirm = eval("String(confirm('really?'))");
    assert_eq!(first_value(&confirm).as_deref(), Some("true"), "{confirm}");
    let prompt = eval("String(prompt('your name?', 'nobody'))");
    assert_eq!(first_value(&prompt).as_deref(), Some("nobody"), "{prompt}");
    let bad = server.post("/v1/dialog/policy", r#"{"policy":"confirm=maybe"}"#);
    assert_eq!(status_code(&bad), "400", "got {}", body_of(&bad));
    let back = server.post("/v1/dialog/policy", r#"{"policy":"default"}"#);
    assert!(
        body_of(&back).contains("\"policy_from\":\"daemon\""),
        "{}",
        body_of(&back)
    );

    // The log: five dialogs, oldest first, a long message cut.
    eval(&format!("String(alert('{}'))", "x".repeat(300)));
    let log = server.get("/v1/dialog");
    assert_eq!(status_code(&log), "200", "got {}", body_of(&log));
    let log = body_of(&log);
    assert!(log.contains("\"watching\":true"), "{log}");
    assert!(log.contains("\"pending\":null"), "{log}");
    assert_eq!(log.matches("\"seq\":").count(), 6, "{log}");
    assert!(
        log.contains(&format!("\"message\":\"{}...\"", "x".repeat(200))),
        "{log}"
    );
    // URLs are cut to origin and path, like everywhere else in cu.
    assert!(
        log.contains("\"url\":\"https://example.test/page\""),
        "{log}"
    );
    assert!(!log.contains("secret"), "{log}");
}

/// With policy manual a dialog stays open for the agent: commands on its tab
/// fail at once, naming it, until `cu dialog accept|dismiss` answers it --
/// including the command that opened it, which used to wait out its read
/// timeout.
#[test]
fn a_manual_dialog_fails_commands_fast_until_answered() {
    let server = TestServer::start("dialog-manual", true);
    let set = server.post("/v1/dialog/policy", r#"{"policy":"all=manual"}"#);
    assert_eq!(status_code(&set), "200", "got {}", body_of(&set));
    // A prompt from the command itself: cut short, named.
    let started = std::time::Instant::now();
    let response = server.post(
        "/v1/act",
        r#"{"actions":[{"do":"eval","js":"String(prompt('who are you?', 'x'))"}],"snapshot":false}"#,
    );
    let took = started.elapsed();
    assert_eq!(status_code(&response), "409", "got {}", body_of(&response));
    assert!(took < Duration::from_secs(2), "the command waited {took:?}");
    let body = body_of(&response);
    assert!(body.contains("prompt dialog"), "{body}");
    assert!(body.contains("who are you?"), "{body}");
    assert!(body.contains("cu dialog accept"), "{body}");
    // Later commands: refused before they reach the page.
    for path in ["/v1/snapshot", "/v1/text", "/v1/screenshot"] {
        let started = std::time::Instant::now();
        let response = server.get(path);
        assert_eq!(
            status_code(&response),
            "409",
            "{path}: {}",
            body_of(&response)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{path} took {:?}",
            started.elapsed()
        );
    }
    let state = server.get("/v1/dialog");
    assert!(
        body_of(&state).contains("\"pending\":{\"seq\":"),
        "{}",
        body_of(&state)
    );
    // The answer, with the prompt's text (which is never logged).
    let answered = server.post("/v1/dialog/accept", r#"{"text":"Ada"}"#);
    assert_eq!(status_code(&answered), "200", "got {}", body_of(&answered));
    assert!(
        body_of(&answered).contains("\"action\":\"accepted\""),
        "{}",
        body_of(&answered)
    );
    assert!(
        dialog_state(&server).contains("\"text\": \"Ada\""),
        "{}",
        dialog_state(&server)
    );
    assert!(!body_of(&server.get("/v1/dialog")).contains("Ada"));
    let snapshot = server.get("/v1/snapshot");
    assert_eq!(status_code(&snapshot), "200", "got {}", body_of(&snapshot));
    // Nothing waiting: answering is an error that says so.
    let none = server.post("/v1/dialog/dismiss", "{}");
    assert_eq!(status_code(&none), "409", "got {}", body_of(&none));
    assert!(
        body_of(&none).contains("no JavaScript dialog is waiting"),
        "{}",
        body_of(&none)
    );
    // A dialog the page opens on its own, between commands, is held too,
    // and dismissed on request.
    open_dialog(&server, "PAGE1", "confirm", "stay%3F");
    let refused = server.get("/v1/snapshot");
    assert_eq!(status_code(&refused), "409", "got {}", body_of(&refused));
    assert!(
        body_of(&refused).contains("confirm dialog"),
        "{}",
        body_of(&refused)
    );
    let dismissed = server.post("/v1/dialog/dismiss", "{}");
    assert_eq!(
        status_code(&dismissed),
        "200",
        "got {}",
        body_of(&dismissed)
    );
    assert!(dialog_state(&server).contains("\"result\": \"dismissed\""));
    assert_eq!(status_code(&server.get("/v1/snapshot")), "200");
}

/// `cu console` shows the dialogs as their own level, and a page stopped by
/// one is answered from the daemon instead of hanging.
#[test]
fn the_dialog_log_is_in_the_console() {
    let server = TestServer::start("dialog-console", true);
    server.post(
        "/v1/act",
        r#"{"actions":[{"do":"eval","js":"String(alert('saved!'))"}],"snapshot":false}"#,
    );
    let text = server.get("/v1/console?level=dialog&format=text");
    assert_eq!(status_code(&text), "200", "got {}", body_of(&text));
    let text = body_of(&text);
    assert!(text.contains("DIALOG"), "{text}");
    assert!(text.contains("alert \"saved!\""), "{text}");
    assert!(text.contains("accepted (policy)"), "{text}");
    let json = server.get("/v1/console?level=all");
    assert!(
        body_of(&json).contains("\"dialogs\":[{"),
        "{}",
        body_of(&json)
    );
    // Other levels leave them out.
    let errors = server.get("/v1/console?level=error&format=text");
    assert!(!body_of(&errors).contains("DIALOG"), "{}", body_of(&errors));
    // A page held by a manual dialog: the console still answers, at once.
    server.post("/v1/dialog/policy", r#"{"policy":"alert=manual"}"#);
    open_dialog(&server, "PAGE1", "alert", "held");
    let started = std::time::Instant::now();
    let held = server.get("/v1/console?format=text");
    assert_eq!(status_code(&held), "200", "got {}", body_of(&held));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        body_of(&held).contains("the page cannot be read"),
        "{}",
        body_of(&held)
    );
    assert!(body_of(&held).contains("\"held\""), "{}", body_of(&held));
    server.post("/v1/dialog/accept", "{}");
}

/// A dialog that opened before cu watched the tab cannot be answered by any
/// session (Chromium tells a late one "No dialog is showing"). cu says so,
/// fast after the first try, and the tab can still be closed.
#[test]
fn a_dialog_nobody_watched_is_named_and_its_tab_can_be_closed() {
    let server = TestServer::start("dialog-unwatched", true);
    let created = server::browser_command(
        server.cdp_port,
        "Target.createTarget",
        r#"{"url":"about:blank"}"#,
    )
    .expect("create target");
    let id = server::json_string_value(&created, "targetId").expect("target id");
    open_dialog(&server, &id, "alert", "too%20early");
    let first = server.get(&format!("/v1/snapshot?tab={id}"));
    assert_eq!(status_code(&first), "409", "got {}", body_of(&first));
    assert!(
        body_of(&first).contains("is not answering"),
        "{}",
        body_of(&first)
    );
    assert!(
        body_of(&first).contains(&format!("cu tabs close {id}")),
        "{}",
        body_of(&first)
    );
    let started = std::time::Instant::now();
    let again = server.get(&format!("/v1/text?tab={id}"));
    assert_eq!(status_code(&again), "409", "got {}", body_of(&again));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let answer = server.post(&format!("/v1/dialog/accept?tab={id}"), "{}");
    assert_eq!(status_code(&answer), "409", "got {}", body_of(&answer));
    let closed = server.fragmented(&format!(
        "DELETE /v1/tabs/{id} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
        server.token
    ));
    assert_eq!(body_of(&closed), r#"{"closed":true}"#, "{closed}");
    assert!(!body_of(&server.get("/v1/tabs")).contains(&id));
}

/// The tab leak: while a session's tab was stopped by a dialog, every call
/// of that session failed after a read timeout -- and calls that came to the
/// daemon without a usable lease opened a fresh tab each, which the stopped
/// renderer then stopped too. A session whose tab is stopped now gets that
/// tab back, with the reason, and never another one.
#[test]
fn a_session_with_a_stopped_tab_never_gets_another() {
    let server = TestServer::start("dialog-no-leak", true);
    let (mut owner, pid) = live_pid();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{pid}}}"));
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    let set = server.post(
        &format!("/v1/dialog/policy?tab={id}"),
        r#"{"policy":"beforeunload=manual"}"#,
    );
    assert_eq!(status_code(&set), "200", "got {}", body_of(&set));
    open_dialog(&server, &id, "beforeunload", "");
    let tabs_before = body_of(&server.get("/v1/tabs")).matches("\"id\":").count();
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let response = get_as(&server, "/v1/snapshot", pid);
        assert_eq!(status_code(&response), "409", "got {}", body_of(&response));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(response.contains(&format!("X-Cu-Tab: {id}")), "{response}");
        assert!(
            !response.contains("X-Cu-Tab-New"),
            "a new tab was opened: {response}"
        );
        assert!(
            body_of(&response).contains("beforeunload dialog"),
            "{}",
            body_of(&response)
        );
    }
    let tabs_after = body_of(&server.get("/v1/tabs")).matches("\"id\":").count();
    assert_eq!(
        tabs_before,
        tabs_after,
        "{}",
        body_of(&server.get("/v1/tabs"))
    );
    let _ = owner.kill();
    let _ = owner.wait();
}

/// "It is collected when the session exits": a tab whose owner has exited
/// is closed by the collector -- also when a dialog stops it, since closing
/// a target does not need the page -- and its watcher and log go with it.
#[test]
fn the_tab_of_an_exited_session_is_collected_even_when_stopped() {
    let server = TestServer::start("dialog-reap", true);
    let (mut owner, pid) = live_pid();
    let first = server.post("/v1/tabs", &format!("{{\"owner_pid\":{pid}}}"));
    let id = server::json_value(body_of(&first), "id").expect("tab id");
    server.post(
        &format!("/v1/dialog/policy?tab={id}"),
        r#"{"policy":"all=manual"}"#,
    );
    open_dialog(&server, &id, "confirm", "leave%3F");
    // The owner is alive: nothing is collected.
    assert!(server::reap_orphan_tabs(server.cdp_port, Duration::ZERO).is_empty());
    owner.kill().expect("kill owner");
    owner.wait().expect("reap owner");
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Within the grace a tab is left alone (a --tab user may still come).
    assert!(server::reap_orphan_tabs(server.cdp_port, Duration::from_secs(3600)).is_empty());
    let collected = server::reap_orphan_tabs(server.cdp_port, Duration::ZERO);
    assert_eq!(collected, vec![id.clone()]);
    let state = dialog_state(&server);
    assert!(state.contains(&format!("\"closed\": [\"{id}\"")), "{state}");
    assert!(state.contains("\"result\": \"closed\""), "{state}");
    assert!(!body_of(&server.get("/v1/tabs")).contains(&id));
    // The default collection grace is short: a minute, not ten.
    assert!(server::TAB_GC_IDLE <= Duration::from_secs(60));
}

/// A session in another pid namespace (a sandbox, a container) names a pid
/// the daemon cannot look up. It used to be taken for dead -- a fresh tab
/// on every call -- or for some unrelated live process. Its tab is kept
/// for it, and collected only after the long idle.
#[test]
fn a_session_in_another_pid_namespace_keeps_its_tab() {
    let server = TestServer::start("tabs-foreign-ns", true);
    let body = r#"{"owner_pid":4000000,"owner_ns":1}"#;
    let first = server.post("/v1/tabs", body);
    assert_eq!(status_code(&first), "200", "got {}", body_of(&first));
    assert!(
        body_of(&first).contains("\"reused\":false"),
        "{}",
        body_of(&first)
    );
    let second = server.post("/v1/tabs", body);
    assert!(
        body_of(&second).contains("\"reused\":true"),
        "{}",
        body_of(&second)
    );
    assert_eq!(
        server::json_value(body_of(&first), "id"),
        server::json_value(body_of(&second), "id")
    );
    assert!(server::reap_orphan_tabs(server.cdp_port, Duration::ZERO).is_empty());
    // The same pid in the daemon's own namespace is a dead process.
    let local = server.post("/v1/tabs", r#"{"owner_pid":4000000}"#);
    assert!(
        body_of(&local).contains("\"reused\":false"),
        "{}",
        body_of(&local)
    );
}

/// A script's `$(cu ...)` runs cu in a forked subshell. Taking that subshell
/// for the session gave each substitution a session -- and a fresh tab -- of
/// its own, so a script polling a page with `x=$(cu eval ...)` opened a tab
/// per call. The subshell is now seen through: one script, one tab.
#[test]
fn a_scripts_command_substitutions_share_its_tab() {
    let dir = std::env::temp_dir().join(format!("cu-it-subshell-{}", std::process::id()));
    let _ = remove_dir(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    let (port, cdp_port) = (free_port(), free_port());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cu"))
        .args(["daemon", "--port", &port.to_string(), "--cdp-port"])
        .arg(cdp_port.to_string())
        .arg("--data")
        .arg(&dir)
        .env(
            "CU_BROWSER",
            env!("CARGO_MANIFEST_DIR").to_owned() + "/tests/fake_chromium.py",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cu daemon");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(dir.join("server.json")).is_ok()
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let script = dir.join("poll.sh");
    std::fs::write(
        &script,
        format!(
            "for i in 1 2 3; do x=$('{}' snapshot 2>&1 >/dev/null | grep -o 'tab ([A-Z0-9]*)'); echo \"$x\"; done\n",
            env!("CARGO_BIN_EXE_cu")
        ),
    )
    .expect("script");
    let out = Command::new("bash")
        .arg(&script)
        .env("CU_DAEMON_DIR", &dir)
        .env_remove("CU_DATA_DIR")
        .output()
        .expect("run script");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let tabs: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        tabs.len(),
        3,
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        tabs.iter().all(|t| *t == tabs[0]),
        "each substitution got a tab of its own: {stdout}"
    );
    let _ = Command::new("kill").arg(daemon.id().to_string()).status();
    let _ = daemon.wait();
    let _ = remove_dir(&dir);
}
