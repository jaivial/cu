//! How input reaches the page: `Instant` (the default, for owned test sites
//! and CI) or `Paced`, with `CU_INPUT=paced`.
//!
//! `Instant` is what cu has always done: a press and a release at the exact
//! centre of the element, and text inserted in one `Input.insertText`.
//! `Paced` sends the events a person's hardware produces, in the order a
//! browser expects them:
//!
//! - the pointer **moves** to the target over a short eased path from where it
//!   last was (`mouseMoved` steps, so hover, `pointerover/enter` and
//!   `mousemove` listeners fire), lands near the centre rather than on it,
//!   and the button is **held** for a moment between press and release;
//! - a text field is **clicked** before it is typed into, and every character
//!   is a **keyDown/keyUp** pair (the `keydown`, `keypress`, `input`, `keyup`
//!   sequence), with a gap between keys.
//!
//! The timing is varied but nothing is ever mistyped: the text that arrives
//! is exactly the text asked for, which is what matters for credentials and
//! other data. Plausible timing is not evidence of a person and is not
//! presented as such; it only stops cu's input from being structurally
//! different from real input (no movement, zero-length clicks, text that
//! appears without key events).
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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

/// A small xorshift generator: timing jitter only, never anything secret.
fn next_random() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Uniform in `[low, high)`.
pub fn between(low: f64, high: f64) -> f64 {
    low + (high - low) * next_random()
}

fn pointers() -> &'static Mutex<HashMap<Tab, (f64, f64)>> {
    static POINTERS: OnceLock<Mutex<HashMap<Tab, (f64, f64)>>> = OnceLock::new();
    POINTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the pointer was left on `tab`; a fresh tab starts it somewhere in
/// the upper part of the viewport rather than at the origin.
pub fn pointer(tab: &Tab) -> (f64, f64) {
    *pointers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(tab.clone())
        .or_insert_with(|| (between(80.0, 400.0), between(60.0, 240.0)))
}

pub fn set_pointer(tab: &Tab, at: (f64, f64)) {
    pointers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), at);
}

/// A point near the centre of a target: within a few pixels, never on the
/// exact centre every time.
pub fn near(center: (f64, f64)) -> (f64, f64) {
    (center.0 + between(-3.0, 3.0), center.1 + between(-2.0, 2.0))
}

/// Points from `from` to `to` (excluding `from`, ending exactly on `to`) and
/// the pause after each: a quadratic curve with a sideways bow, eased in
/// and out, its step count growing with the distance.
pub fn path(from: (f64, f64), to: (f64, f64)) -> Vec<((f64, f64), Duration)> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let distance = (dx * dx + dy * dy).sqrt();
    if distance < 1.0 {
        return vec![(to, Duration::from_millis(8))];
    }
    let steps = ((distance / 25.0).ceil() as usize).clamp(4, 40);
    // A control point off the straight line, so the path bows a little.
    let bow = between(-0.15, 0.15) * distance;
    let (nx, ny) = (-dy / distance, dx / distance);
    let control = (
        from.0 + dx * 0.5 + nx * bow,
        from.1 + dy * 0.5 + ny * bow,
    );
    // Total duration roughly follows Fitts' law, 120-700 ms.
    let total = (90.0 + 110.0 * (distance / 8.0 + 1.0).log2()).clamp(120.0, 700.0);
    (1..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            let e = t * t * (3.0 - 2.0 * t); // smoothstep: slow, fast, slow
            let u = 1.0 - e;
            let point = if i == steps {
                to
            } else {
                (
                    u * u * from.0 + 2.0 * u * e * control.0 + e * e * to.0,
                    u * u * from.1 + 2.0 * u * e * control.1 + e * e * to.1,
                )
            };
            let pause = (total / steps as f64 * between(0.8, 1.2)).max(8.0);
            (point, Duration::from_micros((pause * 1000.0) as u64))
        })
        .collect()
}

/// How long the button stays down in a click.
pub fn hold() -> Duration {
    Duration::from_millis(between(55.0, 130.0) as u64)
}

/// How long a key stays down.
pub fn key_hold() -> Duration {
    Duration::from_millis(between(30.0, 90.0) as u64)
}

/// The gap before the next key; a little longer after a space or
/// punctuation, as typing goes.
pub fn key_gap(previous: char) -> Duration {
    let base = between(45.0, 140.0);
    let extra = if previous == ' ' || previous.is_ascii_punctuation() {
        between(20.0, 120.0)
    } else {
        0.0
    };
    Duration::from_millis((base + extra) as u64)
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
