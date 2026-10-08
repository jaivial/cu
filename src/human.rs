//! Credit: this model is Jaime Villanueva's, copied from `src/human.rs` on his
//! unmerged `feat/human-input` branch (worktree `cu-human`, at bf4112c plus
//! uncommitted work), itself ported from the behaviour layer of
//! `invisible_playwright`. Only the pure model is taken: seeds, motion
//! style, paths, landing, the typing persona. The daemon wiring
//! (`CU_HUMAN`, the global act counter, the per-tab pointer) is replaced by
//! [`crate::input`], which seeds it per session from the profile identity
//! ([`crate::session`]). His branch is left untouched.
//!
//! Human-like input: seeded mouse paths, click holds and typing rhythm.
//!
//! Ported from the behaviour layer of `invisible_playwright` (`_motion.py`,
//! `_behaviour.py`). Everything here is pure arithmetic over a session seed:
//! no browser, no clock, so the properties are testable without a page.
//!
//! The ideas worth keeping from that project:
//!
//! - **Every shape parameter is drawn from the seed** ([`MotionStyle`]), so two
//!   sessions trace measurably different families of paths and one seed
//!   reproduces the same paths exactly.
//! - **A path is a Bezier curve walked by arc length** with a bell-shaped speed
//!   profile, a Fitts-law duration, zero-mean two-axis tremor, an occasional
//!   overshoot pulled back on the way in, and samples never closer than a real
//!   mouse reports (8 ms).
//! - **Clicks land near, not on, the centre** and the button is held down for a
//!   log-normal dwell, instead of a zero-length press at the exact middle.
//! - **Typing has a rhythm**: per-key hold and gap, faster across hands, slower
//!   on one hand and slowest on a repeated key, with occasional hesitations.
use std::f64::consts::PI;

/// Two pointer events closer than this describe a device nobody sells.
pub const SAMPLE_FLOOR_MS: f64 = 8.0;
const MIN_STEPS: usize = 2;
const MAX_STEPS: usize = 160;
const MIN_DURATION_MS: f64 = 40.0;
const MAX_DURATION_MS: f64 = 2000.0;
const EPS: f64 = 1e-9;

