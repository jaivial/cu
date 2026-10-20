//! Push live view of one tab: Chromium's own screencast, relayed over a
//! WebSocket, with the viewer's input coming back on the same socket.
//!
//! This sits next to [`crate::live`], which polls one JPEG per request. The
//! gate is the same: a vault UI session (or the bearer token), so whoever can
//! open the vault can watch and drive the browser, and nobody else. Nothing
//! here recognises or answers a challenge; it relays a person's input and
//! `release` hands the tab back to the agent, exactly as `live` does.
//!
//! Shape of the stream:
//! - Chromium pushes `Page.screencastFrame` events only when the page paints.
//!   Each frame is acknowledged 100 ms after it arrived, each on its own
//!   timer. Chromium keeps at most three frames unacknowledged, so the
//!   ceiling is three frames per 100 ms. That is a ceiling, not a measured
//!   rate: the real rate is read from the viewer page on Kraken.
//! - The DevTools socket is read with `wait_event`, whose idle wait consumes
//!   nothing. If a frame still breaks half way, the connection is reopened and
//!   the screencast restarted instead of reading on from a torn stream.
//! - The vault session is re-checked every few seconds: logging out or an
//!   expired session closes the socket. A bearer caller has no session to
//!   revoke.
//! - The viewer only watches until it sends `take`. Input from a viewer that
//!   has not taken control is ignored.
//! - `take` is exclusive: while a person holds a tab, the daemon refuses the
//!   agent's automatic actions on it with a 409 (see [`taken`]; reads stay
//!   allowed). Control ends on `release`, when the socket closes for any
//!   reason, or when the session is revoked, so it cannot get stuck. A
//!   command the agent had already sent when `take` arrived may still finish.
//! - The viewer may have `FRAME_WINDOW` frames in flight. Past that, only the
//!   newest frame is kept and sent once an ack frees a slot: a slow viewer
//!   skips frames instead of building a queue.
//! - One viewer per tab at a time (each tab has its own count): a new
//!   connection to the same tab takes over and the older one is closed, so a stale page can never keep driving the browser.
//!
//! Binary frame to the viewer: `0x01`, seq (u32 BE), CSS width (u16 BE), CSS
//! height (u16 BE), then the JPEG. Everything else is a JSON text message.
use crate::server::{
    AppState, CdpConnection, Tab, form_value, json_string, json_string_value, json_value,
    named_tab,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Path of the socket.
pub const PATH: &str = "/v1/vault/live/stream";
/// The viewer page that speaks this protocol.
pub const PAGE: &str = include_str!("vault_stream.html");

const ACK_DELAY: Duration = Duration::from_millis(100);
const FRAME_WINDOW: u32 = 3;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// A viewer that has said nothing for this long lost its connection.
const SILENT: Duration = Duration::from_secs(30);
const CONTROL_EVERY: Duration = Duration::from_secs(3);
/// How often a cookie session is checked again.
const SESSION_EVERY: Duration = Duration::from_secs(2);
/// Once a frame has started to arrive, the rest of it must follow within this.
const FRAME_LIMIT: Duration = Duration::from_secs(10);
/// Times the screencast is reopened after a broken read before giving up.
const REOPEN_LIMIT: u32 = 3;
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Counts viewers across all tabs, for unique numbers.
static VIEWER: AtomicU64 = AtomicU64::new(0);

/// The current viewer of each tab. A viewer whose number is no longer the one
/// listed for its tab must stop; a viewer of another tab is not affected.
fn viewers() -> &'static Mutex<std::collections::HashMap<String, u64>> {
    static VIEWERS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    VIEWERS.get_or_init(Default::default)
}

/// The key of a tab in the viewer and control tables.
fn key_of(tab: &Tab) -> String {
    format!("{}:{}", tab.cdp_port, tab.target.as_deref().unwrap_or("default"))
}

/// Tabs a person holds through the viewer, with the viewer that holds each.
fn held_tabs() -> &'static Mutex<std::collections::HashMap<String, u64>> {
    static HELD: std::sync::OnceLock<Mutex<std::collections::HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// Whether a person holds this tab in the live viewer. The route table asks
/// this before it lets an automatic action through.
pub fn taken(tab: &Tab) -> bool {
    let held = held_tabs().lock().unwrap_or_else(|e| e.into_inner());
    if tab.target.is_some() {
        return held.contains_key(&key_of(tab));
    }
    // The agent usually names no tab, while the viewer always names a real
    // one. Resolve the default tab to the page it is pinned to. When it is
    // not pinned yet the page is ambiguous, so any held tab on this browser
    // counts (fail closed).
    match crate::server::pinned_default_target(tab.cdp_port) {
        Some(id) => {
            held.contains_key(&format!("{}:{}", tab.cdp_port, id))
                || held.contains_key(&key_of(tab))
        }
        None => {
            let prefix = format!("{}:", tab.cdp_port);
            held.keys().any(|k| k.starts_with(&prefix))
        }
    }
}

