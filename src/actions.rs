//! Acting on the page by snapshot ref, one action or a batch per request.
//!
//! An agent reads a snapshot, picks `ref=e7`, and acts on it. Two things make
//! that fast:
//!
//! - **Refs are resolved in the page.** The snapshot leaves a ref -> element
//!   registry in the document, so a click is one evaluate to find the element's
//!   centre and two input events -- no selector engine, no DOM query from the
//!   daemon, and the ref keeps pointing at the same element across snapshots.
//! - **A batch runs on one DevTools connection.** `POST /v1/act` takes a list
//!   of actions and, by default, ends with a snapshot, so "fill the form and
//!   submit it" is one HTTP round trip and one tool call instead of ten.
//!
//! Navigation caused by an action is detected from the `Page` events Chromium
//! sends before it answers the input event, so a click that does not navigate
//! costs nothing extra and one that does waits for `DOMContentLoaded` and no
//! longer.
use std::time::{Duration, Instant};

use crate::server::{
    self, CdpConnection, Tab, checkin, checkout, evaluated_string, json_array, json_objects,
    json_string, json_string_value, json_value, open_connection,
};

/// Longest a single `wait` action may sleep.
const MAX_WAIT: Duration = Duration::from_secs(10);
/// Most actions accepted in one batch.
pub const MAX_ACTIONS: usize = 50;

/// One step of a batch, as parsed from the request.
#[derive(Debug, PartialEq)]
pub enum Action {
    Click {
        reference: String,
    },
    Type {
        reference: String,
        text: String,
        clear: bool,
        submit: bool,
    },
    Press {
        key: String,
    },
    Select {
        reference: String,
        value: String,
    },
    Navigate {
        url: String,
    },
    Wait {
        ms: u64,
    },
}

/// A parsed `POST /v1/act` body.
#[derive(Debug, PartialEq)]
pub struct Batch {
    pub actions: Vec<Action>,
    /// End with a fresh snapshot (default): the read an agent does next anyway.
    pub snapshot: bool,
}

/// Parse `{"actions":[...],"snapshot":bool}`.
pub fn parse_batch(body: &str) -> Result<Batch, String> {
    let list = json_array(body, "actions").ok_or("\"actions\" must be an array")?;
    let actions = json_objects(&list)
        .iter()
        .map(|o| parse_action(o))
        .collect::<Result<Vec<_>, _>>()?;
    if actions.is_empty() {
        return Err("\"actions\" is empty".into());
    }
    if actions.len() > MAX_ACTIONS {
        return Err(format!("at most {MAX_ACTIONS} actions per batch"));
    }
    Ok(Batch {
        actions,
        snapshot: flag(body, "snapshot").unwrap_or(true),
    })
}

/// Parse one action object: `{"do":"click","ref":"e3"}` and friends.
pub fn parse_action(object: &str) -> Result<Action, String> {
    let kind = json_string_value(object, "do").ok_or("every action needs \"do\"")?;
    let reference = || -> Result<String, String> {
        let r = json_string_value(object, "ref").ok_or(format!("{kind} needs \"ref\""))?;
        valid_ref(&r)
            .then_some(r.clone())
            .ok_or(format!("\"{r}\" is not a snapshot ref (e.g. e3)"))
    };
    let text = |key: &str| json_string_value(object, key).ok_or(format!("{kind} needs \"{key}\""));
    Ok(match kind.as_str() {
        "click" => Action::Click {
            reference: reference()?,
        },
        "type" | "fill" => Action::Type {
            reference: reference()?,
            text: text("text")?,
            clear: flag(object, "clear").unwrap_or(true),
            submit: flag(object, "submit").unwrap_or(false),
        },
        "press" => {
            let key = text("key")?;
            key_event(&key).ok_or(format!("unsupported key \"{key}\""))?;
            Action::Press { key }
        }
        "select" => Action::Select {
            reference: reference()?,
            value: text("value")?,
        },
        "navigate" => Action::Navigate { url: text("url")? },
        "wait" => Action::Wait {
            ms: json_value(object, "ms")
                .and_then(|v| v.parse().ok())
                .ok_or("wait needs \"ms\"")?,
        },
        other => return Err(format!("unknown action \"{other}\"")),
    })
}

