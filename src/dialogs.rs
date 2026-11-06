//! JavaScript dialogs: `alert`, `confirm`, `prompt` and the "Leave site?"
//! of a `beforeunload` handler, answered by policy the moment they open.
//!
//! An unanswered dialog stops the page's renderer: every later command on
//! the tab (a `Runtime.evaluate` of `document.title`, an `emulate`) waits
//! out its read timeout, and so does every other tab sharing that renderer
//! process (measured on Chromium 156 with one renderer: a fresh
//! `about:blank` tab opened while another tab's dialog was up was stopped
//! too). Instagram's story editor is the case this was found on: it arms a
//! `beforeunload` handler, so the `navigate` after discarding the editor
//! opened a dialog nobody answered.
//!
//! Each tab cu works in has a keeper session with `Page` enabled
//! ([`crate::keeper`]); its reader thread calls [`on_event`] and answers
//! with `Page.handleJavaScriptDialog` on that same session -- the only kind of
//! session Chromium lets answer. The default policy:
//!
//! | dialog       | answer  | why |
//! |--------------|---------|-----|
//! | beforeunload | accept  | the agent asked to leave the page |
//! | alert        | accept  | there is nothing else to do with it |
//! | confirm      | dismiss | "no" to whatever the page asks to do |
//! | prompt       | dismiss | no text is made up |
//!
//! `manual` leaves a dialog open: commands on its tab then fail at once with
//! an error naming it, until `cu dialog accept [TEXT]` or `cu dialog
//! dismiss` answers it. The daemon's policy comes from `CU_DIALOGS`
//! (`beforeunload=accept,confirm=manual`, `all=manual`); a tab's own from
//! `cu dialog policy SPEC`. Every dialog is logged per tab (bounded), shown
//! by `cu dialog`, `cu console` (level `dialog`) and in the reply of the
//! command it happened under.
//!
//! What cannot be done: a dialog that opened before cu watched the tab (a
//! tab cu had never touched, or one from before the daemon started) cannot
//! be answered by any DevTools session. cu notices the tab does not answer,
//! says so, and the way out is closing it (`Target.closeTarget` does not need
//! the page's cooperation).
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::keeper::Keeper;
use crate::server::{Tab, json_object, json_string, json_string_value};

/// Dialogs kept per tab.
pub const LOG_MAX: usize = 20;
/// Characters of a dialog's message kept and shown.
pub const MESSAGE_MAX: usize = 200;

/// The four kinds of JavaScript dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Alert, Kind::Confirm, Kind::Prompt, Kind::BeforeUnload];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Alert => "alert",
            Kind::Confirm => "confirm",
            Kind::Prompt => "prompt",
            Kind::BeforeUnload => "beforeunload",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// What to do when a dialog opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Dismiss,
    /// Leave it open for `cu dialog accept|dismiss`.
    Manual,
}

impl Answer {
    pub fn name(self) -> &'static str {
        match self {
            Answer::Accept => "accept",
            Answer::Dismiss => "dismiss",
            Answer::Manual => "manual",
        }
    }
}

/// The answer for each kind of dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub alert: Answer,
    pub confirm: Answer,
    pub prompt: Answer,
    pub beforeunload: Answer,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            alert: Answer::Accept,
            confirm: Answer::Dismiss,
            prompt: Answer::Dismiss,
            beforeunload: Answer::Accept,
        }
    }
}

impl Policy {
    pub fn get(&self, kind: Kind) -> Answer {
        match kind {
            Kind::Alert => self.alert,
            Kind::Confirm => self.confirm,
            Kind::Prompt => self.prompt,
            Kind::BeforeUnload => self.beforeunload,
        }
    }

    fn set(&mut self, kind: Kind, answer: Answer) {
        match kind {
            Kind::Alert => self.alert = answer,
            Kind::Confirm => self.confirm = answer,
            Kind::Prompt => self.prompt = answer,
            Kind::BeforeUnload => self.beforeunload = answer,
        }
    }