/// FNV-1a: an independent stream per tag from one seed, so adding a new kind
/// of draw never shifts the numbers an existing one produces.
pub fn mix(seed: u64, tag: &str) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64 ^ seed;
    for b in tag.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// SplitMix64: small, fast and good enough for shaping input noise.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64, tag: &str) -> Self {
        Self(mix(seed, tag))
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.random()
    }
    /// Inclusive on both ends.
    pub fn randint(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as u32
    }
    /// Box-Muller.
    pub fn gauss(&mut self, mean: f64, sigma: f64) -> f64 {
        let u1 = self.random().max(f64::MIN_POSITIVE);
        let u2 = self.random();
        mean + sigma * (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
    /// Right-skewed draw whose *median* is `median`, like human timings.
    pub fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
        median * self.gauss(0.0, sigma).exp()
    }
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// The shape parameters of one session, all drawn from the seed.
#[derive(Clone, Debug)]
pub struct MotionStyle {
    knots: usize,
    knot_base: [f64; 3],
    knot_wobble: f64,
    bow_base: [f64; 3],
    bow_frac: f64,
    bow_abs_px: f64,
    bow_knee_px: f64,
    pivot_angle: f64,
    pivot_strength: f64,
    bow_bias: f64,
    bow_cap_px: f64,
    short_move_px: f64,
    target_w_px: f64,
    fitts_a_ms: f64,
    fitts_b_ms: f64,
    dur_jitter: f64,
    step_ms: f64,
    step_jitter: f64,
    ease_a_lo: f64,
    ease_a_hi: f64,
    ease_r_lo: f64,
    ease_r_hi: f64,
    tremor_across_px: f64,
    tremor_aniso: f64,
    tremor_full_px: f64,
    tremor_burst_p: f64,
    tremor_burst_mult: f64,
    tremor_shape: f64,
    overshoot_p: f64,
    overshoot_frac: f64,
    overshoot_cap_px: f64,
    overshoot_min_px: f64,
    over_start_lo: f64,
    over_start_hi: f64,
    over_shape: f64,
    dup_keep_p: f64,
    dup_run_max: u32,
}

impl MotionStyle {
    pub fn from_seed(seed: u64) -> Self {
        let r = &mut Rng::new(seed, "motion:style");
        let knot_base = [
            r.uniform(0.12, 0.38),
            r.uniform(0.38, 0.62),
            r.uniform(0.62, 0.88),
        ];
        let bow_base = [
            r.uniform(0.55, 1.45),
            r.uniform(-0.70, 1.45),
            r.uniform(-0.70, 1.45),
        ];
        let a_lo = r.uniform(1.70, 2.60);
        let a_hi = (a_lo + r.uniform(0.60, 1.40)).min(3.60).max(a_lo + 0.20);
        let r_lo = r.uniform(0.78, 1.20);
        let r_hi = (r_lo + r.uniform(0.25, 0.65)).min(1.85);
        let over_lo = r.uniform(0.48, 0.62);
        let over_hi = (over_lo + r.uniform(0.12, 0.26)).min(0.88);
        Self {
            knots: r.randint(1, 3) as usize,
            knot_base,
            knot_wobble: r.uniform(0.02, 0.07),
            bow_base,
            bow_frac: r.uniform(0.008, 0.042),
            bow_abs_px: r.uniform(1.0, 4.2),
            bow_knee_px: r.uniform(30.0, 90.0),
            pivot_angle: r.uniform(0.0, 2.0 * PI),
            pivot_strength: r.uniform(0.62, 0.96),
            bow_bias: r.uniform(-0.35, 0.35),
            bow_cap_px: r.uniform(40.0, 110.0),
            short_move_px: r.uniform(40.0, 90.0),
            target_w_px: r.uniform(28.0, 56.0),
            fitts_a_ms: r.uniform(90.0, 190.0),
            fitts_b_ms: r.uniform(90.0, 160.0),
            dur_jitter: r.uniform(0.10, 0.28),
            step_ms: r.uniform(8.0, 18.0),
            step_jitter: r.uniform(0.10, 0.35),
            ease_a_lo: a_lo,
            ease_a_hi: a_hi,
            ease_r_lo: r_lo,
            ease_r_hi: r_hi,
            tremor_across_px: r.uniform(0.16, 0.58),
            tremor_aniso: r.uniform(0.35, 0.95),
            tremor_full_px: r.uniform(70.0, 190.0),
            tremor_burst_p: r.uniform(0.05, 0.20),
            tremor_burst_mult: r.uniform(1.5, 2.8),
            tremor_shape: r.uniform(0.60, 1.60),
            overshoot_p: r.uniform(0.14, 0.48),
            overshoot_frac: r.uniform(0.010, 0.040),
            overshoot_cap_px: r.uniform(18.0, 45.0),
            overshoot_min_px: r.uniform(10.0, 34.0),
            over_start_lo: over_lo,
            over_start_hi: over_hi,
            over_shape: r.uniform(0.80, 1.60),
            dup_keep_p: r.uniform(0.10, 0.40),
            dup_run_max: r.randint(1, 3),
        }
    }
}

/// One synthetic mousemove: where, and how long to wait before sending it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Waypoint {
    pub x: f64,
    pub y: f64,
    pub dt_ms: f64,
    pub t_ms: f64,
}

/// The straight line from start to end, with its unit tangent and normal.
struct Axis {
    fx: f64,
    fy: f64,
    tx: f64,
    ty: f64,
    dist: f64,
    ux: f64,
    uy: f64,
    nx: f64,
    ny: f64,
}

impl Axis {
    fn between(fx: f64, fy: f64, tx: f64, ty: f64) -> Self {
        let dist = (tx - fx).hypot(ty - fy);
        let (ux, uy) = ((tx - fx) / dist, (ty - fy) / dist);
        Self {
            fx,
            fy,
            tx,
            ty,
            dist,
            ux,
            uy,
            nx: -uy,
            ny: ux,
        }
    }
}