/// A boolean field of a JSON object.
pub fn flag(object: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let at = object.find(&needle)? + needle.len();
    let rest = object[at..].trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Refs are what the snapshot hands out: `e` and a number. Anything else is
/// refused before it gets near the page.
pub fn valid_ref(r: &str) -> bool {
    r.len() > 1 && r.starts_with('e') && r[1..].bytes().all(|b| b.is_ascii_digit())
}

/// `key`, `code`, Windows virtual key code and the text a key inserts.
pub fn key_event(key: &str) -> Option<(&'static str, &'static str, u32, &'static str)> {
    Some(match key {
        "Enter" => ("Enter", "Enter", 13, "\r"),
        "Tab" => ("Tab", "Tab", 9, ""),
        "Escape" => ("Escape", "Escape", 27, ""),
        "Backspace" => ("Backspace", "Backspace", 8, ""),
        "Delete" => ("Delete", "Delete", 46, ""),
        "Space" | " " => (" ", "Space", 32, " "),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, ""),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, ""),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, ""),
        "Home" => ("Home", "Home", 36, ""),
        "End" => ("End", "End", 35, ""),
        "PageUp" => ("PageUp", "PageUp", 33, ""),
        "PageDown" => ("PageDown", "PageDown", 34, ""),
        _ => return None,
    })
}

/// What one action did, as reported back to the agent.
pub struct Outcome {
    pub navigated: bool,
    /// Set when the action started a download instead of changing the page:
    /// the suggested file name the browser reported.
    pub download: Option<String>,
    /// Set when the action opened a new window or tab (`Page.windowOpen`),
    /// so the agent knows to look at `GET /v1/tabs`.
    pub popup: bool,
    pub ms: u128,
}

/// Run a batch. Stops at the first action that fails and says which.
///
/// Returns the JSON response body.
pub fn run_batch(tab: &Tab, batch: &Batch) -> Result<String, String> {
    let started = Instant::now();
    let mut page = Page::open(tab)?;
    let mut results = Vec::new();
    let mut failure = None;
    for (i, action) in batch.actions.iter().enumerate() {
        let t = Instant::now();
        match page.run(action) {
            Ok(outcome) => {
                let mut line = format!(
                    "{{\"ok\":true,\"navigated\":{},\"ms\":{}",
                    outcome.navigated,
                    t.elapsed().as_millis().max(outcome.ms)
                );
                // A download or a popup changes what the agent should do
                // next, so both are named rather than left to be inferred.
                if let Some(name) = &outcome.download {
                    line.push_str(&format!(",\"download\":{}", json_string(name)));
                }
                if outcome.popup {
                    line.push_str(",\"popup\":true");
                }
                results.push(line + "}");
            }
            Err(error) => {
                results.push(format!(
                    "{{\"ok\":false,\"error\":{}}}",
                    json_string(&error)
                ));
                failure = Some((i, error));
                break;
            }
        }
    }
    let snapshot = if batch.snapshot {
        match page.snapshot() {
            Ok(text) => format!(",\"snapshot\":{}", json_string(&text)),
            Err(e) => format!(",\"snapshot_error\":{}", json_string(&e)),
        }
    } else {
        String::new()
    };
    page.close();
    let error = failure
        .map(|(i, e)| format!(",\"failed\":{i},\"error\":{}", json_string(&e)))
        .unwrap_or_default();
    Ok(format!(
        "{{\"ok\":{},\"results\":[{}]{error},\"ms\":{}{snapshot}}}",
        error.is_empty(),
        results.join(","),
        started.elapsed().as_millis()
    ))
}

/// The page a batch acts on: one DevTools connection with `Page` events on.
struct Page {
    tab: Tab,
    connection: Option<CdpConnection>,
    /// Main frame id: navigation events for sub-frames are not ours.
    frame: String,
    /// Every frame of the page, main document first, so an action on an
    /// iframe ref knows where that frame sits.
    frames: Vec<server::Frame>,
    /// Whether `DOM.enable` was sent: resolving a frame's owner element
    /// needs it, and paying for it on every batch would tax the common case.
    dom_enabled: bool,
    /// Set when a command left the connection mid-frame; it is then dropped
    /// instead of pooled.
    poisoned: bool,
}

