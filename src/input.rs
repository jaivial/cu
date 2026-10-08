//! How input reaches the page: `Instant` (the default, for owned test sites
//! and CI) or `Paced`, with `CU_INPUT=paced`.
//!
//! `Instant` is what cu has always done: a press and a release at the exact
//! centre of the element, text inserted in one `Input.insertText`, and an
//! instant `scrollIntoView` to bring a target on screen.
//!
//! `Paced` sends the events a person's hardware produces, in the order a
//! browser expects them, shaped by the human model in [`crate::human`]
//! (Jaime's, from `feat/human-input`):
//!
//! - **one hand per session**: every shape parameter (curvature, speed,
//!   tremor, overshoot, typing rhythm, click dwell) is drawn from a seed, and
//!   the seed is the profile's identity seed ([`crate::session`]) -- mixed
//!   with the context name for a `?context=` jar -- so one profile keeps its
//!   hand across restarts and two profiles have different ones, while each
//!   movement is still a fresh draw (no restart replays a path). `CU_SEED`
//!   replays a run exactly;
//! - the pointer **moves** along a Bezier path walked by arc length with a
//!   bell-shaped speed profile, Fitts-law duration, zero-mean tremor and an
//!   occasional overshoot pulled back, never sampled faster than a real mouse
//!   (8 ms); it starts from where it was left (a fresh tab: somewhere
//!   plausible, never the origin), lands inside the middle of the target box
//!   rather than on its centre, and the button is **held** for a log-normal
//!   dwell;
//! - a target off screen is **scrolled to with the wheel** (`mouseWheel`
//!   notches in flicks), not teleported; the instant scroll stays as the
//!   fallback when the wheel cannot reach it (an inner scroller);
//! - a field is **focused the way a person would**: with Tab when it is the
//!   next field after the focused one, otherwise with a click; every
//!   character is a **keyDown/keyUp** pair with a per-key hold and a gap that
//!   is shorter across hands, longer on one hand or a repeated key, with the
//!   occasional hesitation.
//!
//! Nothing is ever mistyped: the field receives exactly the given text,
//! which is what matters for credentials. Plausible timing is not evidence
//! of a person and is not presented as such; it only stops cu's input from
//! being structurally different from real input.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::human::{self, MotionStyle, Persona, Rng, Waypoint};
use crate::server::Tab;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPolicy {
    Instant,
    Paced,
}

impl InputPolicy {
    /// `CU_INPUT=paced`, otherwise `Instant`.
    pub fn from_env() -> Self {
        match std::env::var("CU_INPUT").as_deref() {
            Ok("paced") => Self::Paced,
            _ => Self::Instant,
        }
    }
}

/// One session's hand: its motion style and its typist.
struct Hand {
    seed: u64,
    style: MotionStyle,
    persona: Persona,
}

/// The seed of the session `tab` belongs to: `CU_SEED`, else the profile
/// identity's seed (mixed with the context name for a named context), else
/// one fresh seed for the daemon's life.
fn session_seed(tab: &Tab) -> u64 {
    static FALLBACK: OnceLock<u64> = OnceLock::new();
    let base = std::env::var("CU_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(crate::session::seed)
        .unwrap_or_else(|| *FALLBACK.get_or_init(human::random_seed));
    match crate::server::context_of(tab) {
        Some(name) => human::mix(base, &format!("context:{name}")),
        None => base,
    }
}

fn hand(tab: &Tab) -> &'static Hand {
    static HANDS: OnceLock<Mutex<HashMap<u64, &'static Hand>>> = OnceLock::new();
    let seed = session_seed(tab);
    let mut hands = HANDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    hands.entry(seed).or_insert_with(|| {
        Box::leak(Box::new(Hand {
            seed,
            style: MotionStyle::from_seed(seed),
            persona: Persona::from_seed(seed),
        }))
    })
}

/// Every movement, click, word and scroll gets its own random stream. The
/// count starts at a fresh offset per daemon, so a profile keeps its hand
/// (the style) but never replays the exact same path after a restart; with
/// `CU_SEED` it starts at 0, so a run is reproducible.
fn next_act() -> u64 {
    static ACTS: OnceLock<AtomicU64> = OnceLock::new();
    ACTS.get_or_init(|| {
        let replay = std::env::var("CU_SEED").is_ok_and(|v| v.parse::<u64>().is_ok());
        AtomicU64::new(if replay { 0 } else { human::random_seed() >> 1 })
    })
    .fetch_add(1, Ordering::Relaxed)
}

