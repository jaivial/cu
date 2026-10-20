//! Remote live view and control of one tab, for a person who has to finish
//! a check (a captcha, a "confirm it's you" page) that an agent must not
//! solve. The vault UI session gates every route here, exactly as it gates
//! the credentials: whoever can open the vault can watch and drive the
//! browser, and nobody else.
//!
//! The page polls a JPEG frame and sends what a finger does (a tap, a drag
//! that scrolls, typed text, a few keys). Nothing here recognises or answers
//! a challenge; it only relays a human's input, paced like a human's, and
//! `release` hands the tab back to the agent.
use crate::server::{
    AppState, Tab, form_value, json_escape, json_string, json_value, named_tab, tabs_json,
};
use std::time::Duration;

pub const PAGE: &str = include_str!("vault_live.html");

type Reply = (&'static str, &'static str, String);

fn err(status: &'static str, message: &str) -> Reply {
    (
        status,
        "application/json",
        format!("{{\"error\":{}}}", json_string(message)),
    )
}

fn ok(body: String) -> Reply {
    ("200 OK", "application/json", body)
}

/// A small, cheap, unseeded jitter in `[lo, hi]` milliseconds: enough that
/// keystrokes are not evenly spaced, with no dependency on a random crate.
fn jitter(lo: u64, hi: u64) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos.wrapping_mul(6364136223846793005).rotate_left(17) % (hi - lo + 1);
    Duration::from_millis(lo + mixed)
}

fn pick_tab(state: &AppState, query: &str) -> Result<Tab, String> {
    let wanted = form_value(query, "tab");
    if wanted.is_empty() {
        Ok(Tab::default_for(state.cdp_port))
    } else {
        named_tab(state.cdp_port, &wanted)
    }
}