impl Page {
    fn open(tab: &Tab) -> Result<Self, String> {
        // A pooled connection may belong to a browser that has gone away; the
        // first command finds out, and one fresh connection is tried.
        let mut connection = checkout(tab)?;
        if connection.call("Page.enable", "{}").is_err() {
            connection = open_connection(tab)?;
            connection.call("Page.enable", "{}")?;
        }
        let tree = connection.call("Page.getFrameTree", "{}")?;
        let frames = server::flatten_frame_tree(&tree);
        let frame = frames
            .first()
            .map(|f| f.id.clone())
            .ok_or("page has no main frame")?;
        Ok(Self {
            tab: tab.clone(),
            connection: Some(connection),
            frame,
            frames,
            dom_enabled: false,
            poisoned: false,
        })
    }

    fn conn(&mut self) -> &mut CdpConnection {
        self.connection.as_mut().expect("page is open")
    }

    /// Stop `Page` events and give the connection back, so it does not fill
    /// up with events nobody reads.
    fn close(mut self) {
        if let Some(mut connection) = self.connection.take()
            && !self.poisoned
            && connection.call("Page.disable", "{}").is_ok()
        {
            checkin(&self.tab, connection);
        }
    }

    fn run(&mut self, action: &Action) -> Result<Outcome, String> {
        let started = Instant::now();
        let (mut download, mut popup) = (None, false);
        let navigated = match action {
            Action::Click { reference } => {
                let (x, y) = self.locate(reference, true)?;
                let mut watch = NavWatch::new(&self.watch_frame(reference));
                for kind in ["mousePressed", "mouseReleased"] {
                    self.input(
                        "Input.dispatchMouseEvent",
                        &format!(
                            "{{\"type\":\"{kind}\",\"x\":{x},\"y\":{y},\"button\":\"left\",\"clickCount\":1}}"
                        ),
                        &mut watch,
                    )?;
                }
                let navigated = self.settle(&mut watch)?;
                download = watch.download.take();
                popup = watch.popup;
                navigated
            }
            Action::Type {
                reference,
                text,
                clear,
                submit,
            } => {
                self.focus(reference, *clear)?;
                let mut watch = NavWatch::new(&self.watch_frame(reference));
                self.input(
                    "Input.insertText",
                    &format!("{{\"text\":{}}}", json_string(text)),
                    &mut watch,
                )?;
                if *submit {
                    self.key("Enter", &mut watch)?;
                }
                let navigated = self.settle(&mut watch)?;
                download = watch.download.take();
                popup = watch.popup;
                navigated
            }
            Action::Press { key } => {
                let mut watch = NavWatch::new(&self.frame);
                self.key(key, &mut watch)?;
                let navigated = self.settle(&mut watch)?;
                download = watch.download.take();
                popup = watch.popup;
                navigated
            }
            Action::Select { reference, value } => {
                self.registry_for(
                    reference,
                    &format!("select({},{})", json_string(reference), json_string(value)),
                )?;
                false
            }
            Action::Navigate { url } => {
                let mut watch = NavWatch::new(&self.frame);
                watch.started = true;
                let reply = self.input(
                    "Page.navigate",
                    &format!("{{\"url\":{}}}", json_string(url)),
                    &mut watch,
                )?;
                if let Some(error) = json_string_value(&reply, "errorText") {
                    return Err(format!("navigation failed: {error}"));
                }
                self.settle(&mut watch)?;
                true
            }
            Action::Wait { ms } => {
                std::thread::sleep(Duration::from_millis(*ms).min(MAX_WAIT));
                false
            }
        };
        Ok(Outcome {
            navigated,
            download,
            popup,
            ms: started.elapsed().as_millis(),
        })
    }

    /// Call a registry helper in the frame that owns `reference`, or explain
    /// that the page has no refs yet.
    fn registry_for(&mut self, reference: &str, call: &str) -> Result<String, String> {
        let context = self.enter(reference)?;
        self.registry_in(context, call)
    }

    fn registry_in(&mut self, context: Option<i64>, call: &str) -> Result<String, String> {
        self.eval_in(
            context,
            &format!(
                "{REGISTRY}?{REGISTRY}.{call}:JSON.stringify({{error:'this page has no refs yet; take a snapshot first'}})"
            ),
        )
    }

