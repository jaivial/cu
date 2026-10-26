//! What cu remembers about the page it is working in, between actions.
//!
//! The helpers run in cu's own isolated world of the frame they read (see
//! [`crate::execution`]). That world belongs to a *document*: it survives
//! every action in the same document and dies with the next navigation.
//!
//! Remembering two things about it saves most of the work of an action:
//!
//! - the execution context id of the world, so the world is made once per
//!   document instead of once per action;
//! - that the helper script is installed there, so the ten kilobytes of
//!   JavaScript that installs the ref registry and leaves the page walk in
//!   `window.__cuSnapshot` are sent once per document instead of once per
//!   snapshot. The browser compiles the script every time it is sent, which
//!   was most of the cost of a snapshot.
//!
//! The frame list of the page is remembered as well: the snapshot reads it
//! from the browser anyway, and the actions that follow need the same answer.
//!
//! The memory is a cache, never a source of truth: a world that is gone
//! (the document was replaced) is detected by the browser's own answer, and
//! the entry is dropped and made again.

use crate::server::{Frame, Tab};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What is known about one frame of one page.
#[derive(Clone, Copy, Debug)]
pub struct FrameState {
    /// The execution context id of cu's isolated world in this frame.
    pub context: i64,
    /// Whether the helper script is installed in that world.
    pub installed: bool,
}

fn states() -> &'static Mutex<HashMap<(Tab, String), FrameState>> {
    static STATES: OnceLock<Mutex<HashMap<(Tab, String), FrameState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What is known about `frame` of `tab`, if anything.
pub fn get(tab: &Tab, frame: &str) -> Option<FrameState> {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(tab.clone(), frame.to_string()))
        .copied()
}

/// Remember the isolated world of `frame`, and whether the helper script is
/// installed in it.
pub fn remember(tab: &Tab, frame: &str, context: i64, installed: bool) {
    let mut states = states().lock().unwrap_or_else(|e| e.into_inner());
    states.insert(
        (tab.clone(), frame.to_string()),
        FrameState { context, installed },
    );
}

/// Mark the helper script as installed in `frame`'s world.
pub fn mark_installed(tab: &Tab, frame: &str) {
    let mut states = states().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = states.get_mut(&(tab.clone(), frame.to_string())) {
        entry.installed = true;
    }
}

/// Forget `frame`: its document is gone, or its world was replaced.
pub fn forget(tab: &Tab, frame: &str) {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(tab.clone(), frame.to_string()));
}

/// Forget every frame of `tab`: a navigation replaced them all.
pub fn forget_tab(tab: &Tab) {
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(known, _), _| known != tab);
}

fn pages() -> &'static Mutex<HashMap<Tab, Vec<Frame>>> {
    static PAGES: OnceLock<Mutex<HashMap<Tab, Vec<Frame>>>> = OnceLock::new();
    PAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The frames of a tab's page, when they are known.
///
/// The frame tree is the browser's answer to "what is this page made of",
/// and it changes only when the page navigates or an iframe comes or goes.
/// The snapshot reads it from the browser anyway, so the answer is kept for
/// the actions that follow it.
pub fn page_frames(tab: &Tab) -> Option<Vec<Frame>> {
    pages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
}

/// Remember the frames of a tab's page.
pub fn remember_frames(tab: &Tab, page: Vec<Frame>) {
    if let Some(main) = page.first() {
        main_frames()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tab.clone(), main.id.clone());
    }
    pages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), page);
}

/// The main frame's id, the one every document of the tab hangs in.
///
/// The frame outlives the documents in it: a navigation replaces the
/// document, not the frame. So this answer stays right where the frame list
/// does not.
pub fn main_frame(tab: &Tab) -> Option<String> {
    main_frames()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
}

fn main_frames() -> &'static Mutex<HashMap<Tab, String>> {
    static MAIN: OnceLock<Mutex<HashMap<Tab, String>>> = OnceLock::new();
    MAIN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forget the frames of a tab's page: a navigation replaced them.
pub fn forget_frames(tab: &Tab) {
    pages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tab);
}