    /// This policy with `spec` applied on top: a comma list of
    /// `KIND=ANSWER`, KIND one of alert, confirm, prompt, beforeunload or
    /// `all` (`*`), ANSWER accept, dismiss or manual. Kinds not named keep
    /// their answer.
    pub fn with(mut self, spec: &str) -> Result<Self, String> {
        let mut any = false;
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (kind, answer) = part
                .split_once('=')
                .ok_or_else(|| format!("dialog policy {part:?}: write KIND=ANSWER"))?;
            let answer = match answer.trim().to_ascii_lowercase().as_str() {
                "accept" => Answer::Accept,
                "dismiss" => Answer::Dismiss,
                "manual" => Answer::Manual,
                other => {
                    return Err(format!(
                        "dialog policy: {other:?} is not accept, dismiss or manual"
                    ));
                }
            };
            match kind.trim().to_ascii_lowercase().as_str() {
                "all" | "*" => Kind::ALL.into_iter().for_each(|k| self.set(k, answer)),
                name => self.set(
                    Kind::parse(name).ok_or_else(|| {
                        format!(
                            "dialog policy: {name:?} is not alert, confirm, prompt, beforeunload or all"
                        )
                    })?,
                    answer,
                ),
            }
            any = true;
        }
        if !any {
            return Err("dialog policy: give KIND=ANSWER, e.g. confirm=manual".into());
        }
        Ok(self)
    }

    /// `alert=accept,confirm=dismiss,prompt=dismiss,beforeunload=accept`.
    pub fn spec(&self) -> String {
        Kind::ALL
            .iter()
            .map(|k| format!("{}={}", k.name(), self.get(*k).name()))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn json(&self) -> String {
        format!(
            "{{{}}}",
            Kind::ALL
                .iter()
                .map(|k| format!("\"{}\":\"{}\"", k.name(), self.get(*k).name()))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// The daemon's policy: the default with `CU_DIALOGS` on top. A spec that
/// does not parse is reported once and ignored.
pub fn daemon_policy() -> Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    *POLICY.get_or_init(|| match std::env::var("CU_DIALOGS") {
        Ok(spec) if !spec.trim().is_empty() => match Policy::default().with(&spec) {
            Ok(policy) => {
                eprintln!("cu: dialog policy {}", policy.spec());
                policy
            }
            Err(error) => {
                eprintln!("cu: CU_DIALOGS ignored: {error}");
                Policy::default()
            }
        },
        _ => Policy::default(),
    })
}

/// One dialog, as logged.
#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: u64,
    pub kind: Kind,
    pub message: String,
    pub url: String,
    /// `accepted`, `dismissed`, `pending`, `closed` or `failed: ...`.
    pub action: String,
    /// Who answered: `policy`, `agent` (cu dialog), `browser` (closed
    /// outside cu: a person, the page went away) or `-` while pending.
    pub by: &'static str,
    /// Unix time in milliseconds the dialog opened.
    pub at_ms: u128,
}

impl Entry {
    pub fn json(&self) -> String {
        format!(
            "{{\"seq\":{},\"type\":\"{}\",\"message\":{},\"url\":{},\"action\":{},\"by\":\"{}\",\"at\":{}}}",
            self.seq,
            self.kind.name(),
            json_string(&self.message),
            json_string(&self.url),
            json_string(&self.action),
            self.by,
            self.at_ms
        )
    }

    /// One console-style line: `#d3 12:01:02.123 DIALOG    beforeunload "..." -> accepted (policy)`.
    pub fn line(&self) -> String {
        let secs = (self.at_ms / 1000) as u64;
        let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
        format!(
            "#d{} {h:02}:{m:02}:{s:02}.{:03} UTC {:<10}{} {:?} at {} -> {} ({})",
            self.seq,
            self.at_ms % 1000,
            "DIALOG",
            self.kind.name(),
            self.message,
            self.url,
            self.action,
            self.by
        )
    }
}

/// A dialog left open for the agent.
#[derive(Clone, Debug)]
struct Pending {
    seq: u64,
    kind: Kind,
    message: String,
    default_prompt: String,
    since: Instant,
}

#[derive(Default)]
struct TabState {
    policy: Option<Policy>,
    pending: Option<Pending>,
    log: VecDeque<Entry>,
}

type Key = (u16, String);

fn states() -> &'static Mutex<HashMap<Key, TabState>> {
    static STATES: OnceLock<Mutex<HashMap<Key, TabState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn truncate(message: &str) -> String {
    let mut out: String = message.chars().take(MESSAGE_MAX).collect();
    if message.chars().count() > MESSAGE_MAX {
        out.push_str("...");
    }
    out
}

fn with_state<T>(key: &Key, f: impl FnOnce(&mut TabState) -> T) -> T {
    let mut states = states().lock().unwrap_or_else(|e| e.into_inner());
    f(states.entry(key.clone()).or_default())
}

fn update_entry(key: &Key, seq: u64, action: String, by: &'static str) {
    with_state(key, |state| {
        if let Some(entry) = state.log.iter_mut().find(|e| e.seq == seq) {
            entry.action = action;
            entry.by = by;
        }
    })
}

/// An event from a keeper's session.
pub fn on_event(keeper: &Keeper, event: &str) {
    let Some(method) = json_string_value(event, "method") else {
        return;
    };
    let key = (keeper.cdp_port, keeper.target.clone());
    match method.as_str() {
        "Page.javascriptDialogOpening" => opened(keeper, &key, event),
        "Page.javascriptDialogClosed" => {
            let params = json_object(event, "params").unwrap_or(event);
            let accepted = crate::server::json_value(params, "result").as_deref() == Some("true");
            let pending = with_state(&key, |state| state.pending.take());
            if let Some(pending) = pending {
                // Answered outside cu: a person in a headful window, or the
                // page went away under it.
                update_entry(
                    &key,
                    pending.seq,
                    (if accepted { "accepted" } else { "dismissed" }).into(),
                    "browser",
                );
                eprintln!(
                    "cu: dialog: {} on tab {} was closed outside cu",
                    pending.kind.name(),
                    key.1
                );
            }
        }
        _ => {}
    }
}

fn opened(keeper: &Keeper, key: &Key, event: &str) {
    let params = json_object(event, "params").unwrap_or(event);
    let kind = json_string_value(params, "type")
        .and_then(|t| Kind::parse(&t))
        .unwrap_or(Kind::Alert);
    let message = truncate(&json_string_value(params, "message").unwrap_or_default());
    let url = crate::challenge::redact(&json_string_value(params, "url").unwrap_or_default());
    let default_prompt = json_string_value(params, "defaultPrompt").unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let answer = with_state(key, |state| state.policy)
        .unwrap_or_else(daemon_policy)
        .get(kind);
    let mut entry = Entry {
        seq,
        kind,
        message: message.clone(),
        url,
        action: "pending".into(),
        by: "-",
        at_ms: now_ms(),
    };
    match answer {
        Answer::Accept | Answer::Dismiss => {
            let accept = answer == Answer::Accept;
            entry.action = (if accept { "accepted" } else { "dismissed" }).into();
            entry.by = "policy";
            push(key, entry);
            let params = handle_params(accept, kind, Some(&default_prompt));
            let note_key = key.clone();
            // The reply comes back on the reader thread that is calling us:
            // never wait for it here.
            let sent = keeper.send(
                "Page.handleJavaScriptDialog",
                &params,
                Some(Box::new(move |reply: &str| {
                    if reply.contains("\"error\":{") {
                        let why = json_string_value(reply, "message").unwrap_or_default();
                        update_entry(&note_key, seq, format!("failed: {why}"), "policy");
                    }
                })),
            );
            if let Err(error) = sent {
                update_entry(key, seq, format!("failed: {error}"), "policy");
            }
            eprintln!(
                "cu: dialog: {} on tab {} {} by policy: {:?}",
                kind.name(),
                key.1,
                if accept { "accepted" } else { "dismissed" },
                message
            );
        }
        Answer::Manual => {
            with_state(key, |state| {
                state.pending = Some(Pending {
                    seq,
                    kind,
                    message: message.clone(),
                    default_prompt,
                    since: Instant::now(),
                })
            });
            push(key, entry);
            eprintln!(
                "cu: dialog: {} on tab {} left open (manual policy): {:?}",
                kind.name(),
                key.1,
                message
            );
            // A command already waiting on the tab would wait out its read
            // timeout: cut its connection so it fails now, naming the dialog.
            crate::server::interrupt_target(key.0, &key.1);
        }
    }
}

fn push(key: &Key, entry: Entry) {
    with_state(key, |state| {
        state.log.push_back(entry);
        while state.log.len() > LOG_MAX {
            state.log.pop_front();
        }
    })
}

fn handle_params(accept: bool, kind: Kind, text: Option<&str>) -> String {
    match (accept, kind, text) {
        (true, Kind::Prompt, Some(text)) => {
            format!("{{\"accept\":true,\"promptText\":{}}}", json_string(text))
        }
        _ => format!("{{\"accept\":{accept}}}"),
    }
}

/// The keeper of a tab ended: a dialog left open there is gone with it.
pub fn keeper_gone(cdp_port: u16, target: &str) {
    let key = (cdp_port, target.to_string());
    let pending = with_state(&key, |state| state.pending.take());
    if let Some(pending) = pending {
        update_entry(&key, pending.seq, "closed".into(), "browser");
    }
}

/// The tab was closed: forget everything about it.
pub fn forget(cdp_port: u16, target: &str) {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cdp_port, target.to_string()));
}