/// Give the tab back, if this viewer is the one holding it.
fn give_back(shared: &Shared) {
    let mut held = held_tabs().lock().unwrap_or_else(|e| e.into_inner());
    if held.get(&shared.tab_key) == Some(&shared.me) {
        held.remove(&shared.tab_key);
    }
}

/// Whether this request asks for the live stream socket.
pub fn wants(method: &str, path: &str, head: &str) -> bool {
    method == "GET"
        && path.split('?').next() == Some(PATH)
        && header(head, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn refuse(stream: &mut TcpStream, status: &str, why: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{why}",
        why.len()
    );
}

/// Same-origin only: a page on another site must not be able to open this
/// socket with the person's cookie. A request with no `Origin` (a local
/// client) is allowed; it still has to pass the session or bearer gate.
fn origin_ok(head: &str) -> bool {
    let Some(origin) = header(head, "origin") else {
        return true;
    };
    let host = header(head, "host").unwrap_or("");
    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or("");
    !host.is_empty() && origin_host.eq_ignore_ascii_case(host)
}

/// Take over the connection: gate it, upgrade it, and serve the stream until
/// either side goes away. Runs on the connection's own thread.
pub fn serve(mut stream: TcpStream, head: &str, path: &str, bearer_ok: bool, state: &AppState) {
    if !origin_ok(head) {
        return refuse(&mut stream, "403 Forbidden", "cross-origin socket refused");
    }
    let session_token = if bearer_ok {
        None
    } else {
        header(head, "cookie").and_then(crate::vault::cookie_token)
    };
    if !bearer_ok {
        if !session_alive(&state.data_dir, session_token.as_deref()) {
            return refuse(&mut stream, "401 Unauthorized", "vault: log in first");
        }
    }
    let Some(key) = header(head, "sec-websocket-key") else {
        return refuse(&mut stream, "400 Bad Request", "missing Sec-WebSocket-Key");
    };
    if let Err(e) = state.browser.wait(Duration::from_secs(20)) {
        return refuse(&mut stream, "503 Service Unavailable", &e);
    }
    let query = path.split_once('?').map_or("", |(_, q)| q);
    let wanted = form_value(query, "tab");
    let tab = if wanted.is_empty() {
        Ok(Tab::default_for(state.cdp_port))
    } else {
        named_tab(state.cdp_port, &wanted)
    };
    let tab = match tab {
        Ok(t) => t,
        Err(e) => return refuse(&mut stream, "400 Bad Request", &e),
    };
    let quality = form_value(query, "q").parse::<u32>().unwrap_or(60).clamp(20, 90);
    let max_width = form_value(query, "w").parse::<u32>().unwrap_or(1280).clamp(320, 2560);

    let accept = crate::server::base64_encode(&sha1(format!("{key}{GUID}").as_bytes()));
    if write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .is_err()
    {
        return;
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(SILENT));
    let Ok(writer) = stream.try_clone() else { return };

    let me = VIEWER.fetch_add(1, Ordering::SeqCst) + 1;
    let tab_key = key_of(&tab);
    viewers().lock().unwrap_or_else(|e| e.into_inner()).insert(tab_key.clone(), me);
    let shared = Arc::new(Shared {
        writer: Mutex::new(writer),
        stop: AtomicBool::new(false),
        window: Mutex::new(Window::default()),
        size: Mutex::new((0.0, 0.0)),
        controlling: AtomicBool::new(false),
        tab_key: tab_key.clone(),
        me,
    });
    let pusher = {
        let shared = Arc::clone(&shared);
        let tab = tab.clone();
        let data_dir = state.data_dir.clone();
        std::thread::spawn(move || {
            push_frames(&shared, &tab, quality, max_width, &data_dir, session_token.as_deref())
        })
    };
    read_input(&mut stream, &shared, &tab);
    shared.stop.store(true, Ordering::SeqCst);
    let _ = pusher.join();
    shared.controlling.store(false, Ordering::SeqCst);
    give_back(&shared);
    {
        let mut all = viewers().lock().unwrap_or_else(|e| e.into_inner());
        if all.get(&shared.tab_key) == Some(&me) {
            all.remove(&shared.tab_key);
        }
    }
    let _ = shared.write(0x8, &[0x03, 0xe8]);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Whether a vault UI session token is still valid (logout and expiry end it).
fn session_alive(data_dir: &PathBuf, token: Option<&str>) -> bool {
    crate::vault::Vault::open(data_dir)
        .ok()
        .and_then(|v| v.ui_session_check(token))
        .is_some()
}

#[derive(Default)]
struct Window {
    /// Frames sent and not yet acknowledged by the viewer.
    in_flight: u32,
    /// Sequence of the last frame sent.
    seq: u32,
    /// The newest frame that did not fit in the window.
    pending: Option<(Vec<u8>, u16, u16)>,
}

struct Shared {
    writer: Mutex<TcpStream>,
    stop: AtomicBool,
    window: Mutex<Window>,
    /// CSS size of the page as the last frame described it.
    size: Mutex<(f64, f64)>,
    /// Set by the viewer's `take`, cleared by `release`.
    controlling: AtomicBool,
    tab_key: String,
    me: u64,
}

impl Shared {
    fn live(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
            && viewers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&self.tab_key)
                .is_some_and(|current| *current == self.me)
    }

    fn write(&self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else if payload.len() < 65536 {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(&frame).inspect_err(|_| {
            self.stop.store(true, Ordering::SeqCst);
        })
    }

    fn text(&self, json: &str) {
        let _ = self.write(0x1, json.as_bytes());
    }

    /// Send a frame if the viewer has room, else keep it as the newest one.
    fn offer(&self, jpeg: Vec<u8>, w: u16, h: u16) {
        let mut win = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if win.in_flight >= FRAME_WINDOW {
            win.pending = Some((jpeg, w, h));
            return;
        }
        self.send_locked(&mut win, jpeg, w, h);
    }

    fn send_locked(&self, win: &mut Window, jpeg: Vec<u8>, w: u16, h: u16) {
        win.seq = win.seq.wrapping_add(1);
        win.in_flight += 1;
        let mut out = Vec::with_capacity(jpeg.len() + 9);
        out.push(1);
        out.extend_from_slice(&win.seq.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&jpeg);
        let _ = self.write(0x2, &out);
    }

    /// The viewer drew a frame: free a slot and send the newest waiting one.
    fn acked(&self) {
        let mut win = self.window.lock().unwrap_or_else(|e| e.into_inner());
        win.in_flight = win.in_flight.saturating_sub(1);
        if win.in_flight < FRAME_WINDOW
            && let Some((jpeg, w, h)) = win.pending.take()
        {
            self.send_locked(&mut win, jpeg, w, h);
        }
    }
}

/// Chromium to viewer: run the screencast on its own DevTools connection and
/// relay each frame. Returns when the viewer is gone, replaced, logged out, or
/// the browser could not be read again.
fn push_frames(
    shared: &Shared,
    tab: &Tab,
    quality: u32,
    max_width: u32,
    data_dir: &PathBuf,
    token: Option<&str>,
) {
    let mut reopened = 0;
    let mut last_beat = Instant::now();
    let mut last_control = Instant::now() - CONTROL_EVERY;
    let mut last_session = Instant::now();
    'connection: while shared.live() {
        let mut cdp = match CdpConnection::open(tab) {
            Ok(c) => c,
            Err(e) => return fail(shared, &e),
        };
        let started = cdp.call(
            "Page.startScreencast",
            &format!(
                "{{\"format\":\"jpeg\",\"quality\":{quality},\"maxWidth\":{max_width},\"everyNthFrame\":1}}"
            ),
        );
        if let Err(e) = started {
            return fail(shared, &e);
        }
        // Each frame is acknowledged 100 ms after it arrived, on its own timer.
        let mut acks: VecDeque<(Instant, String)> = VecDeque::new();
        while shared.live() {
            let now = Instant::now();
            while acks.front().is_some_and(|(due, _)| *due <= now) {
                if let Some((_, session)) = acks.pop_front() {
                    cdp.send("Page.screencastFrameAck", &format!("{{\"sessionId\":{session}}}"));
                }
            }
            let idle = acks
                .front()
                .map_or(Duration::from_millis(250), |(due, _)| due.saturating_duration_since(now))
                .min(Duration::from_millis(250));
            match cdp.wait_event(idle, FRAME_LIMIT) {
                Ok(Some(text)) if text.contains("\"Page.screencastFrame\"") => {
                    let session = json_value(&text, "sessionId").unwrap_or_default();
                    let data = json_value(&text, "data").unwrap_or_default();
                    let w = number(&text, "deviceWidth").unwrap_or(0.0);
                    let h = number(&text, "deviceHeight").unwrap_or(0.0);
                    if !session.is_empty() {
                        // Acknowledged whether or not the viewer takes this frame:
                        // Chromium stops sending while frames go unacknowledged.
                        acks.push_back((Instant::now() + ACK_DELAY, session));
                    }
                    if let Some(jpeg) = base64_decode(&data) {
                        *shared.size.lock().unwrap_or_else(|e| e.into_inner()) = (w, h);
                        shared.offer(jpeg, w.round() as u16, h.round() as u16);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    // The stream may be torn: never read on from it.
                    reopened += 1;
                    if reopened > REOPEN_LIMIT {
                        return fail(shared, &format!("DevTools stream lost: {e}"));
                    }
                    shared.text("{\"t\":\"reopening\"}");
                    continue 'connection;
                }
            }
            if last_beat.elapsed() >= HEARTBEAT {
                last_beat = Instant::now();
                let _ = shared.write(0x9, b"cu");
            }
            if last_session.elapsed() >= SESSION_EVERY {
                last_session = Instant::now();
                if token.is_some() && !session_alive(data_dir, token) {
                    shared.text("{\"t\":\"error\",\"error\":\"vault session ended\"}");
                    shared.stop.store(true, Ordering::SeqCst);
                    shared.controlling.store(false, Ordering::SeqCst);
                    give_back(shared);
                    // Closes the viewer's socket, which ends its reader too.
                    let _ = shared.writer.lock().map(|w| w.shutdown(std::net::Shutdown::Both));
                    break 'connection;
                }
            }
            if last_control.elapsed() >= CONTROL_EVERY {
                last_control = Instant::now();
                let held = crate::challenge::last(tab)
                    .is_some_and(|a| a.state == crate::challenge::ChallengeState::HumanRequired);
                let controlling = shared.controlling.load(Ordering::SeqCst);
                shared.text(&format!(
                    "{{\"t\":\"control\",\"held\":{held},\"controlling\":{controlling}}}"
                ));
            }
        }
        let _ = cdp.call("Page.stopScreencast", "{}");
        return;
    }
}

fn fail(shared: &Shared, why: &str) {
    shared.text(&format!("{{\"t\":\"error\",\"error\":{}}}", json_string(why)));
    shared.stop.store(true, Ordering::SeqCst);
}

/// One client frame, unmasked. `None` on a closed or broken socket.
fn read_client(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).ok()?;
    let opcode = head[0] & 0x0f;
    // Fragmented messages are not part of this protocol.
    if head[0] & 0x80 == 0 || opcode == 0 {
        return None;
    }
    // A client must mask what it sends.
    if head[1] & 0x80 == 0 {
        return None;
    }
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).ok()?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        return None;
    }
    if len > MAX_CLIENT_MESSAGE {
        return None;
    }
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).ok()?;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((opcode, payload))
}

