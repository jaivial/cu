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
    json_string_value, json_strings,
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
    /// Defence signals from the main document's response headers
    /// (`cf-mitigated: challenge`, `x-datadome: protected`), when the
    /// navigation was observed on the network.
    pub hints: Vec<String>,
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

fn hint(probe: &Probe, value: &str) -> bool {
    probe.hints.iter().any(|h| h == value)
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

    // DataDome's own page, read from the inline `dd` config it serves before
    // any frame exists (seen on real 403s): `t:'bv'` is a block, `rt:'i'` the
    // automatic device check (it may reload into the site), `rt:'c'` the
    // CAPTCHA.
    if has(probe, "dd-t-bv") {
        return Verdict::new(Blocked, Some("datadome"), vec!["datadome block page (t=bv)".into()]);
    }
    if has(probe, "dd-rt-c") {
        return Verdict::new(
            HumanRequired,
            Some("datadome"),
            vec!["datadome captcha page (rt=c)".into()],
        );
    }
    if has(probe, "dd-rt-i") {
        let mut verdict = Verdict::new(
            ChallengePending,
            Some("datadome"),
            vec!["datadome device check (rt=i)".into()],
        );
        verdict.interstitial = true;
        return verdict;
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
    // The title and text are localised (a Spanish profile reads "Un
    // momento..."), so the language-independent signals come first: the
    // `_cf_chl_opt` config of the challenge page, its error-text element and
    // the `cf-mitigated: challenge` response header.
    let cf_page = title.starts_with("just a moment")
        || title.starts_with("attention required")
        || has(probe, "cf-challenge")
        || has(probe, "cf-chl-opt")
        || hint(probe, "cf-mitigated: challenge")
        // The challenge rewrites the address to carry `__cf_chl_*` tokens;
        // a page that loaded fine under such an address (the reload after a
        // pass) is not itself a challenge, so the token only counts with a
        // non-2xx status.
        || (probe.url.contains("__cf_chl") && !(200..300).contains(&probe.status));
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
        if hint(probe, "cf-mitigated: challenge") {
            signals.push("cf-mitigated: challenge header".into());
        }
        if has(probe, "cf-chl-opt") {
            signals.push("_cf_chl_opt challenge config".into());
        }
        if title.starts_with("just a moment") || title.starts_with("attention required") {
            signals.push(format!("title: {}", probe.title));
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

    // The site's own account checkpoint ("suspicious login", "confirm it's
    // you", a code sent by SMS/e-mail, 2FA): no vendor, a 200, and the
    // gate used to call it ready. A checkpoint-shaped path plus a form or
    // the wording, or a one-time-code form plus the wording, is a person's
    // job; either signal alone is not proof, but it is not ready either.
    if let Some(verdict) = first_party(probe) {
        return verdict;
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
    // A response a vendor marked as mitigated is never a plain page, even if
    // its DOM carried no marker this probe knows.
    if web && hint(probe, "cf-mitigated: challenge") {
        return Verdict::new(
            ChallengePending,
            Some("cloudflare"),
            vec!["cf-mitigated: challenge header".into()],
        );
    }
    if web && status >= 400 && hint(probe, "x-datadome: protected") {
        return Verdict::new(
            Unknown,
            Some("datadome"),
            vec![format!("status {status} from a datadome-protected origin")],
        );
    }
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

/// Path segments sites use for their own account checkpoints (Instagram
/// `/challenge/`, Facebook `/checkpoint/`, `/auth_platform/codeentry/`,
/// two-factor pages).
const CHECKPOINT_PATHS: &[&str] = &[
    "/challenge/",
    "/checkpoint/",
    "/auth_platform/",
    "/two_factor",
    "/two-factor",
    "/twofactor",
    "/2fa",
    "/mfa/",
    "/login/verify",
    "/login/challenge",
    "/account/verify",
    "/accounts/suspended",
    "/identity/verify",
];

/// Wording of account checkpoints, lower case (the probe lower-cases the
/// text). English and Spanish; the language-independent signals are the
/// path and the code form.
const CHECKPOINT_TEXT: &[&str] = &[
    "suspicious login",
    "unusual login",
    "unusual activity",
    "unusual sign-in",
    "confirm it's you",
    "confirm it\u{2019}s you",
    "confirm that it's you",
    "confirm that it\u{2019}s you",
    "verify it's you",
    "verify it\u{2019}s you",
    "help us confirm",
    "enter the code",
    "enter the 6-digit code",
    "security code",
    "verification code",
    "confirmation code",
    "login code",
    "two-factor authentication",
    "2-step verification",
    "two-step verification",
    "authenticator app",
    "we sent a code",
    "we've sent a code",
    "confirma que eres t\u{fa}",
    "confirmar que eres t\u{fa}",
    "inicio de sesi\u{f3}n inusual",
    "actividad inusual",
    "c\u{f3}digo de seguridad",
    "c\u{f3}digo de verificaci\u{f3}n",
    "c\u{f3}digo de confirmaci\u{f3}n",
    "introduce el c\u{f3}digo",
    "verificaci\u{f3}n en dos pasos",
];

/// The site's own checkpoint, from the path, a verification-code form and
/// the wording (see [`classify`]).
fn first_party(probe: &Probe) -> Option<Verdict> {
    use ChallengeState::*;
    let path = {
        let rest = probe.url.split_once("://").map(|(_, r)| r).unwrap_or("");
        let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
        let cut = path.find(['?', '#']).unwrap_or(path.len());
        let mut path = path[..cut].to_lowercase();
        if !path.ends_with('/') {
            path.push('/');
        }
        path
    };
    // Hash-routed SPAs keep the route in the fragment (`#/challenge/...`).
    let fragment = probe.url.split_once('#').map(|(_, f)| f.to_lowercase());
    let by_path = CHECKPOINT_PATHS.iter().find(|p| {
        path.contains(*p) || fragment.as_deref().is_some_and(|f| f.contains(&p[1..]))
    });
    let code_form = has(probe, "code-form");
    let form = has(probe, "form-input");
    let wording = text_has(probe, CHECKPOINT_TEXT);
    let mut signals = vec![];
    if let Some(p) = by_path {
        signals.push(format!("checkpoint path {p}"));
    }
    if code_form {
        signals.push("verification code form".into());
    }
    if let Some(w) = &wording {
        signals.push(w.clone());
    }
    let state = match (by_path.is_some(), code_form, wording.is_some(), form) {
        (true, _, _, true) | (true, _, true, _) | (false, true, true, _) => HumanRequired,
        (true, _, _, _) | (false, true, false, _) => Unknown,
        _ => return None,
    };
    Some(Verdict::new(state, Some("first-party"), signals))
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
  mark('cf-challenge', '#challenge-error-text, #challenge-success-text', false);
  // Inline configs of vendor pages (script text is DOM; the page's globals
  // are not visible from this world, the text is).
  for (const sc of document.querySelectorAll('script:not([src])')) {
    const t = sc.textContent || '';
    if (t.length > 20000) continue;
    if (t.includes('_cf_chl_opt')) markers.push('cf-chl-opt');
    if (t.includes('captcha-delivery.com')) {
      const rt = /['"]rt['"]\s*:\s*['"](\w)['"]/.exec(t), tt = /['"]t['"]\s*:\s*['"](\w+)['"]/.exec(t);
      if (tt && tt[1] === 'bv') markers.push('dd-t-bv');
      else if (rt) markers.push('dd-rt-' + rt[1]);
    }
  }
  mark('captcha', '[id*="captcha" i], [class*="captcha" i]', true);
  // Account checkpoints: a visible one-time-code field (by autocomplete or
  // by name/id/label), or a row of single-character boxes; and whether any
  // form field is on screen at all.
  const fields = [...document.querySelectorAll('input:not([type=hidden]):not([type=submit]):not([type=button]):not([type=image]), textarea, select')].filter(shown);
  if (fields.length) markers.push('form-input');
  const codeName = /(^|[_\-\s])(otp|totp|passcode|2fa|mfa)([_\-\s]|$)|(security|verification|verify|confirmation|confirm|sms|email|login|auth|one.?time|two.?factor|approvals?)[_\-\s]?code/i;
  const label = (f) => (f.name || '') + ' ' + (f.id || '') + ' ' + (f.getAttribute('aria-label') || '') + ' ' + (f.placeholder || '');
  // A card's "security code" (CVV) is not an account checkpoint.
  const card = (f) => (f.autocomplete || '').startsWith('cc-') || /cvv|cvc|csc|card/i.test(label(f));
  const single = fields.filter((f) => f.tagName === 'INPUT' && f.maxLength === 1);
  if (fields.some((f) => !card(f) && (f.autocomplete === 'one-time-code' || codeName.test(label(f))))
      || (single.length >= 4 && single.length <= 8))
    markers.push('code-form');
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
    let context = crate::execution::isolated_context(&mut cmd, &main.id)?;
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
        hints: Vec::new(),
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
pub fn assess<F>(cmd: F, budget: Duration) -> Assessment
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    assess_response(cmd, budget, None)
}

/// [`assess`], with what the network saw of the main document: its status
/// stands in when the page did not record one, and its header hints count
/// as evidence.
pub fn assess_response<F>(
    mut cmd: F,
    budget: Duration,
    response: Option<&crate::scheduler::DocumentResponse>,
) -> Assessment
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let started = Instant::now();
    loop {
        let error = match probe(&mut cmd) {
            Ok(mut probe) => {
                if let Some(response) = response {
                    if probe.status == 0 {
                        probe.status = response.status;
                    }
                    probe.hints = response.hints.clone();
                }
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

/// Counters over every assessment of the daemon's life, for
/// `/v1/diagnostics`.
#[derive(Default)]
struct Metrics {
    assessed: u64,
    by_state: HashMap<&'static str, u64>,
    by_vendor: HashMap<&'static str, u64>,
    /// `ready` verdicts.
    ready: u64,
    /// A `ready` page whose next check, on the same tab and origin, found a
    /// defence: cu said success and the origin was in fact challenging (the
    /// false-success count cu can observe).
    ready_contradicted: u64,
    /// Human hand-offs that were released by hand rather than cleared.
    released: u64,
    /// Hand-offs the gate's re-check found cleared.
    cleared: u64,
    /// Re-checks run because the page changed its address without a
    /// navigation (History API, hash change) or navigated by itself.
    url_rechecks: u64,
    /// Of those, how many changed the state on record.
    url_recheck_changes: u64,
}

fn metrics() -> &'static Mutex<Metrics> {
    static METRICS: OnceLock<Mutex<Metrics>> = OnceLock::new();
    METRICS.get_or_init(Default::default)
}

/// The `challenge` object of `/v1/diagnostics`.
pub fn metrics_json() -> String {
    let m = metrics().lock().unwrap_or_else(|e| e.into_inner());
    let defences: u64 = ["challenge_pending", "human_required", "blocked", "rate_limited"]
        .iter()
        .map(|s| m.by_state.get(s).copied().unwrap_or(0))
        .sum();
    let ratio = |n: u64, d: u64| {
        if d == 0 {
            "null".to_string()
        } else {
            format!("{:.4}", n as f64 / d as f64)
        }
    };
    let map = |h: &HashMap<&'static str, u64>| {
        let mut v: Vec<_> = h.iter().collect();
        v.sort();
        v.iter()
            .map(|(k, n)| format!("{}:{n}", json_string(k)))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"assessed\":{},\"states\":{{{}}},\"vendors\":{{{}}},\"challenge_rate\":{},\
         \"ready\":{},\"ready_contradicted\":{},\"false_success_rate\":{},\
         \"handoffs_cleared\":{},\"handoffs_released\":{},\
         \"url_rechecks\":{},\"url_recheck_changes\":{},\
         \"note\":\"challenge_rate = defence verdicts / assessments; false_success_rate = ready verdicts whose next check on the same tab and origin found a defence / ready verdicts (a lower bound: only what a later check saw)\"}}",
        m.assessed,
        map(&m.by_state),
        map(&m.by_vendor),
        ratio(defences, m.assessed),
        m.ready,
        m.ready_contradicted,
        ratio(m.ready_contradicted, m.ready),
        m.cleared,
        m.released,
        m.url_rechecks,
        m.url_recheck_changes,
    )
}

/// Remember the latest assessment of a tab and log any defence.
pub fn record(tab: &Tab, assessment: &Assessment) {
    {
        let previous = last(tab);
        let mut m = metrics().lock().unwrap_or_else(|e| e.into_inner());
        m.assessed += 1;
        *m.by_state.entry(assessment.state.name()).or_default() += 1;
        if let Some(vendor) = assessment.vendor {
            *m.by_vendor.entry(vendor).or_default() += 1;
        }
        if assessment.state == ChallengeState::Ready {
            m.ready += 1;
        }
        if let Some(previous) = previous {
            // Same origin, not same URL: a challenge page often rewrites
            // its own address (Cloudflare's `history.replaceState`).
            if previous.state == ChallengeState::Ready
                && assessment.state.is_defence()
                && crate::scheduler::origin_of(&previous.url).is_some()
                && crate::scheduler::origin_of(&previous.url)
                    == crate::scheduler::origin_of(&assessment.url)
            {
                m.ready_contradicted += 1;
            }
        }
    }
    store(tab, assessment);
}

/// Log and remember an assessment without counting it: the gate's re-checks
/// of a paused tab are not new navigations.
fn store(tab: &Tab, assessment: &Assessment) {
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
    stored_at()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), Instant::now());
}

/// When each tab's assessment was last stored, so a re-check scheduled by
/// an address change can see that a navigation already assessed the page.
fn stored_at() -> &'static Mutex<HashMap<Tab, Instant>> {
    static AT: OnceLock<Mutex<HashMap<Tab, Instant>>> = OnceLock::new();
    AT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pause between an address change and its re-check: a SPA pushes the new
/// URL first and renders the new view after.
const URL_RECHECK_DELAY: Duration = Duration::from_millis(400);

/// Last full URL of every page target, as the browser announced it.
fn target_urls() -> &'static Mutex<HashMap<String, String>> {
    static URLS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    URLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Feed a browser-level target event. A page whose address changed --
/// `history.pushState`/`replaceState`, a hash change, or a navigation cu
/// did not make (a checkpoint redirecting into the feed, an interstitial
/// reloading into the site) -- is re-checked after a short pause, so the
/// state on record (and the gate) follows a single-page app instead of
/// keeping the verdict of the page it was on before. Only tabs cu has
/// assessed are followed. A check stored after the pause (a slow
/// navigation's own assessment) wins; one stored before it (the immediate
/// check after a click that pushed a route whose view renders later) does
/// not.
pub fn see_target_event(cdp_port: u16, event: &str) {
    if !enabled() || !event.contains("\"Target.targetInfoChanged\"") {
        return;
    }
    let Some(info) = crate::server::json_object(event, "targetInfo") else {
        return;
    };
    if json_string_value(info, "type").as_deref() != Some("page") {
        return;
    }
    let (Some(id), Some(url)) = (
        json_string_value(info, "targetId"),
        json_string_value(info, "url"),
    ) else {
        return;
    };
    let previous = target_urls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), url.clone());
    // Title changes and the like also fire this event; only a new address
    // matters, and the first sighting has nothing to compare with.
    if previous.is_none_or(|p| p == url) {
        return;
    }
    let tabs: Vec<Tab> = crate::server::tabs_for_target(cdp_port, &id)
        .into_iter()
        .filter(|t| last(t).is_some())
        .collect();
    if tabs.is_empty() {
        return;
    }
    let changed_at = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(URL_RECHECK_DELAY);
        for tab in tabs {
            recheck_after_url_change(&tab, changed_at);
        }
    });
}