/// `(innerWidth, innerHeight)` of the tab in CSS pixels.
fn viewport(tab: &Tab) -> Result<(f64, f64), String> {
    let reply = tab.command(
        "Runtime.evaluate",
        "{\"expression\":\"innerWidth+','+innerHeight\",\"returnByValue\":true}",
    )?;
    let value = reply
        .split("\"value\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .ok_or("the page did not report its size")?;
    let (w, h) = value.split_once(',').ok_or("bad viewport")?;
    let w: f64 = w.parse().map_err(|_| "bad viewport width")?;
    let h: f64 = h.parse().map_err(|_| "bad viewport height")?;
    if w < 1.0 || h < 1.0 {
        return Err("the tab has no size".into());
    }
    Ok((w, h))
}

/// A JSON number under `key`. `json_value` stops at the first non-digit, so
/// it cannot read `0.4` or `-300`.
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

/// One DevTools input command; a protocol error in the reply is an error here
/// too, so a page that refuses input is reported instead of silently ignored.
fn cdp(tab: &Tab, method: &str, params: &str) -> Result<(), String> {
    let reply = tab.command(method, params)?;
    if reply.contains("\"error\"") {
        return Err(format!(
            "{method}: {}",
            json_value(&reply, "message").unwrap_or(reply)
        ));
    }
    Ok(())
}

fn mouse(tab: &Tab, kind: &str, x: f64, y: f64, extra: &str) -> Result<(), String> {
    cdp(
        tab,
        "Input.dispatchMouseEvent",
        &format!("{{\"type\":\"{kind}\",\"x\":{x:.1},\"y\":{y:.1}{extra}}}"),
    )
}

fn key(tab: &Tab, name: &str) -> Result<(), String> {
    let (code, vk, text) = match name {
        "Enter" => ("Enter", 13, "\\r"),
        "Backspace" => ("Backspace", 8, ""),
        "Tab" => ("Tab", 9, ""),
        "Escape" => ("Escape", 27, ""),
        "ArrowDown" => ("ArrowDown", 40, ""),
        "ArrowUp" => ("ArrowUp", 38, ""),
        "ArrowLeft" => ("ArrowLeft", 37, ""),
        "ArrowRight" => ("ArrowRight", 39, ""),
        "Space" => ("Space", 32, " "),
        _ => return Err(format!("unsupported key {name}")),
    };
    let key_name = if name == "Space" { " " } else { name };
    let text_part = if text.is_empty() {
        String::new()
    } else {
        format!(",\"text\":\"{text}\"")
    };
    cdp(
        tab,
        "Input.dispatchKeyEvent",
        &format!(
            "{{\"type\":\"keyDown\",\"key\":{},\"code\":\"{code}\",\"windowsVirtualKeyCode\":{vk}{text_part}}}",
            json_string(key_name)
        ),
    )?;
    std::thread::sleep(jitter(30, 70));
    cdp(
        tab,
        "Input.dispatchKeyEvent",
        &format!(
            "{{\"type\":\"keyUp\",\"key\":{},\"code\":\"{code}\",\"windowsVirtualKeyCode\":{vk}}}",
            json_string(key_name)
        ),
    )
    .map(|_| ())
}

/// Type `text` into whatever has focus, a character at a time with human
/// spacing. Characters a key event cannot carry go in as inserted text.
fn type_text(tab: &Tab, text: &str) -> Result<(), String> {
    for c in text.chars().take(500) {
        if c == '\n' {
            key(tab, "Enter")?;
        } else if c.is_ascii() && !c.is_control() {
            let (name, code, vk) = crate::input::key_of(c);
            let common = format!(
                "\"key\":{},\"code\":{},\"windowsVirtualKeyCode\":{vk}",
                json_string(&name),
                json_string(&code)
            );
            cdp(
                tab,
                "Input.dispatchKeyEvent",
                &format!(
                    "{{\"type\":\"keyDown\",{common},\"text\":{}}}",
                    json_string(&c.to_string())
                ),
            )?;
            std::thread::sleep(jitter(25, 70));
            cdp(
                tab,
                "Input.dispatchKeyEvent",
                &format!("{{\"type\":\"keyUp\",{common}}}"),
            )?;
        } else {
            cdp(
                tab,
                "Input.insertText",
                &format!("{{\"text\":{}}}", json_string(&c.to_string())),
            )?;
        }
        std::thread::sleep(jitter(45, 140));
    }
    Ok(())
}

/// Answer a live-view request, or `None` when the path is not one of ours.
pub fn api(method: &str, full_path: &str, body: &str, state: &AppState) -> Option<Reply> {
    let (path, query) = full_path.split_once('?').unwrap_or((full_path, ""));
    let rest = path.strip_prefix("/v1/vault/live/")?;
    if let Err(e) = state.browser.wait(Duration::from_secs(20)) {
        return Some(err("503 Service Unavailable", &e));
    }
    Some(match (method, rest) {
        ("GET", "tabs") => match tabs_json(state.cdp_port) {
            Ok(tabs) => ok(tabs),
            Err(e) => err("502 Bad Gateway", &e),
        },
        ("GET", "frame") => {
            let tab = match pick_tab(state, query) {
                Ok(t) => t,
                Err(e) => return Some(err("400 Bad Request", &e)),
            };
            let quality = form_value(query, "q")
                .parse::<u32>()
                .unwrap_or(55)
                .clamp(20, 90);
            let shot = tab.command(
                "Page.captureScreenshot",
                &format!("{{\"format\":\"jpeg\",\"quality\":{quality}}}"),
            );
            match shot {
                Ok(result) => {
                    let data = json_value(&result, "data").unwrap_or_default();
                    let (w, h) = viewport(&tab).unwrap_or((0.0, 0.0));
                    let url = tab
                        .command(
                            "Runtime.evaluate",
                            "{\"expression\":\"location.href\",\"returnByValue\":true}",
                        )
                        .ok()
                        .and_then(|r| {
                            r.split("\"value\":\"")
                                .nth(1)
                                .map(|s| s.split('"').next().unwrap_or("").to_string())
                        })
                        .unwrap_or_default();
                    ok(format!(
                        "{{\"jpeg_base64\":\"{data}\",\"width\":{w},\"height\":{h},\"url\":\"{}\",\"held\":{}}}",
                        json_escape(&url),
                        crate::challenge::gate(&tab).is_some()
                    ))
                }
                Err(e) => err("502 Bad Gateway", &e),
            }
        }
        ("POST", "input") => {
            let tab = match pick_tab(state, query) {
                Ok(t) => t,
                Err(e) => return Some(err("400 Bad Request", &e)),
            };
            let kind = json_value(body, "do").unwrap_or_default();
            // A tab nobody is looking at ignores input; this one is being looked at.
            let _ = tab.command("Page.bringToFront", "{}");
            let _ = tab.command("Emulation.setFocusEmulationEnabled", "{\"enabled\":true}");
            let result = match kind.as_str() {
                "tap" | "scroll" => {
                    let (Some(fx), Some(fy)) = (number(body, "x"), number(body, "y")) else {
                        return Some(err("400 Bad Request", "x and y (0..1) are required"));
                    };
                    let (w, h) = match viewport(&tab) {
                        Ok(v) => v,
                        Err(e) => return Some(err("502 Bad Gateway", &e)),
                    };
                    let (x, y) = ((fx.clamp(0.0, 1.0) * w), (fy.clamp(0.0, 1.0) * h));
                    if kind == "tap" {
                        let steps = (|| {
                            // A real pointer arrives, rests a moment, then presses.
                            mouse(
                                &tab,
                                "mouseMoved",
                                x - 6.0,
                                y - 4.0,
                                ",\"button\":\"none\",\"buttons\":0",
                            )?;
                            std::thread::sleep(jitter(40, 110));
                            mouse(
                                &tab,
                                "mouseMoved",
                                x,
                                y,
                                ",\"button\":\"none\",\"buttons\":0",
                            )?;
                            std::thread::sleep(jitter(60, 160));
                            mouse(
                                &tab,
                                "mousePressed",
                                x,
                                y,
                                ",\"button\":\"left\",\"buttons\":1,\"clickCount\":1",
                            )?;
                            std::thread::sleep(jitter(50, 120));
                            mouse(
                                &tab,
                                "mouseReleased",
                                x,
                                y,
                                ",\"button\":\"left\",\"buttons\":0,\"clickCount\":1",
                            )
                        })();
                        steps
                    } else {
                        let dy = number(body, "dy").unwrap_or(0.0).clamp(-3000.0, 3000.0);
                        let dx = number(body, "dx").unwrap_or(0.0).clamp(-3000.0, 3000.0);
                        mouse(
                            &tab,
                            "mouseWheel",
                            x,
                            y,
                            &format!(",\"deltaX\":{dx:.0},\"deltaY\":{dy:.0}"),
                        )
                    }
                }
                "type" => type_text(
                    &tab,
                    &crate::server::json_string_value(body, "text").unwrap_or_default(),
                ),
                "press" => key(
                    &tab,
                    &crate::server::json_string_value(body, "name").unwrap_or_default(),
                ),
                _ => Err("do must be tap, scroll, type or press".into()),
            };
            match result {
                Ok(()) => ok("{\"ok\":true}".into()),
                Err(e) => err("502 Bad Gateway", &e),
            }
        }
        ("POST", "release") => {
            let tab = match pick_tab(state, query) {
                Ok(t) => t,
                Err(e) => return Some(err("400 Bad Request", &e)),
            };
            ok(format!(
                "{{\"released\":{}}}",
                crate::challenge::release(&tab)
            ))
        }
        _ => err("404 Not Found", "no such live route"),
    })
}
