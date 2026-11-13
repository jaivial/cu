//! Device emulation that stays put: a phone or a tablet, on one tab, until it
//! is turned off.
//!
//! What a site checks before it serves its phone layout is more than the
//! viewport. Instagram's web story editor is the case this was measured on:
//! with `innerWidth` 390 and touch on but `screen.orientation` still
//! `landscape-primary` it answers "Rotate your device", and with a desktop
//! User-Agent it shows a tablet-app upsell instead of the phone page. A
//! [`Device`] is therefore the whole set a phone reports: the CSS viewport
//! and the screen behind it, a portrait (or landscape) orientation, a phone's
//! device pixel ratio, touch with five points, and an Android Chrome
//! User-Agent whose client hints (`navigator.userAgentData`, `Sec-CH-UA-*`)
//! say the same thing. The engine version in both is this browser's own,
//! read from `/json/version`, so the claim is the build that really answers.
//!
//! **Why a connection of its own.** Every one of these overrides belongs to
//! the DevTools session that set it, and Chromium drops them the moment that
//! session's websocket closes (measured on Chromium 156: metrics, touch and
//! User-Agent all fall back when the emulating connection closes, while a
//! reload or a navigation keeps them as long as it stays open). cu used to
//! set them on a pooled connection, which the daemon drops whenever a
//! navigation outlasts the settle budget, a read times out or the pool
//! overflows -- so a page that reloads itself (Instagram's editor does) could
//! come back as a desktop. Here the overrides are set on the tab's *keeper*
//! ([`crate::keeper`]): the one session cu holds open for the tab's whole
//! life, read all the time (it also answers the tab's JavaScript dialogs and
//! carries its console capture).
//!
//! The metrics are also shared state: any session's
//! `Emulation.clearDeviceMetricsOverride` clears them for every session (a
//! `cu shot --width` does exactly that when it is done), and re-sending the
//! same metrics on the keeper is not enough to bring them back -- Chromium
//! only half-applies a repeated identical override (the viewport comes back,
//! the screen, ratio and orientation do not). [`reassert`] clears on the
//! keeper first and sets again, which puts the whole device back.
//!
//! `desktop` resets what the keeper set (the keeper stays), so the tab is exactly
//! what an un-emulated cu tab is: the browser's own window and the same
//! User-Agent every other cu tab answers with.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use std::time::Duration;

use crate::keeper::Keeper;
use crate::server::{Tab, json_string, json_string_value};

/// Largest viewport side accepted, in CSS pixels (as for `cu shot`).
pub const MAX_SIDE: u32 = 8192;
/// Largest device scale factor accepted (as for `cu shot`).
pub const MAX_SCALE: f64 = 4.0;
/// Touch points a phone reports in `navigator.maxTouchPoints`.
pub const TOUCH_POINTS: u32 = 5;

/// What kind of device the User-Agent claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// An Android phone: `Mobile Safari` in the UA, `mobile: true` hints.
    Phone,
    /// An Android tablet: no `Mobile` token, `mobile: false` hints.
    Tablet,
    /// A desktop browser in a window of this size (`viewport` with
    /// `mobile: false`): the browser's own User-Agent, a mouse, no
    /// `mobile` flag. Only the viewport and the screen are overridden.
    Desktop,
}

impl Form {
    /// The device model the UA and the client hints name.
    pub fn model(self) -> &'static str {
        match self {
            Form::Phone => "Pixel 8",
            Form::Tablet => "Pixel Tablet",
            Form::Desktop => "desktop",
        }
    }
}

/// A full device to emulate on a tab.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    /// Viewport (and screen) width in CSS pixels.
    pub width: u32,
    /// Viewport (and screen) height in CSS pixels.
    pub height: u32,
    /// Device pixel ratio.
    pub scale: f64,
    /// Chromium's `mobile` flag: meta viewport, overlay scrollbars, text
    /// autosizing.
    pub mobile: bool,
    /// What the User-Agent says the device is.
    pub form: Form,
}