fn key_of(tab: &Tab) -> Option<Key> {
    tab.resolved_target().map(|t| (tab.cdp_port, t))
}

/// Why commands on `tab` cannot run right now, if they cannot: a dialog
/// left open for the agent, or a tab that did not answer when cu started
/// watching it. Never blocks.
pub fn blocked(tab: &Tab) -> Option<String> {
    let key = key_of(tab)?;
    let pending = with_state(&key, |state| state.pending.clone());
    if let Some(p) = pending {
        return Some(format!(
            "a JavaScript {} dialog is open on tab {} ({:?}, open {:.1} s) and the dialog policy for it is manual: answer it with `cu dialog accept [TEXT]` or `cu dialog dismiss` (MCP: dialog), or close the tab",
            p.kind.name(),
            key.1,
            p.message,
            p.since.elapsed().as_secs_f64()
        ));
    }
    let keeper = crate::keeper::existing(key.0, &key.1)?;
    keeper.unanswered().then(|| {
        format!(
            "tab {} is not answering: it did not acknowledge cu's dialog watcher within {} s. Most likely a JavaScript dialog opened before cu watched this tab (no DevTools session can answer that one), or another tab's dialog holds the renderer it shares. Close it with `cu tabs close {}`.",
            key.1,
            crate::keeper::ENABLE_WAIT.as_secs(),
            key.1
        )
    })
}