/// A reach that lands past its target and is pulled back. `amp == 0` means
/// this movement did not overshoot.
#[derive(Default)]
struct Overshoot {
    amp: f64,
    perp: f64,
    u0: f64,
    shape: f64,
}

impl Overshoot {
    fn weight_at(&self, u: f64) -> f64 {
        if self.amp <= 0.0 || u <= self.u0 {
            return 0.0;
        }
        (PI * (u - self.u0) / (1.0 - self.u0))
            .sin()
            .max(0.0)
            .powf(self.shape)
    }
}

fn bezier_point(ctrl: &[(f64, f64)], t: f64) -> (f64, f64) {
    let mut pts = ctrl.to_vec();
    while pts.len() > 1 {
        pts = pts
            .windows(2)
            .map(|w| {
                (
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                )
            })
            .collect();
    }
    pts[0]
}

/// Cumulative arc length of the curve sampled uniformly in t.
fn arc_table(ctrl: &[(f64, f64)], n: usize) -> (Vec<f64>, Vec<f64>) {
    let ts: Vec<f64> = (0..n).map(|i| i as f64 / (n - 1) as f64).collect();
    let pts: Vec<_> = ts.iter().map(|&t| bezier_point(ctrl, t)).collect();
    let mut cum = vec![0.0];
    for i in 1..n {
        let d = (pts[i].0 - pts[i - 1].0).hypot(pts[i].1 - pts[i - 1].1);
        cum.push(cum[i - 1] + d);
    }
    (ts, cum)
}

fn t_at_arclen(ts: &[f64], cum: &[f64], s: f64) -> f64 {
    let total = *cum.last().unwrap();
    if total <= EPS {
        return 0.0;
    }
    let s = clamp(s, 0.0, total);
    let j = cum.partition_point(|&c| c < s);
    if j == 0 {
        return ts[0];
    }
    if j >= cum.len() {
        return *ts.last().unwrap();
    }
    let span = cum[j] - cum[j - 1];
    let f = if span <= EPS {
        0.0
    } else {
        (s - cum[j - 1]) / span
    };
    ts[j - 1] + (ts[j] - ts[j - 1]) * f
}

/// Cumulative distance for a Beta-shaped speed density: zero speed at both
/// ends, one interior peak.
fn profile_table(a: f64, b: f64) -> Vec<f64> {
    let n = 65;
    let ts: Vec<f64> = (0..n).map(|i| i as f64 / (n - 1) as f64).collect();
    let dens: Vec<f64> = ts
        .iter()
        .map(|&t| t.powf(a - 1.0) * (1.0 - t).powf(b - 1.0))
        .collect();
    let mut cum = vec![0.0];
    for i in 1..n {
        cum.push(cum[i - 1] + 0.5 * (dens[i] + dens[i - 1]) * (ts[i] - ts[i - 1]));
    }
    let total = cum[n - 1];
    let mut out: Vec<f64> = cum.iter().map(|c| c / total).collect();
    out[n - 1] = 1.0;
    out
}

fn profile_at(table: &[f64], u: f64) -> f64 {
    if u <= 0.0 {
        return 0.0;
    }
    if u >= 1.0 {
        return 1.0;
    }
    let x = u * (table.len() - 1) as f64;
    let i = x as usize;
    if i >= table.len() - 1 {
        return table[table.len() - 1];
    }
    table[i] + (table[i + 1] - table[i]) * (x - i as f64)
}

