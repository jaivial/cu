//! Idle tabs stop painting: a window nobody has driven for a few seconds is
//! put behind a blank tab.
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
//! - the browser's first tab is the active one.
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
//! So every command stamps the tab it runs on, and once a second the parker
//! looks: when no tab of a window has been used for [`after`] and none is in
//! the middle of a command, it activates a blank tab of that window
//! (`Target.activateTarget`), which hides the others (same dashboard: 0 %).
//! A hidden tab still runs its scripts, timers and requests; `snapshot`,
//! `text`, `console`, `click` and `type` work on it as they did before it was
//! ever brought forward, and what needs frames (a capture, a scroll, a touch)
//! brings it forward again itself.
//!
//! Not done where a person may be looking: a browser with a window (headful,
//! Min), and a tab open in the live viewer. `CU_PARK_AFTER=0` turns it off;
//! `CU_PARK_AFTER=<seconds>` changes the wait.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::server::{browser_command, json_string, json_string_value};

/// Idle time after which a window's tabs are parked. Long enough for what an
/// action starts to finish painting (a lazy list after a scroll, a menu's
/// transition, a chart drawing itself), short next to the time an agent takes
/// to read a result and send the next command.
const DEFAULT_AFTER: Duration = Duration::from_secs(3);
/// How often the parker looks. A look with nothing stale reads memory only.
const TICK: Duration = Duration::from_secs(1);

/// The idle time before a window is parked, or `None` when parking is off.
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

/// When a tab was last used, and how many commands are running on it now.
#[derive(Clone, Copy, Debug)]
struct Use {
    at: Instant,
    busy: u32,
}

#[derive(Default)]
struct Browser {
    used: HashMap<Arc<str>, Use>,
    /// The blank tab last put in front of each window (context -> target).
    /// When it is closed the window's other tab is in front again, with
    /// nobody having used it: see [`reopened`].
    parked: HashMap<String, String>,
}

fn browser(cdp_port: u16) -> &'static Mutex<Browser> {
    static BROWSERS: OnceLock<Mutex<HashMap<u16, &'static Mutex<Browser>>>> = OnceLock::new();
    BROWSERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cdp_port)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(Browser::default()))))
}

/// A command running on a tab. While one exists the tab's window is not
/// parked, however long the command takes; dropping it stamps the tab.
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
        at: Instant::now(),
        busy: 0,
    });
    entry.at = Instant::now();
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
            entry.at = Instant::now();
            entry.busy = entry.busy.saturating_sub(1);
        }
    }
}

/// A command was sent to `target` without waiting for its reply.
pub fn touch(cdp_port: u16, target: &Arc<str>) {
    drop(begin(cdp_port, target));
}

/// A page target, as the planner needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub id: String,
    /// The browser context: tabs of one context share a window.
    pub context: String,
    pub blank: bool,
}

/// What to do about the used tabs of one window.
#[derive(Debug, PartialEq)]
struct Plan {
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
}

/// Decide, from memory alone, which windows are idle.
///
/// A window is idle when every used tab in it was last used at least `idle`
/// ago and none has a command running. A window with a tab a person is
/// looking at (`watched`) is left alone and looked at again next time. A used
/// tab the browser no longer lists is just forgotten.
fn plan(
    used: &HashMap<Arc<str>, Use>,
    pages: &[Page],
    now: Instant,
    idle: Duration,
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
            tabs: gone,
            park: None,
        });
    }
    let mut contexts: Vec<&str> = pages
        .iter()
        .filter(|p| used.contains_key(p.id.as_str()))
        .map(|p| p.context.as_str())
        .collect();
    contexts.sort_unstable();
    contexts.dedup();
    for context in contexts {
        let window: Vec<&Page> = pages.iter().filter(|p| p.context == context).collect();
        let tabs: Vec<(Arc<str>, Use, bool)> = window
            .iter()
            .filter_map(|p| {
                used.get_key_value(p.id.as_str())
                    .map(|(id, state)| (Arc::clone(id), *state, p.blank))
            })
            .collect();
        let resting = tabs
            .iter()
            .all(|(_, state, _)| state.busy == 0 && now.duration_since(state.at) >= idle);
        if !resting || window.iter().any(|p| watched(&p.id)) {
            continue;
        }
        // Only a page with something on it is worth hiding.
        let hide = tabs.iter().any(|(_, _, blank)| !blank);
        plans.push(Plan {
            tabs: tabs.into_iter().map(|(id, _, _)| id).collect(),
            park: hide.then(|| Park {
                context: context.to_string(),
                blank: window.iter().find(|p| p.blank).map(|p| p.id.clone()),
            }),
        });
    }
    plans
}