/// Dialogs waiting for the agent on any tab of this browser, as one line
/// each: what to look at when a command times out on a page that shares a
/// renderer with a stopped one.
pub fn pending_anywhere(cdp_port: u16) -> Vec<String> {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((port, _), _)| *port == cdp_port)
        .filter_map(|((_, target), state)| {
            state.pending.as_ref().map(|p| {
                format!(
                    "tab {target}: {} dialog {:?} (manual policy)",
                    p.kind.name(),
                    p.message
                )
            })
        })
        .collect()
}

/// The latest dialog sequence number: what [`since_json`] counts from.
pub fn mark() -> u64 {
    SEQ.load(Ordering::SeqCst)
}

/// The dialogs `tab` logged after `mark`, as a JSON array, or `None`.
pub fn since_json(tab: &Tab, mark: u64) -> Option<String> {
    let key = key_of(tab)?;
    let entries: Vec<String> = with_state(&key, |state| {
        state
            .log
            .iter()
            .filter(|e| e.seq > mark)
            .map(Entry::json)
            .collect()
    });
    (!entries.is_empty()).then(|| format!("[{}]", entries.join(",")))
}

/// The logged dialogs of `tab`, oldest first.
pub fn entries(tab: &Tab) -> Vec<Entry> {
    key_of(tab)
        .map(|key| with_state(&key, |state| state.log.iter().cloned().collect()))
        .unwrap_or_default()
}

/// `GET /v1/dialog`: the policy in force, a dialog waiting and the log.
pub fn json(tab: &Tab) -> String {
    let Some(key) = key_of(tab) else {
        return "{\"error\":\"no page\"}".into();
    };
    let keeper = crate::keeper::existing(key.0, &key.1);
    let (policy, pending, log) = with_state(&key, |state| {
        (
            state.policy,
            state.pending.clone(),
            state.log.iter().map(Entry::json).collect::<Vec<_>>(),
        )
    });
    let pending = match pending {
        Some(p) => format!(
            "{{\"seq\":{},\"type\":\"{}\",\"message\":{},\"open_s\":{:.1}}}",
            p.seq,
            p.kind.name(),
            json_string(&p.message),
            p.since.elapsed().as_secs_f64()
        ),
        None => "null".into(),
    };
    format!(
        "{{\"tab\":{},\"watching\":{},\"answering\":{},\"pending\":{pending},\"policy\":{},\"policy_from\":\"{}\",\"log\":[{}]}}",
        json_string(&key.1),
        keeper.as_ref().is_some_and(|k| k.watching()),
        !keeper.as_ref().is_some_and(|k| k.unanswered()),
        policy.unwrap_or_else(daemon_policy).json(),
        if policy.is_some() { "tab" } else { "daemon" },
        log.join(",")
    )
}

