//! One DevTools session per tab cu works in, held for the tab's life and
//! read all the time.
//!
//! Three things need a session that outlives the pooled connections the
//! commands run on, because Chromium ties them to the session that asked:
//!
//! - **JavaScript dialogs** ([`crate::dialogs`]). Measured on Chromium 156:
//!   `Page.javascriptDialogOpening` goes only to the sessions that had
//!   `Page` enabled when the dialog opened, only such a session can answer
//!   it (any other gets "No dialog is showing"), and a fresh session's
//!   `Page.enable` blocks while the dialog is open. So the answer has to come
//!   from a session that was already watching, and that session has to be
//!   *reading* -- a dialog opens between commands too (a timer, the page's
//!   own navigation, a `navigate` away from a page with a `beforeunload`
//!   handler), and while it waits the page's renderer is stopped, along with
//!   every other tab sharing that renderer process.
//! - **Device emulation** ([`crate::emulation`]): the overrides die with the
//!   session that set them.
//! - **Console capture** ([`crate::console`]): a script registered with
//!   `Page.addScriptToEvaluateOnNewDocument` lives as long as its session.
//!
//! So each tab gets one *keeper*: a websocket with `Page` enabled, a reader
//! thread that is blocked in `read` until the browser says something (no
//! polling, no timer; a small stack), answering dialog events as they come and
//! handing command replies to whoever is waiting for them. Commands are
//! written from any thread under a lock, so emulation and console can use the
//! same session. The keeper ends when its socket does (tab closed, browser
//! gone); the next command on the tab opens another.
use std::collections::HashMap;
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::server::{CdpConnection, Tab};

/// How long attaching waits for `Page.enable`. A tab answers in about a
/// millisecond; one that does not within this is stopped (a dialog that
/// opened before cu watched it, or a renderer another tab's dialog holds),
/// and a command on it would only hang.
pub const ENABLE_WAIT: Duration = Duration::from_secs(3);

/// Where the keeper's `Page.enable` stands.
const ENABLING: u8 = 0;
const ENABLED: u8 = 1;
const UNANSWERED: u8 = 2;

/// What the reader does with one reply, on its own thread.
pub type Note = Box<dyn FnOnce(&str) + Send>;

/// The keeper of one tab.
pub struct Keeper {
    pub cdp_port: u16,
    /// The DevTools target id of the tab.
    pub target: String,
    /// Unique per keeper: a new one means the session (and whatever was set
    /// on it) is new.
    pub generation: u64,
    writer: Mutex<TcpStream>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, mpsc::Sender<String>>>,
    /// Replies the reader itself waits for without blocking: command id ->
    /// what to do with it (a dialog answer, whose outcome is logged).
    notes: Mutex<HashMap<u64, Note>>,
    alive: AtomicBool,
    enable_id: AtomicU64,
    enable_state: AtomicU8,
    /// The console capture script is registered on this session.
    pub console: AtomicBool,
}

impl Keeper {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Whether `Page.enable` was answered: the keeper hears dialogs.
    pub fn watching(&self) -> bool {
        self.enable_state.load(Ordering::SeqCst) == ENABLED
    }

    /// Whether the tab did not answer `Page.enable` within [`ENABLE_WAIT`]
    /// (and has not since).
    pub fn unanswered(&self) -> bool {
        self.enable_state.load(Ordering::SeqCst) == UNANSWERED
    }

    fn write(&self, id: u64, method: &str, params: &str) -> Result<(), String> {
        let message = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
        let frame = crate::server::client_frame(message.as_bytes());
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(&frame).map_err(|e| {
            self.alive.store(false, Ordering::SeqCst);
            e.to_string()
        })
    }

    /// Send a command without waiting for its reply. `then`, if given, runs
    /// on the reader thread with the reply.
    pub fn send(&self, method: &str, params: &str, then: Option<Note>) -> Result<u64, String> {
        if !self.alive() {
            return Err("the tab's DevTools session is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Some(then) = then {
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, then);
        }
        self.write(id, method, params).map(|_| id)
    }