/// Android version the User-Agent and the client hints claim.
const ANDROID: &str = "14";
/// `navigator.platform` of Chrome on a 64-bit Android phone or tablet.
const ANDROID_PLATFORM: &str = "Linux armv81";

impl Device {
    /// Plan the device an `emulate` step asks for; `None` is `desktop`,
    /// which turns emulation off.
    ///
    /// - `mobile`: 390x844 at 3x, a modern phone;
    /// - `tablet`: 820x1180 at 2x;
    /// - `custom`: `width` x `height`, mobile below 820 px wide, at 3x when
    ///   mobile and 2x otherwise.
    ///
    /// `orientation` (`portrait` or `landscape`) turns the device: the sides
    /// are swapped when they do not match it. Without it the orientation
    /// follows the sides. `scale` overrides the pixel ratio of any mode.
    pub fn plan(
        mode: &str,
        width: Option<u32>,
        height: Option<u32>,
        orientation: Option<&str>,
        scale: Option<f64>,
    ) -> Result<Option<Self>, String> {
        Self::plan_with(mode, width, height, orientation, scale, None)
    }

    /// [`Device::plan`] with the `mobile` flag given for `custom`: `true`
    /// is a phone (touch, Android User-Agent) whatever the width, `false` a
    /// desktop window of that size ([`Form::Desktop`]); `None` decides by
    /// the width, as `plan` does.
    pub fn plan_with(
        mode: &str,
        width: Option<u32>,
        height: Option<u32>,
        orientation: Option<&str>,
        scale: Option<f64>,
        mobile_flag: Option<bool>,
    ) -> Result<Option<Self>, String> {
        let (mut w, mut h, mobile, form) = match mode {
            "desktop" => return Ok(None),
            "mobile" => (390, 844, true, Form::Phone),
            "tablet" => (820, 1180, true, Form::Tablet),
            "custom" => {
                let (w, h) = (width.unwrap_or(390), height.unwrap_or(844));
                let mobile = mobile_flag.unwrap_or(w < 820);
                let form = match (mobile, mobile_flag) {
                    (true, _) => Form::Phone,
                    (false, Some(false)) => Form::Desktop,
                    (false, _) => Form::Tablet,
                };
                (w, h, mobile, form)
            }
            _ => return Err("emulate \"mode\" must be mobile, tablet, desktop or custom".into()),
        };
        for (name, side) in [("width", w), ("height", h)] {
            if !(1..=MAX_SIDE).contains(&side) {
                return Err(format!(
                    "emulate \"{name}\" must be 1..{MAX_SIDE} CSS pixels, got {side}"
                ));
            }
        }
        match orientation {
            None => {}
            Some("portrait") if w > h => std::mem::swap(&mut w, &mut h),
            Some("landscape") if h > w => std::mem::swap(&mut w, &mut h),
            Some("portrait" | "landscape") => {}
            Some(other) => {
                return Err(format!(
                    "emulate \"orientation\" must be portrait or landscape, got {other}"
                ));
            }
        }
        let scale = match scale {
            Some(s) if s > 0.0 && s <= MAX_SCALE => s,
            Some(s) => {
                return Err(format!(
                    "emulate \"scale\" must be above 0 and up to {MAX_SCALE}, got {s}"
                ));
            }
            None if form == Form::Tablet => 2.0,
            None if form == Form::Desktop => 1.0,
            None => 3.0,
        };
        Ok(Some(Self {
            width: w,
            height: h,
            scale,
            mobile,
            form,
        }))
    }

    /// Whether the device is held landscape: wider than it is tall.
    pub fn landscape(&self) -> bool {
        self.width > self.height
    }

    /// `screen.orientation.type`, in CDP's spelling, and its angle.
    pub fn orientation(&self) -> (&'static str, u32) {
        if self.landscape() {
            ("landscapePrimary", 90)
        } else {
            ("portraitPrimary", 0)
        }
    }

