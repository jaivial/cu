//! Idle tabs stop painting: a headless window is kept behind a blank tab
//! unless something needs it in front.
//!
//! A tab cu opens is a background tab, and a background tab produces no
//! frames: it costs no CPU however much the page animates. These put a tab
//! in front, and nothing ever put it back:
//!
//! - `scroll`, a tap and the touch gestures call `Page.bringToFront`, because
//!   wheel and touch input wait for a frame that a hidden tab never makes;
//! - a capture does too, for the same reason (see
//!   `crate::server::in_front_for_capture`): a screenshot of a background tab
//!   that is not animating took five seconds;
//! - the first tab of a browser context is visible from birth: the context
//!   gets a window of its own, whatever `background` says;
//! - the browser's first tab is the active one, and when the tab in front is
//!   closed the browser puts a neighbour there.
//!
//! A tab in front paints for as long as the page animates, and a looping CSS
//! animation or a `requestAnimationFrame` loop never stops. Measured on a
//! headless Chromium 156 with software rendering, on a dashboard with sixteen
//! pulsing dots, with no command being sent: 16-18 % of a core at 60 frames
//! a second, and 200 % with `--disable-frame-rate-limit` (2,700 frames a
//! second). A QA daemon left with such a tab used 4 d 22 h of CPU in two
//! days. It also keeps the browser from ever being idle, which is the only
//! time a memory governor (slimium's) collects.
//!
//! So what brings a tab forward also says how long it should stay there:
//! [`after`] for a scroll or a touch, whose page goes on painting (a lazy
//! list, a menu's transition, a chart drawing itself); [`CAPTURE_LINGER`] for
//! a capture, which is over when it returns and only stays for the capture
//! that often follows it. Every other command (`snapshot`, `text`, `console`,
//! `click`, `type`, `navigate`) works on a hidden tab and holds nothing in
//! front. Twice a second the parker looks: a window with no command running
//! in it and nothing left to stay for gets its blank tab activated
//! (`Target.activateTarget`), which hides the others (same dashboard: 0 %).
//! Whenever a page appears or disappears, every window is looked at again,
//! because that is when the browser changes what is in front by itself.
//!
//! A hidden tab still runs its scripts, timers and requests, and what needs
//! frames brings it forward again itself.
//!
//! Not done where a person may be looking: a browser with a window (headful,
//! Min), and a tab open in the live viewer. `CU_PARK_AFTER=0` turns it off;
//! `CU_PARK_AFTER=<seconds>` changes the stay after a scroll or a touch.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::server::{browser_command, json_string, json_string_value};

/// How long a tab stays in front after a scroll or a touch. Long enough for
/// what the action starts to finish painting, short next to the time an
/// agent takes to read a result and send the next command.
const DEFAULT_AFTER: Duration = Duration::from_secs(3);
/// How long a tab stays in front after a capture: nothing is left to paint,
/// but captures come in runs (the same page at three widths), and a tab that
/// is hidden between them is repainted from cold for each (120-210 ms
/// instead of 77 ms).
pub const CAPTURE_LINGER: Duration = Duration::from_secs(1);
/// How long a window is left as it is after any command: one operation is
/// several commands with microseconds between them, and a look that fell in
/// such a gap would hide the tab half way through a capture.
const COMMAND_GAP: Duration = Duration::from_millis(250);
/// How often the parker looks. A look with nothing due reads memory only.
const TICK: Duration = Duration::from_millis(500);

/// The stay in front after a scroll or a touch, or `None` when parking is off.
pub fn after() -> Option<Duration> {
    static AFTER: OnceLock<Option<Duration>> = OnceLock::new();
    *AFTER.get_or_init(|| parse_after(std::env::var("CU_PARK_AFTER").ok().as_deref()))
}

