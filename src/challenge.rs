//! Is the page the agent asked for, or a bot-defence page standing in front
//! of it?
//!
//! A navigation that "succeeded" onto a Cloudflare interstitial, a DataDome
//! CAPTCHA or an Akamai "Access Denied" is not a success, and an agent that
//! keeps clicking on one burns the session's reputation and loops. Each page
//! is therefore classified after it settles:
//!
//! - `ready`: an http(s) page that answered 2xx/3xx and shows no defence
//!   signal -- the only state that means "go on";
//! - `challenge_pending`: an automatic check is running (an interstitial that
//!   may clear by itself) or a CAPTCHA widget sits on an otherwise normal
//!   page;
//! - `human_required`: an interactive challenge is on screen, or an
//!   interstitial did not clear within its budget. Automatic interaction with
//!   the tab stops until a person has dealt with it (see [`gate`]);
//! - `blocked`: the site refused the client outright;
//! - `rate_limited`: 429 or a vendor rate-limit page;
//! - `unknown`: the evidence does not decide it (a 403 with no known marker,
//!   no response status, a page still loading). Never reported as ready.
//!
//! The probe only reads the DOM, from cu's isolated world, so the page's own
//! scripts never see it run. Nothing here solves or evades a challenge, and
//! no URL query, cookie or token is ever reported: URLs are cut to origin and
//! path because challenge URLs carry tokens (`__cf_chl_tk`, DataDome `cid`).
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::server::{
    Tab, evaluated_string, flatten_frame_tree, json_array, json_number, json_string,
    json_string_value, json_strings, json_value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChallengeState {
    Ready,
    ChallengePending,
    HumanRequired,
    Blocked,
    RateLimited,
    Unknown,
}

impl ChallengeState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::ChallengePending => "challenge_pending",
            Self::HumanRequired => "human_required",
            Self::Blocked => "blocked",
            Self::RateLimited => "rate_limited",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this state is a defence the origin put up (and so a reason
    /// for the scheduler to back off), as opposed to ready or undecided.
    pub fn is_defence(self) -> bool {
        matches!(
            self,
            Self::ChallengePending | Self::HumanRequired | Self::Blocked | Self::RateLimited
        )
    }
}

/// What the in-page probe saw. Built by [`PROBE_JS`] plus the frame tree.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub url: String,
    pub title: String,
    pub ready_state: String,
    /// Main document HTTP status from the navigation timing entry; 0 when
    /// the browser did not record one.
    pub status: u16,
    /// The start of the body text, lower-cased.
    pub text: String,
    /// Named DOM markers the probe found (see `PROBE_JS`).
    pub markers: Vec<String>,
    /// Every frame URL known: frame tree and iframe `src`s, with a `!`
    /// prefix when the iframe element is visible.
    pub frames: Vec<String>,
}

/// The classification and the evidence it rests on.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub state: ChallengeState,
    pub vendor: Option<&'static str>,
    /// An interstitial that may clear by itself (worth a bounded wait).
    pub interstitial: bool,
    pub signals: Vec<String>,
}

impl Verdict {
    fn new(state: ChallengeState, vendor: Option<&'static str>, signals: Vec<String>) -> Self {
        Self {
            state,
            vendor,
            interstitial: false,
            signals,
        }
    }
}

fn has(probe: &Probe, marker: &str) -> bool {
    probe.markers.iter().any(|m| m == marker)
}

/// A frame whose URL contains `needle`; `visible` restricts to iframes
/// whose element is on screen.
fn frame(probe: &Probe, needle: &str, visible: bool) -> bool {
    probe.frames.iter().any(|f| {
        let shown = f.starts_with('!');
        f.contains(needle) && (!visible || shown)
    })
}

fn text_has(probe: &Probe, phrases: &[&str]) -> Option<String> {
    phrases
        .iter()
        .find(|p| probe.text.contains(*p) || probe.title.to_lowercase().contains(*p))
        .map(|p| format!("text: \"{p}\""))
}