    /// `Emulation.setDeviceMetricsOverride` params: the viewport, a screen of
    /// the same size, the pixel ratio and the orientation.
    pub fn metrics_params(&self) -> String {
        let (kind, angle) = self.orientation();
        format!(
            "{{\"width\":{w},\"height\":{h},\"deviceScaleFactor\":{s},\"mobile\":{m},\
             \"screenWidth\":{w},\"screenHeight\":{h},\
             \"screenOrientation\":{{\"type\":\"{kind}\",\"angle\":{angle}}}}}",
            w = self.width,
            h = self.height,
            s = self.scale,
            m = self.mobile,
        )
    }

    /// `Emulation.setTouchEmulationEnabled` params.
    pub fn touch_params(&self) -> String {
        format!("{{\"enabled\":true,\"maxTouchPoints\":{TOUCH_POINTS}}}")
    }

    /// `Emulation.setUserAgentOverride` params for `engine`: an Android
    /// Chrome of the same build, with client hints that agree with it.
    /// `accept_language` is the list the browser was launched with, so the
    /// override does not change what the HTTP header says.
    pub fn agent_params(&self, engine: &Engine, accept_language: Option<&str>) -> String {
        let language = accept_language
            .map(|l| format!("\"acceptLanguage\":{},", json_string(l)))
            .unwrap_or_default();
        format!(
            "{{\"userAgent\":{},{language}\"platform\":{},\"userAgentMetadata\":{}}}",
            json_string(&self.user_agent(engine)),
            json_string(ANDROID_PLATFORM),
            self.metadata(engine)
        )
    }

    /// The User-Agent string: Chrome on Android, with the reduced version
    /// (`156.0.0.0`) Chrome itself sends.
    pub fn user_agent(&self, engine: &Engine) -> String {
        let mobile = match self.form {
            Form::Phone => "Mobile ",
            Form::Tablet | Form::Desktop => "",
        };
        format!(
            "Mozilla/5.0 (Linux; Android {ANDROID}; {}) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{}.0.0.0 {mobile}Safari/537.36",
            self.form.model(),
            engine.major
        )
    }

    /// `userAgentMetadata`: what `navigator.userAgentData` and the
    /// `Sec-CH-UA-*` headers report. Without it an override empties
    /// `navigator.userAgentData`, which no real browser does.
    pub fn metadata(&self, engine: &Engine) -> String {
        let (brands, full) = brand_lists(engine);
        let form = match self.form {
            Form::Phone => "Mobile",
            Form::Tablet => "Tablet",
            Form::Desktop => "Desktop",
        };
        format!(
            "{{\"brands\":{brands},\"fullVersionList\":{full},\"fullVersion\":{},\
             \"platform\":\"Android\",\"platformVersion\":\"{ANDROID}.0.0\",\"architecture\":\"\",\
             \"model\":{},\"mobile\":{},\"bitness\":\"\",\"wow64\":false,\"formFactors\":[\"{form}\"]}}",
            json_string(&engine.full),
            json_string(self.form.model()),
            self.form == Form::Phone,
        )
    }

    /// One line for the agent: `390x844 @3x portrait, mobile, touch, Android
    /// Chrome 156 (Pixel 8)`.
    pub fn describe(&self, engine: &Engine) -> String {
        if self.form == Form::Desktop {
            return format!(
                "{}x{} @{}x desktop window: the browser's own User-Agent, mouse input",
                self.width, self.height, self.scale
            );
        }
        format!(
            "{}x{} @{}x {}, {}touch, Android Chrome {} ({})",
            self.width,
            self.height,
            self.scale,
            if self.landscape() {
                "landscape"
            } else {
                "portrait"
            },
            if self.mobile { "mobile, " } else { "" },
            engine.major,
            self.form.model()
        )
    }
}

/// The browser build the emulated User-Agent is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    /// `156`.
    pub major: String,
    /// `156.0.8078.79`.
    pub full: String,
}