fn control_polygon(rng: &mut Rng, axis: &Axis, st: &MotionStyle) -> Vec<(f64, f64)> {
    // A stroke swinging about a per-seed joint bows away from it, so the same
    // endpoints give one session a left-hand arc and another a right-hand one.
    let swing = -(st.pivot_angle.cos() * axis.nx + st.pivot_angle.sin() * axis.ny);
    let amp_scale =
        st.bow_frac * axis.dist + st.bow_abs_px * axis.dist / (axis.dist + st.bow_knee_px);
    let n_knots = if axis.dist < st.short_move_px {
        1
    } else {
        st.knots
    };
    let mut axial: Vec<f64> = (0..n_knots)
        .map(|j| clamp(st.knot_base[j] + rng.gauss(0.0, st.knot_wobble), 0.04, 0.96))
        .collect();
    axial.sort_by(f64::total_cmp);
    let mut ctrl = vec![(axis.fx, axis.fy)];
    for (j, u) in axial.into_iter().enumerate() {
        let mag = st.bow_base[j];
        let amp = amp_scale
            * (st.pivot_strength * swing * mag
                + (1.0 - st.pivot_strength) * rng.gauss(st.bow_bias, 1.0) * mag);
        let amp = clamp(amp, -st.bow_cap_px, st.bow_cap_px);
        ctrl.push((
            axis.fx + axis.ux * axis.dist * u + axis.nx * amp,
            axis.fy + axis.uy * axis.dist * u + axis.ny * amp,
        ));
    }
    ctrl.push((axis.tx, axis.ty));
    ctrl
}

/// Fitts' law, fed the width of the thing being hit, with log-normal spread.
fn movement_duration(rng: &mut Rng, axis: &Axis, st: &MotionStyle, target_w: f64) -> f64 {
    let w = if target_w > 0.0 {
        target_w
    } else {
        st.target_w_px
    };
    let bits = (axis.dist / w + 1.0).log2();
    let d = (st.fitts_a_ms + st.fitts_b_ms * bits) * rng.gauss(0.0, st.dur_jitter).exp();
    clamp(d, MIN_DURATION_MS, MAX_DURATION_MS)
}

/// Sample times, never closer together than a real mouse reports. Samples
/// under the floor are dropped, not squeezed, so the schedule keeps its shape.
fn sample_times(rng: &mut Rng, duration: f64, st: &MotionStyle) -> Vec<f64> {
    let n = ((duration / st.step_ms).round() as usize).clamp(MIN_STEPS, MAX_STEPS);
    let incs: Vec<f64> = (0..n)
        .map(|_| rng.gauss(0.0, st.step_jitter).exp())
        .collect();
    let total: f64 = incs.iter().sum();
    let mut times = vec![0.0];
    let mut acc = 0.0;
    for c in incs {
        acc += c;
        times.push(duration * acc / total);
    }
    *times.last_mut().unwrap() = duration;
    let mut kept = vec![0.0];
    for &t in &times[1..times.len() - 1] {
        if t - kept[kept.len() - 1] >= SAMPLE_FLOOR_MS {
            kept.push(t);
        }
    }
    if kept.len() > 1 && duration - kept[kept.len() - 1] < SAMPLE_FLOOR_MS {
        kept.pop();
    }
    kept.push(duration);
    kept
}

fn draw_overshoot(rng: &mut Rng, axis: &Axis, st: &MotionStyle) -> Overshoot {
    if axis.dist < st.overshoot_min_px || rng.random() >= st.overshoot_p {
        return Overshoot::default();
    }
    let amp = clamp(
        st.overshoot_frac * axis.dist * rng.gauss(0.0, 0.35).exp(),
        0.0,
        st.overshoot_cap_px,
    );
    Overshoot {
        amp,
        perp: amp * rng.gauss(0.0, 0.40),
        u0: rng.uniform(st.over_start_lo, st.over_start_hi),
        shape: st.over_shape,
    }
}