    /// The isolated world of the frame that owns `reference`, or `None` for
    /// the main document (its default world is where the registry of a
    /// single-frame page lives). Isolated worlds are created with the fixed
    /// name `cu`, so the walk's registry is the one found here later.
    fn enter(&mut self, reference: &str) -> Result<Option<i64>, String> {
        match self.subframe_of(reference) {
            None => Ok(None),
            Some(frame) => self.world(&frame).map(Some),
        }
    }

    /// The iframe `reference` lives in, if it is not the main document.
    fn subframe_of(&mut self, reference: &str) -> Option<String> {
        server::frame_for_ref(&self.tab, reference).filter(|f| *f != self.frame)
    }

    fn world(&mut self, frame_id: &str) -> Result<i64, String> {
        let reply = self.conn().call(
            "Page.createIsolatedWorld",
            &format!(
                "{{\"frameId\":{},\"worldName\":\"cu\"}}",
                json_string(frame_id)
            ),
        )?;
        json_value(&reply, "executionContextId")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| "the frame is gone; take a new snapshot".to_string())
    }

    /// Run a script in the page and return the string it produced. A script
    /// that reports `{"error":...}` becomes an `Err` with that message.
    fn eval(&mut self, expression: &str) -> Result<String, String> {
        self.eval_in(None, expression)
    }

    /// [`eval`](Self::eval) in an execution context: `Some(context)` for an
    /// iframe's isolated world, `None` for the page's default world.
    fn eval_in(&mut self, context: Option<i64>, expression: &str) -> Result<String, String> {
        // Trailing comma on the fragment: the format below separates the
        // expression and this fragment with one comma and has none after it.
        let context = context
            .map(|id| format!("\"contextId\":{id},"))
            .unwrap_or_default();
        let reply = self.conn().call(
            "Runtime.evaluate",
            &format!(
                "{{\"expression\":{},{}\"returnByValue\":true}}",
                json_string(expression),
                context
            ),
        )?;
        if reply.contains("\"exceptionDetails\"") {
            return Err(json_string_value(&reply, "description")
                .unwrap_or_else(|| "script failed in the page".into()));
        }
        let value = evaluated_string(&reply).unwrap_or_default();
        match json_string_value(&value, "error") {
            Some(error) => Err(error),
            None => Ok(value),
        }
    }

    /// Centre of the element behind `reference`, scrolled into view, in the
    /// coordinates the mouse events are dispatched in.
    fn locate(&mut self, reference: &str, hit_test: bool) -> Result<(f64, f64), String> {
        let subframe = self.subframe_of(reference);
        let context = match &subframe {
            None => None,
            Some(frame) => Some(self.world(frame)?),
        };
        let value = self.registry_in(
            context,
            &format!("locate({},{hit_test})", json_string(reference)),
        )?;
        let x = json_value(&value, "x").and_then(|v| v.parse().ok());
        let y = json_value(&value, "y").and_then(|v| v.parse().ok());
        let (mut x, mut y) = x
            .zip(y)
            .ok_or_else(|| format!("could not locate {reference}"))?;
        if let Some(frame) = subframe {
            // The frame-local point is in the iframe's own viewport; shift it
            // into the main document's, scrolling every iframe element into
            // view on the way up.
            let (dx, dy) = self.frame_offset(&frame)?;
            x += dx;
            y += dy;
            // The top document decides what is really at that point: an
            // overlay above the iframe would swallow the click.
            let at = self.eval(&format!(
                "(function(){{var e=document.elementFromPoint({x},{y});return e?e.tagName:''}})()"
            ))?;
            match at.as_str() {
                "IFRAME" | "FRAME" => {}
                "" => return Err(format!("{reference} is not visible in the top document")),
                other => {
                    return Err(format!(
                        "{reference} is covered by {}",
                        other.to_lowercase()
                    ));
                }
            }
        }
        Ok((x, y))
    }

    /// Main-document coordinates of `frame`'s top-left corner: the sum of the
    /// owning iframe elements' rects from the top down, each scrolled into
    /// view before its rect is read, so a scroll can never invalidate a rect
    /// already taken.
    fn frame_offset(&mut self, frame_id: &str) -> Result<(f64, f64), String> {
        let mut chain = vec![frame_id.to_string()];
        let mut current = frame_id.to_string();
        loop {
            let parent = self
                .frames
                .iter()
                .find(|f| f.id == current)
                .and_then(|f| f.parent.clone())
                .ok_or_else(|| {
                    "that frame is not part of this page any more; take a new snapshot".to_string()
                })?;
            if parent == self.frame {
                break;
            }
            chain.push(parent.clone());
            current = parent;
        }
        if !self.dom_enabled {
            self.conn().call("DOM.enable", "{}")?;
            self.dom_enabled = true;
        }
        let mut offset = (0.0f64, 0.0f64);
        for child in chain.iter().rev() {
            let owner = self.conn().call(
                "DOM.getFrameOwner",
                &format!("{{\"frameId\":{}}}", json_string(child)),
            )?;
            let backend = json_value(&owner, "backendNodeId")
                .ok_or_else(|| "iframe element not found; take a new snapshot".to_string())?;
            let resolved = self.conn().call(
                "DOM.resolveNode",
                &format!("{{\"backendNodeId\":{backend}}}"),
            )?;
            let object = json_string_value(&resolved, "objectId")
                .ok_or_else(|| "iframe element is gone; take a new snapshot".to_string())?;
            let rect = self.conn().call(
                "Runtime.callFunctionOn",
                &format!(
                    "{{\"objectId\":{},\"functionDeclaration\":{},\"returnByValue\":true}}",
                    json_string(&object),
                    json_string(SCROLL_AND_RECT)
                ),
            )?;
            let value = evaluated_string(&rect)
                .ok_or_else(|| "iframe position unknown; take a new snapshot".to_string())?;
            let dx: f64 = json_value(&value, "x")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| "could not locate the iframe".to_string())?;
            let dy: f64 = json_value(&value, "y")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| "could not locate the iframe".to_string())?;
            offset.0 += dx;
            offset.1 += dy;
        }
        Ok(offset)
    }

    /// The frame a ref-based action's navigation watch should follow: the
    /// frame the ref lives in, because a form inside an iframe navigates the
    /// iframe, not the page.
    fn watch_frame(&self, reference: &str) -> String {
        server::frame_for_ref(&self.tab, reference).unwrap_or_else(|| self.frame.clone())
    }

    fn focus(&mut self, reference: &str, clear: bool) -> Result<(), String> {
        self.registry_for(
            reference,
            &format!("focus({},{clear})", json_string(reference)),
        )
        .map(|_| ())
    }

    fn key(&mut self, key: &str, watch: &mut NavWatch) -> Result<(), String> {
        let (key, code, vk, text) = key_event(key).ok_or(format!("unsupported key \"{key}\""))?;
        let text = if text.is_empty() {
            String::new()
        } else {
            format!(",\"text\":{}", json_string(text))
        };
        let common = format!(
            "\"key\":{},\"code\":\"{code}\",\"windowsVirtualKeyCode\":{vk}",
            json_string(key)
        );
        self.input(
            "Input.dispatchKeyEvent",
            &format!("{{\"type\":\"keyDown\",{common}{text}}}"),
            watch,
        )?;
        self.input(
            "Input.dispatchKeyEvent",
            &format!("{{\"type\":\"keyUp\",{common}}}"),
            watch,
        )
        .map(|_| ())
    }

    /// Send an input command, feeding the events that precede its reply to
    /// the navigation watch.
    fn input(
        &mut self,
        method: &str,
        params: &str,
        watch: &mut NavWatch,
    ) -> Result<String, String> {
        let reply = self
            .conn()
            .call_observed(method, params, &mut |event| watch.see(event))?;
        if let Some(message) = json_value(&reply, "message").filter(|_| reply.contains("\"error\""))
        {
            return Err(format!("{method}: {message}"));
        }
        Ok(reply)
    }

    /// If the action started a navigation, wait until the new page is usable.
    /// Returns whether the page really navigated.
    ///
    /// A link click usually announces its navigation before Chromium answers
    /// the input event; a form submit may announce it just after, so one
    /// barrier round trip (~0.3 ms) is made first. A handler that navigates
    /// from a timer is not waited for; the next snapshot shows where it went.
    ///
    /// "Navigated" means the main frame *committed* a navigation
    /// (`Page.frameNavigated`) or moved within the document (a hash change).
    /// A download and an aborted navigation both schedule and start loading
    /// and then stop without committing, so the page the agent is looking at
    /// has not changed and `navigated` must say false -- for a download the
    /// file name is reported instead.
    fn settle(&mut self, watch: &mut NavWatch) -> Result<bool, String> {
        if !watch.started {
            let _ = self
                .conn()
                .call_observed("Runtime.evaluate", BARRIER, &mut |event| watch.see(event));
        }
        if !watch.started {
            return Ok(false);
        }
        if !watch.done {
            let deadline = Instant::now() + server::SETTLE_MAX;
            let clean = self.conn().pump_events(deadline, &mut |event| {
                watch.see(event);
                watch.done
            })?;
            if !clean {
                self.poisoned = true;
                // The stream may hold half a frame; carry on on a fresh one.
                let mut fresh = open_connection(&self.tab)?;
                fresh.call("Page.enable", "{}")?;
                self.connection = Some(fresh);
            }
        }
        if !watch.committed {
            // Chromium fires `Page.downloadWillBegin` just after the frame
            // stops for a download, and a window may already be opening; one
            // more observed round trip picks those up before deciding.
            let _ = self
                .conn()
                .call_observed("Runtime.evaluate", BARRIER, &mut |event| watch.see(event));
        }
        Ok(watch.committed || watch.within)
    }

    /// The closing snapshot of a batch: the frame tree the batch opened
    /// with, walked so iframe contents and their refs are included.
    fn snapshot(&mut self) -> Result<String, String> {
        let tab = self.tab.clone();
        let frames = std::mem::take(&mut self.frames);
        let walked = server::walk_frames(&tab, frames.clone(), |method, params| {
            self.conn().call(method, params)
        });
        self.frames = frames;
        let (text, pairs) = walked?;
        server::note_ref_frames(&tab, &pairs);
        Ok(text)
    }
}