impl Engine {
    /// From the `Browser` field of `/json/version`: `Chrome/156.0.8078.79`,
    /// `HeadlessChrome/156.0.8078.79`, or any `Name/x.y.z`.
    pub fn from_product(product: &str) -> Option<Self> {
        let version = product
            .rsplit_once('/')
            .map(|(_, v)| v)
            .unwrap_or(product)
            .trim();
        let major = version.split('.').next()?;
        if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let parts = version.split('.').count();
        let full = format!("{version}{}", ".0".repeat(4usize.saturating_sub(parts)));
        Some(Self {
            major: major.to_string(),
            full,
        })
    }

    /// The running browser's engine, read from its DevTools endpoint.
    pub fn of_browser(cdp_port: u16) -> Result<Self, String> {
        let version = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version")?;
        let product = json_string_value(&version, "Browser")
            .filter(|p| !p.is_empty())
            .ok_or("the browser did not report its product")?;
        Self::from_product(&product)
            .ok_or_else(|| format!("cannot read an engine version from {product:?}"))
    }
}

/// The GREASE brand Chromium pairs with `Chromium` for this major version,
/// and its version: the algorithm of
/// `embedder_support::GenerateBrandVersionList`, so the list is the one this
/// build really reports (measured: Chromium 156 says `Not:A-Brand` 8,
/// listed first).
pub fn grease(major: u32) -> (String, &'static str) {
    const CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const VERSIONS: [&str; 3] = ["8", "99", "24"];
    let seed = major as usize;
    (
        format!(
            "Not{}A{}Brand",
            CHARS[seed % CHARS.len()],
            CHARS[(seed + 1) % CHARS.len()]
        ),
        VERSIONS[seed % VERSIONS.len()],
    )
}

