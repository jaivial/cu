//! Per-origin traffic budgets: how many navigations may run against one
//! origin at once, how far apart they start, how often a failure is retried,
//! and how long an origin is left alone after it pushed back.
//!
//! Sites score sessions on their request pattern, not only on one request: a
//! burst of parallel tabs against one origin, a retry loop on a 429, or a
//! reload straight back into a challenge all read as automation and make the
//! next check harder. The scheduler sits in front of every navigation cu
//! starts (`/v1/navigate`, and `navigate` actions in a batch):
//!
//! - **concurrency**: at most `concurrency` navigations in flight per origin;
//! - **rate**: navigation starts to one origin at least `interval` apart;
//! - **Retry-After**: a 429/503 that sends `Retry-After` keeps the origin
//!   closed until then;
//! - **challenge backoff**: a challenge, block or rate-limit page without a
//!   header backs the origin off exponentially (2 s, 4 s, ... up to 5 min);
//!   a ready page resets it;
//! - **bounded retry**: a 429/503 whose wait fits the budget, or a transient
//!   network error, is retried at most `retries` times. Challenges and
//!   blocks are never retried automatically.
//!
//! A navigation that would have to wait longer than `max_wait` fails at once
//! with the time left, instead of blocking the agent.
use std::collections::HashMap;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::challenge::ChallengeState;
use crate::policy::BrowserMode;
use crate::server::json_string;

/// The budget every origin gets.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub concurrency: usize,
    pub interval: Duration,
    pub retries: u32,
    pub max_wait: Duration,
}

impl Budget {
    /// Defaults per mode, each overridable from the environment:
    /// `CU_ORIGIN_CONCURRENCY`, `CU_ORIGIN_INTERVAL_MS`, `CU_ORIGIN_RETRIES`,
    /// `CU_ORIGIN_MAX_WAIT_MS`. Fast-test keeps owned test sites unthrottled;
    /// compatibility paces like one person with a couple of tabs.
    pub fn for_mode(mode: BrowserMode) -> Self {
        let base = match mode {
            BrowserMode::FastTest => Self {
                concurrency: 16,
                interval: Duration::ZERO,
                retries: 0,
                max_wait: Duration::from_secs(30),
            },
            BrowserMode::Compatibility => Self {
                concurrency: 2,
                interval: Duration::from_millis(1000),
                retries: 2,
                max_wait: Duration::from_secs(30),
            },
        };
        let num = |key: &str| std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok());
        Self {
            concurrency: num("CU_ORIGIN_CONCURRENCY")
                .map(|n| n.max(1) as usize)
                .unwrap_or(base.concurrency),
            interval: num("CU_ORIGIN_INTERVAL_MS")
                .map(Duration::from_millis)
                .unwrap_or(base.interval),
            retries: num("CU_ORIGIN_RETRIES")
                .map(|n| n.min(5) as u32)
                .unwrap_or(base.retries),
            max_wait: num("CU_ORIGIN_MAX_WAIT_MS")
                .map(|n| Duration::from_millis(n.min(600_000)))
                .unwrap_or(base.max_wait),
        }
    }
}

/// Longest an origin is backed off without a `Retry-After` saying so.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Longest `Retry-After` honoured as-is (a site asking for a day is capped).
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

#[derive(Default)]
struct OriginState {
    in_flight: usize,
    last_start: Option<Instant>,
    not_before: Option<Instant>,
    /// Consecutive defence outcomes; drives the exponential backoff.
    strikes: u32,
    reason: Option<String>,
    navigations: u64,
    defences: u64,
}

struct Scheduler {
    budget: Mutex<Budget>,
    origins: Mutex<HashMap<String, OriginState>>,
    changed: Condvar,
}

fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(|| Scheduler {
        budget: Mutex::new(Budget::for_mode(BrowserMode::FastTest)),
        origins: Mutex::new(HashMap::new()),
        changed: Condvar::new(),
    })
}

/// Set the budget for this daemon (called once at start).
pub fn configure(budget: Budget) {
    *scheduler().budget.lock().unwrap_or_else(|e| e.into_inner()) = budget;
}

pub fn budget() -> Budget {
    *scheduler().budget.lock().unwrap_or_else(|e| e.into_inner())
}

/// `scheme://host[:port]` of an http(s) URL, lower-cased; `None` for
/// anything else (`about:`, `data:`, `file:`), which is never scheduled.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // Credentials in the URL are not part of the origin, and not logged.
    let host = authority.rsplit('@').next()?.to_ascii_lowercase();
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

