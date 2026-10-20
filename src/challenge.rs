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

/// The page Chromium serves itself when an origin refused the request
/// outright (`chrome-error://chromewebdata/`, "Access to <site> was denied",
/// "HTTP ERROR 403"). Cloudflare's edge, X/Twitter, Reddit and friends
/// answer an automated client with this renderer-internal page: no vendor
/// marker in the DOM, a 200 for the chrome-error URL in the navigation
/// entry, and the real status only in the title's `HTTP ERROR <code>`.
/// Never a normal page: it reads as Blocked, with the vendor that explains
/// where it came from when the network saw one.
const CHROME_ERROR_URL: &str = "chrome-error://";
const DENY_PHRASES: &[(&str, u16)] = &[
    ("access to", 403),
    ("you don't have authorization to view this page", 403),
    ("http error 403", 403),
    ("http error 401", 401),
    ("http error 429", 429),
];

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
        return Verdict::new(
            Blocked,
            Some("datadome"),
            vec!["datadome block page (t=bv)".into()],
        );
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
    // Chromium's own "the site refused you" page (chrome-error://chromewebdata/).
    // The probe still runs in it: the URL is the renderer's error page, the
    // title carries the site's name, and the body has the exact wording.
    // Whatever the status says, this is a refusal, not a page to act on.
    // Checked before the `web` test: the error URL is not http(s), so it
    // would otherwise read as "not an http(s) page" and be called ready.
    if probe.url.starts_with(CHROME_ERROR_URL) {
        let phrase = DENY_PHRASES
            .iter()
            .find(|(p, _)| probe.text.contains(*p) || title.contains(*p))
            .map(|(p, _)| *p);
        let mut signals = vec![format!("chrome error page ({CHROME_ERROR_URL}…)")];
        signals.extend(phrase.map(|p| format!("text: \"{p}\"")));
        signals.push(format!(
            "title: {}",
            probe.title.chars().take(80).collect::<String>()
        ));
        return Verdict::new(Blocked, Some("chrome-deny"), signals);
    }
    if probe.ready_state == "loading" {
        return Verdict::new(Unknown, None, vec!["page still loading".into()]);
    }
    if !web {
        return Verdict::new(Ready, None, vec!["not an http(s) page".into()]);
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
    let by_path = CHECKPOINT_PATHS
        .iter()
        .find(|p| path.contains(*p) || fragment.as_deref().is_some_and(|f| f.contains(&p[1..])));
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

/// The fingerprint values [`crate::stealth`] installs, read from the page's
/// own world -- the one world where `navigator.userAgent` is the one the
/// navigation carried and the renderer answers to.
///
/// Read once, after the probe: a page that has just decided what to show has
/// already read everything, and this is what says why. It runs in the
/// browser's own world (no `contextId`), which is the whole point.
fn observe_stealth(cmd: &mut impl FnMut(&str, &str) -> Result<String, String>) {
    let params = crate::execution::evaluate_params(crate::stealth::OBSERVE_JS, "", false);
    let observed = cmd("Runtime.evaluate", &params)
        .ok()
        .and_then(|reply| crate::server::evaluated_string(&reply));
    if let Some(observed) = observed {
        crate::stealth::record_observed(&observed);
    }
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
                observe_stealth(&mut cmd);
                return Assessment::new(&probe, verdict, waited);
            }
            Err(e) => e,
        };
        if started.elapsed() >= budget.max(Duration::from_millis(1500)) {
            observe_stealth(&mut cmd);
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
                // A first-party checkpoint with a code form is often just an
                // OTP: mintable from a stored seed, or a short hand-off link
                // instead of a full live takeover.
                if self.vendor == Some("first-party")
                    && self.signals.iter().any(|s| s.contains("code"))
                {
                    return Some(
                        "a one-time code page: if the vault holds a seed for this domain, \
                         `cu otp code <domain>` mints the code to fill; otherwise \
                         `cu otp request <domain> --wait` sends the person a short link to \
                         type it into, and answers here when they do. A full takeover \
                         (/vault/live) is the fallback; automatic actions on this tab are \
                         paused until a fresh check shows it cleared (cu challenge), or \
                         the hand-off is released (cu challenge release)",
                    );
                }
                "a person must complete this check in the browser (cu already runs headful on \
                 an X display by default; a daemon started with CU_HEADLESS=1 is the one that \
                 needs CU_HEADLESS=0 to get a window, and CU_STEALTH=0 turns the hardening off); \
                 automatic actions on this tab are paused until a fresh check shows it cleared \
                 (cu challenge), or the hand-off is released (cu challenge release)"
            }
            ChallengeState::ChallengePending => {
                "a check is in progress or a CAPTCHA widget is on the page; do not treat the page as loaded"
            }
            ChallengeState::Blocked => {
                if self.vendor == Some("chrome-deny") {
                    "the browser was refused by the origin or its CDN (a 40x chrome-error page); \
                     the state does not depend on the browser alone -- a visible clear cookie might \
                     have expired, or the origin may only refuse robots. Retry later or try another \
                     route; do not keep the session looping"
                } else {
                    "the site refused this client; retrying will not help"
                }
            }
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
    let defences: u64 = [
        "challenge_pending",
        "human_required",
        "blocked",
        "rate_limited",
    ]
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
         \"url_rechecks\":{},\"url_recheck_changes\":{},\"handoffs\":{},\
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
        handoff::json(),
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
    let before = last(tab).map(|a| a.state);
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
    let held = assessment.state == ChallengeState::HumanRequired;
    match (before == Some(ChallengeState::HumanRequired), held) {
        (false, true) => handoff::begin(tab, assessment),
        (true, false) => handoff::end(tab, assessment.state.name(), Some(assessment)),
        _ => {}
    }
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
    fresh
        .signals
        .push("re-checked after the address changed".into());
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
        handoff::end(tab, "released", None);
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