/// `brands` and `fullVersionList` as JSON arrays: `Chromium` at this
/// version and the GREASE brand, in the order Chromium shuffles them to.
pub fn brand_lists(engine: &Engine) -> (String, String) {
    let major: u32 = engine.major.parse().unwrap_or(0);
    let (grease_brand, grease_version) = grease(major);
    let mut entries = [
        (
            grease_brand,
            grease_version.to_string(),
            format!("{grease_version}.0.0.0"),
        ),
        (
            "Chromium".to_string(),
            engine.major.clone(),
            engine.full.clone(),
        ),
    ];
    // Two brands: the seed's parity decides which comes first.
    if major % 2 == 1 {
        entries.swap(0, 1);
    }
    let list = |full: bool| {
        let items: Vec<String> = entries
            .iter()
            .map(|(brand, short, long)| {
                format!(
                    "{{\"brand\":{},\"version\":{}}}",
                    json_string(brand),
                    json_string(if full { long } else { short })
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    };
    (list(false), list(true))
}

// ── the keeper ──────────────────────────────────────────────────────────────

/// One emulated tab: the device and the keeper session that holds it.
struct Held {
    device: Device,
    /// The commands that put the device on a tab, in order.
    commands: Vec<(&'static str, String)>,
    /// The generation of the keeper the device was put on: a different (or
    /// no) keeper means the session holding the overrides is gone.
    generation: Option<u64>,
}

type Registry = Mutex<HashMap<Tab, Arc<Mutex<Held>>>>;

fn registry() -> &'static Registry {
    static HELD: OnceLock<Registry> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

fn entry(tab: &Tab) -> Option<Arc<Mutex<Held>>> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
}

/// The commands that put `device` on a tab of a browser whose build is
/// `engine`. The metrics are cleared before they are set, so a repeat of an
/// identical override is applied in full rather than half.
pub fn commands(
    device: &Device,
    engine: &Engine,
    accept_language: Option<&str>,
) -> Vec<(&'static str, String)> {
    if device.form == Form::Desktop {
        // Only the window: the touch and User-Agent overrides an earlier
        // device put on this keeper are taken off.
        return vec![
            ("Emulation.clearDeviceMetricsOverride", "{}".to_string()),
            (
                "Emulation.setDeviceMetricsOverride",
                device.metrics_params(),
            ),
            (
                "Emulation.setTouchEmulationEnabled",
                "{\"enabled\":false}".to_string(),
            ),
            (
                "Emulation.setUserAgentOverride",
                "{\"userAgent\":\"\"}".to_string(),
            ),
        ];
    }
    vec![
        ("Emulation.clearDeviceMetricsOverride", "{}".to_string()),
        (
            "Emulation.setDeviceMetricsOverride",
            device.metrics_params(),
        ),
        ("Emulation.setTouchEmulationEnabled", device.touch_params()),
        (
            "Emulation.setUserAgentOverride",
            device.agent_params(engine, accept_language),
        ),
    ]
}

/// The commands that undo [`commands`] on the session that sent them: an
/// empty User-Agent is "no override".
pub const RESET: [(&str, &str); 3] = [
    ("Emulation.clearDeviceMetricsOverride", "{}"),
    ("Emulation.setTouchEmulationEnabled", "{\"enabled\":false}"),
    ("Emulation.setUserAgentOverride", "{\"userAgent\":\"\"}"),
];

/// How long the browser gets to take the overrides.
const SEND_WAIT: Duration = Duration::from_secs(5);

/// What went wrong sending the device: the socket (worth a fresh keeper) or
/// the browser refusing a command (not).
enum Failure {
    Socket(String),
    Refused(String),
}

/// Send `commands` on the keeper in one round trip; the first refusal is
/// the error.
fn send_all(keeper: &Keeper, commands: &[(&str, &str)]) -> Result<(), Failure> {
    let replies = keeper.call_many(commands, SEND_WAIT);
    for (index, reply) in replies.into_iter().enumerate() {
        let reply = reply.map_err(Failure::Socket)?;
        if reply.contains("\"error\":{") {
            let message = json_string_value(&reply, "message").unwrap_or(reply);
            return Err(Failure::Refused(format!(
                "{}: {message}",
                commands[index].0
            )));
        }
    }
    Ok(())
}

/// The Accept-Language the browser was launched with (the profile's
/// identity), so the User-Agent override keeps the header as it was.
fn accept_language() -> Option<String> {
    crate::session::identity().map(|i| crate::session::accept_languages(&i.locale))
}

/// Emulate `device` on `tab` from the tab's keeper session, replacing any
/// emulation the tab had. Returns a one-line description of what is now in
/// force.
pub fn apply(tab: &Tab, device: Device) -> Result<String, String> {
    let engine = Engine::of_browser(tab.cdp_port)?;
    let commands = commands(&device, &engine, accept_language().as_deref());
    let slot = {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(tab.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Held {
                    device: device.clone(),
                    commands: Vec::new(),
                    generation: None,
                }))
            })
            .clone()
    };
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    held.device = device.clone();
    held.commands = commands;
    if let Err(error) = put_on(&mut held, tab) {
        // Nothing holds the device: do not pretend it is in force.
        if let Some(keeper) = current_keeper(tab) {
            let _ = send_all(&keeper, &RESET);
        }
        drop(held);
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        if map.get(tab).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
            map.remove(tab);
        }
        return Err(error);
    }
    Ok(device.describe(&engine))
}

/// Put the remembered device on the tab over its keeper, opening a keeper
/// when there is none or the one there was has died. A socket that breaks
/// under the commands gets one retry on a fresh keeper.
fn put_on(held: &mut Held, tab: &Tab) -> Result<(), String> {
    let commands: Vec<(&str, String)> = held.commands.clone();
    let commands: Vec<(&str, &str)> = commands.iter().map(|(m, p)| (*m, p.as_str())).collect();
    for attempt in 0..2 {
        let keeper = crate::keeper::ensure(tab)?;
        match send_all(&keeper, &commands) {
            Ok(()) => {
                held.generation = Some(keeper.generation);
                return Ok(());
            }
            Err(Failure::Socket(_)) if attempt == 0 && !keeper.alive() => continue,
            Err(Failure::Socket(error) | Failure::Refused(error)) => return Err(error),
        }
    }
    Err("the browser did not take the emulation".into())
}