fn recheck_after_url_change(tab: &Tab, changed_at: Instant) {
    let fresh_enough = stored_at()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .is_some_and(|at| *at > changed_at + URL_RECHECK_DELAY);
    let Some(before) = last(tab) else {
        return;
    };
    if fresh_enough {
        return;
    }
    let mut fresh = assess(|m, p| tab.command(m, p), Duration::ZERO);
    if fresh.state == ChallengeState::Unknown
        && (fresh.url.is_empty() || fresh.signals.iter().any(|s| s == "page still loading"))
    {
        // The probe failed (the page is mid-navigation); that navigation's
        // own assessment, or the next address change, will tell.
        return;
    }
    // Recorded by a later check while this one ran: keep that one.
    if stored_at()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .is_some_and(|at| *at > changed_at + URL_RECHECK_DELAY)
    {
        return;
    }
    // Nothing new (typically cu's own navigation, already assessed): leave
    // the record and the counters alone.
    if fresh.url == before.url && fresh.state == before.state {
        return;
    }
    fresh.signals.push("re-checked after the address changed".into());
    {
        let mut m = metrics().lock().unwrap_or_else(|e| e.into_inner());
        m.url_rechecks += 1;
        if before.state != fresh.state {
            m.url_recheck_changes += 1;
        }
        if before.state == ChallengeState::Ready && fresh.state.is_defence() {
            // cu had said ready and the page turned out (or turned into) a
            // defence: the false success the SPA used to hide.
            m.ready_contradicted += 1;
        }
        if before.state == ChallengeState::HumanRequired && fresh.state == ChallengeState::Ready {
            m.cleared += 1;
        }
    }
    if before.state != fresh.state {
        eprintln!(
            "cu: challenge: {} -> {} on {} after an address change",
            before.state.name(),
            fresh.state.name(),
            fresh.url
        );
    }
    // A pending interstitial re-read without a wait on a tab a person is
    // handling stays a hand-off, as in the gate.
    if before.state == ChallengeState::HumanRequired
        && fresh.state == ChallengeState::ChallengePending
    {
        fresh.state = ChallengeState::HumanRequired;
    }
    store(tab, &fresh);
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
    if last(tab).is_some_and(|a| a.state == ChallengeState::HumanRequired) {
        metrics().lock().unwrap_or_else(|e| e.into_inner()).released += 1;
    }
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
            if fresh.state == ChallengeState::Ready {
                metrics().lock().unwrap_or_else(|e| e.into_inner()).cleared += 1;
            }
            store(tab, &fresh);
            None
        }
        // Still on the challenge (an interstitial re-probed without a wait
        // reads as pending): keep the hand-off, with the fresh evidence.
        ChallengeState::HumanRequired | ChallengeState::ChallengePending => {
            let mut held = fresh;
            held.state = ChallengeState::HumanRequired;
            store(tab, &held);
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