/// Decide the state from what the probe saw. Ordered from the most to the
/// least specific; anything that is not clearly a normal page ends as
/// `unknown`, never `ready`.
pub fn classify(probe: &Probe) -> Verdict {
    use ChallengeState::*;
    let title = probe.title.to_lowercase();
    let status = probe.status;

    // Rate limits first: a 429 is unambiguous whatever the page says.
    if status == 429 {
        return Verdict::new(RateLimited, None, vec!["status 429".into()]);
    }
    if let Some(s) = text_has(probe, &["error 1015", "you are being rate limited"]) {
        return Verdict::new(RateLimited, Some("cloudflare"), vec![s]);
    }

    // DataDome: the CAPTCHA frame says which page it is with `t=`: `fe` is a
    // challenge, `bv` a block.
    if frame(probe, "captcha-delivery.com", false) {
        if frame(probe, "t=bv", false) {
            return Verdict::new(
                Blocked,
                Some("datadome"),
                vec!["datadome block frame".into()],
            );
        }
        return Verdict::new(
            HumanRequired,
            Some("datadome"),
            vec!["datadome captcha frame".into()],
        );
    }

    // Cloudflare block pages (WAF rule, 1020 and friends).
    if let Some(s) = text_has(
        probe,
        &[
            "sorry, you have been blocked",
            "error 1020",
            "error code: 1020",
            "error 1006",
            "error 1009",
            "error 1010",
            "error 1012",
        ],
    ) && (probe.text.contains("cloudflare") || has(probe, "cf-error"))
    {
        return Verdict::new(Blocked, Some("cloudflare"), vec![s]);
    }

    // Cloudflare interstitial ("Just a moment...", managed challenge).
    let cf_page = title.starts_with("just a moment")
        || title.starts_with("attention required")
        || has(probe, "cf-challenge")
        || probe.url.contains("__cf_chl");
    let cf_text = text_has(
        probe,
        &[
            "verify you are human",
            "verifying you are human",
            "checking your browser",
            "checking if the site connection is secure",
            "performing security verification",
            "enable javascript and cookies to continue",
        ],
    );
    if cf_page || (cf_text.is_some() && has(probe, "cf-platform")) {
        let mut signals = vec![];
        if cf_page {
            signals.push(format!("title/markers: {}", probe.title));
        }
        signals.extend(cf_text);
        let mut verdict = Verdict::new(ChallengePending, Some("cloudflare"), signals);
        verdict.interstitial = true;
        return verdict;
    }

    // PerimeterX / HUMAN press-and-hold.
    if has(probe, "px-captcha") {
        return Verdict::new(
            HumanRequired,
            Some("perimeterx"),
            vec!["#px-captcha".into()],
        );
    }

    // Imperva / Incapsula.
    if probe.text.contains("incapsula incident id") || frame(probe, "_Incapsula_Resource", false) {
        if frame(probe, "_Incapsula_Resource", true)
            && (has(probe, "captcha") || frame(probe, "captcha", false))
        {
            return Verdict::new(
                HumanRequired,
                Some("imperva"),
                vec!["incapsula captcha".into()],
            );
        }
        return Verdict::new(
            Blocked,
            Some("imperva"),
            vec!["incapsula incident page".into()],
        );
    }

    // Akamai edge refusal.
    if title.contains("access denied")
        && probe.text.contains("you don't have permission to access")
        && probe.text.contains("reference #")
    {
        return Verdict::new(Blocked, Some("akamai"), vec!["akamai access denied".into()]);
    }

    // AWS WAF: a 405 CAPTCHA page or a 202 silent challenge.
    if has(probe, "awswaf") {
        if status == 405 || has(probe, "awswaf-captcha") {
            return Verdict::new(
                HumanRequired,
                Some("aws-waf"),
                vec!["aws waf captcha".into()],
            );
        }
        if status == 202 {
            let mut verdict = Verdict::new(
                ChallengePending,
                Some("aws-waf"),
                vec!["aws waf challenge".into()],
            );
            verdict.interstitial = true;
            return verdict;
        }
    }

    // A CAPTCHA puzzle open on screen, whatever the page.
    if frame(probe, "recaptcha/api2/bframe", true)
        || frame(probe, "recaptcha/enterprise/bframe", true)
    {
        return Verdict::new(
            HumanRequired,
            Some("recaptcha"),
            vec!["recaptcha challenge open".into()],
        );
    }
    if probe
        .frames
        .iter()
        .any(|f| f.starts_with('!') && f.contains("hcaptcha.com") && f.contains("frame=challenge"))
    {
        return Verdict::new(
            HumanRequired,
            Some("hcaptcha"),
            vec!["hcaptcha challenge open".into()],
        );
    }
    if frame(probe, "arkoselabs.com", true) || frame(probe, "funcaptcha.com", true) {
        return Verdict::new(
            HumanRequired,
            Some("arkose"),
            vec!["arkose challenge".into()],
        );
    }

    // A widget on an otherwise normal page: it may pass invisibly or may need
    // a click before the form submits. Not an interstitial: no waiting.
    for (needle, vendor) in [
        ("challenges.cloudflare.com", "cloudflare-turnstile"),
        ("recaptcha/api2/anchor", "recaptcha"),
        ("recaptcha/enterprise/anchor", "recaptcha"),
        ("hcaptcha.com", "hcaptcha"),
    ] {
        if probe
            .frames
            .iter()
            .any(|f| f.starts_with('!') && f.contains(needle) && !f.contains("size=invisible"))
        {
            return Verdict::new(
                ChallengePending,
                Some(vendor),
                vec![format!("{vendor} widget on the page")],
            );
        }
    }
    if has(probe, "turnstile-widget") {
        return Verdict::new(
            ChallengePending,
            Some("cloudflare-turnstile"),
            vec!["turnstile widget on the page".into()],
        );
    }

    // No vendor signal. Only a plainly healthy page is ready.
    let web = probe.url.starts_with("http://") || probe.url.starts_with("https://");
    if !web {
        return Verdict::new(Ready, None, vec!["not an http(s) page".into()]);
    }
    if probe.ready_state == "loading" {
        return Verdict::new(Unknown, None, vec!["page still loading".into()]);
    }
    match status {
        200..=399 => Verdict::new(Ready, None, vec![format!("status {status}, no signals")]),
        0 => Verdict::new(Unknown, None, vec!["no response status recorded".into()]),
        403 | 503 => Verdict::new(
            Unknown,
            None,
            vec![format!("status {status} with no known vendor marker")],
        ),
        other => Verdict::new(Unknown, None, vec![format!("status {other}")]),
    }
}