/// The keeper of `tab`, if one is open.
fn current_keeper(tab: &Tab) -> Option<Arc<Keeper>> {
    tab.resolved_target()
        .and_then(|target| crate::keeper::existing(tab.cdp_port, &target))
}

/// Turn emulation off on `tab` (`desktop`): reset what the keeper set, so
/// the tab is back to the browser's own window and cu's own User-Agent. The
/// keeper itself stays: it also watches the tab for dialogs. Returns whether
/// the tab had been emulated.
pub fn clear(tab: &Tab) -> bool {
    let slot = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tab);
    let Some(slot) = slot else { return false };
    let held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(keeper) = current_keeper(tab)
        && Some(keeper.generation) == held.generation
    {
        let _ = send_all(&keeper, &RESET);
    }
    true
}

/// The device `tab` is emulating, if any.
pub fn device(tab: &Tab) -> Option<Device> {
    entry(tab).map(|slot| {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .device
            .clone()
    })
}

/// Whether input on `tab` should be a finger rather than a mouse: it is
/// emulating a touch device.
pub fn touches(tab: &Tab) -> bool {
    device(tab).is_some_and(|d| d.form != Form::Desktop)
}

/// Before a command on `tab`: if the tab is emulated and the keeper the
/// device was put on has died (the browser restarted, the socket was cut),
/// put the device back on the tab's new keeper. Otherwise it costs a map
/// lookup.
pub fn ensure(tab: &Tab) {
    let Some(slot) = entry(tab) else { return };
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if current_keeper(tab).is_some_and(|k| Some(k.generation) == held.generation) {
        // The keeper is the one the device went on; check the page still
        // sees it (a cross-process navigation, a capture another tool made
        // at another size): the screen is the device's whenever the
        // override is in force. Not on a page a dialog has stopped, where
        // an evaluation would wait for the dialog.
        let width = held.device.width;
        if crate::dialogs::blocked(tab).is_none()
            && let Ok(reply) = tab.command(
                "Runtime.evaluate",
                "{\"expression\":\"screen.width\",\"returnByValue\":true}",
            )
            && let Some(seen) = crate::server::json_number(&reply, "value")
            && seen as u32 != width
        {
            eprintln!(
                "cu: emulation: the page saw a {seen} px screen instead of {width}; the device is put back"
            );
            if let Err(error) = put_on(&mut held, tab) {
                eprintln!("cu: emulation: could not put the device back on the tab: {error}");
            }
        }
        return;
    }
    match put_on(&mut held, tab) {
        Ok(()) => eprintln!("cu: emulation: the keeper had gone; the device is back on the tab"),
        Err(error) => eprintln!("cu: emulation: could not put the device back on the tab: {error}"),
    }
}

/// Put the device back after something else replaced or cleared the
/// metrics (a `cu shot --width` capture): clear on the keeper, then set it
/// all again. Returns whether the tab was emulated.
pub fn reassert(tab: &Tab) -> bool {
    let Some(slot) = entry(tab) else { return false };
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(error) = put_on(&mut held, tab) {
        eprintln!("cu: emulation: could not put the device back on the tab: {error}");
    }
    true
}

/// The tab is gone: forget its device (its keeper goes with the tab).
pub fn forget(tab: &Tab) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tab);
}