fn pointers() -> &'static Mutex<HashMap<Tab, (f64, f64)>> {
    static POINTERS: OnceLock<Mutex<HashMap<Tab, (f64, f64)>>> = OnceLock::new();
    POINTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the pointer was left on `tab`, if it has moved there yet.
pub fn pointer(tab: &Tab) -> Option<(f64, f64)> {
    pointers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .copied()
}

pub fn set_pointer(tab: &Tab, at: (f64, f64)) {
    pointers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), at);
}

/// A plausible resting place for a tab's pointer before its first move, in
/// a `width` x `height` viewport: never the origin.
pub fn origin(tab: &Tab, width: f64, height: f64) -> (f64, f64) {
    human::initial_pointer(hand(tab).seed, next_act(), width, height)
}

/// A planned click: the path to the point, the point, the button hold.
pub struct ClickPlan {
    /// Waypoints after the start, each with the pause before it is sent.
    pub path: Vec<((f64, f64), Duration)>,
    pub point: (f64, f64),
    pub hold: Duration,
}

fn ms(value: f64) -> Duration {
    Duration::from_micros((value.max(0.0) * 1000.0) as u64)
}

fn waypoints(path: Vec<Waypoint>) -> Vec<((f64, f64), Duration)> {
    // The first waypoint is the start itself; only a path of one point (a
    // move under a pixel) has nothing else to send.
    let skip = usize::from(path.len() > 1);
    path.into_iter()
        .skip(skip)
        .map(|w| ((w.x, w.y), ms(w.dt_ms)))
        .collect()
}

/// Plan a click on the `w` x `h` box centred at `centre`, from `from`.
pub fn plan_click(tab: &Tab, from: (f64, f64), centre: (f64, f64), w: f64, h: f64) -> ClickPlan {
    let hand = hand(tab);
    let n = next_act();
    let point = human::landing_point(
        &mut Rng::new(hand.seed, &format!("land:{n}")),
        centre.0,
        centre.1,
        w.max(1.0),
        h.max(1.0),
    );
    let path = human::plan_path(
        &mut Rng::new(hand.seed, &format!("move:{n}")),
        from,
        point,
        &hand.style,
        w.min(h).max(1.0),
    );
    ClickPlan {
        path: waypoints(path),
        point,
        hold: ms(hand.persona.click_dwell_ms(n)),
    }
}

/// Per-character `(hold, gap)` for `text`, from the session's typist.
pub fn plan_typing(tab: &Tab, text: &str) -> Vec<(Duration, Duration)> {
    hand(tab)
        .persona
        .plan_typing(text, next_act())
        .into_iter()
        .map(|(hold, gap)| (ms(hold), ms(gap)))
        .collect()
}

/// Pixels one wheel notch scrolls (Chromium's default for a line-based
/// wheel). A real wheel reports fixed notches, so the delta is not jittered;
/// the rhythm is.
pub const NOTCH_PX: f64 = 100.0;

/// The wheel events for scrolling `dy` pixels: `(delta, pause before)`.
/// Notches come in flicks of a few, close together, with a longer pause
/// between flicks, as a finger rolls a wheel. The last notch carries the
/// remainder so the total is exact.
pub fn plan_scroll(tab: &Tab, dy: f64) -> Vec<(f64, Duration)> {
    let hand = hand(tab);
    let r = &mut Rng::new(hand.seed, &format!("scroll:{}", next_act()));
    let sign = dy.signum();
    let mut left = dy.abs();
    let mut out = Vec::new();
    let mut in_flick = 0u32;
    let mut flick = r.randint(2, 6);
    while left > 0.5 {
        let delta = left.min(NOTCH_PX);
        let pause = if out.is_empty() {
            r.uniform(40.0, 140.0)
        } else if in_flick < flick {
            r.log_normal(38.0, 0.35)
        } else {
            in_flick = 0;
            flick = r.randint(2, 6);
            r.log_normal(260.0, 0.45)
        };
        in_flick += 1;
        out.push((sign * delta, ms(pause)));
        left -= delta;
    }
    out
}

/// The `Input.dispatchKeyEvent` key/code/keyCode of a typed character, for
/// the characters a US layout types directly; others get the character as
/// `key` and no code, which browsers accept for text input.
pub fn key_of(c: char) -> (String, String, u32) {
    match c {
        'a'..='z' => (
            c.to_string(),
            format!("Key{}", c.to_ascii_uppercase()),
            c.to_ascii_uppercase() as u32,
        ),
        'A'..='Z' => (c.to_string(), format!("Key{c}"), c as u32),
        '0'..='9' => (c.to_string(), format!("Digit{c}"), c as u32),
        ' ' => (" ".into(), "Space".into(), 32),
        _ => (c.to_string(), String::new(), 0),
    }
}