/// Plan one movement from `from` to `to`. Endpoints are exact; the first
/// waypoint is the start (sent at once), the last is the target.
pub fn plan_path(
    rng: &mut Rng,
    from: (f64, f64),
    to: (f64, f64),
    st: &MotionStyle,
    target_w: f64,
) -> Vec<Waypoint> {
    let (fx, fy, tx, ty) = (from.0, from.1, to.0, to.1);
    if (tx - fx).hypot(ty - fy) < EPS || (fx.round() == tx.round() && fy.round() == ty.round()) {
        return vec![Waypoint {
            x: tx,
            y: ty,
            dt_ms: 0.0,
            t_ms: 0.0,
        }];
    }
    let axis = Axis::between(fx, fy, tx, ty);
    let ctrl = control_polygon(rng, &axis, st);
    let duration = movement_duration(rng, &axis, st, target_w);
    let times = sample_times(rng, duration, st);
    let ease_a = rng.uniform(st.ease_a_lo, st.ease_a_hi);
    let prof = profile_table(ease_a, ease_a * rng.uniform(st.ease_r_lo, st.ease_r_hi));
    let over = draw_overshoot(rng, &axis, st);

    // Walk the curve by arc length so the speed profile is really the speed.
    let n_arc = ((axis.dist / 3.0) as usize + 8).clamp(24, 240);
    let (ts, cum) = arc_table(&ctrl, n_arc);
    let length = cum[cum.len() - 1];
    let last = times.len() - 1;
    let mut raw: Vec<(f64, f64, f64)> = times
        .iter()
        .enumerate()
        .map(|(i, &tm)| {
            if i == 0 {
                return (fx, fy, 0.0);
            }
            if i == last {
                return (tx, ty, tm);
            }
            let u = tm / duration;
            let (mut px, mut py) =
                bezier_point(&ctrl, t_at_arclen(&ts, &cum, profile_at(&prof, u) * length));
            let w = over.weight_at(u);
            px += axis.ux * over.amp * w + axis.nx * over.perp * w;
            py += axis.uy * over.amp * w + axis.ny * over.perp * w;
            (px, py, tm)
        })
        .collect();

    // Tremor: two independent, zero-mean components on every interior point.
    let gain = (axis.dist / st.tremor_full_px).sqrt().min(1.0);
    let across = st.tremor_across_px * gain;
    let along = across * st.tremor_aniso;
    for p in raw.iter_mut().take(last).skip(1) {
        let w = (PI * p.2 / duration).sin().max(0.0).powf(st.tremor_shape);
        let mut a = rng.gauss(0.0, along);
        let mut c = rng.gauss(0.0, across);
        if rng.random() < st.tremor_burst_p {
            a += rng.gauss(0.0, along * st.tremor_burst_mult);
            c += rng.gauss(0.0, across * st.tremor_burst_mult);
        }
        p.0 += (axis.ux * a + axis.nx * c) * w;
        p.1 += (axis.uy * a + axis.ny * c) * w;
    }

    // Bounded runs of repeated device pixels; the endpoint always survives.
    let mut out: Vec<Waypoint> = Vec::with_capacity(raw.len());
    let mut keys: Vec<(i64, i64)> = Vec::with_capacity(raw.len());
    let (mut prev_t, mut run) = (0.0, 0u32);
    for (i, &(px, py, tm)) in raw.iter().enumerate() {
        let key = (px.round() as i64, py.round() as i64);
        if keys.last() == Some(&key) {
            run += 1;
            if !(run <= st.dup_run_max && rng.random() < st.dup_keep_p) {
                if i != last {
                    continue;
                }
                out.pop();
                keys.pop();
                prev_t = out.last().map_or(0.0, |w| w.t_ms);
                run = 0;
            }
        } else {
            run = 0;
        }
        out.push(Waypoint {
            x: px,
            y: py,
            dt_ms: tm - prev_t,
            t_ms: tm,
        });
        keys.push(key);
        prev_t = tm;
    }
    out
}

/// Where inside a box the pointer stops: Gaussian about the centre, clipped to
/// its middle 80%, essentially never the exact centre pixel.
pub fn landing_point(rng: &mut Rng, cx: f64, cy: f64, w: f64, h: f64) -> (f64, f64) {
    let (spread, keep) = (0.20, 0.40);
    (
        clamp(rng.gauss(cx, w * spread), cx - keep * w, cx + keep * w),
        clamp(rng.gauss(cy, h * spread), cy - keep * h, cy + keep * h),
    )
}

/// A plausible resting place for the cursor before the first move of a page:
/// never the origin, which no real navigation leaves it at.
pub fn initial_pointer(seed: u64, nonce: u64, vw: f64, vh: f64) -> (f64, f64) {
    let r = &mut Rng::new(seed, &format!("pointer-origin:{nonce}"));
    if r.random() < 0.58 {
        (
            r.uniform(0.18 * vw, 0.82 * vw),
            r.uniform(0.12 * vh, 0.78 * vh),
        )
    } else {
        (
            r.uniform(0.08 * vw, 0.72 * vw),
            r.uniform(0.005 * vh, 0.09 * vh),
        )
    }
}