/// Follows the `Page` events of one action to tell whether it navigated and
/// when the new document is usable.
struct NavWatch {
    frame: String,
    started: bool,
    done: bool,
    /// The frame committed a real navigation (`Page.frameNavigated`).
    committed: bool,
    /// The frame moved within the same document (hash change).
    within: bool,
    /// A download began in this frame; the suggested file name.
    download: Option<String>,
    /// A new window or tab was opened from this page.
    popup: bool,
}

impl NavWatch {
    fn new(frame: &str) -> Self {
        Self {
            frame: frame.to_string(),
            started: false,
            done: false,
            committed: false,
            within: false,
            download: None,
            popup: false,
        }
    }

    fn see(&mut self, event: &str) {
        let Some(method) = json_string_value(event, "method") else {
            return;
        };
        let main = json_string_value(event, "frameId").is_none_or(|f| f == self.frame);
        match method.as_str() {
            // Opening a new tab is not a navigation of this page.
            "Page.frameRequestedNavigation" if main => {
                if json_string_value(event, "disposition").as_deref() != Some("newTab")
                    && json_string_value(event, "disposition").as_deref() != Some("newWindow")
                {
                    self.started = true;
                }
            }
            "Page.frameStartedLoading" | "Page.frameScheduledNavigation" if main => {
                self.started = true
            }
            "Page.navigatedWithinDocument" if main => {
                self.started = true;
                self.done = true;
                self.within = true;
            }
            // The commit is what makes a navigation real: a download or a
            // stopped load fires the events above and never this one.
            // `Page.frameNavigated` carries the frame id as `params.frame.id`,
            // which is the first `id` in the message.
            "Page.frameNavigated" => {
                if json_string_value(event, "id").as_deref() == Some(self.frame.as_str()) {
                    self.committed = true;
                }
            }
            "Page.downloadWillBegin" if main => {
                self.download = json_string_value(event, "suggestedFilename");
            }
            "Page.windowOpen" if main => self.popup = true,
            "Page.domContentEventFired" if self.started => self.done = true,
            "Page.frameStoppedLoading" if main && self.started => self.done = true,
            _ => {}
        }
    }
}