/// Viewer to Chromium: acks and input until the viewer leaves.
fn read_input(stream: &mut TcpStream, shared: &Shared, tab: &Tab) {
    let mut focused = false;
    while shared.live() {
        let Some((opcode, payload)) = read_client(stream) else {
            return;
        };
        match opcode {
            0x8 => return,
            0x9 => {
                let _ = shared.write(0xA, &payload);
            }
            0x1 => {
                let Ok(text) = String::from_utf8(payload) else {
                    return;
                };
                handle_message(&text, shared, tab, &mut focused);
            }
            _ => {}
        }
    }
}

fn dispatch(tab: &Tab, method: &str, params: &str) {
    let _ = tab.command(method, params);
}

fn handle_message(msg: &str, shared: &Shared, tab: &Tab, focused: &mut bool) {
    let kind = json_string_value(msg, "t").unwrap_or_default();
    match kind.as_str() {
        "ack" => shared.acked(),
        "ping" => shared.text("{\"t\":\"pong\"}"),
        "take" => {
            // Exclusive: from here the daemon refuses the agent's automatic
            // actions on this tab until the person gives it back.
            held_tabs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(shared.tab_key.clone(), shared.me);
            shared.controlling.store(true, Ordering::SeqCst);
            shared.text("{\"t\":\"control\",\"controlling\":true}");
        }
        "release" => {
            shared.controlling.store(false, Ordering::SeqCst);
            give_back(shared);
            *focused = false;
            let released = crate::challenge::release(tab);
            shared.text(&format!("{{\"t\":\"released\",\"released\":{released}}}"));
        }
        "mouse" | "wheel" | "key" | "text" => {
            if !shared.controlling.load(Ordering::SeqCst) {
                shared.text("{\"t\":\"denied\"}");
                return;
            }
            if !*focused {
                // A tab nobody is looking at ignores input; this one is being looked at.
                dispatch(tab, "Page.bringToFront", "{}");
                dispatch(tab, "Emulation.setFocusEmulationEnabled", "{\"enabled\":true}");
                *focused = true;
            }
            let (w, h) = *shared.size.lock().unwrap_or_else(|e| e.into_inner());
            let x = number(msg, "x").unwrap_or(0.0).clamp(0.0, 1.0) * w;
            let y = number(msg, "y").unwrap_or(0.0).clamp(0.0, 1.0) * h;
            let modifiers = number(msg, "mod").unwrap_or(0.0).clamp(0.0, 15.0) as u32;
            match kind.as_str() {
                "mouse" => {
                    let action = json_string_value(msg, "a").unwrap_or_default();
                    let button = match json_string_value(msg, "b").as_deref() {
                        Some("middle") => "middle",
                        Some("right") => "right",
                        Some("left") => "left",
                        _ => "none",
                    };
                    let (kind, buttons, count) = match action.as_str() {
                        "down" => ("mousePressed", 1, 1),
                        "up" => ("mouseReleased", 0, 1),
                        _ => ("mouseMoved", if button == "none" { 0 } else { 1 }, 0),
                    };
                    dispatch(
                        tab,
                        "Input.dispatchMouseEvent",
                        &format!(
                            "{{\"type\":\"{kind}\",\"x\":{x:.1},\"y\":{y:.1},\"button\":\"{button}\",\"buttons\":{buttons},\"clickCount\":{count},\"modifiers\":{modifiers}}}"
                        ),
                    );
                }
                "wheel" => {
                    let dx = number(msg, "dx").unwrap_or(0.0).clamp(-3000.0, 3000.0);
                    let dy = number(msg, "dy").unwrap_or(0.0).clamp(-3000.0, 3000.0);
                    dispatch(
                        tab,
                        "Input.dispatchMouseEvent",
                        &format!(
                            "{{\"type\":\"mouseWheel\",\"x\":{x:.1},\"y\":{y:.1},\"deltaX\":{dx:.0},\"deltaY\":{dy:.0},\"modifiers\":{modifiers}}}"
                        ),
                    );
                }
                "key" => {
                    let down = json_string_value(msg, "a").as_deref() != Some("up");
                    let key = json_string_value(msg, "key").unwrap_or_default();
                    if key.is_empty() || key.len() > 32 {
                        return;
                    }
                    let code = json_string_value(msg, "code").unwrap_or_default();
                    let vk = number(msg, "vk").unwrap_or(0.0).clamp(0.0, 255.0) as u32;
                    let text = json_string_value(msg, "text").unwrap_or_default();
                    let text_part = if down && !text.is_empty() && text.chars().count() == 1 {
                        format!(",\"text\":{}", json_string(&text))
                    } else {
                        String::new()
                    };
                    dispatch(
                        tab,
                        "Input.dispatchKeyEvent",
                        &format!(
                            "{{\"type\":\"{}\",\"key\":{},\"code\":{},\"windowsVirtualKeyCode\":{vk},\"modifiers\":{modifiers}{text_part}}}",
                            if down { "keyDown" } else { "keyUp" },
                            json_string(&key),
                            json_string(&code)
                        ),
                    );
                }
                _ => {
                    // Whole strings (mobile keyboards, IME, paste) go in as inserted text.
                    let text = json_string_value(msg, "text").unwrap_or_default();
                    if !text.is_empty() && text.len() <= 4096 {
                        dispatch(tab, "Input.insertText", &format!("{{\"text\":{}}}", json_string(&text)));
                    }
                }
            }
        }
        _ => {}
    }
}

/// A JSON number under `key` (`json_value` stops at the first non-digit).
fn number(body: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let rest = body[body.find(&needle)? + needle.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// SHA-1, for `Sec-WebSocket-Accept` only (RFC 6455 requires it; it protects
/// nothing here, the session gate does).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// The `frame-ancestors` value for the viewer page: the one origin in
/// `CU_LIVE_FRAME_ORIGIN` when it is a plain `https://host[:port]`, else
/// `'none'`. No wildcards, paths or lists are accepted.
pub fn frame_ancestors() -> String {
    let raw = std::env::var("CU_LIVE_FRAME_ORIGIN").unwrap_or_default();
    let raw = raw.trim();
    let ok = raw.strip_prefix("https://").is_some_and(|h| {
        !h.is_empty()
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
    });
    if ok { raw.to_string() } else { "'none'".to_string() }
}
