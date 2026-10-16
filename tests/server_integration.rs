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

/// Every `Browser.setDownloadBehavior` the daemon sent, as the fake browser saw
/// them: what decides where a download from `?context=NAME` lands.
fn download_behaviour(server: &TestServer) -> String {
    server::http_get(
        &format!("127.0.0.1:{}", server.cdp_port),
        "/download-behaviour",
    )
    .expect("download behaviour")
}

// --- basic auth: the per-tab responder, against a fake CDP `Fetch` -------
//
// The responder answers `Fetch.authRequired` on a connection of its own, so
// the fake browser models the `Fetch` domain: it arms what it is told to arm,
// it challenges intercepted requests, and it records every `Fetch` command the
// daemon sent. These helpers are how a test sees that.

/// Every `Fetch.*` command the daemon sent, in order.
fn fetch_log(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/fetch-log").expect("fetch log")
}

/// A `Fetch.enable` that landed while another live connection already had the
/// same host armed: two interceptors over one host, which every request on it
/// would have to be answered twice for.
fn fetch_overlaps(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/fetch-overlaps")
        .expect("fetch overlaps")
}

/// Which connections are intercepting what, right now.
fn fetch_armed(server: &TestServer) -> String {
    server::http_get(&format!("127.0.0.1:{}", server.cdp_port), "/fetch-armed")
        .expect("fetch armed")
}

/// Make the fake challenge `host` the way a page's own request does after the
/// document has settled: no daemon command in between, so only a background
/// responder can answer it. Returns the fake's report of how many live
/// connections the challenge reached.
fn challenge_now(server: &TestServer, host: &str, request: &str) -> String {
    server::http_get(
        &format!("127.0.0.1:{}", server.cdp_port),
        &format!("/challenge?host={host}&request={request}"),
    )
    .expect("challenge pushed")
}

/// Store a basic-auth pair for `host` in the server's vault, as a user would
/// through the vault UI.
fn vault_basic_auth(server: &TestServer, host: &str, user: &str, password: &str) {
    let response = server.post(
        "/v1/vault/credentials",
        &format!(
            "{{\"domain\":{},\"label\":\"{} basic auth\",\"fields\":[{{\"name\":\"username\",\"value\":{},\"secret\":false}},{{\"name\":\"password\",\"value\":{},\"secret\":true}}]}}",
            server::json_string(host),
            host,
            server::json_string(user),
            server::json_string(password),
        ),
    );
    assert_eq!(
        status_code(&response),
        "200",
        "credential not stored: {}",
        body_of(&response)
    );
}