    /// Send `commands` together and wait (up to `timeout` in all) for their
    /// replies, in order. A reply that does not come is an error naming the
    /// wait; a dead socket is an error for every command.
    pub fn call_many(
        &self,
        commands: &[(&str, &str)],
        timeout: Duration,
    ) -> Vec<Result<String, String>> {
        if !self.alive() {
            return commands
                .iter()
                .map(|_| Err("the tab's DevTools session is closed".into()))
                .collect();
        }
        let mut receivers = Vec::with_capacity(commands.len());
        for (method, params) in commands {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel();
            self.waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
            if let Err(error) = self.write(id, method, params) {
                self.waiting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                receivers.push(Err(error));
            } else {
                receivers.push(Ok((id, rx)));
            }
        }
        let deadline = Instant::now() + timeout;
        receivers
            .into_iter()
            .map(|slot| {
                let (id, rx) = slot?;
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(reply) => Ok(reply),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.waiting
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        Err(format!(
                            "the tab did not answer within {:.1} s",
                            timeout.as_secs_f64()
                        ))
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Err("the tab's DevTools session closed".into())
                    }
                }
            })
            .collect()
    }

    /// One command and its reply.
    pub fn call(&self, method: &str, params: &str, timeout: Duration) -> Result<String, String> {
        self.call_many(&[(method, params)], timeout)
            .pop()
            .unwrap_or_else(|| Err("no reply".into()))
    }

    /// Close the session: the reader ends and the keeper is forgotten.
    pub fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(Shutdown::Both);
    }

    fn dispatch(&self, text: &str) {
        match crate::server::cdp_message_id(text) {
            Some(id) => {
                if id == self.enable_id.load(Ordering::SeqCst) && !text.contains("\"error\":{") {
                    self.enable_state.store(ENABLED, Ordering::SeqCst);
                }
                let note = self
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(note) = note {
                    note(text);
                }
                let waiter = self
                    .waiting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(tx) = waiter {
                    let _ = tx.send(text.to_string());
                }
            }
            None => crate::dialogs::on_event(self, text),
        }
    }

    fn read_loop(self: Arc<Self>, mut stream: TcpStream) {
        // Blocked in `read` until the browser speaks: an idle tab costs
        // nothing but this thread's (small) stack.
        let _ = stream.set_read_timeout(None);
        while let Ok(text) = crate::server::read_cdp_message(&mut stream) {
            self.dispatch(&text);
        }
        self.alive.store(false, Ordering::SeqCst);
        // Waiters see their channel close; nothing is left half-answered.
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let key = (self.cdp_port, self.target.clone());
        let slot = registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(slot) = slot {
            let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
            if held
                .as_ref()
                .is_some_and(|k| k.generation == self.generation)
            {
                *held = None;
            }
        }
        crate::dialogs::keeper_gone(self.cdp_port, &self.target);
    }
}

type Slot = Arc<Mutex<Option<Arc<Keeper>>>>;

fn registry() -> &'static Mutex<HashMap<(u16, String), Slot>> {
    static KEEPERS: OnceLock<Mutex<HashMap<(u16, String), Slot>>> = OnceLock::new();
    KEEPERS.get_or_init(|| Mutex::new(HashMap::new()))
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// The live keeper of `tab`, opened (and `Page` enabled on it) when there is
/// none. The default tab is watched as the page it is pinned to.
pub fn ensure(tab: &Tab) -> Result<Arc<Keeper>, String> {
    let target = tab
        .resolved_target()
        .ok_or("the browser has no page to watch")?;
    watch(tab.cdp_port, &target)
}