fn parse_after(value: Option<&str>) -> Option<Duration> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(DEFAULT_AFTER);
    };
    if matches!(
        value.to_lowercase().as_str(),
        "0" | "off" | "false" | "no" | "never"
    ) {
        return None;
    }
    match value.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Some(Duration::from_secs_f64(seconds))
        }
        Ok(_) => None,
        // Not a number: the default is safer than no parking at all.
        Err(_) => Some(DEFAULT_AFTER),
    }
}

/// Set once [`spawn`] has started the parker. Without one nothing would put
/// a tab back, so nothing is stamped and nothing is brought forward for it:
/// `cu start` (one server, no daemon) behaves as it always did.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether this browser's tabs may be hidden and shown by cu: a parker is
/// running and nobody has a window on them.
pub fn applies() -> bool {
    RUNNING.load(Ordering::Relaxed)
        && !crate::diagnostics::headful()
        && !crate::minbrowser::active()
}

/// What cu knows about a tab it has driven since the tab's window was last
/// looked at.
#[derive(Clone, Copy, Debug)]
struct Use {
    /// The window is left as it is until then.
    until: Instant,
    /// Commands running on the tab now.
    busy: u32,
}

#[derive(Default)]
struct Browser {
    used: HashMap<Arc<str>, Use>,
    /// Tabs known to be behind a blank one: hidden by the parker and not
    /// brought forward since. Driving one of these leaves nothing to hide.
    resting: HashSet<String>,
    /// Windows (browser contexts) to look at whether or not a tab of theirs
    /// was driven: the set of pages changed, so what is in front may have.
    pending: HashSet<String>,
    /// The set of pages changed and `pending` has not been filled from it yet.
    disturbed: bool,
}

fn browsers() -> &'static Mutex<HashMap<u16, &'static Mutex<Browser>>> {
    static BROWSERS: OnceLock<Mutex<HashMap<u16, &'static Mutex<Browser>>>> = OnceLock::new();
    BROWSERS.get_or_init(Default::default)
}

fn browser(cdp_port: u16) -> &'static Mutex<Browser> {
    browsers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cdp_port)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(Browser::default()))))
}

/// A command running on a tab. While one exists the tab's window is not
/// parked, however long the command takes.
pub struct InUse {
    cdp_port: u16,
    target: Arc<str>,
}

/// A command starts on `target`.
///
/// This waits while the parker is hiding this browser's windows, so the
/// command's own `Page.bringToFront` always reaches the browser after the
/// parker's `Target.activateTarget`, never before it.
pub fn begin(cdp_port: u16, target: &Arc<str>) -> Option<InUse> {
    RUNNING
        .load(Ordering::Relaxed)
        .then(|| stamp(cdp_port, target))
}

fn stamp(cdp_port: u16, target: &Arc<str>) -> InUse {
    let mut state = browser(cdp_port).lock().unwrap_or_else(|e| e.into_inner());
    let entry = state.used.entry(Arc::clone(target)).or_insert(Use {
        until: Instant::now(),
        busy: 0,
    });
    entry.busy += 1;
    InUse {
        cdp_port,
        target: Arc::clone(target),
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut state = browser(self.cdp_port)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Gone when the parker forgot the tab in between: nothing to count.
        if let Some(entry) = state.used.get_mut(&*self.target) {
            entry.busy = entry.busy.saturating_sub(1);
            entry.until = entry.until.max(Instant::now() + COMMAND_GAP);
        }
    }
}

/// A command was sent to `target` without waiting for its reply.
pub fn touch(cdp_port: u16, target: &Arc<str>) {
    drop(begin(cdp_port, target));
}

/// `target` was brought forward, or is about to be, and should stay in front
/// for `stay`: the parker leaves its window alone until then.
pub fn keep(cdp_port: u16, target: &str, stay: Duration) {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    hold(cdp_port, target, stay);
}

fn hold(cdp_port: u16, target: &str, stay: Duration) {
    let mut state = browser(cdp_port).lock().unwrap_or_else(|e| e.into_inner());
    let until = Instant::now() + stay;
    let entry = state
        .used
        .entry(Arc::from(target))
        .or_insert(Use { until, busy: 0 });
    entry.until = entry.until.max(until);
    state.resting.remove(target);
}