/// The windows whose parking tab is gone and that nobody is using: closing
/// the blank tab in front (an agent tidying up the `about:blank` it sees in
/// `cu tabs`) puts a parked page back in front, and no command stamps it, so
/// without this it would paint until somebody used it again.
fn reopened(
    parked: &HashMap<String, String>,
    used: &HashMap<Arc<str>, Use>,
    pages: &[Page],
    watched: &dyn Fn(&str) -> bool,
) -> Vec<(String, Option<Park>)> {
    parked
        .iter()
        .filter(|(_, blank)| !pages.iter().any(|p| p.id == **blank))
        .map(|(context, _)| {
            let window: Vec<&Page> = pages.iter().filter(|p| &p.context == context).collect();
            // A window in use is parked again by the ordinary rule once it
            // rests; one with nothing but blank tabs has nothing to hide.
            let leave = window.iter().all(|p| p.blank)
                || window.iter().any(|p| used.contains_key(p.id.as_str()))
                || window.iter().any(|p| watched(&p.id));
            (
                context.clone(),
                (!leave).then(|| Park {
                    context: context.clone(),
                    blank: window.iter().find(|p| p.blank).map(|p| p.id.clone()),
                }),
            )
        })
        .collect()
}

/// Put a blank tab of `context` in front, making one if the window has none.
/// Returns the blank tab.
fn park(cdp_port: u16, park: &Park) -> Result<String, String> {
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
        _ => Ok(blank),
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

/// One look: park the windows of `cdp_port` that have gone idle.
///
/// The browser's lock is held while a window is being hidden, which is what
/// makes a command that starts now wait for it (see [`begin`]). A look that
/// finds nothing stale does no I/O and holds the lock for a map scan.
fn pass(cdp_port: u16, idle: Duration) {
    let mut guard = browser(cdp_port).lock().unwrap_or_else(|e| e.into_inner());
    let state = &mut *guard;
    if state.used.is_empty() && state.parked.is_empty() {
        return;
    }
    if !applies() {
        state.used.clear();
        state.parked.clear();
        return;
    }
    let now = Instant::now();
    let stale = state
        .used
        .values()
        .any(|u| u.busy == 0 && now.duration_since(u.at) >= idle);
    if !stale && state.parked.is_empty() {
        return;
    }
    let pages = crate::targets::pages();
    let watched = |id: &str| crate::live_stream::watched(cdp_port, id);
    // Not tried again until the window is used again: a browser that cannot
    // do this once will not do it a second later either.
    let failed = |error: String| {
        eprintln!("cu: could not put the idle tabs of a window in the background: {error}")
    };
    for (context, again) in reopened(&state.parked, &state.used, &pages, &watched) {
        state.parked.remove(&context);
        if let Some(window) = again {
            match park(cdp_port, &window) {
                Ok(blank) => {
                    state.parked.insert(context, blank);
                }
                Err(error) => failed(error),
            }
        }
    }
    if !stale {
        return;
    }
    for step in plan(&state.used, &pages, now, idle, &watched) {
        if let Some(window) = &step.park {
            match park(cdp_port, window) {
                Ok(blank) => {
                    state.parked.insert(window.context.clone(), blank);
                }
                Err(error) => failed(error),
            }
        }
        for tab in step.tabs {
            state.used.remove(&tab);
        }
    }
}

/// The daemon's parker: once a second, hide the windows that have gone idle.
pub fn spawn(cdp_port: u16) {
    let Some(idle) = after() else {
        return;
    };
    RUNNING.store(true, Ordering::Relaxed);
    thread::spawn(move || {
        loop {
            thread::sleep(TICK);
            pass(cdp_port, idle);
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

    fn used(entries: &[(&str, u64, u32)], now: Instant) -> HashMap<Arc<str>, Use> {
        entries
            .iter()
            .map(|(id, ago, busy)| {
                (
                    Arc::from(*id),
                    Use {
                        at: now - Duration::from_secs(*ago),
                        busy: *busy,
                    },
                )
            })
            .collect()
    }

    const IDLE: Duration = Duration::from_secs(5);
    fn nobody(_: &str) -> bool {
        false
    }

    #[test]
    fn the_wait_is_three_seconds_unless_told_otherwise() {
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
    fn an_idle_window_is_parked_behind_its_blank_tab() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [page("A", "ctx", false), page("B", "ctx", true)];
        let plans = plan(&used(&[("A", 6, 0)], now), &pages, now, IDLE, &nobody);
        assert_eq!(
            plans,
            vec![Plan {
                tabs: vec![Arc::from("A")],
                park: Some(Park {
                    context: "ctx".into(),
                    blank: Some("B".into()),
                }),
            }]
        );
    }

    #[test]
    fn a_window_without_a_blank_tab_gets_one_made() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [page("A", "ctx", false)];
        let plans = plan(&used(&[("A", 9, 0)], now), &pages, now, IDLE, &nobody);
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].park,
            Some(Park {
                context: "ctx".into(),
                blank: None,
            })
        );
    }

    #[test]
    fn a_window_in_use_is_left_alone() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [
            page("A", "ctx", false),
            page("B", "ctx", false),
            page("Z", "ctx", true),
        ];
        // B was used a second ago: another agent is working in this window,
        // and hiding A's window would hide B's tab under it.
        let fresh = used(&[("A", 30, 0), ("B", 1, 0)], now);
        assert!(plan(&fresh, &pages, now, IDLE, &nobody).is_empty());
        // A command still running counts, however long ago it started.
        let running = used(&[("A", 30, 1)], now);
        assert!(plan(&running, &pages, now, IDLE, &nobody).is_empty());
    }

    #[test]
    fn windows_are_judged_one_by_one() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [
            page("A", "one", false),
            page("A0", "one", true),
            page("B", "two", false),
        ];
        let plans = plan(
            &used(&[("A", 20, 0), ("B", 1, 0)], now),
            &pages,
            now,
            IDLE,
            &nobody,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].tabs, vec![Arc::<str>::from("A")]);
        assert_eq!(plans[0].park.as_ref().unwrap().context, "one");
    }

    #[test]
    fn a_tab_a_person_is_watching_keeps_its_window_in_front() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [page("A", "ctx", false), page("B", "ctx", true)];
        let watched = |id: &str| id == "A";
        assert!(plan(&used(&[("A", 30, 0)], now), &pages, now, IDLE, &watched).is_empty());
    }

    #[test]
    fn a_blank_tab_and_a_closed_tab_are_forgotten_without_a_park() {
        let now = Instant::now() + Duration::from_secs(60);
        let pages = [page("B", "ctx", true)];
        let mut plans = plan(
            &used(&[("B", 30, 0), ("gone", 30, 0)], now),
            &pages,
            now,
            IDLE,
            &nobody,
        );
        plans.sort_by_key(|p| p.tabs[0].to_string());
        assert_eq!(
            plans,
            vec![
                Plan {
                    tabs: vec![Arc::from("B")],
                    park: None,
                },
                Plan {
                    tabs: vec![Arc::from("gone")],
                    park: None,
                },
            ]
        );
    }

    #[test]
    fn a_window_whose_parking_tab_was_closed_is_parked_again() {
        let now = Instant::now() + Duration::from_secs(60);
        let parked: HashMap<String, String> = [
            ("closed".to_string(), "B1".to_string()),
            ("fine".to_string(), "B2".to_string()),
            ("busy".to_string(), "B3".to_string()),
            ("empty".to_string(), "B4".to_string()),
        ]
        .into();
        let pages = [
            page("A", "closed", false),
            page("C", "fine", false),
            page("B2", "fine", true),
            page("D", "busy", false),
        ];
        let mut again = reopened(&parked, &used(&[("D", 1, 0)], now), &pages, &nobody);
        again.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            again,
            vec![
                // In use: the ordinary rule parks it when it rests.
                ("busy".to_string(), None),
                // Its blank tab is gone and nobody is using it: a new one.
                (
                    "closed".to_string(),
                    Some(Park {
                        context: "closed".into(),
                        blank: None,
                    })
                ),
                // The whole window is gone.
                ("empty".to_string(), None),
            ]
        );
    }

    #[test]
    fn a_running_command_is_counted_until_it_ends() {
        // A port no other test uses: the registry is process-wide.
        let port = 60_123;
        let target: Arc<str> = Arc::from("T");
        let busy = |port| browser(port).lock().unwrap().used.get("T").map(|u| u.busy);
        // No parker runs in a test process, so the door the commands use
        // stamps nothing.
        assert!(begin(port, &target).is_none());
        assert_eq!(busy(port), None);
        let running = stamp(port, &target);
        assert_eq!(busy(port), Some(1));
        drop(stamp(port, &target));
        assert_eq!(busy(port), Some(1));
        drop(running);
        assert_eq!(busy(port), Some(0));
    }
}