/// Polls `read` until `ready` says the thing being waited for has happened, or
/// the budget runs out.
///
/// The responder answers in the background, and a challenge leaves more than
/// one trace in the log: the intercepted request is continued first and the
/// credentials go over in a second command a moment later. Waiting for the
/// request to appear is therefore not waiting for the answer -- the test would
/// read the log in between and call it unanswered.
fn wait_until(
    mut read: impl FnMut() -> String,
    ready: impl Fn(&str) -> bool,
    budget: Duration,
) -> String {
    let started = std::time::Instant::now();
    loop {
        let state = read();
        if ready(&state) || started.elapsed() >= budget {
            return state;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until exactly `want` connections are intercepting, which is what
/// "the responder owns this host" looks like from outside.
fn wait_for_interceptors(server: &TestServer, want: usize, budget: Duration) -> String {
    wait_until(
        || fetch_armed(server),
        |armed| armed.matches("\"session\":").count() == want,
        budget,
    )
}

/// Every `Fetch` log entry about `request`, as the fake saw it. A log is
/// every answer in one string, so "did this challenge get answered, and with
/// what" has to be asked of the entries for *that* request: the navigation's
/// own answer to the same host sits right beside the responder's in the same
/// log and would otherwise vouch for it. One request can have several entries
/// (a paused request and an auth challenge share their id), so all of them are
/// returned.
fn fetch_answers_for(log: &str, request: &str) -> String {
    let needle = format!("\"requestId\": \"{request}\"");
    log.split("}, {")
        .filter(|entry| entry.contains(&needle))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn a_vault_pair_answers_the_challenge_a_navigation_meets() {
    // The whole point of the responder: navigating to a host behind basic
    // auth comes back as a loaded page instead of a dialog a headless browser
    // cannot see. The pair comes from the vault, over `Fetch.continueWithAuth`.
    let server = TestServer::start("auth-challenge", true);
    // A password with a quote in it: the pair has to survive JSON escaping on
    // its way through the vault and back out in `Fetch.continueWithAuth`.
    vault_basic_auth(&server, "protected.test", "alice", "p@ss\"w0rd");

    let response = server.post("/v1/navigate", r#"{"url":"https://protected.test/app"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        body.contains("\"basic_auth\":\"responded\""),
        "the challenge was not answered: {body}"
    );
    // And the report is the post-auth one, not the 401 that came first.
    assert!(body.contains("\"status\":200"), "bare 401 reported: {body}");
    assert!(
        body.contains("\"vault\":{\"session\":\"none\",\"credentials\":1}"),
        "the vault was not consulted: {body}"
    );

    // The fake saw the credential handed over, spelled the way current Chrome
    // insists on (`authChallengeResponse`, not the flat form).
    let log = fetch_log(&server);
    assert!(
        log.contains("\"method\": \"Fetch.enable\"")
            && log.contains("\"handleAuthRequests\": true"),
        "interception was not armed: {log}"
    );
    assert!(
        log.contains("\"hosts\": [\"protected.test\"]"),
        "not scoped to the host: {log}"
    );
    let answer = log
        .split("}, {")
        .find(|entry| entry.contains("Fetch.continueWithAuth"))
        .unwrap_or("");
    assert!(
        answer.contains("\"response\": \"ProvideCredentials\""),
        "no credentials provided: {log}"
    );
    assert!(
        answer.contains("\"username\": \"alice\"")
            && answer.contains("\"password\": \"p@ss\\\"w0rd\""),
        "the wrong pair, or one that lost its escaping: {answer}"
    );
}

#[test]
fn the_responder_keeps_answering_after_the_document_has_settled() {
    // The gap this responder exists to close: a page whose next request lands
    // after the navigation is over, with no command in between. Chromium does
    // not reliably cache what `Fetch.continueWithAuth` handed it, so the
    // challenge comes again -- and nobody would be reading the tab's stream
    // except the responder.
    let server = TestServer::start("auth-after-settle", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");
    let response = server.post("/v1/navigate", r#"{"url":"https://protected.test/app"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("\"basic_auth\":\"responded\""),
        "the first challenge went unanswered: {}",
        body_of(&response)
    );

    // The navigation gave the host to a responder of its own: exactly one live
    // connection is intercepting, and it is not the one that navigated.
    let armed = wait_for_interceptors(&server, 1, Duration::from_secs(5));
    assert!(
        armed.contains("protected.test"),
        "one interceptor for the host expected, got {armed}"
    );

    // Now the page asks for something else on that host, with nothing running.
    let pushed = challenge_now(&server, "protected.test", "after-settle");
    assert!(
        pushed.contains("\"challenged\": 1"),
        "no live connection was intercepting to challenge: {pushed}"
    );

    // And it is answered, from the pair, on its own. The entries for *this*
    // request are what say so: the navigation's own answer to the same host
    // is in the same log and would otherwise vouch for the responder.
    let log = wait_until(
        || fetch_log(&server),
        |log| fetch_answers_for(log, "after-settle").contains("Fetch.continueWithAuth"),
        Duration::from_secs(5),
    );
    let answers = fetch_answers_for(&log, "after-settle");
    assert!(
        answers.contains("Fetch.continueWithAuth"),
        "the late challenge went unanswered: {log}"
    );
    assert!(
        answers.contains("\"response\": \"ProvideCredentials\""),
        "answered without credentials: {answers}"
    );
    assert!(
        answers.contains("\"username\": \"alice\"") && answers.contains("\"password\": \"s3cr3t\""),
        "the wrong pair: {answers}"
    );
    // The paused request is continued too, or the page would sit waiting.
    assert!(
        answers.contains("Fetch.continueRequest"),
        "an intercepted request stayed paused: {answers}"
    );
}

#[test]
fn two_browsers_keep_their_own_responders() {
    // The map of live responders is per *process*, but a target id is per
    // *browser* -- both fakes here call their first tab `PAGE1`. Keyed by the
    // target alone, the two servers file their responders under one key and
    // whichever navigates second stops the other's, a responder nobody asked
    // to stop: the first browser's later challenges go unanswered.
    let first = TestServer::start("auth-two-browsers-a", true);
    let second = TestServer::start("auth-two-browsers-b", true);
    vault_basic_auth(&first, "alpha.test", "alice", "alpha-pw");
    vault_basic_auth(&second, "beta.test", "bob", "beta-pw");

    for server in [&first, &second] {
        let host = if server.addr == first.addr {
            "alpha.test"
        } else {
            "beta.test"
        };
        let response = server.post(
            &format!("/v1/navigate?tab=PAGE1"),
            &format!(r#"{{"url":"https://{host}/app"}}"#),
        );
        assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
        assert!(
            body_of(&response).contains("\"basic_auth\":\"responded\""),
            "{host} went unanswered: {}",
            body_of(&response)
        );
    }

    // Each responder must still be armed for its own host. With a shared key
    // the second navigation stopped the first, so one browser is left with
    // nothing watching it and challenges there are never answered.
    for (server, host) in [(&first, "alpha.test"), (&second, "beta.test")] {
        let armed = wait_until(
            || fetch_armed(server),
            |armed| armed.contains(host),
            Duration::from_secs(5),
        );
        assert!(
            armed.contains(host),
            "{host} has no live responder: {armed}"
        );
        let pushed = challenge_now(server, host, &format!("late-{host}"));
        assert!(
            pushed.contains("\"challenged\": 1"),
            "nothing was intercepting {host}: {pushed}"
        );
        let log = wait_until(
            || fetch_log(server),
            |log| {
                fetch_answers_for(log, &format!("late-{host}")).contains("Fetch.continueWithAuth")
            },
            Duration::from_secs(5),
        );
        let answers = fetch_answers_for(&log, &format!("late-{host}"));
        let pair = if host == "alpha.test" {
            ("alice", "alpha-pw")
        } else {
            ("bob", "beta-pw")
        };
        assert!(
            answers.contains(&format!("\"username\": \"{}\"", pair.0))
                && answers.contains(&format!("\"password\": \"{}\"", pair.1)),
            "{host} was answered with the wrong pair, or by the other browser: {answers}"
        );
    }
}

#[test]
fn a_host_without_a_credential_is_never_intercepted() {
    // Nothing to answer with: arming `Fetch` would only pause requests with
    // no way to release them, which is the hang the responder was meant to
    // stop. A host the vault knows nothing about is left alone.
    let server = TestServer::start("auth-no-credential", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");

    let response = server.post("/v1/navigate", r#"{"url":"https://open.test/app"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        !body.contains("basic_auth"),
        "a challenge was reported for a host with no credential: {body}"
    );
    assert!(
        body.contains("\"vault\":{\"session\":\"none\",\"credentials\":0}"),
        "the vault was not consulted: {body}"
    );
    // No interception was armed at all -- not for this host, not for any.
    assert_eq!(fetch_log(&server), "[]", "interception armed for nothing");
    assert_eq!(fetch_armed(&server), "[]", "something stayed armed");
    // And with nothing armed, a challenge has nowhere to go.
    let pushed = challenge_now(&server, "open.test", "nobody-home");
    assert!(
        pushed.contains("\"challenged\": 0"),
        "a connection was intercepting a host with no credential: {pushed}"
    );
}

#[test]
fn closing_a_tab_stops_its_responder() {
    // The responder holds a DevTools connection of its own for as long as the
    // tab lives. Closing the tab must take it with it: an interception left
    // armed over a target that no longer exists is a thread reading a stream
    // nobody will ever answer.
    let server = TestServer::start("auth-tab-closed", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");
    // Navigate a tab named by id, so the close can name it back.
    let response = server.post(
        "/v1/navigate?tab=PAGE1",
        r#"{"url":"https://protected.test/app"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let armed = wait_for_interceptors(&server, 1, Duration::from_secs(5));
    assert!(
        armed.contains("protected.test"),
        "the responder never armed: {armed}"
    );

    // The last tab may not be closed, so open a second one first.
    server.post(
        "/v1/navigate?context=alice",
        r#"{"url":"https://example.test"}"#,
    );
    let request = format!(
        "DELETE /v1/tabs/PAGE1 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n...gth: 0\r\n\r\n",
        server.token
    );
    let closed = server.fragmented(&request);
    assert_eq!(status_code(&closed), "200", "got {}", body_of(&closed));

    // The interception was given back rather than orphaned: every arming has a
    // matching release in the log. A responder that never released it would
    // still be "gone" after the close -- Chromium drops the socket -- so the
    // count is what says it let go on its own.
    let log = wait_until(
        || fetch_log(&server),
        |log| {
            log.contains("Fetch.enable")
                && log.matches("Fetch.enable").count() == log.matches("Fetch.disable").count()
        },
        Duration::from_secs(5),
    );
    assert_eq!(
        log.matches("Fetch.enable").count(),
        log.matches("Fetch.disable").count(),
        "an interception was armed and never released: {log}"
    );
    assert_eq!(
        fetch_armed(&server),
        "[]",
        "the responder is still intercepting a closed tab"
    );
}

#[test]
fn re_navigating_a_host_does_not_leave_two_interceptors_on_it() {
    // A navigation to a credentialled host arms `Fetch` itself, so the tab's
    // background responder has to let go first: two interceptors over one host
    // would both have to answer every request on it.
    let server = TestServer::start("auth-rearm", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");

    let response = server.post("/v1/navigate", r#"{"url":"https://protected.test/app"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("\"basic_auth\":\"responded\""),
        "the first challenge went unanswered: {}",
        body_of(&response)
    );
    wait_for_interceptors(&server, 1, Duration::from_secs(5));

    // Same host again: the responder is stopped and the navigation's own
    // interception takes its place.
    let response = server.post("/v1/navigate", r#"{"url":"https://protected.test/other"}"#);
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    assert!(
        body_of(&response).contains("\"basic_auth\":\"responded\""),
        "the second challenge went unanswered: {}",
        body_of(&response)
    );

    // The fake records any moment two live connections held the same host,
    // which happens inside the navigation rather than after it -- so by the
    // time it has answered, an overlap would already be on the list.
    let overlaps = fetch_overlaps(&server);
    assert_eq!(overlaps, "[]", "two interceptors over one host: {overlaps}");
    // Whatever is intercepting now is exactly one connection: the navigation
    // gave its interception back and the responder took the host again.
    let armed = wait_for_interceptors(&server, 1, Duration::from_secs(5));
    assert!(
        armed.contains("protected.test"),
        "the responder did not take the host back: {armed}"
    );
}

#[test]
fn turning_the_responder_off_arms_nothing() {
    // `"auth":false` is a caller that wants the raw challenge (to see the
    // 401, or to answer it in the page). Nothing may be armed for it, or the
    // requests it would have inspected stay paused.
    let server = TestServer::start("auth-disabled", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");

    let response = server.post(
        "/v1/navigate",
        r#"{"url":"https://protected.test/app","auth":false}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    let body = body_of(&response);
    assert!(
        !body.contains("basic_auth"),
        "the responder answered a challenge it was told to leave: {body}"
    );
    assert_eq!(fetch_log(&server), "[]", "interception armed anyway");
    assert_eq!(fetch_armed(&server), "[]", "something stayed armed");
}

#[test]
fn each_tab_gets_its_own_responder() {
    // Two tabs, two credentialled hosts, two responders: the responder is
    // keyed by tab, so neither stops the other and neither answers for a host
    // it has no pair for.
    let server = TestServer::start("auth-two-tabs", true);
    vault_basic_auth(&server, "protected.test", "alice", "s3cr3t");
    vault_basic_auth(&server, "other.test", "bob", "hunter2");
    let response = server.post(
        "/v1/navigate?tab=PAGE1",
        r#"{"url":"https://protected.test/app"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));
    wait_for_interceptors(&server, 1, Duration::from_secs(5));
    let response = server.post(
        "/v1/navigate?context=alice",
        r#"{"url":"https://other.test/app"}"#,
    );
    assert_eq!(status_code(&response), "200", "got {}", body_of(&response));

    // The context tab's responder is armed for its own host, and the first
    // tab's is untouched: one interceptor per host, on two connections.
    let armed = wait_until(
        || fetch_armed(&server),
        |armed| armed.contains("other.test"),
        Duration::from_secs(5),
    );
    assert!(
        armed.contains("protected.test") && armed.contains("other.test"),
        "both hosts should be answered: {armed}"
    );
    assert_eq!(
        fetch_overlaps(&server),
        "[]",
        "two tabs' responders collide: {}",
        fetch_overlaps(&server)
    );
    // A challenge to the context tab's host is answered with that tab's pair,
    // not the other tab's: the responder is keyed by tab.
    challenge_now(&server, "other.test", "ctx-challenge");
    let log = wait_until(
        || fetch_log(&server),
        |log| fetch_answers_for(log, "ctx-challenge").contains("Fetch.continueWithAuth"),
        Duration::from_secs(5),
    );
    let answer = fetch_answers_for(&log, "ctx-challenge");
    assert!(
        answer.contains("\"username\": \"bob\"") && answer.contains("\"password\": \"hunter2\""),
        "the wrong tab's pair answered: {answer}"
    );
    assert!(
        !answer.contains("alice"),
        "the first tab's pair answered for the second: {answer}"
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