/// A page target appeared or went away. The browser decides by itself what
/// is in front then (a new context's first tab; the neighbour of a tab that
/// was closed, which may be a page the parker had hidden), so every window
/// is looked at again.
pub fn disturbed() {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let all: Vec<&'static Mutex<Browser>> = browsers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .copied()
        .collect();
    for state in all {
        state.lock().unwrap_or_else(|e| e.into_inner()).disturbed = true;
    }
}

/// A page target, as the planner needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub id: String,
    /// The browser context: tabs of one context share a window.
    pub context: String,
    pub blank: bool,
}

/// What to do about one window, or about tabs that are gone.
#[derive(Debug, PartialEq)]
struct Plan {
    /// The window, when this is about one: it is no longer pending.
    context: Option<String>,
    /// The tabs to forget once this is carried out.
    tabs: Vec<Arc<str>>,
    /// The window to park, if there is something in it to hide.
    park: Option<Park>,
}

#[derive(Debug, PartialEq)]
struct Park {
    context: String,
    /// A blank tab the window already has; `None` when one has to be made.
    blank: Option<String>,
    /// The window's pages: all of them are behind the blank tab afterwards.
    pages: Vec<String>,
}

/// Decide, from memory alone, which windows to put behind their blank tab.
///
/// A window is looked at when a tab of it was driven or when it is `pending`.
/// It is left for the next look while a command runs in it, while something
/// brought forward is still meant to stay (`until`), or while a person has
/// one of its tabs open (`watched`). Otherwise its driven tabs are forgotten,
/// and it is parked if one of them may be in front: a driven tab that is not
/// `resting`, or any page at all when the window is pending. A driven tab
/// the browser no longer lists is just forgotten.
fn plan(
    used: &HashMap<Arc<str>, Use>,
    resting: &HashSet<String>,
    pending: &HashSet<String>,
    pages: &[Page],
    now: Instant,
    watched: &dyn Fn(&str) -> bool,
) -> Vec<Plan> {
    let mut plans = Vec::new();
    let gone: Vec<Arc<str>> = used
        .keys()
        .filter(|id| !pages.iter().any(|p| p.id == ***id))
        .cloned()
        .collect();
    if !gone.is_empty() {
        plans.push(Plan {
            context: None,
            tabs: gone,
            park: None,
        });
    }
    let mut contexts: Vec<&str> = pages
        .iter()
        .filter(|p| used.contains_key(p.id.as_str()) || pending.contains(&p.context))
        .map(|p| p.context.as_str())
        .collect();
    contexts.sort_unstable();
    contexts.dedup();
    for context in contexts {
        let window: Vec<&Page> = pages.iter().filter(|p| p.context == context).collect();
        let tabs: Vec<(Arc<str>, Use, &Page)> = window
            .iter()
            .filter_map(|p| {
                used.get_key_value(p.id.as_str())
                    .map(|(id, state)| (Arc::clone(id), *state, *p))
            })
            .collect();
        let settled = tabs
            .iter()
            .all(|(_, state, _)| state.busy == 0 && state.until <= now);
        if !settled || window.iter().any(|p| watched(&p.id)) {
            continue;
        }
        // Only a page with something on it is worth hiding.
        let hide = if pending.contains(context) {
            window.iter().any(|p| !p.blank)
        } else {
            tabs.iter()
                .any(|(_, _, p)| !p.blank && !resting.contains(&p.id))
        };
        plans.push(Plan {
            context: Some(context.to_string()),
            tabs: tabs.into_iter().map(|(id, _, _)| id).collect(),
            park: hide.then(|| Park {
                context: context.to_string(),
                blank: window.iter().find(|p| p.blank).map(|p| p.id.clone()),
                pages: window.iter().map(|p| p.id.clone()).collect(),
            }),
        });
    }
    plans
}