const LEFT_HAND: &str = "`12345qwertasdfgzxcvb~!@#$%QWERTASDFGZXCVB";
const RIGHT_HAND: &str = "67890-=yuiop[]\\hjkl;'nm,./^&*()_+YUIOP{}|HJKL:\"NM<>?";

fn hand(c: char) -> Option<bool> {
    if LEFT_HAND.contains(c) {
        Some(false)
    } else if RIGHT_HAND.contains(c) {
        Some(true)
    } else {
        None
    }
}

/// One session's hand at the keyboard and on the mouse button.
#[derive(Clone, Debug)]
pub struct Persona {
    pub seed: u64,
    dwell_median_ms: f64,
    dwell_sigma: f64,
    gap_median_ms: f64,
    gap_sigma: f64,
    alternate_hand_factor: f64,
    same_hand_factor: f64,
    same_key_factor: f64,
    hesitation_rate: f64,
    hesitation_median_ms: f64,
    hesitation_sigma: f64,
    click_dwell_median_ms: f64,
    click_dwell_sigma: f64,
}

impl Persona {
    pub fn from_seed(seed: u64) -> Self {
        let r = &mut Rng::new(seed, "typing-persona");
        let mut p = Self {
            seed,
            dwell_median_ms: r.uniform(62.0, 118.0),
            dwell_sigma: r.uniform(0.22, 0.42),
            gap_median_ms: r.uniform(95.0, 235.0),
            gap_sigma: r.uniform(0.34, 0.62),
            alternate_hand_factor: r.uniform(0.74, 0.92),
            same_hand_factor: r.uniform(1.04, 1.24),
            same_key_factor: r.uniform(1.25, 1.75),
            hesitation_rate: r.uniform(0.02, 0.07),
            hesitation_median_ms: r.uniform(420.0, 1250.0),
            hesitation_sigma: r.uniform(0.40, 0.70),
            click_dwell_median_ms: 0.0,
            click_dwell_sigma: 0.0,
        };
        let r = &mut Rng::new(seed, "pointer-persona");
        p.click_dwell_median_ms = r.uniform(58.0, 124.0);
        p.click_dwell_sigma = r.uniform(0.20, 0.40);
        p
    }

    /// How long the mouse button stays down for the `nonce`-th click.
    pub fn click_dwell_ms(&self, nonce: u64) -> f64 {
        Rng::new(self.seed, &format!("click:{nonce}"))
            .log_normal(self.click_dwell_median_ms, self.click_dwell_sigma)
    }

    /// One `(dwell_ms, gap_ms)` per character: how long the key is held, and
    /// the wait before the next key goes down (0 after the last).
    pub fn plan_typing(&self, text: &str, nonce: u64) -> Vec<(f64, f64)> {
        let r = &mut Rng::new(self.seed, &format!("typing:{nonce}"));
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::with_capacity(chars.len());
        for (i, &c) in chars.iter().enumerate() {
            let dwell = r.log_normal(self.dwell_median_ms, self.dwell_sigma);
            let Some(&next) = chars.get(i + 1) else {
                out.push((dwell, 0.0));
                break;
            };
            let mut gap = r.log_normal(self.gap_median_ms, self.gap_sigma);
            if c == next {
                gap *= self.same_key_factor;
            } else if let (Some(a), Some(b)) = (hand(c), hand(next)) {
                gap *= if a != b {
                    self.alternate_hand_factor
                } else {
                    self.same_hand_factor
                };
            }
            if r.random() < self.hesitation_rate {
                gap += r.log_normal(self.hesitation_median_ms, self.hesitation_sigma);
            }
            out.push((dwell, gap));
        }
        out
    }
}

/// A fresh seed when none is given: time and pid, mixed.
pub fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    mix(nanos ^ ((std::process::id() as u64) << 32), "seed")
}