/// Answer the dialog left open on `tab` (`cu dialog accept [TEXT]` /
/// `dismiss`). `text` is a prompt's answer; it is never logged.
pub fn answer(tab: &Tab, accept: bool, text: Option<&str>) -> Result<String, String> {
    let key = key_of(tab).ok_or("no page")?;
    let pending = with_state(&key, |state| state.pending.clone());
    let keeper = crate::keeper::existing(key.0, &key.1);
    let Some(pending) = pending else {
        if keeper.as_ref().is_some_and(|k| k.unanswered()) {
            return Err(blocked(tab).unwrap_or_else(|| "the tab is not answering".into()));
        }
        return Err(format!(
            "no JavaScript dialog is waiting on tab {} (dialogs the policy answers are in the log: cu dialog)",
            key.1
        ));
    };
    let keeper = keeper.ok_or("the tab's dialog watcher is gone; close the tab")?;
    let text = match (text, pending.kind) {
        (Some(t), _) => Some(t.to_string()),
        (None, Kind::Prompt) => Some(pending.default_prompt.clone()),
        _ => None,
    };
    // Taken out before the answer goes: Chromium announces the dialog closed
    // before it replies, and that event must not read as "closed outside cu".
    with_state(&key, |state| {
        if state.pending.as_ref().is_some_and(|p| p.seq == pending.seq) {
            state.pending = None;
        }
    });
    let restore = || {
        with_state(&key, |state| {
            if state.pending.is_none() {
                state.pending = Some(pending.clone());
            }
        })
    };
    let reply = match keeper.call(
        "Page.handleJavaScriptDialog",
        &handle_params(accept, pending.kind, text.as_deref()),
        Duration::from_secs(3),
    ) {
        Ok(reply) => reply,
        Err(error) => {
            restore();
            return Err(error);
        }
    };
    if reply.contains("\"error\":{") {
        let why = json_string_value(reply.as_str(), "message").unwrap_or(reply);
        if why.contains("No dialog is showing") {
            update_entry(&key, pending.seq, "closed".into(), "browser");
        } else {
            restore();
        }
        return Err(format!("the browser refused the answer: {why}"));
    }
    let action = if accept { "accepted" } else { "dismissed" };
    update_entry(&key, pending.seq, action.into(), "agent");
    eprintln!(
        "cu: dialog: {} on tab {} {action} by the agent",
        pending.kind.name(),
        key.1
    );
    Ok(format!(
        "{{\"answered\":true,\"type\":\"{}\",\"action\":\"{action}\",\"message\":{}}}",
        pending.kind.name(),
        json_string(&pending.message)
    ))
}

/// Set `tab`'s own policy: `spec` on top of what is in force there, or
/// `default` for the daemon's again.
pub fn set_policy(tab: &Tab, spec: &str) -> Result<String, String> {
    let key = key_of(tab).ok_or("no page")?;
    let spec = spec.trim();
    let policy = if spec.eq_ignore_ascii_case("default") {
        None
    } else {
        let current = with_state(&key, |state| state.policy).unwrap_or_else(daemon_policy);
        Some(current.with(spec)?)
    };
    with_state(&key, |state| state.policy = policy);
    Ok(format!(
        "{{\"tab\":{},\"policy\":{},\"policy_from\":\"{}\"}}",
        json_string(&key.1),
        policy.unwrap_or_else(daemon_policy).json(),
        if policy.is_some() { "tab" } else { "daemon" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_leaves_and_says_no() {
        let p = Policy::default();
        assert_eq!(
            p.spec(),
            "alert=accept,confirm=dismiss,prompt=dismiss,beforeunload=accept"
        );
    }

    #[test]
    fn a_spec_changes_only_what_it_names() {
        let p = Policy::default()
            .with("beforeunload=manual, confirm=accept")
            .unwrap();
        assert_eq!(p.beforeunload, Answer::Manual);
        assert_eq!(p.confirm, Answer::Accept);
        assert_eq!(p.alert, Answer::Accept);
        assert_eq!(p.prompt, Answer::Dismiss);
        let all = Policy::default().with("all=manual").unwrap();
        assert!(Kind::ALL.iter().all(|k| all.get(*k) == Answer::Manual));
        assert!(Policy::default().with("confirm=maybe").is_err());
        assert!(Policy::default().with("popup=accept").is_err());
        assert!(Policy::default().with("confirm").is_err());
        assert!(Policy::default().with("").is_err());
    }

    #[test]
    fn a_prompt_accepted_carries_its_text_and_nothing_else_does() {
        assert_eq!(
            handle_params(true, Kind::Prompt, Some("Ada")),
            "{\"accept\":true,\"promptText\":\"Ada\"}"
        );
        assert_eq!(
            handle_params(false, Kind::Prompt, Some("Ada")),
            "{\"accept\":false}"
        );
        assert_eq!(handle_params(true, Kind::Alert, None), "{\"accept\":true}");
    }

    #[test]
    fn a_long_message_is_cut() {
        let long = "x".repeat(500);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), MESSAGE_MAX + 3);
        assert!(cut.ends_with("..."));
        assert_eq!(truncate("short"), "short");
    }
}