/// The in-page half of the probe: DOM reads only, returned as JSON. Runs in
/// cu's isolated world, so page scripts neither see it nor can fake its
/// globals; nothing it touches is written.
pub const PROBE_JS: &str = r#"JSON.stringify((function(){
  const q = (s) => { try { return document.querySelector(s); } catch (e) { return null; } };
  const shown = (el) => {
    if (!el) return false;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none' && s.opacity !== '0';
  };
  const markers = [];
  const mark = (name, sel, visible) => { const el = q(sel); if (el && (!visible || shown(el))) markers.push(name); };
  mark('cf-challenge', '#challenge-form, #challenge-running, #cf-challenge-running, #challenge-stage, .cf-browser-verification, #cf-please-wait', false);
  mark('cf-platform', 'script[src*="/cdn-cgi/challenge-platform/h/"], script[src*="/cdn-cgi/challenge-platform/orchestrate/"]', false);
  mark('cf-error', '#cf-error-details, .cf-error-overview, #cf-wrapper', false);
  mark('turnstile-widget', '.cf-turnstile', true);
  mark('px-captcha', '#px-captcha', true);
  mark('awswaf', 'script[src*="awswaf.com"], script[src*="token.awswaf"]', false);
  mark('awswaf-captcha', '#captcha-container, awswaf-captcha', true);
  mark('captcha', '[id*="captcha" i], [class*="captcha" i]', true);
  const frames = [...document.querySelectorAll('iframe, frame')].map((f) => (shown(f) ? '!' : '') + (f.src || ''));
  let status = 0;
  try { const nav = performance.getEntriesByType('navigation')[0]; status = (nav && nav.responseStatus) || 0; } catch (e) {}
  const body = document.body ? (document.body.innerText || '') : '';
  return { url: location.href, title: document.title || '', ready: document.readyState, status,
           text: body.slice(0, 3000).toLowerCase(), markers, frames };
})())"#;