/// Scroll an iframe's owner element into view and report its top-left
/// corner, run on the element object itself so it works in any world.
const SCROLL_AND_RECT: &str = "function(){this.scrollIntoView({block:'center',inline:'center',behavior:'instant'});const r=this.getBoundingClientRect();return JSON.stringify({x:r.left,y:r.top});}";

/// A no-op evaluate, used as a barrier. Its reply comes after every event the
/// renderer emitted while handling the input before it, including the
/// navigation request of a form submit. Awaiting a task (`setTimeout(0)` or a
/// `MessageChannel` post) instead cost 12-15 ms per action: after input,
/// headless Chromium runs the next task only on its next frame.
const BARRIER: &str = "{\"expression\":\"0\"}";

/// The in-page side of refs, installed by the snapshot.
const REGISTRY: &str = "window.__cu";

/// JavaScript that defines `window.__cu`: the ref registry and the helpers the
/// actions call. Prepended to the snapshot walk, so a page that has been
/// snapshotted can be acted on without another install round trip.
pub const REGISTRY_JS: &str = r#"
if (!window.__cu) {
  const byRef = new Map(), ids = new WeakMap();
  let next = window.__cuBase || 0;
  const err = (m) => JSON.stringify({ error: m });
  const get = (ref) => {
    const el = byRef.get(ref)?.deref();
    return el && el.isConnected ? el : null;
  };
  Object.defineProperty(window, '__cu', { enumerable: false, value: {
    next() { return next; },
    ref(el) {
      let r = ids.get(el);
      if (!r) { r = 'e' + (++next); ids.set(el, r); byRef.set(r, new WeakRef(el)); }
      return r;
    },
    locate(ref, hit) {
      const el = get(ref);
      if (!el) return err(ref + ' is not on the page any more; take a new snapshot');
      let b = el.getBoundingClientRect();
      if (b.width < 1 || b.height < 1) return err(ref + ' is not visible');
      if (b.top < 0 || b.left < 0 || b.bottom > innerHeight || b.right > innerWidth) {
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        b = el.getBoundingClientRect();
      }
      const x = b.left + b.width / 2, y = b.top + b.height / 2;
      if (hit) {
        // `document.elementFromPoint` stops at a shadow host: at the centre of
        // a control inside a web component it reports the host, which reads as
        // "covered". Descend into the roots it exposes and compare against the
        // element that really is painted there.
        let at = document.elementFromPoint(x, y);
        while (at && at.shadowRoot) {
          const inner = at.shadowRoot.elementFromPoint(x, y);
          if (!inner || inner === at) break;
          at = inner;
        }
        if (at && at !== el && !el.contains(at) && !(at.control === el) && !at.contains(el)) {
          const what = at.tagName.toLowerCase() + (at.id ? '#' + at.id : '');
          return err(ref + ' is covered by ' + what);
        }
      }
      return JSON.stringify({ x, y });
    },
    focus(ref, clear) {
      const el = get(ref);
      if (!el) return err(ref + ' is not on the page any more; take a new snapshot');
      el.scrollIntoView({ block: 'center', behavior: 'instant' });
      el.focus();
      if (document.activeElement !== el && !el.contains(document.activeElement))
        return err(ref + ' cannot take text');
      if (clear) {
        if ('value' in el) { el.value = ''; }
        else if (el.isContentEditable) { document.getSelection().selectAllChildren(el); }
      }
      return '{}';
    },
    select(ref, value) {
      const el = get(ref);
      if (!el) return err(ref + ' is not on the page any more; take a new snapshot');
      if (el.tagName !== 'SELECT') return err(ref + ' is not a select');
      const o = [...el.options].find((o) => o.value === value || o.label.trim() === value);
      if (!o) return err('no option "' + value + '" in ' + ref);
      el.value = o.value;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return '{}';
    },
  }});
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_parsed_in_order_with_defaults() {
        let batch = parse_batch(
            r#"{"actions":[{"do":"type","ref":"e2","text":"a \"q\""},{"do":"click","ref":"e10"},{"do":"press","key":"Enter"},{"do":"wait","ms":5}]}"#,
        )
        .expect("parses");
        assert!(batch.snapshot);
        assert_eq!(
            batch.actions,
            vec![
                Action::Type {
                    reference: "e2".into(),
                    text: "a \"q\"".into(),
                    clear: true,
                    submit: false
                },
                Action::Click {
                    reference: "e10".into()
                },
                Action::Press {
                    key: "Enter".into()
                },
                Action::Wait { ms: 5 },
            ]
        );
    }

    #[test]
    fn the_closing_snapshot_can_be_turned_off() {
        let batch = parse_batch(r#"{"actions":[{"do":"click","ref":"e1"}],"snapshot":false}"#)
            .expect("parses");
        assert!(!batch.snapshot);
    }

    #[test]
    fn a_ref_that_is_not_a_snapshot_ref_is_refused() {
        for bad in ["", "e", "x1", "e1')", "e1;alert(1)"] {
            assert!(!valid_ref(bad), "{bad:?} accepted");
            let body = format!(
                r#"{{"actions":[{{"do":"click","ref":{}}}]}}"#,
                json_string(bad)
            );
            assert!(parse_batch(&body).is_err(), "{bad:?} parsed");
        }
        assert!(valid_ref("e42"));
    }

    #[test]
    fn malformed_batches_say_what_is_wrong() {
        assert!(parse_batch("{}").unwrap_err().contains("actions"));
        assert!(
            parse_batch(r#"{"actions":[]}"#)
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            parse_batch(r#"{"actions":[{"do":"fly"}]}"#)
                .unwrap_err()
                .contains("fly")
        );
        assert!(
            parse_batch(r#"{"actions":[{"do":"press","key":"Hyper"}]}"#)
                .unwrap_err()
                .contains("Hyper")
        );
        let many = vec![r#"{"do":"wait","ms":0}"#; MAX_ACTIONS + 1].join(",");
        assert!(parse_batch(&format!(r#"{{"actions":[{many}]}}"#)).is_err());
    }

    #[test]
    fn navigation_is_recognised_only_on_the_main_frame() {
        let mut watch = NavWatch::new("MAIN");
        watch.see(r#"{"method":"Page.frameStartedLoading","params":{"frameId":"SUB"}}"#);
        assert!(!watch.started);
        watch.see(r#"{"method":"Page.frameRequestedNavigation","params":{"frameId":"MAIN","disposition":"newTab"}}"#);
        assert!(!watch.started, "a new tab is not this page navigating");
        watch.see(r#"{"method":"Page.frameStartedLoading","params":{"frameId":"MAIN"}}"#);
        assert!(watch.started && !watch.done);
        watch.see(r#"{"method":"Page.domContentEventFired","params":{"timestamp":1}}"#);
        assert!(watch.done);
    }

    #[test]
    fn a_download_is_not_a_navigation_and_keeps_its_name() {
        let mut watch = NavWatch::new("MAIN");
        watch.see(r#"{"method":"Page.frameRequestedNavigation","params":{"frameId":"MAIN","disposition":"currentTab"}}"#);
        watch.see(r#"{"method":"Page.frameStartedLoading","params":{"frameId":"MAIN"}}"#);
        watch.see(r#"{"method":"Page.frameStoppedLoading","params":{"frameId":"MAIN"}}"#);
        watch.see(
            r#"{"method":"Page.downloadWillBegin","params":{"frameId":"MAIN","suggestedFilename":"report.pdf"}}"#,
        );
        assert!(watch.started && watch.done);
        // No commit happened: the page the agent sees did not change.
        assert!(!watch.committed);
        assert_eq!(watch.download.as_deref(), Some("report.pdf"));
        assert!(
            !watch.committed || watch.within,
            "must not report navigation"
        );
    }

    #[test]
    fn a_committing_navigation_is_recognised_by_its_frame_id() {
        let mut watch = NavWatch::new("MAIN");
        watch
            .see(r#"{"method":"Page.frameNavigated","params":{"frame":{"id":"OTHER","url":"x"}}}"#);
        assert!(!watch.committed, "another frame committing is not ours");
        watch.see(r#"{"method":"Page.frameNavigated","params":{"frame":{"id":"MAIN","url":"x"}}}"#);
        assert!(watch.committed);
    }

    #[test]
    fn opening_a_window_is_reported_as_a_popup() {
        let mut watch = NavWatch::new("MAIN");
        watch.see(
            r#"{"method":"Page.windowOpen","params":{"url":"https://x.test/","windowName":"_blank"}}"#,
        );
        assert!(watch.popup && !watch.started, "a popup is not a navigation");
    }

    #[test]
    fn a_hash_change_is_a_finished_navigation() {
        let mut watch = NavWatch::new("MAIN");
        watch.see(
            r#"{"method":"Page.navigatedWithinDocument","params":{"frameId":"MAIN","url":"x#a"}}"#,
        );
        assert!(watch.started && watch.done);
    }
}