/// The live keeper of target `target`, opened when there is none.
pub fn watch(cdp_port: u16, target: &str) -> Result<Arc<Keeper>, String> {
    let slot = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry((cdp_port, target.to_string()))
        .or_default()
        .clone();
    // Per tab: two requests racing on a new tab open one keeper, and a tab
    // being attached does not hold up any other tab.
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(keeper) = held.as_ref()
        && keeper.alive()
    {
        return Ok(Arc::clone(keeper));
    }
    let keeper = open(cdp_port, target)?;
    *held = Some(Arc::clone(&keeper));
    // Enabled before anyone gets the keeper, so a dialog the next command
    // causes is heard. A tab that does not answer is noted, not refused:
    // the reply flips the state when it comes.
    let id = keeper.next_id.fetch_add(1, Ordering::SeqCst);
    keeper.enable_id.store(id, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    keeper
        .waiting
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, tx);
    keeper.write(id, "Page.enable", "{}")?;
    if rx.recv_timeout(ENABLE_WAIT).is_err() && !keeper.watching() {
        keeper
            .enable_state
            .compare_exchange(ENABLING, UNANSWERED, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
        eprintln!(
            "cu: tab {target} did not answer Page.enable within {} s: a dialog that opened before cu watched it (or another tab's dialog holding the same renderer) may be blocking it",
            ENABLE_WAIT.as_secs()
        );
    }
    Ok(keeper)
}

fn open(cdp_port: u16, target: &str) -> Result<Arc<Keeper>, String> {
    let connection = CdpConnection::target(cdp_port, target)?;
    let stream = connection.into_stream()?;
    let _ = stream.set_nodelay(true);
    let reader = stream.try_clone().map_err(|e| e.to_string())?;
    let keeper = Arc::new(Keeper {
        cdp_port,
        target: target.to_string(),
        generation: GENERATION.fetch_add(1, Ordering::SeqCst),
        writer: Mutex::new(stream),
        next_id: AtomicU64::new(1),
        waiting: Mutex::new(HashMap::new()),
        notes: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        enable_id: AtomicU64::new(0),
        enable_state: AtomicU8::new(ENABLING),
        console: AtomicBool::new(false),
    });
    let owned = Arc::clone(&keeper);
    std::thread::Builder::new()
        .name(format!("cu-keeper-{target}"))
        .stack_size(256 * 1024)
        .spawn(move || owned.read_loop(reader))
        .map_err(|e| e.to_string())?;
    Ok(keeper)
}

/// The keeper of a target, if one is open; never opens one (a fresh
/// `Page.enable` would block on a tab whose dialog is open).
pub fn existing(cdp_port: u16, target: &str) -> Option<Arc<Keeper>> {
    let slot = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(cdp_port, target.to_string()))
        .cloned()?;
    let held = slot.lock().unwrap_or_else(|e| e.into_inner());
    held.as_ref().filter(|k| k.alive()).cloned()
}

/// The tab is gone (or cu let it go): close its keeper.
pub fn forget(cdp_port: u16, target: &str) {
    let slot = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cdp_port, target.to_string()));
    if let Some(slot) = slot
        && let Some(keeper) = slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    {
        keeper.close();
    }
}

/// The targets of `cdp_port` with a live keeper.
pub fn watched(cdp_port: u16) -> Vec<String> {
    let slots: Vec<((u16, String), Slot)> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((port, _), _)| *port == cdp_port)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    slots
        .into_iter()
        .filter(|(_, slot)| {
            slot.lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|k| k.alive())
        })
        .map(|((_, target), _)| target)
        .collect()
}

/// A browser-level target event: a page another watched page opened (a
/// popup) is watched from the moment it exists, so a dialog it shows before
/// any command reaches it is answered too.
pub fn see_target_event(cdp_port: u16, event: &str) {
    if crate::server::json_string_value(event, "method").as_deref() != Some("Target.targetCreated")
    {
        return;
    }
    let Some(info) = crate::server::json_object(event, "targetInfo") else {
        return;
    };
    if crate::server::json_string_value(info, "type").as_deref() != Some("page") {
        return;
    }
    let (Some(id), Some(opener)) = (
        crate::server::json_string_value(info, "targetId"),
        crate::server::json_string_value(info, "openerId").filter(|o| !o.is_empty()),
    ) else {
        return;
    };
    if existing(cdp_port, &opener).is_none() {
        return;
    }
    std::thread::spawn(move || {
        if let Err(error) = watch(cdp_port, &id) {
            eprintln!("cu: could not watch popup {id} for dialogs: {error}");
        }
    });
}
