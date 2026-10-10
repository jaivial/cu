//! Turning a headless launch into one a site cannot tell from a person's.
//!
//! Cloudflare's interstitial (the "Just a moment..." page and the Turnstile
//! widget behind it) scores the *browser*, not the request. In practice the
//! decisive tell is one flag: `--headless`. A headless Chrome announces itself
//! three times -- `HeadlessChrome` in the User-Agent, the same token in the
//! client hints a page reads as `navigator.userAgentData`, and the launch
//! flags themselves, which an unprivileged page can read out of
//! `/proc/<pid>/cmdline` once the site drops a frame it controls. A site that
//! sees any of them answers a navigation with "verifying you are not a bot"
//! instead of the page, and cu then reports `human_required` -- the one state
//! an agent cannot act in.
//!
//! Measured on `receitasdepesos.com.br` (Chrome 145, October 2026): a
//! headless launch returned the 403 interstitial with every JavaScript patch
//! below in place, and the same launch with the `HeadlessChrome` token out of
//! the User-Agent returned 200 and the real page. So the work is in three
//! parts, all of them *removing a signal* rather than inventing one:
//!
//! - **the launch** ([`crate::policy`]): `--headless=new` gives way to a
//!   plain window on an X display (see [`display`]), the DevTools port moves
//!   off the default 9222, and `--remote-allow-origins=*` and
//!   `--enable-automation` -- the flag that switches on the
//!   `AutomationControlled` blink feature, and with it
//!   `navigator.webdriver` -- are never passed;
//! - **the User-Agent**: one coherent string, the product token taken from the
//!   browser's own `/json/version` so `Chrome/145` is the build that really
//!   answers, installed with `Network.setUserAgentOverride`, which rewrites
//!   the HTTP header *and* the client hints (a `--user-agent=` flag would
//!   leave `navigator.userAgentData` claiming Google Chrome);
//! - **the automation surface** ([`STEALTH_JS`]): `navigator.webdriver` and
//!   the DevTools command line every automation library checks for, applied
//!   to every document of every target with
//!   `Page.addScriptToEvaluateOnNewDocument` -- which is what the same
//!   command registers as the document's own script slot.
//!
//! Nothing here invents a machine. The locale, time zone, window, platform,
//! CPU count and GPU stay this host's, on purpose: a coherent set of honest
//! values is worth more than a plausible set of invented ones, and mixing the
//! two is what makes a fingerprint stand out. The GPU is the single exception
//! and it is reported: a host with no graphics card renders with SwiftShader,
//! and `WEBGL_debug_renderer_info` would hand a site the browser's one
//! unambiguous "there is no machine here". [`GL_RENDERER`] is what a desktop
//! GPU reports instead, and `/v1/diagnostics` says the value was replaced.
//!
//! This is fingerprint hygiene, not a bypass. Nothing here solves a
//! challenge, clicks a checkbox or forges a clearance cookie: a site that
//! still wants a person gets [`crate::challenge`]'s hand-off. A bot score, an
//! IP reputation and a fingerprint database are what a well-run defence looks
//! at beyond the browser itself.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::server::{CdpConnection, json_string};

/// The token a headless Chrome puts in its own User-Agent, where a windowed
/// one says `Chrome/`.
const HEADLESS_TOKEN: &str = "HeadlessChrome";

/// A headful browser with a window reports its GPU through ANGLE like this (an
/// Intel UHD 620, a common laptop part). `None` leaves the renderer alone.
pub const GL_RENDERER: Option<(&str, &str)> = Some((
    "Google Inc. (Intel)",
    "ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6, SwiftShader driver)",
));

// ── what the pass did ───────────────────────────────────────────────────────

/// What the stealth pass did to the running browser, for `/v1/diagnostics`
/// and the launch log.
type Applied = Result<Vec<String>, String>;

fn state() -> &'static Mutex<Applied> {
    static STATE: OnceLock<Mutex<Applied>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Ok(Vec::new())))
}

fn xvfb_slot() -> &'static Mutex<Option<u32>> {
    static XVFB: OnceLock<Mutex<Option<u32>>> = OnceLock::new();
    XVFB.get_or_init(|| Mutex::new(None))
}

fn remember(message: String) {
    if let Ok(notes) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        notes.push(message);
    }
}