/// Put a blank tab of `context` in front, making one if the window has none.
fn park(cdp_port: u16, park: &Park) -> Result<(), String> {
    let blank = match &park.blank {
        Some(id) => id.clone(),
        None => create_blank(cdp_port, &park.context)?,
    };
    let reply = browser_command(
        cdp_port,
        "Target.activateTarget",
        &format!("{{\"targetId\":{}}}", json_string(&blank)),
    )?;
    match json_string_value(&reply, "message") {
        Some(message) if reply.contains("\"error\":{") => Err(message),
        _ => Ok(()),
    }
}

/// A blank background tab in `context`.
///
/// `Target.createTarget` takes a context id only for a context made with
/// `Target.createBrowserContext`; the profile's own context is the one a
/// target is created in when none is named.
fn create_blank(cdp_port: u16, context: &str) -> Result<String, String> {
    let created = browser_command(cdp_port, "Target.getBrowserContexts", "{}")?;
    let named = created.contains(&json_string(context));
    let reply = browser_command(
        cdp_port,
        "Target.createTarget",
        &if named {
            format!(
                "{{\"url\":\"about:blank\",\"background\":true,\"browserContextId\":{}}}",
                json_string(context)
            )
        } else {
            "{\"url\":\"about:blank\",\"background\":true}".to_string()
        },
    )?;
    json_string_value(&reply, "targetId")
        .filter(|id| !id.is_empty())
        .ok_or_else(|| json_string_value(&reply, "message").unwrap_or(reply))
}

/// One look: park the windows of `cdp_port` that nothing needs in front.
///
/// The browser's lock is held while a window is being hidden, which is what
/// makes a command that starts now wait for it (see [`begin`]). A look with
/// nothing due does no I/O and holds the lock for a map scan.
fn pass(cdp_port: u16) {
    let mut guard = browser(cdp_port).lock().unwrap_or_else(|e| e.into_inner());
    let state = &mut *guard;
    if state.used.is_empty() && state.pending.is_empty() && !state.disturbed {
        return;
    }
    if !applies() {
        *state = Browser::default();
        return;
    }
    let now = Instant::now();
    let due = state.used.values().any(|u| u.busy == 0 && u.until <= now);
    if !due && state.pending.is_empty() && !state.disturbed {
        return;
    }
    let pages = crate::targets::pages();
    if std::mem::take(&mut state.disturbed) {
        state
            .pending
            .extend(pages.iter().map(|p| p.context.clone()));
    }
    state
        .pending
        .retain(|context| pages.iter().any(|p| &p.context == context));
    state.resting.retain(|id| pages.iter().any(|p| &p.id == id));
    let watched = |id: &str| crate::live_stream::watched(cdp_port, id);
    for step in plan(
        &state.used,
        &state.resting,
        &state.pending,
        &pages,
        now,
        &watched,
    ) {
        if let Some(window) = &step.park {
            match park(cdp_port, window) {
                Ok(()) => state.resting.extend(window.pages.iter().cloned()),
                // Not tried again until the window is driven again: a browser
                // that cannot do this once will not do it a moment later.
                Err(error) => eprintln!(
                    "cu: could not put the idle tabs of a window in the background: {error}"
                ),
            }
        }
        for tab in step.tabs {
            state.used.remove(&tab);
        }
        if let Some(context) = step.context {
            state.pending.remove(&context);
        }
    }
}