/// A navigation slot on one origin; released when dropped.
pub struct Permit {
    origin: String,
    /// How long the navigation waited for its slot.
    pub waited: Duration,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let s = scheduler();
        let mut origins = s.origins.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = origins.get_mut(&self.origin) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        s.changed.notify_all();
    }
}

/// Why a navigation was not allowed to start.
#[derive(Debug)]
pub struct Refused {
    pub origin: String,
    pub retry_in: Duration,
    pub reason: String,
}

impl Refused {
    pub fn json(&self) -> String {
        format!(
            "{{\"error\":{},\"origin\":{},\"retry_in_ms\":{}}}",
            json_string(&format!(
                "{} is backing off ({}); retry in {} ms",
                self.origin,
                self.reason,
                self.retry_in.as_millis()
            )),
            json_string(&self.origin),
            self.retry_in.as_millis()
        )
    }
}

/// Wait for a slot to navigate to `url`'s origin. `Ok(None)` for URLs that
/// are not scheduled. Fails at once if the origin is closed for longer than
/// the budget's `max_wait`.
pub fn acquire(url: &str) -> Result<Option<Permit>, Refused> {
    let Some(origin) = origin_of(url) else {
        return Ok(None);
    };
    let budget = budget();
    let s = scheduler();
    let started = Instant::now();
    let mut origins = s.origins.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = Instant::now();
        let state = origins.entry(origin.clone()).or_default();
        let mut ready_at = now;
        if let Some(t) = state.not_before {
            ready_at = ready_at.max(t);
        }
        if let Some(t) = state.last_start {
            ready_at = ready_at.max(t + budget.interval);
        }
        let wait = ready_at.saturating_duration_since(now);
        if wait + started.elapsed() > budget.max_wait {
            return Err(Refused {
                origin: origin.clone(),
                retry_in: wait,
                reason: state
                    .reason
                    .clone()
                    .unwrap_or_else(|| "rate budget".into()),
            });
        }
        if wait.is_zero() && state.in_flight < budget.concurrency {
            state.in_flight += 1;
            state.last_start = Some(now);
            state.navigations += 1;
            return Ok(Some(Permit {
                origin,
                waited: started.elapsed(),
            }));
        }
        if started.elapsed() >= budget.max_wait {
            return Err(Refused {
                origin: origin.clone(),
                retry_in: Duration::from_millis(100),
                reason: format!("{} navigations already in flight", state.in_flight),
            });
        }
        // Woken early by a released slot; otherwise when the wait is over.
        let nap = if wait.is_zero() {
            budget.max_wait.saturating_sub(started.elapsed())
        } else {
            wait
        };
        origins = s
            .changed
            .wait_timeout(origins, nap.max(Duration::from_millis(1)))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// What a navigation ran into, for the origin's record.
pub struct Outcome<'a> {
    pub url: &'a str,
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub state: ChallengeState,
}

/// Record how a navigation went. Returns how long the origin is now closed
/// (zero when it is not).
pub fn report(outcome: &Outcome) -> Duration {
    let Some(origin) = origin_of(outcome.url) else {
        return Duration::ZERO;
    };
    let s = scheduler();
    let mut origins = s.origins.lock().unwrap_or_else(|e| e.into_inner());
    let state = origins.entry(origin.clone()).or_default();
    let pushed_back = outcome.state.is_defence() || matches!(outcome.status, 429 | 503);
    let closed = if !pushed_back {
        if outcome.state == ChallengeState::Ready {
            state.strikes = 0;
            state.reason = None;
        }
        Duration::ZERO
    } else {
        state.strikes = state.strikes.saturating_add(1);
        state.defences += 1;
        let backoff = Duration::from_secs(2)
            .saturating_mul(1 << (state.strikes - 1).min(10))
            .min(MAX_BACKOFF);
        let wait = outcome
            .retry_after
            .map(|r| r.min(MAX_RETRY_AFTER))
            .unwrap_or(backoff);
        state.reason = Some(match outcome.retry_after {
            Some(_) => format!("status {} with Retry-After", outcome.status),
            None => format!("{} page, backoff #{}", outcome.state.name(), state.strikes),
        });
        state.not_before = Some(Instant::now() + wait);
        eprintln!(
            "cu: origin {origin}: {}; closed for {} ms",
            state.reason.as_deref().unwrap_or(""),
            wait.as_millis()
        );
        wait
    };
    s.changed.notify_all();
    closed
}

