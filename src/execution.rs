//! Where cu's helper scripts run.
//!
//! Every helper (snapshot walk, ref registry, text read, settle wait,
//! screenshot measure, challenge probe) runs in cu's own isolated world of
//! the frame it reads: a separate JavaScript global over the same DOM, made
//! with `Page.createIsolatedWorld`. The page's scripts cannot see what cu
//! defines there (`window.__cu` used to be a property of the page's own
//! `window`), cannot observe the evaluations, and cannot shadow the built-ins
//! the helpers call. Isolated worlds of one name persist per document, so the
//! registry a snapshot installs is the one a later action finds.
//!
//! The page's main world is used only where the logic needs it, and named
//! as such with [`World::Main`]: the batch barrier (`0`, which reads and writes
//! nothing) and an explicit `CU_HELPER_WORLD=main` override, kept as a way
//! back to the old behaviour if a page misbehaves with isolated helpers.
use crate::server::{json_string, json_value};

/// The JavaScript world a helper evaluates in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum World {
    /// cu's isolated world of the frame (the default).
    Isolated,
    /// The page's own world: only where the logic needs page state.
    Main,
}

/// The world helpers run in: isolated unless `CU_HELPER_WORLD=main`.
pub fn helper_world() -> World {
    match std::env::var("CU_HELPER_WORLD").as_deref() {
        Ok("main") => World::Main,
        _ => World::Isolated,
    }
}

/// Name of cu's isolated world. One name for every helper, so they share
/// the registry the snapshot installs.
pub const WORLD_NAME: &str = "cu";

/// The `"contextId":N,` fragment that puts a `Runtime.evaluate` in `world`
/// of `frame_id` (empty for the main world). The trailing comma lets callers
/// splice it between the expression and the next field.
///
/// Fails when the frame is gone, so the caller can say so instead of
/// evaluating somewhere else.
pub fn context_fragment<F>(cmd: &mut F, world: World, frame_id: &str) -> Result<String, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    match world {
        World::Main => Ok(String::new()),
        World::Isolated => isolated_context(cmd, frame_id).map(|id| format!("\"contextId\":{id},")),
    }
}

/// The execution context id of cu's isolated world in `frame_id`.
pub fn isolated_context<F>(cmd: &mut F, frame_id: &str) -> Result<i64, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let reply = cmd(
        "Page.createIsolatedWorld",
        &format!(
            "{{\"frameId\":{},\"worldName\":\"{WORLD_NAME}\"}}",
            json_string(frame_id)
        ),
    )?;
    json_value(&reply, "executionContextId")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "the frame is gone; take a new snapshot".to_string())
}

/// The main frame's id, from the frame tree.
pub fn main_frame<F>(cmd: &mut F) -> Result<String, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    let tree = cmd("Page.getFrameTree", "{}")?;
    crate::server::flatten_frame_tree(&tree)
        .into_iter()
        .next()
        .map(|f| f.id)
        .ok_or_else(|| "page has no main frame".to_string())
}

/// `Runtime.evaluate` params for `expression` in the context `fragment`
/// names (see [`context_fragment`]).
pub fn evaluate_params(expression: &str, fragment: &str, await_promise: bool) -> String {
    format!(
        "{{\"expression\":{},{fragment}{}\"returnByValue\":true}}",
        json_string(expression),
        if await_promise {
            "\"awaitPromise\":true,"
        } else {
            ""
        }
    )
}