/// The daemon's parker: twice a second, hide the windows nothing needs in
/// front.
pub fn spawn(cdp_port: u16) {
    if after().is_none() {
        return;
    }
    RUNNING.store(true, Ordering::Relaxed);
    thread::spawn(move || {
        loop {
            thread::sleep(TICK);
            pass(cdp_port);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, context: &str, blank: bool) -> Page {
        Page {
            id: id.into(),
            context: context.into(),
            blank,
        }
    }

    /// (tab, seconds since it may be parked; negative = still to stay, commands running)
    fn used(entries: &[(&str, i64, u32)], now: Instant) -> HashMap<Arc<str>, Use> {
        entries
            .iter()
            .map(|(id, ago, busy)| {
                let until = if *ago >= 0 {
                    now - Duration::from_secs(*ago as u64)
                } else {
                    now + Duration::from_secs(ago.unsigned_abs())
                };
                (Arc::from(*id), Use { until, busy: *busy })
            })
            .collect()
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn nobody(_: &str) -> bool {
        false
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn the_stay_is_three_seconds_unless_told_otherwise() {
        assert_eq!(parse_after(None), Some(DEFAULT_AFTER));
        assert_eq!(parse_after(Some("")), Some(DEFAULT_AFTER));
        assert_eq!(parse_after(Some("2.5")), Some(Duration::from_millis(2500)));
        assert_eq!(parse_after(Some("soon")), Some(DEFAULT_AFTER));
        assert_eq!(DEFAULT_AFTER, Duration::from_secs(3));
        for off in ["0", "off", "false", "no", "never", "-1", " 0 "] {
            assert_eq!(parse_after(Some(off)), None, "{off}");
        }
    }

    #[test]
    fn a_window_nothing_needs_is_parked_behind_its_blank_tab() {
        let now = later();
        let pages = [page("A", "ctx", false), page("B", "ctx", true)];
        let plans = plan(
            &used(&[("A", 0, 0)], now),
            &set(&[]),
            &set(&[]),
            &pages,
            now,
            &nobody,
        );
        assert_eq!(
            plans,
            vec![Plan {
                context: Some("ctx".into()),
                tabs: vec![Arc::from("A")],
                park: Some(Park {
                    context: "ctx".into(),
                    blank: Some("B".into()),
                    pages: vec!["A".into(), "B".into()],
                }),
            }]
        );
    }

    #[test]
    fn a_window_without_a_blank_tab_gets_one_made() {
        let now = later();
        let pages = [page("A", "ctx", false)];
        let plans = plan(
            &used(&[("A", 9, 0)], now),
            &set(&[]),
            &set(&[]),
            &pages,
            now,
            &nobody,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].park.as_ref().unwrap().blank, None);
    }

    #[test]
    fn a_tab_meant_to_stay_or_in_use_keeps_its_window() {
        let now = later();
        let pages = [
            page("A", "ctx", false),
            page("B", "ctx", false),
            page("Z", "ctx", true),
        ];
        // B was scrolled a moment ago and stays for two more seconds: hiding
        // the window for A would hide B under another agent.
        let staying = used(&[("A", 30, 0), ("B", -2, 0)], now);
        assert!(plan(&staying, &set(&[]), &set(&[]), &pages, now, &nobody).is_empty());
        // A command still running counts, whatever its time says.
        let running = used(&[("A", 30, 1)], now);
        assert!(plan(&running, &set(&[]), &set(&[]), &pages, now, &nobody).is_empty());
        // A pending window waits the same way.
        assert!(plan(&staying, &set(&[]), &set(&["ctx"]), &pages, now, &nobody).is_empty());
    }

    #[test]
    fn a_command_on_a_hidden_tab_leaves_nothing_to_hide() {
        let now = later();
        let pages = [page("A", "ctx", false), page("B", "ctx", true)];
        // A was parked and has not been brought forward since: a snapshot of
        // it is forgotten without touching the browser.
        let plans = plan(
            &used(&[("A", 0, 0)], now),
            &set(&["A", "B"]),
            &set(&[]),
            &pages,
            now,
            &nobody,
        );
        assert_eq!(
            plans,
            vec![Plan {
                context: Some("ctx".into()),
                tabs: vec![Arc::from("A")],
                park: None,
            }]
        );
    }

    #[test]
    fn a_pending_window_is_parked_whatever_was_known_about_it() {
        let now = later();
        let pages = [
            page("A", "one", false),
            page("A0", "one", true),
            page("B0", "two", true),
        ];
        // No tab was driven, and A was resting: but a page went away, and the
        // browser may have put A in front in its place.
        let mut plans = plan(
            &used(&[], now),
            &set(&["A", "A0"]),
            &set(&["one", "two"]),
            &pages,
            now,
            &nobody,
        );
        plans.sort_by(|a, b| a.context.cmp(&b.context));
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].park.as_ref().unwrap().context, "one");
        assert_eq!(plans[0].park.as_ref().unwrap().blank.as_deref(), Some("A0"));
        // Nothing but a blank tab: looked at, nothing to hide.
        assert_eq!(plans[1].context.as_deref(), Some("two"));
        assert_eq!(plans[1].park, None);
    }

    #[test]
    fn windows_are_judged_one_by_one() {
        let now = later();
        let pages = [
            page("A", "one", false),
            page("A0", "one", true),
            page("B", "two", false),
        ];
        let plans = plan(
            &used(&[("A", 20, 0), ("B", -1, 0)], now),
            &set(&[]),
            &set(&[]),
            &pages,
            now,
            &nobody,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].tabs, vec![Arc::<str>::from("A")]);
        assert_eq!(plans[0].park.as_ref().unwrap().context, "one");
    }

    #[test]
    fn a_tab_a_person_is_watching_keeps_its_window_in_front() {
        let now = later();
        let pages = [page("A", "ctx", false), page("B", "ctx", true)];
        let watched = |id: &str| id == "A";
        let driven = used(&[("A", 30, 0)], now);
        assert!(plan(&driven, &set(&[]), &set(&[]), &pages, now, &watched).is_empty());
        assert!(plan(&driven, &set(&[]), &set(&["ctx"]), &pages, now, &watched).is_empty());
    }

    #[test]
    fn a_blank_tab_and_a_closed_tab_are_forgotten_without_a_park() {
        let now = later();
        let pages = [page("B", "ctx", true)];
        let mut plans = plan(
            &used(&[("B", 30, 0), ("gone", 30, 0)], now),
            &set(&[]),
            &set(&[]),
            &pages,
            now,
            &nobody,
        );
        plans.sort_by_key(|p| p.tabs[0].to_string());
        assert_eq!(
            plans,
            vec![
                Plan {
                    context: Some("ctx".into()),
                    tabs: vec![Arc::from("B")],
                    park: None,
                },
                Plan {
                    context: None,
                    tabs: vec![Arc::from("gone")],
                    park: None,
                },
            ]
        );
    }

    #[test]
    fn a_command_is_counted_while_it_runs_and_a_stay_is_kept() {
        // A port no other test uses: the registry is process-wide.
        let port = 60_123;
        let target: Arc<str> = Arc::from("T");
        let state = |port| browser(port).lock().unwrap().used.get("T").copied();
        // No parker runs in a test process, so the doors the commands use
        // record nothing.
        assert!(begin(port, &target).is_none());
        keep(port, "T", Duration::from_secs(9));
        assert!(state(port).is_none());

        let running = stamp(port, &target);
        assert_eq!(state(port).unwrap().busy, 1);
        drop(stamp(port, &target));
        assert_eq!(state(port).unwrap().busy, 1);
        drop(running);
        let ended = state(port).unwrap();
        assert_eq!(ended.busy, 0);
        // Left as it is for a moment after a command, not for seconds.
        let left = ended.until.saturating_duration_since(Instant::now());
        assert!(left <= COMMAND_GAP, "{left:?}");

        // Brought forward to stay: the window is held, and the tab is no
        // longer taken for hidden. A shorter stay does not cut a longer one.
        browser(port).lock().unwrap().resting.insert("T".into());
        hold(port, "T", Duration::from_secs(3));
        hold(port, "T", Duration::from_secs(1));
        let held = state(port).unwrap().until;
        assert!(held > Instant::now() + Duration::from_secs(2));
        assert!(!browser(port).lock().unwrap().resting.contains("T"));
    }
}