/// Human hand-off: what happens around a tab that turned `human_required`.
///
/// - With a headful browser the tab is brought to the front
///   (`Page.bringToFront` + `Target.activateTarget`), so the person sees it.
/// - The hand-off is announced on stderr and, if `CU_HANDOFF_NOTIFY` is set,
///   by running that command through `sh -c` with `CU_HANDOFF_TAB`,
///   `CU_HANDOFF_URL` (origin and path), `CU_HANDOFF_VENDOR` and
///   `CU_HANDOFF_EVENT` (`begin` / `cleared` / `released` / ...) in its
///   environment (e.g. `notify-send "cu: $CU_HANDOFF_EVENT" "$CU_HANDOFF_URL"`).
///   Nothing is interpolated into the command.
/// - A watcher re-checks the tab every second, exactly as the gate does, so
///   the tab is released the moment the page reads ready -- without waiting
///   for the agent's next action -- for up to `CU_HANDOFF_WATCH_S` (default
///   900 s). After that the gate still re-checks on every action.
/// - When it ends, the pass is recorded for diagnostics: vendor, signals,
///   how long it took, how many re-checks, how it ended, and which cookies
///   appeared for the page's URL meanwhile -- names, domain and expiry
///   only, never values.
/// - An agent does not have to guess: `GET /v1/handoff` reports the open
///   hand-offs and the last passes, and `GET /v1/challenge?wait=S` (or
///   `cu handoff wait`) parks until the tab is released, the wait budget
///   runs out, or the verdict turns into something no person can fix.
pub mod handoff {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{Assessment, ChallengeState};
    use crate::server::{Tab, json_objects, json_string, json_string_value};

    struct Open {
        id: u64,
        started: Instant,
        started_unix: u64,
        url: String,
        vendor: Option<&'static str>,
        signals: Vec<String>,
        cookies: Vec<String>,
        rechecks: u64,
        foreground: Option<String>,
    }

    /// What a [`wait`] returned: the assessment the wait ended on and what it
    /// cost.
    pub struct Outcome {
        pub assessment: Assessment,
        pub waited: Duration,
        /// Probes spent on this wait, so a long wait is visible in the reply.
        pub polls: u64,
        /// The person has not finished (or the budget ran out): the agent
        /// must not treat the tab as usable.
        pub timed_out: bool,
    }

    struct Pass {
        url: String,
        vendor: Option<&'static str>,
        signals: Vec<String>,
        outcome: String,
        final_url: String,
        final_state: String,
        started_unix: u64,
        ms: u128,
        rechecks: u64,
        foreground: Option<String>,
        new_cookies: Vec<String>,
    }

    #[derive(Default)]
    struct Book {
        open: HashMap<Tab, Open>,
        done: VecDeque<Pass>,
        next_id: u64,
    }