/// Origin and path of `url`, without query or fragment: what is safe to log.
pub fn redact(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    url[..cut].to_string()
}

/// Run the probe through `cmd` (a tab's pooled command or a batch's
/// connection): the frame tree, cu's isolated world of the main frame, and
/// one evaluation there.
pub fn probe<F>(mut cmd: F) -> Result<Probe, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let tree = cmd("Page.getFrameTree", "{}")?;
    let frames = flatten_frame_tree(&tree);
    let main = frames.first().ok_or("page has no main frame")?;
    let world = cmd(
        "Page.createIsolatedWorld",
        &format!(
            "{{\"frameId\":{},\"worldName\":\"cu\"}}",
            json_string(&main.id)
        ),
    )?;
    let context = json_value(&world, "executionContextId").ok_or("the page is gone")?;
    let reply = cmd(
        "Runtime.evaluate",
        &format!(
            "{{\"expression\":{},\"contextId\":{context},\"returnByValue\":true}}",
            json_string(PROBE_JS)
        ),
    )?;
    let value = evaluated_string(&reply).ok_or("the page did not answer the probe")?;
    let mut probe = Probe {
        url: json_string_value(&value, "url").unwrap_or_default(),
        title: json_string_value(&value, "title").unwrap_or_default(),
        ready_state: json_string_value(&value, "ready").unwrap_or_default(),
        status: json_number(&value, "status").unwrap_or(0.0) as u16,
        text: json_string_value(&value, "text").unwrap_or_default(),
        markers: json_array(&value, "markers")
            .map(|a| json_strings(&a))
            .unwrap_or_default(),
        frames: json_array(&value, "frames")
            .map(|a| json_strings(&a))
            .unwrap_or_default(),
    };
    // The frame tree also holds frames the DOM query cannot reach (inside
    // closed shadow roots); their visibility is unknown, so no `!`.
    probe
        .frames
        .extend(frames.iter().skip(1).map(|f| f.url.clone()));
    Ok(probe)
}