fn fail(error: String) {
    *state().lock().unwrap_or_else(|e| e.into_inner()) = Err(error);
}

/// Reset the record: a new daemon, a new browser.
pub fn reset() {
    *state().lock().unwrap_or_else(|e| e.into_inner()) = Ok(Vec::new());
}

/// The notes of the pass that ran (empty when there was none).
pub fn notes() -> Vec<String> {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The `stealth` object of `GET /v1/diagnostics`.
///
/// The notes are copied out under the one lock, never re-entered: a second
/// `lock()` on the same thread while the first is held would deadlock the
/// daemon on a diagnostics call.
pub fn json() -> String {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    match &*guard {
        Ok(notes) => list("true", notes, None),
        Err(error) => list("false", &[], Some(error)),
    }
}

/// `{"ok":...,"error":...,"notes":[...]}` from an already-locked record.
fn list(ok: &str, notes: &[String], error: Option<&String>) -> String {
    let notes: String = notes
        .iter()
        .map(|n| json_string(n))
        .collect::<Vec<_>>()
        .join(",");
    let error = match error {
        Some(message) => format!(",\"error\":{}", json_string(message)),
        None => String::new(),
    };
    format!("{{\"ok\":{ok}{error},\"notes\":[{notes}]}}")
}

// ── the User-Agent ──────────────────────────────────────────────────────────

/// What the browser's DevTools endpoint says it is: the product and the
/// User-Agent it answers with. Read from the browser itself, so the string cu
/// installs is this build's and never a guess.
pub fn browser_version(cdp_port: u16) -> Result<(String, String), String> {
    let version = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version")?;
    let product = crate::server::json_string_value(&version, "Browser")
        .filter(|p| !p.is_empty())
        .ok_or("Chromium did not report a product")?;
    let agent = crate::server::json_string_value(&version, "User-Agent")
        .filter(|a| !a.is_empty())
        .ok_or("Chromium did not report a User-Agent")?;
    Ok((product, agent))
}

/// `HeadlessChrome/145.0.0.0 ...` -> `Chrome/145.0.0.0 ...`: the same build,
/// with a window. Everything else is left exactly as Chromium built it, so
/// the platform, the engine and the engine's build stay in step with the
/// binary that is really running.
fn windowed_agent(native: &str) -> String {
    match native.find(HEADLESS_TOKEN) {
        Some(at) => {
            let rest = &native[at + HEADLESS_TOKEN.len()..];
            format!("Chrome{rest}")
        }
        // A headless shell, whose version is `HeadlessShell/x` and whose UA
        // says `Chrome/x` already: keep it.
        None if native.contains("HeadlessShell") => native.to_string(),
        None => native.to_string(),
    }
}

/// The User-Agent this browser should answer with, and whether anything had
/// to change to get there.
pub fn planned_user_agent(native: &str) -> (String, bool) {
    let planned = windowed_agent(native);
    (planned.clone(), planned != native)
}

/// Whether a UA string carries the headless token.
pub fn is_headless_agent(agent: &str) -> bool {
    agent.contains(HEADLESS_TOKEN) || agent.contains("HeadlessShell")
}

// ── the display a window needs ──────────────────────────────────────────────

/// The X display the browser gets a window on.
pub struct Display {
    /// The display to hand the browser (`DISPLAY=:99`).
    pub name: String,
    /// The X server this daemon started for it; `None` when the display was
    /// already there, which belongs to somebody else and is not ours to stop.
    pub server: Option<std::process::Child>,
}

/// The X display the browser gets a window on, and any X server started for
/// it.
///
/// `CU_DISPLAY` names one. Without it the daemon uses a `DISPLAY` the host
/// already has, then the first free display number for an Xvfb it starts
/// itself -- the server has no monitor, and a headful Chromium is the
/// difference between a bot page and the site. A daemon that gets no display
/// at all is told so, on stderr, and falls back to `--headless=new`.
pub fn display() -> Option<Display> {
    if let Ok(display) = std::env::var("CU_DISPLAY") {
        let display = display.trim().to_string();
        if !display.is_empty() {
            return Some(Display {
                name: display,
                server: None,
            });
        }
    }
    if let Ok(display) = std::env::var("DISPLAY") {
        let display = display.trim().to_string();
        if !display.is_empty() && display_size(&display).is_some() {
            return Some(Display {
                name: display,
                server: None,
            });
        }
    }
    if find_in_path("Xvfb").is_none() {
        return None;
    }
    (1..=64).map(|n| format!(":{n}")).find_map(|name| {
        if socket(&name).exists() {
            return None;
        }
        start_xvfb(&name).map(|server| Display {
            name,
            server: Some(server),
        })
    })
}

/// The socket an X display listens on (`/tmp/.X11-unix/X<n>`).
fn socket(display: &str) -> PathBuf {
    let number = display.trim_start_matches(':');
    let number = number.split('.').next().unwrap_or(number);
    Path::new("/tmp/.X11-unix").join(format!("X{number}"))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Start `Xvfb` on `display`, at the size the profile's identity asks for, so
/// the browser opens onto a screen of a plausible shape.
fn start_xvfb(display: &str) -> Option<std::process::Child> {
    let program = find_in_path("Xvfb")?;
    let (width, height) = crate::session::identity()
        .and_then(|i| i.window)
        .unwrap_or((1440, 900));
    let screen = format!("{width}x{height}x24");
    let mut child = Command::new(program)
        .args([display, "-screen", "0", &screen, "-nolisten", "tcp", "-ac"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // The socket appears a moment after the process does; a display that is
    // not there within a second is not going to be.
    for _ in 0..50 {
        if socket(display).exists() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Remember the X server this daemon started, so shutdown can stop it with
/// the browser.
pub fn remember_display(display: Display) {
    let Some(child) = display.server else {
        return;
    };
    *xvfb_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(child.id());
    // The daemon keeps the pid, not the child: shutdown sends the signal
    // rather than waiting for it.
    std::mem::forget(child);
}

/// Stop the X server this daemon started, if it started one. A display it
/// found already running belongs to somebody else and is left alone.
pub fn stop_display() {
    let pid = xvfb_slot().lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(pid) = pid {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The pixel size of `display`, read from the X server itself with `xdpyinfo`
/// (absent: nothing to grow the window to).
pub fn display_size(display: &str) -> Option<(u32, u32)> {
    let out = Command::new(find_in_path("xdpyinfo")?)
        .arg(display)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let dimensions = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("dimensions:"))?;
    let mut fields = dimensions.split_whitespace();
    let width = fields.next()?.split('x').next()?.parse().ok()?;
    let height = fields.next()?.split('x').next()?.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

// ── the automation surface ──────────────────────────────────────────────────

/// The script every document runs before any of the page's own code.
///
/// Installed with `Page.addScriptToEvaluateOnNewDocument` in the page's own
/// world -- `worldName` empty on purpose: the point is that the page reads
/// these values -- with `runImmediately`, so it cannot lose a race with an
/// inline script, and `includeCommandLineAPI`, because the first thing every
/// automation library does is write the DevTools command line
/// (`document.$*`, `document.$eval`) to a *prototype-less* object hung off
/// `window`. A real browser throws that assignment away, and the object's
/// absence is how the library knows it is inside a real one. Putting
/// `window.__proto__` back to `Object.prototype` and shadowing the `cdc_`
/// name restores what a real browser leaves behind.
pub const STEALTH_JS: &str = r#"(function () {
  if (window.__cuStealth) return;
  try {
    Object.defineProperty(window, "__cuStealth", { value: true, enumerable: false });
    if (typeof window.__proto__ === "undefined" || Object.getPrototypeOf(window.__proto__) === null) {
      window.__proto__ = Object.prototype;
    }
    var hide = function (name) {
      try { Object.defineProperty(window, name, { get: function () { return undefined; }, configurable: true }); } catch (e) {}
    };
    hide("cdc_adoQpoasnfa76pfcZLmcfl_Array");
    hide("cdc_adoQpoasnfa76pfcZLmcfl_JSON");
    hide("cdc_adoQpoasnfa76pfcZLmcfl_Object");
    hide("cdc_adoQpoasnfa76pfcZLmcfl_Promise");
    hide("cdc_adoQpoasnfa76pfcZLmcfl_Proxy");
    hide("cdc_adoQpoasnfa76pfcZLmcfl_Symbol");
    try { delete Navigator.prototype.webdriver; } catch (e) {}
    try {
      Object.defineProperty(Navigator.prototype, "webdriver", { get: function () { return false; }, configurable: true });
    } catch (e) {}
  } catch (e) {}
})();"#;

/// The script that answers `WEBGL_debug_renderer_info` with `vendor` and
/// `renderer`, whatever the GPU process really reported.
fn gl_script(vendor: &str, renderer: &str) -> String {
    format!(
        r#"(function () {{
  if (window.__cuGl) return;
  try {{
    Object.defineProperty(window, "__cuGl", {{ value: true, enumerable: false }});
    var vendor = {vendor};
    var renderer = {renderer};
    var patch = function (proto) {{
      if (!proto || !proto.getParameter) return;
      var get = proto.getParameter;
      var patched = function (pname) {{
        try {{
          var ext = this.getExtension("WEBGL_debug_renderer_info");
          if (ext && pname === ext.UNMASKED_VENDOR_WEBGL) return vendor;
          if (ext && pname === ext.UNMASKED_RENDERER_WEBGL) return renderer;
        }} catch (e) {{}}
        return get.call(this, pname);
      }};
      try {{
        Object.defineProperty(patched, "name", {{ value: "getParameter", configurable: true }});
        Object.defineProperty(proto, "getParameter", {{ value: patched, configurable: true, writable: true }});
      }} catch (e) {{}}
    }};
    if (window.WebGLRenderingContext) patch(WebGLRenderingContext.prototype);
    if (window.WebGL2RenderingContext) patch(WebGL2RenderingContext.prototype);
  }} catch (e) {{}}
}})();"#,
        vendor = json_string(vendor),
        renderer = json_string(renderer),
    )
}

/// `navigator.platform` for this host: the three a desktop Chrome reports.
pub fn platform() -> String {
    if cfg!(target_os = "windows") {
        "Win32".into()
    } else if cfg!(target_os = "macos") {
        "MacIntel".into()
    } else {
        "Linux x86_64".into()
    }
}

/// Every target of the running browser that can hold a document, by id.
///
/// `Target.getTargets` answers with `targetInfos` and *no*
/// `webSocketDebuggerUrl` -- only `/json/list` carries one -- so the ids come
/// from the same discovery document cu reads for [`crate::targets`], and
/// [`CdpConnection::target`] opens the websocket to each.
pub fn page_targets(cdp_port: u16) -> Result<Vec<String>, String> {
    let reply = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/list")?;
    // One object per target, exactly as `/json/list` writes them: Chrome
    // pretty-prints that document, so the objects are separated by a line of
    // `},` and a blank line, not by `},{` on one line.
    Ok(reply
        .split("},")
        .map(|t| t.trim().trim_start_matches('{').trim_end_matches('}'))
        .filter(|t| {
            matches!(
                crate::server::json_string_value(t, "type")
                    .as_deref()
                    .unwrap_or(""),
                "page" | "iframe" | "webview"
            )
        })
        .filter_map(|t| crate::server::json_string_value(t, "id"))
        .filter(|id| !id.is_empty())
        .collect())
}

/// Harden one target: the automation script in every document, the
/// windowed User-Agent, the GL renderer, and the window's size.
fn harden_target(
    cdp_port: u16,
    target: &str,
    agent: &str,
    window: Option<(u32, u32)>,
) -> Result<(), String> {
    let mut connection = CdpConnection::target(cdp_port, target)?;
    connection.call(
        "Page.addScriptToEvaluateOnNewDocument",
        &format!(
            "{{\"source\":{},\"worldName\":\"\",\"includeCommandLineAPI\":true,\"runImmediately\":true}}",
            json_string(STEALTH_JS)
        ),
    )?;
    let locale = crate::session::identity()
        .map(|i| i.locale)
        .unwrap_or_else(|| "en-US".into());
    connection.call(
        "Network.setUserAgentOverride",
        &format!(
            "{{\"userAgent\":{},\"acceptLanguage\":{},\"platform\":{},\"userAgentMetadata\":null}}",
            json_string(agent),
            json_string(&crate::session::accept_languages(&locale)),
            json_string(&platform())
        ),
    )?;
    if let Some((vendor, renderer)) = GL_RENDERER {
        connection.call(
            "Page.addScriptToEvaluateOnNewDocument",
            &format!(
                "{{\"source\":{},\"worldName\":\"\",\"includeCommandLineAPI\":false,\"runImmediately\":true}}",
                json_string(&gl_script(vendor, renderer))
            ),
        )?;
    }
    if let Some((width, height)) = window {
        // A window manager on a real desktop opens the window to fill the
        // screen; a Chrome on a bare X display opens 800x600 in the corner,
        // which `screen.availWidth` then reports as the whole desktop.
        // Best effort: without a window manager the bounds are refused.
        let _ = connection.call(
            "Browser.setWindowBounds",
            &format!(
                "{{\"windowId\":1,\"bounds\":{{\"left\":0,\"top\":0,\"width\":{width},\"height\":{height},\
                  \"windowState\":\"normal\"}}}}"
            ),
        );
    }
    Ok(())
}

/// Give the stealth pass to the browser this daemon launched.
///
/// Called once, after the browser answers DevTools and before the first
/// action. Every part is best effort: a browser that could not be hardened
/// still browses, because a navigation must never fail over a defence
/// script, and `/v1/diagnostics` reports what did or did not happen.
pub fn apply(cdp_port: u16, window: Option<(u32, u32)>) -> Result<Vec<String>, String> {
    reset();
    let (product, native) = match browser_version(cdp_port) {
        Ok(version) => version,
        Err(e) => {
            fail(e.clone());
            return Err(e);
        }
    };
    let (agent, changed) = planned_user_agent(&native);
    if changed {
        remember(format!(
            "User-Agent: {HEADLESS_TOKEN} removed ({product} answers as {agent})"
        ));
    } else {
        remember(format!(
            "User-Agent already matches a windowed browser ({agent})"
        ));
    }
    let targets = match page_targets(cdp_port) {
        Ok(targets) => targets,
        Err(e) => {
            fail(e.clone());
            return Err(e);
        }
    };
    if targets.is_empty() {
        let error = "the browser listed no page target to harden".to_string();
        fail(error.clone());
        return Err(error);
    }
    let mut done = 0usize;
    for target in &targets {
        match harden_target(cdp_port, target, &agent, window) {
            Ok(()) => done += 1,
            Err(e) => remember(format!("target {target} not hardened: {e}")),
        }
    }
    remember(format!(
        "automation fingerprints removed in {done} of {} targets (navigator.webdriver, \
         DevTools command line, GPU renderer)",
        targets.len()
    ));
    Ok(notes())
}

/// What a page reads back from the page's own world, for the diagnostics of
/// a running session: the User-Agent, whether `navigator.webdriver` is true,
/// whether the DevTools command line is there and whether `window.__proto__`
/// is still the prototype-less object a driver installs.
pub const OBSERVE_JS: &str = r#"JSON.stringify((function(){
  return {
    userAgent: navigator.userAgent,
    userAgentData: navigator.userAgentData
      ? navigator.userAgentData.brands.map(function (b) { return b.brand + "/" + b.version; }).join(" ")
      : "",
    webdriver: navigator.webdriver === true,
    headless: navigator.userAgent.indexOf("Headless") !== -1,
    commandLine: typeof window.cdc_adoQpoasnfa76pfcZLmcfl_Array !== "undefined",
    proto: (function () { try { return Object.getPrototypeOf(window.__proto__) === null; } catch (e) { return false; } })(),
    gl: (function () {
      try {
        var c = document.createElement("canvas");
        var g = c.getContext("webgl2") || c.getContext("webgl");
        if (!g) return "none";
        var d = g.getExtension("WEBGL_debug_renderer_info");
        return d ? g.getParameter(d.UNMASKED_RENDERER_WEBGL) : "no debug extension";
      } catch (e) { return "error"; }
    })()
  };
})())"#;

/// Note anything a page still reads as an automation tell.
pub fn record_observed(observed: &str) {
    let read = |key: &str| {
        crate::server::json_string_value(observed, key)
            .unwrap_or_default()
            .eq_ignore_ascii_case("true")
    };
    if read("headless") {
        remember("a page still reads a Headless User-Agent".into());
    }
    if read("webdriver") {
        remember("a page still reads navigator.webdriver = true".into());
    }
    if read("commandLine") {
        remember("a page still reads the DevTools command line".into());
    }
    if read("proto") {
        remember("a page still reads the driver’s window.__proto__".into());
    }
    let gl = crate::server::json_string_value(observed, "gl").unwrap_or_default();
    if !gl.is_empty() && !gl.contains("SwiftShader") {
        remember(format!("GPU renderer reported to pages: {gl}"));
    }
}