    const KEEP: usize = 20;

    fn book() -> &'static Mutex<Book> {
        static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
        BOOK.get_or_init(Default::default)
    }

    fn watch_for() -> Duration {
        Duration::from_secs(
            std::env::var("CU_HANDOFF_WATCH_S")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
        )
    }

    /// Longest a caller may be parked in [`wait`], whatever it asked for:
    /// an agent waiting for a person must not hold a request thread (or a
    /// daemon route) for ever. `CU_HANDOFF_WAIT_MAX_S`, default 900 s.
    pub fn wait_ceiling() -> Duration {
        Duration::from_secs(
            std::env::var("CU_HANDOFF_WAIT_MAX_S")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
        )
    }

    /// The pause sequence a wait uses between re-checks: quick at first (a
    /// person solving a Turnstile is usually faster than that), then slower
    /// so a long wait costs the daemon little. Fixed, bounded and
    /// deterministic -- no jitter to test around.
    pub const BACKOFF_MS: [u64; 5] = [250, 500, 1000, 2000, 4000];
    /// A wait never probes more often than this, whatever the budget.
    pub const MIN_POLL_MS: u64 = 250;

    /// How long to sleep before re-check number `attempt` (0 is the first).
    pub fn backoff(attempt: u32) -> Duration {
        let ms = BACKOFF_MS[(attempt as usize).min(BACKOFF_MS.len() - 1)];
        Duration::from_millis(ms.max(MIN_POLL_MS))
    }

    /// Probes one wait may spend before its budget: a 60 s wait is not
    /// allowed to hold the daemon at one probe a second for an hour.
    pub fn max_checks(timeout: Duration) -> u64 {
        let checks = timeout.as_millis() / (BACKOFF_MS[BACKOFF_MS.len() - 1] as u128);
        checks.max(1).min(240) as u64
    }

    /// `name@domain exp=...` for every cookie the browser would send to
    /// the tab's current page. Values are never read out of the reply.
    fn cookies(tab: &Tab) -> Vec<String> {
        let Ok(reply) = tab.command("Network.getCookies", "{}") else {
            return Vec::new();
        };
        let Some(list) = crate::server::json_array(&reply, "cookies") else {
            return Vec::new();
        };
        let mut out: Vec<String> = json_objects(&list)
            .iter()
            .filter_map(|c| {
                let name = json_string_value(c, "name")?;
                let domain = json_string_value(c, "domain").unwrap_or_default();
                Some(format!("{name}@{domain}"))
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn notify(event: &str, tab: &Tab, url: &str, vendor: Option<&str>) {
        let tab_id = tab.target.clone().unwrap_or_else(|| "default".into());
        eprintln!(
            "cu: hand-off {event}: tab {tab_id} {url} ({})",
            vendor.unwrap_or("-")
        );
        let Ok(command) = std::env::var("CU_HANDOFF_NOTIFY") else {
            return;
        };
        if command.trim().is_empty() {
            return;
        }
        let spawned = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("CU_HANDOFF_EVENT", event)
            .env("CU_HANDOFF_TAB", &tab_id)
            .env("CU_HANDOFF_URL", url)
            .env("CU_HANDOFF_VENDOR", vendor.unwrap_or(""))
            .stdin(std::process::Stdio::null())
            .spawn();
        match spawned {
            // Reaped on a thread: a slow notifier never holds up cu.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => eprintln!("cu: CU_HANDOFF_NOTIFY failed: {e}"),
        }
    }

    /// Put the tab in front of the person, when there is a window to show.
    fn foreground(tab: &Tab) -> Option<String> {
        if !crate::diagnostics::headful() {
            return Some("headless: no window to bring forward".into());
        }
        let page = tab.command("Page.bringToFront", "{}");
        let target = tab
            .target
            .clone()
            .or_else(|| crate::server::pinned_default_target(tab.cdp_port));
        let activated = target.map(|id| {
            crate::server::browser_command(
                tab.cdp_port,
                "Target.activateTarget",
                &format!("{{\"targetId\":{}}}", json_string(&id)),
            )
        });
        Some(match (page, activated) {
            (Ok(_), Some(Ok(_))) => "brought to front".into(),
            (Ok(_), _) => "brought to front (page only)".into(),
            (Err(e), _) => format!("bringToFront failed: {e}"),
        })
    }

    pub(super) fn begin(tab: &Tab, assessment: &Assessment) {
        let id = {
            let mut b = book().lock().unwrap_or_else(|e| e.into_inner());
            if b.open.contains_key(tab) {
                return;
            }
            b.next_id += 1;
            let id = b.next_id;
            b.open.insert(
                tab.clone(),
                Open {
                    id,
                    started: Instant::now(),
                    started_unix: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    url: assessment.url.clone(),
                    vendor: assessment.vendor,
                    signals: assessment.signals.clone(),
                    cookies: Vec::new(),
                    rechecks: 0,
                    foreground: None,
                },
            );
            id
        };
        let tab = tab.clone();
        let (url, vendor) = (assessment.url.clone(), assessment.vendor);
        // Off the request thread: the reply that reported the challenge
        // must not wait on window management or a notifier.
        std::thread::spawn(move || {
            let before = cookies(&tab);
            let shown = foreground(&tab);
            if let Some(open) = book()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .open
                .get_mut(&tab)
                .filter(|o| o.id == id)
            {
                open.cookies = before;
                open.foreground = shown;
            }
            notify("begin", &tab, &url, vendor);
            watch(&tab, id);
        });
    }

    /// Re-check the tab every second while this hand-off is open.
    fn watch(tab: &Tab, id: u64) {
        let until = Instant::now() + watch_for();
        while Instant::now() < until {
            std::thread::sleep(Duration::from_secs(1));
            {
                let mut b = book().lock().unwrap_or_else(|e| e.into_inner());
                match b.open.get_mut(tab) {
                    Some(open) if open.id == id => open.rechecks += 1,
                    _ => return,
                }
            }
            // The gate's own re-check: it stores the fresh state, and a
            // cleared one ends the hand-off through `store`.
            if super::gate(tab).is_none() {
                return;
            }
        }
        eprintln!(
            "cu: hand-off watch on {} expired after {} s; the gate still re-checks on every action",
            tab.target.as_deref().unwrap_or("default"),
            watch_for().as_secs()
        );
    }

    pub(super) fn end(tab: &Tab, outcome: &str, fresh: Option<&Assessment>) {
        let Some(open) = book()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
            .remove(tab)
        else {
            return;
        };
        let after = cookies(tab);
        let new_cookies = after
            .into_iter()
            .filter(|c| !open.cookies.contains(c))
            .collect();
        let pass = Pass {
            url: open.url.clone(),
            vendor: open.vendor,
            signals: open.signals,
            outcome: if outcome == ChallengeState::Ready.name() {
                "cleared".into()
            } else {
                outcome.into()
            },
            final_url: fresh.map(|a| a.url.clone()).unwrap_or_default(),
            final_state: fresh
                .map(|a| a.state.name().to_string())
                .unwrap_or_else(|| "released".into()),
            started_unix: open.started_unix,
            ms: open.started.elapsed().as_millis(),
            rechecks: open.rechecks,
            foreground: open.foreground,
            new_cookies,
        };
        notify(&pass.outcome, tab, &open.url, open.vendor);
        let mut b = book().lock().unwrap_or_else(|e| e.into_inner());
        b.done.push_front(pass);
        b.done.truncate(KEEP);
    }

    /// The open hand-off on this tab, as the agent sees it: whether one is
    /// waiting, for how long, and what the person has to solve.
    pub fn open_for(tab: &Tab) -> Option<String> {
        let b = book().lock().unwrap_or_else(|e| e.into_inner());
        let open = b.open.get(tab)?;
        Some(format!(
            "{{\"tab\":{},\"url\":{},\"vendor\":{},\"open_ms\":{},\"rechecks\":{},\"foreground\":{},\"signals\":[{}]}}",
            json_string(tab.target.as_deref().unwrap_or("default")),
            json_string(&open.url),
            open.vendor
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            open.started.elapsed().as_millis(),
            open.rechecks,
            open.foreground
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            open.signals
                .iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(","),
        ))
    }

    /// Park until the hand-off on this tab ends, or `timeout` is spent.
    ///
    /// This is what lets an agent *resume* instead of retrying blind: it
    /// asks the daemon to watch the tab it was paused on, and the answer
    /// says what the tab is now -- ready to continue, still waiting, or
    /// something no person could fix from here. Re-checks use the same gate
    /// the watcher uses (bounded, backoff-ed, no interaction), so the wait
    /// never solves or touches anything, and the budget is a hard ceiling
    /// (see [`wait_ceiling`]) on both the wait and the number of probes.
    ///
    /// Returns the assessment that ended the wait (`ready` when the person
    /// finished), or the last one seen with `timed_out` set when the budget
    /// ran out. A tab that never had a hand-off answers at once with a
    /// fresh assessment, so the same call is safe to make either way.
    pub fn wait(tab: &Tab, timeout: Duration) -> Outcome {
        let budget = timeout.min(wait_ceiling());
        let started = Instant::now();
        // A wait nobody asked to wait: one assessment, no polling.
        if budget.is_zero() {
            return Outcome {
                assessment: probe_once(tab),
                waited: Duration::ZERO,
                polls: 1,
                timed_out: false,
            };
        }
        let mut polls = 0u64;
        let mut attempt = 0u32;
        let mut last = probe_once(tab);
        polls += 1;
        // No open hand-off and the page is not a challenge: there is nothing
        // to wait for, and polling a ready page forever would only burn
        // budget. `challenge_pending` (an interstitial that may clear by
        // itself) is exactly what this call is for, so it keeps waiting.
        let mut was_open = open_for(tab).is_some();
        while started.elapsed() + backoff(attempt) <= budget
            && polls < max_checks(budget)
            && (was_open
                || last.state == super::ChallengeState::ChallengePending
                || last.state == super::ChallengeState::HumanRequired)
        {
            std::thread::sleep(backoff(attempt).min(budget - started.elapsed()));
            attempt += 1;
            polls += 1;
            last = probe_once(tab);
            if open_for(tab).is_some() {
                was_open = true;
                continue;
            }
            match last.state {
                // Cleared: the person is done, the tab goes back to
                // automation. This is the answer the agent was waiting for.
                super::ChallengeState::Ready
                // Turned into something no person can fix from here: waiting
                // longer would only waste the budget.
                | super::ChallengeState::Blocked
                | super::ChallengeState::RateLimited => break,
                _ => {}
            }
        }
        let timed_out =
            matches!(last.state, super::ChallengeState::HumanRequired) || open_for(tab).is_some();
        Outcome {
            assessment: last,
            waited: started.elapsed(),
            polls,
            timed_out,
        }
    }

    /// One gate-style re-check: it stores what it sees (so the watcher, the
    /// gate and this wait all agree on the state) and returns it.
    fn probe_once(tab: &Tab) -> super::Assessment {
        super::gate(tab).unwrap_or_else(|| {
            let fresh = super::assess(|m, p| tab.command(m, p), Duration::ZERO);
            super::record(tab, &fresh);
            fresh
        })
    }

    /// The wait budget the daemon will clamp to, in seconds, and the pause
    /// sequence it uses: enough for an agent to know what to ask for and how
    /// often it will be probed.
    pub fn outcome_hint() -> String {
        format!(
            "wait up to {} s (CU_HANDOFF_WAIT_MAX_S), re-checked every {} ms at most",
            wait_ceiling().as_secs(),
            BACKOFF_MS[BACKOFF_MS.len() - 1]
        )
    }

    /// What [`wait`] came back with, as JSON: the assessment (so the agent
    /// sees the state and the signals), how long it waited, how many probes
    /// it cost, and whether the person is still needed.
    pub fn outcome_json(outcome: &Outcome) -> String {
        format!(
            "{{\"resolved\":{},\"timed_out\":{},\"waited_ms\":{},\"polls\":{},\"assessment\":{}}}",
            !outcome.timed_out,
            outcome.timed_out,
            outcome.waited.as_millis(),
            outcome.polls,
            outcome.assessment.json(),
        )
    }

    /// `challenge.handoffs` in diagnostics: open hand-offs and the last
    /// passes, newest first.
    pub fn json() -> String {
        let b = book().lock().unwrap_or_else(|e| e.into_inner());
        let strings = |v: &[String]| {
            v.iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(",")
        };
        let vendor = |v: Option<&str>| v.map(json_string).unwrap_or_else(|| "null".into());
        let open = b
            .open
            .iter()
            .map(|(tab, o)| {
                format!(
                    "{{\"tab\":{},\"url\":{},\"vendor\":{},\"open_ms\":{},\"rechecks\":{},\"foreground\":{}}}",
                    json_string(tab.target.as_deref().unwrap_or("default")),
                    json_string(&o.url),
                    vendor(o.vendor),
                    o.started.elapsed().as_millis(),
                    o.rechecks,
                    o.foreground.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let passes = b
            .done
            .iter()
            .map(|p| {
                format!(
                    "{{\"url\":{},\"vendor\":{},\"signals\":[{}],\"outcome\":{},\"final_state\":{},\"final_url\":{},\"started_unix\":{},\"ms\":{},\"rechecks\":{},\"foreground\":{},\"new_cookies\":[{}]}}",
                    json_string(&p.url),
                    vendor(p.vendor),
                    strings(&p.signals),
                    json_string(&p.outcome),
                    json_string(&p.final_state),
                    json_string(&p.final_url),
                    p.started_unix,
                    p.ms,
                    p.rechecks,
                    p.foreground.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
                    strings(&p.new_cookies),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"open\":[{open}],\"passes\":[{passes}]}}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Assessment, ChallengeState, Probe, Verdict, classify};

    fn probe(url: &str, title: &str, status: u16, text: &str, markers: &[&str]) -> Probe {
        Probe {
            url: url.into(),
            title: title.into(),
            ready_state: "complete".into(),
            status,
            text: text.into(),
            markers: markers.iter().map(|m| m.to_string()).collect(),
            frames: Vec::new(),
            hints: Vec::new(),
        }
    }

    #[test]
    fn a_chrome_error_page_is_blocked_not_unknown() {
        // What x.com's edge answers an automated client today: Chromium's own
        // renderer error page, with the real wording inside the body.
        let p = probe(
            "chrome-error://chromewebdata/",
            "x.com",
            403,
            "access to x.com was denied you don't have authorization to view this page. http error 403",
            &[],
        );
        let verdict = classify(&p);
        assert_eq!(verdict.state, ChallengeState::Blocked, "{verdict:?}");
        assert_eq!(verdict.vendor, Some("chrome-deny"), "{verdict:?}");
        assert!(
            verdict
                .signals
                .iter()
                .any(|s| s.contains("chrome error page")),
            "{verdict:?}"
        );
        assert!(
            verdict
                .signals
                .iter()
                .any(|s| s.contains("http error 403") || s.contains("access to")),
            "{verdict:?}"
        );
    }

    #[test]
    fn the_deny_page_is_recognised_by_the_url_alone() {
        // Sites vary the wording; the error page itself is the signal, and a
        // plain 200-for-chrome-error is still not a page to act on.
        let p = probe(
            "chrome-error://chromewebdata/",
            "example.test",
            200,
            "",
            &[],
        );
        let verdict = classify(&p);
        assert_eq!(verdict.state, ChallengeState::Blocked, "{verdict:?}");
        assert_eq!(verdict.vendor, Some("chrome-deny"), "{verdict:?}");
    }

    #[test]
    fn an_interstitial_vendor_still_beats_the_chrome_error_page() {
        // A challenge page served under chrome-error is unusual, but if a
        // known vendor left its marker, the vendor wins the naming and the
        // state stays the more useful one (the probe still saw one).
        let mut p = probe(
            "chrome-error://chromewebdata/",
            "x.com",
            403,
            "access denied",
            &[],
        );
        p.markers.push("cf-error".into());
        let verdict = classify(&p);
        assert_eq!(verdict.vendor, Some("chrome-deny"), "{verdict:?}");
    }

    #[test]
    fn a_real_403_without_a_marker_is_still_unknown() {
        // Only the renderer-internal page speaks for the browser; an origin's
        // own 403 body (a private storefront, an API) must not be renamed.
        let p = probe("https://example.test/secret", "403", 403, "", &[]);
        let verdict = classify(&p);
        assert_eq!(verdict.state, ChallengeState::Unknown, "{verdict:?}");
        assert_eq!(verdict.vendor, None, "{verdict:?}");
    }

    #[test]
    fn the_profiles_deny_wording_is_not_a_chrome_error_page() {
        // A real site can say "access denied" in its own 403 body: without
        // chrome-error:// in the URL the classification is untouched.
        let p = probe(
            "https://shop.example.test/private",
            "Private",
            403,
            "access to this page is denied",
            &[],
        );
        let verdict = classify(&p);
        assert_eq!(verdict.state, ChallengeState::Unknown, "{verdict:?}");
    }

    #[test]
    fn a_wait_rechecks_quickly_first_and_then_leaves_the_person_alone() {
        use super::handoff;
        // A Turnstile is usually solved faster than a second, so the first
        // re-checks are close together; after that the daemon stops asking
        // every 250 ms and settles at the slowest step. Bounded and
        // monotonic: a wait never gets *more* eager as it runs.
        let pauses: Vec<u64> = (0..8)
            .map(handoff::backoff)
            .map(|d| d.as_millis() as u64)
            .collect();
        assert_eq!(pauses[0], 250, "{pauses:?}");
        for pair in pauses.windows(2) {
            assert!(pair[1] >= pair[0], "not monotonic: {pauses:?}");
        }
        assert!(pauses[0] < pauses[4], "no easing at all: {pauses:?}");
        // And the slowest step is a ceiling for every later one.
        let last = handoff::BACKOFF_MS[handoff::BACKOFF_MS.len() - 1];
        assert_eq!(handoff::backoff(99).as_millis() as u64, last);
        assert!(handoff::backoff(0).as_millis() >= handoff::MIN_POLL_MS as u128);
    }

    #[test]
    fn a_wait_costs_a_bounded_number_of_probes() {
        use super::handoff;
        use std::time::Duration;
        // A long wait is not a tight loop against the browser: the slowest
        // step sets the rate, and the probe count is clamped on both sides
        // so a tiny budget still gets one look and a huge one stays finite.
        assert_eq!(handoff::max_checks(Duration::from_secs(60)), 15);
        assert_eq!(handoff::max_checks(Duration::ZERO), 1);
        assert!(handoff::max_checks(Duration::from_secs(86_400)) <= 240);
        // 15 probes at no less than the slowest step fits inside 60 s.
        assert!(handoff::max_checks(Duration::from_secs(60)) * 4000 <= 60_000);
    }

    #[test]
    fn a_wait_reports_what_it_cost_and_whether_a_person_is_still_needed() {
        use super::handoff;
        use std::time::Duration;
        let assessment = Assessment::new(
            &Probe {
                url: "https://example.test/cf-chl/verify?__cf_chl_tk=secret".into(),
                title: "Just a moment...".into(),
                ready_state: "complete".into(),
                status: 403,
                text: "checking your browser".into(),
                markers: vec!["cf-chl-opt".into()],
                frames: Vec::new(),
                hints: Vec::new(),
            },
            Verdict::new(ChallengeState::HumanRequired, Some("cloudflare"), vec![]),
            Duration::from_millis(10_000),
        );
        let held = handoff::Outcome {
            assessment: assessment.clone(),
            waited: Duration::from_secs(30),
            polls: 9,
            timed_out: true,
        };
        let json = handoff::outcome_json(&held);
        assert!(json.contains("\"timed_out\":true"), "{json}");
        assert!(json.contains("\"polls\":9"), "{json}");
        assert!(json.contains("\"resolved\":false"), "{json}");
        // The challenge URL's token never leaves: the assessment cuts the
        // query, so a wait cannot leak what the challenge page carries.
        assert!(
            json.contains("https://example.test/cf-chl/verify"),
            "{json}"
        );
        assert!(!json.contains("__cf_chl_tk"), "token leaked: {json}");

        let done = handoff::Outcome {
            assessment,
            waited: Duration::from_millis(700),
            polls: 3,
            timed_out: false,
        };
        let json = handoff::outcome_json(&done);
        assert!(json.contains("\"resolved\":true"), "{json}");
        assert!(json.contains("\"timed_out\":false"), "{json}");
    }

    #[test]
    fn the_wait_budget_hint_names_the_ceiling_and_the_pace() {
        use super::handoff;
        let hint = handoff::outcome_hint();
        assert!(hint.contains("CU_HANDOFF_WAIT_MAX_S"), "{hint}");
        assert!(
            hint.contains(&handoff::BACKOFF_MS[handoff::BACKOFF_MS.len() - 1].to_string()),
            "{hint}"
        );
    }

    #[test]
    fn a_healthy_page_still_reads_ready() {
        let p = probe("https://example.test/", "Example", 200, "hello", &[]);
        let verdict = classify(&p);
        assert_eq!(verdict.state, ChallengeState::Ready, "{verdict:?}");
    }
}