/// How long an interstitial is given to clear by itself before it is handed
/// to a person: `CU_CHALLENGE_WAIT_MS`, default 10 s, at most 60 s.
pub fn wait_budget() -> Duration {
    let ms = std::env::var("CU_CHALLENGE_WAIT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10_000)
        .min(60_000);
    Duration::from_millis(ms)
}

/// Probe, and if the page is an interstitial, keep probing every 500 ms
/// until it clears or `budget` is spent; one that does not clear becomes
/// `human_required`. A probe that fails mid-navigation (the interstitial
/// reloading into the real page) is retried, not reported.
pub fn assess<F>(mut cmd: F, budget: Duration) -> Assessment
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let started = Instant::now();
    loop {
        let error = match probe(&mut cmd) {
            Ok(probe) => {
                let mut verdict = classify(&probe);
                let waited = started.elapsed();
                if verdict.interstitial && verdict.state == ChallengeState::ChallengePending {
                    if waited < budget {
                        std::thread::sleep(Duration::from_millis(500).min(budget - waited));
                        continue;
                    }
                    if !budget.is_zero() {
                        verdict.state = ChallengeState::HumanRequired;
                        verdict.signals.push(format!(
                            "interstitial did not clear within {} ms",
                            budget.as_millis()
                        ));
                    }
                }
                return Assessment::new(&probe, verdict, waited);
            }
            Err(e) => e,
        };
        if started.elapsed() >= budget.max(Duration::from_millis(1500)) {
            return Assessment {
                state: ChallengeState::Unknown,
                vendor: None,
                signals: vec![format!("probe failed: {error}")],
                url: String::new(),
                title: String::new(),
                status: 0,
                waited: started.elapsed(),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One assessment of one tab, safe to log and to hand back to the agent.
#[derive(Clone, Debug)]
pub struct Assessment {
    pub state: ChallengeState,
    pub vendor: Option<&'static str>,
    pub signals: Vec<String>,
    /// Origin and path only.
    pub url: String,
    pub title: String,
    pub status: u16,
    pub waited: Duration,
}

impl Assessment {
    fn new(probe: &Probe, verdict: Verdict, waited: Duration) -> Self {
        Self {
            state: verdict.state,
            vendor: verdict.vendor,
            signals: verdict.signals.into_iter().map(|s| redact(&s)).collect(),
            url: redact(&probe.url),
            title: probe.title.chars().take(120).collect(),
            status: probe.status,
            waited,
        }
    }

    pub fn json(&self) -> String {
        let mut out = format!(
            "{{\"state\":{},\"vendor\":{},\"url\":{},\"title\":{},\"status\":{},\"waited_ms\":{},\"signals\":[{}]",
            json_string(self.state.name()),
            self.vendor
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_string(&self.url),
            json_string(&self.title),
            self.status,
            self.waited.as_millis(),
            self.signals
                .iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(",")
        );
        if let Some(hint) = self.hint() {
            out.push_str(&format!(",\"next\":{}", json_string(hint)));
        }
        out.push('}');
        out
    }

    fn hint(&self) -> Option<&'static str> {
        Some(match self.state {
            ChallengeState::Ready => return None,
            ChallengeState::HumanRequired => {
                "a person must complete this check in the browser (run cu headful with CU_HEADLESS=0); \
                 automatic actions on this tab are paused until a fresh check shows it cleared \
                 (cu challenge), or the hand-off is released (cu challenge release)"
            }
            ChallengeState::ChallengePending => {
                "a check is in progress or a CAPTCHA widget is on the page; do not treat the page as loaded"
            }
            ChallengeState::Blocked => "the site refused this client; retrying will not help",
            ChallengeState::RateLimited => {
                "the site is rate limiting; wait before the next request"
            }
            ChallengeState::Unknown => {
                "not confirmed as the real page; inspect it before relying on it"
            }
        })
    }
}

fn states() -> &'static Mutex<HashMap<Tab, Assessment>> {
    static STATES: OnceLock<Mutex<HashMap<Tab, Assessment>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remember the latest assessment of a tab and log any defence.
pub fn record(tab: &Tab, assessment: &Assessment) {
    if assessment.state != ChallengeState::Ready {
        eprintln!(
            "cu: challenge: {} {} on {} ({})",
            assessment.state.name(),
            assessment.vendor.unwrap_or("-"),
            assessment.url,
            assessment.signals.join("; ")
        );
    }
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), assessment.clone());
}

/// The last assessment of a tab, if any.
pub fn last(tab: &Tab) -> Option<Assessment> {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
}

/// Give up a pending human hand-off: the tab is handed back to automation.
pub fn release(tab: &Tab) -> bool {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tab)
        .is_some()
}

/// Refuse automatic interaction with a tab a person is verifying.
///
/// Only a tab whose last assessment was `human_required` is gated, and it is
/// re-probed first, so a challenge the person has completed releases the tab
/// at once. Returns the assessment that blocks, if any.
pub fn gate(tab: &Tab) -> Option<Assessment> {
    let last = last(tab)?;
    if last.state != ChallengeState::HumanRequired {
        return None;
    }
    let fresh = assess(|m, p| tab.command(m, p), Duration::ZERO);
    match fresh.state {
        // Cleared, or turned into something no person can fix from here:
        // the tab goes back to automation with the new state on record.
        ChallengeState::Ready | ChallengeState::Blocked | ChallengeState::RateLimited => {
            record(tab, &fresh);
            None
        }
        // Still on the challenge (an interstitial re-probed without a wait
        // reads as pending): keep the hand-off, with the fresh evidence.
        ChallengeState::HumanRequired | ChallengeState::ChallengePending => {
            let mut held = fresh;
            held.state = ChallengeState::HumanRequired;
            record(tab, &held);
            Some(held)
        }
        // A re-probe that cannot tell is no proof the person finished.
        ChallengeState::Unknown => {
            let mut held = last;
            held.signals
                .push("re-check inconclusive; still waiting for the person".into());
            Some(held)
        }
    }
}

/// Whether challenge assessment runs after navigations (`CU_CHALLENGE=0`
/// turns it off, for owned test sites where the extra probe is pure cost).
pub fn enabled() -> bool {
    std::env::var("CU_CHALLENGE")
        .map(|v| v != "0")
        .unwrap_or(true)
}