/// A tab an emulated tab opened (a popup, a `target=_blank` link) is the
/// same device: on the first command on it, the opener's device goes on it
/// too, as it would on a real phone, where every page has the phone's
/// screen.
pub fn inherit(tab: &Tab) {
    let Some(target) = tab.target.as_deref() else {
        return;
    };
    if entry(tab).is_some() {
        return;
    }
    let Some(opener) = crate::targets::opener_of(target) else {
        return;
    };
    let Some(device) = device(&Tab {
        cdp_port: tab.cdp_port,
        target: Some(opener.clone()),
    }) else {
        return;
    };
    match apply(tab, device) {
        Ok(what) => {
            eprintln!("cu: emulation: popup {target} of {opener} is the same device: {what}")
        }
        Err(error) => {
            eprintln!("cu: emulation: popup {target} could not take its opener's device: {error}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::from_product("Chrome/156.0.8078.79").unwrap()
    }

    fn mobile() -> Device {
        Device::plan("mobile", None, None, None, None)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn mobile_is_a_portrait_phone_at_three_x() {
        let d = mobile();
        assert_eq!(
            (d.width, d.height, d.scale, d.mobile),
            (390, 844, 3.0, true)
        );
        assert_eq!(d.form, Form::Phone);
        assert_eq!(d.orientation(), ("portraitPrimary", 0));
        assert_eq!(
            d.metrics_params(),
            "{\"width\":390,\"height\":844,\"deviceScaleFactor\":3,\"mobile\":true,\
             \"screenWidth\":390,\"screenHeight\":844,\
             \"screenOrientation\":{\"type\":\"portraitPrimary\",\"angle\":0}}"
        );
        assert_eq!(d.touch_params(), "{\"enabled\":true,\"maxTouchPoints\":5}");
    }

    #[test]
    fn tablet_is_two_x_and_says_tablet() {
        let d = Device::plan("tablet", None, None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!((d.width, d.height, d.scale), (820, 1180, 2.0));
        assert_eq!(d.form, Form::Tablet);
        assert_eq!(
            d.user_agent(&engine()),
            "Mozilla/5.0 (Linux; Android 14; Pixel Tablet) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36"
        );
        let meta = d.metadata(&engine());
        assert!(meta.contains("\"mobile\":false"), "{meta}");
        assert!(meta.contains("\"formFactors\":[\"Tablet\"]"), "{meta}");
    }

    #[test]
    fn desktop_plans_nothing() {
        assert_eq!(Device::plan("desktop", None, None, None, None), Ok(None));
    }

    #[test]
    fn the_orientation_follows_the_sides_or_turns_them() {
        // Unasked: wider than tall is landscape, at 90 degrees.
        let wide = Device::plan("custom", Some(844), Some(390), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(wide.orientation(), ("landscapePrimary", 90));
        assert!(wide.metrics_params().contains(
            "\"screenWidth\":844,\"screenHeight\":390,\"screenOrientation\":{\"type\":\"landscapePrimary\",\"angle\":90}"
        ));
        // Asked for: the phone is turned, and its sides swap.
        let turned = Device::plan("mobile", None, None, Some("landscape"), None)
            .unwrap()
            .unwrap();
        assert_eq!((turned.width, turned.height), (844, 390));
        assert_eq!(turned.orientation(), ("landscapePrimary", 90));
        let upright = Device::plan("custom", Some(844), Some(390), Some("portrait"), None)
            .unwrap()
            .unwrap();
        assert_eq!((upright.width, upright.height), (390, 844));
        assert_eq!(upright.orientation(), ("portraitPrimary", 0));
        assert!(Device::plan("mobile", None, None, Some("sideways"), None).is_err());
    }

    #[test]
    fn custom_keeps_its_mobile_rule_and_takes_a_scale() {
        let small = Device::plan("custom", Some(360), Some(740), None, None)
            .unwrap()
            .unwrap();
        assert!(small.mobile);
        assert_eq!((small.scale, small.form), (3.0, Form::Phone));
        let big = Device::plan("custom", Some(1024), Some(1366), None, Some(1.5))
            .unwrap()
            .unwrap();
        assert!(!big.mobile);
        assert_eq!((big.scale, big.form), (1.5, Form::Tablet));
        assert!(Device::plan("custom", Some(0), Some(10), None, None).is_err());
        assert!(Device::plan("custom", Some(390), Some(9000), None, None).is_err());
        assert!(Device::plan("mobile", None, None, None, Some(0.0)).is_err());
        assert!(Device::plan("mobile", None, None, None, Some(5.0)).is_err());
        assert!(Device::plan("phone", None, None, None, None).is_err());
    }

    #[test]
    fn the_engine_comes_from_the_browsers_product() {
        assert_eq!(
            engine(),
            Engine {
                major: "156".into(),
                full: "156.0.8078.79".into()
            }
        );
        let headless = Engine::from_product("HeadlessChrome/145.0.7632.6").unwrap();
        assert_eq!(headless.major, "145");
        assert_eq!(
            Engine::from_product("FakeChromium/1.0").unwrap().full,
            "1.0.0.0"
        );
        assert_eq!(Engine::from_product("Chrome/x"), None);
    }

    #[test]
    fn the_phone_user_agent_is_android_chrome_of_this_build() {
        let d = mobile();
        assert_eq!(
            d.user_agent(&engine()),
            "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Mobile Safari/537.36"
        );
        let params = d.agent_params(&engine(), Some("es-ES,es"));
        assert!(
            params.contains("\"acceptLanguage\":\"es-ES,es\""),
            "{params}"
        );
        assert!(params.contains("\"platform\":\"Linux armv81\""), "{params}");
        let meta = d.metadata(&engine());
        for part in [
            "\"platform\":\"Android\"",
            "\"platformVersion\":\"14.0.0\"",
            "\"model\":\"Pixel 8\"",
            "\"mobile\":true",
            "\"fullVersion\":\"156.0.8078.79\"",
            "{\"brand\":\"Chromium\",\"version\":\"156\"}",
            "{\"brand\":\"Chromium\",\"version\":\"156.0.8078.79\"}",
        ] {
            assert!(meta.contains(part), "{part} missing from {meta}");
        }
        // No language from the profile: the header is left as it was.
        assert!(!d.agent_params(&engine(), None).contains("acceptLanguage"));
    }

    #[test]
    fn the_brands_are_the_ones_chromium_reports() {
        // Measured on Chromium 156.0.8078.79: navigator.userAgentData.brands
        // is [Not:A-Brand 8, Chromium 156], and fullVersionList the same
        // with 8.0.0.0 and the full build.
        let (brands, full) = brand_lists(&engine());
        assert_eq!(
            brands,
            "[{\"brand\":\"Not:A-Brand\",\"version\":\"8\"},{\"brand\":\"Chromium\",\"version\":\"156\"}]"
        );
        assert_eq!(
            full,
            "[{\"brand\":\"Not:A-Brand\",\"version\":\"8.0.0.0\"},{\"brand\":\"Chromium\",\"version\":\"156.0.8078.79\"}]"
        );
        // An odd major puts Chromium first.
        let odd = Engine::from_product("Chrome/145.0.1.2").unwrap();
        assert!(brand_lists(&odd).0.starts_with("[{\"brand\":\"Chromium\""));
    }

    #[test]
    fn the_device_is_cleared_before_it_is_set() {
        let methods: Vec<&str> = commands(&mobile(), &engine(), None)
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        assert_eq!(
            methods,
            [
                "Emulation.clearDeviceMetricsOverride",
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setTouchEmulationEnabled",
                "Emulation.setUserAgentOverride"
            ]
        );
        for (_, params) in commands(&mobile(), &engine(), Some("en-US,en")) {
            assert!(params.starts_with('{') && params.ends_with('}'), "{params}");
        }
        // The reset undoes each of them.
        assert_eq!(RESET.len(), 3);
        assert!(
            RESET
                .iter()
                .any(|(m, p)| *m == "Emulation.setUserAgentOverride" && p.contains("\"\""))
        );
    }

    #[test]
    fn the_description_says_what_is_in_force() {
        assert_eq!(
            mobile().describe(&engine()),
            "390x844 @3x portrait, mobile, touch, Android Chrome 156 (Pixel 8)"
        );
    }
}