/// Whether a failed navigation (`errorText`) is worth one more try: the
/// network hiccupped, the site did not refuse anything.
pub fn transient(error: &str) -> bool {
    [
        "ERR_CONNECTION_RESET",
        "ERR_CONNECTION_CLOSED",
        "ERR_EMPTY_RESPONSE",
        "ERR_TIMED_OUT",
        "ERR_CONNECTION_TIMED_OUT",
        "ERR_NETWORK_CHANGED",
        "ERR_HTTP2_PROTOCOL_ERROR",
        "ERR_QUIC_PROTOCOL_ERROR",
    ]
    .iter()
    .any(|e| error.contains(e))
}

/// Pause before retry number `attempt` (1-based) after a transient error.
pub fn retry_pause(attempt: u32) -> Duration {
    Duration::from_millis(500).saturating_mul(1 << attempt.saturating_sub(1).min(4))
}

/// A `Retry-After` value: delay seconds or an HTTP date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = http_date(value)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// Unix seconds of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, _] = parts.as_slice() else {
        return None;
    };
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| month.eq_ignore_ascii_case(m))? as i64
        + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let mut hms = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

/// The main document's response, read off `Network.responseReceived`
/// events: its status and `Retry-After`, nothing else (no cookies, no other
/// headers are kept).
#[derive(Clone, Debug, Default)]
pub struct DocumentResponse {
    pub request_id: String,
    pub frame_id: String,
    pub status: u16,
    pub retry_after: Option<Duration>,
    /// Vendor signals a defence puts in the response headers themselves
    /// (`cf-mitigated: challenge`, `x-datadome: protected`): names and
    /// values of those two headers only, never cookies.
    pub hints: Vec<String>,
}

/// A document response from one CDP event, if it is one.
pub fn document_response(event: &str) -> Option<DocumentResponse> {
    if !event.contains("\"Network.responseReceived\"") {
        return None;
    }
    if crate::server::json_string_value(event, "type").as_deref() != Some("Document") {
        return None;
    }
    let response = crate::server::json_object(event, "response")?;
    let headers = crate::server::json_object(response, "headers").unwrap_or("{}");
    // Header names keep the server's case over HTTP/1.1 and are lower-case
    // over HTTP/2; look for both spellings.
    let retry_after = ["retry-after", "Retry-After", "RETRY-AFTER"]
        .iter()
        .find_map(|k| crate::server::json_string_value(headers, k))
        .and_then(|v| parse_retry_after(&v));
    let header = |names: &[&str]| {
        names
            .iter()
            .find_map(|k| crate::server::json_string_value(headers, k))
    };
    let mut hints = Vec::new();
    if let Some(v) = header(&["cf-mitigated", "Cf-Mitigated", "CF-Mitigated"]) {
        hints.push(format!("cf-mitigated: {}", v.to_ascii_lowercase()));
    }
    if let Some(v) = header(&["x-datadome", "X-Datadome", "X-DataDome"]) {
        hints.push(format!("x-datadome: {}", v.to_ascii_lowercase()));
    }
    Some(DocumentResponse {
        hints,
        request_id: crate::server::json_string_value(event, "requestId").unwrap_or_default(),
        frame_id: crate::server::json_string_value(event, "frameId").unwrap_or_default(),
        status: crate::server::json_number(response, "status").unwrap_or(0.0) as u16,
        retry_after,
    })
}

/// Per-origin state for `/v1/diagnostics`.
pub fn json() -> String {
    let budget = budget();
    let now = Instant::now();
    let origins = scheduler()
        .origins
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<String> = origins
        .iter()
        .map(|(origin, s)| {
            format!(
                "{{\"origin\":{},\"in_flight\":{},\"navigations\":{},\"defences\":{},\"strikes\":{},\"closed_ms\":{},\"reason\":{}}}",
                json_string(origin),
                s.in_flight,
                s.navigations,
                s.defences,
                s.strikes,
                s.not_before
                    .map(|t| t.saturating_duration_since(now).as_millis())
                    .unwrap_or(0),
                s.reason
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".into())
            )
        })
        .collect();
    list.sort();
    format!(
        "{{\"budget\":{{\"concurrency\":{},\"interval_ms\":{},\"retries\":{},\"max_wait_ms\":{}}},\"origins\":[{}]}}",
        budget.concurrency,
        budget.interval.as_millis(),
        budget.retries,
        budget.max_wait.as_millis(),
        list.join(",")
    )
}
