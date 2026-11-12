//! Persistent permission grants (`cu grant`).
//!
//! `Browser.grantPermissions` lasts as long as the browser-level DevTools
//! session that sent it, so the inject thread ([`crate::inject::spawn`])
//! holds one open, checks it every pass and, when it dropped or the browser
//! restarted, reconnects and applies every grant again.

use crate::inject::{now_secs, write_private};
use crate::server::{
    CdpConnection, json_array, json_objects, json_string, json_string_value, json_strings,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub name: String,
    pub origin: String,
    pub permissions: Vec<String>,
}

impl Grant {
    fn permissions_json(&self) -> String {
        let p: Vec<String> = self.permissions.iter().map(|p| json_string(p)).collect();
        format!("[{}]", p.join(","))
    }

    pub fn json(&self) -> String {
        format!(
            "{{\"name\":{},\"origin\":{},\"permissions\":{}}}",
            json_string(&self.name),
            json_string(&self.origin),
            self.permissions_json()
        )
    }

    pub fn parse(object: &str) -> Option<Grant> {
        let name = json_string_value(object, "name").filter(|n| !n.is_empty())?;
        let permissions = match json_array(object, "permissions") {
            Some(a) => json_strings(&a),
            None => json_string_value(object, "permissions")
                .unwrap_or_default()
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        };
        Some(Grant {
            name,
            origin: json_string_value(object, "origin").unwrap_or_default(),
            permissions,
        })
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("grant.json")
}

pub fn load(dir: &Path) -> Vec<Grant> {
    std::fs::read_to_string(path(dir))
        .map(|s| {
            json_objects(&s)
                .iter()
                .filter_map(|o| Grant::parse(o))
                .collect()
        })
        .unwrap_or_default()
}

fn save(dir: &Path, grants: &[Grant]) -> Result<(), String> {
    let body: Vec<String> = grants.iter().map(Grant::json).collect();
    write_private(&path(dir), &format!("[{}]\n", body.join(",")))
}

#[derive(Default)]
struct State {
    connection: Option<CdpConnection>,
    /// Grant name -> the grant as last applied on this connection.
    applied: HashMap<String, String>,
    /// Grant name -> (last error, unix time).
    errors: HashMap<String, (String, u64)>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn connect(cdp_port: u16) -> Result<CdpConnection, String> {
    let version = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version")?;
    let ws = json_string_value(&version, "webSocketDebuggerUrl")
        .ok_or("Chromium did not expose a browser endpoint")?;
    CdpConnection::connect(&ws)
}

fn failed(reply: &str) -> Option<String> {
    crate::server::json_object(reply, "error")
        .map(|e| json_string_value(e, "message").unwrap_or_else(|| e.to_string()))
}

/// The held browser session, alive (probed) or opened again.
fn live(state: &mut State, cdp_port: u16) -> Result<&mut CdpConnection, String> {
    let alive = state
        .connection
        .as_mut()
        .is_some_and(|c| c.call("Browser.getVersion", "{}").is_ok());
    if !alive {
        state.connection = None;
        state.applied.clear();
        state.connection = Some(connect(cdp_port)?);
    }
    Ok(state.connection.as_mut().expect("connected above"))
}

/// One command on the held browser-level session.
pub fn browser_call(cdp_port: u16, method: &str, params: &str) -> Result<String, String> {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    let reply = live(&mut state, cdp_port)?.call(method, params)?;
    match failed(&reply) {
        Some(e) => Err(e),
        None => Ok(reply),
    }
}

/// Apply every grant not yet applied on the live session.
pub fn tick(dir: &Path, cdp_port: u16) {
    let grants = load(dir);
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    state
        .errors
        .retain(|name, _| grants.iter().any(|g| &g.name == name));
    if grants.is_empty() {
        return;
    }
    if let Err(e) = live(&mut state, cdp_port) {
        for g in &grants {
            state.errors.insert(g.name.clone(), (e.clone(), now_secs()));
        }
        return;
    }
    for g in &grants {
        let signature = g.json();
        if state.applied.get(&g.name) == Some(&signature) {
            continue;
        }
        let params = format!(
            "{{\"origin\":{},\"permissions\":{}}}",
            json_string(&g.origin),
            g.permissions_json()
        );
        let Some(connection) = state.connection.as_mut() else {
            return;
        };
        let outcome = connection
            .call("Browser.grantPermissions", &params)
            .and_then(|reply| failed(&reply).map_or(Ok(()), Err));
        match outcome {
            Ok(()) => {
                state.applied.insert(g.name.clone(), signature);
                state.errors.remove(&g.name);
            }
            Err(e) => {
                state.errors.insert(g.name.clone(), (e, now_secs()));
            }
        }
    }
}

pub fn add(dir: &Path, cdp_port: u16, body: &str) -> Result<String, String> {
    let grant = Grant::parse(body).ok_or("grant add needs a name")?;
    if grant.origin.is_empty() || grant.permissions.is_empty() {
        return Err("grant add needs an origin and at least one permission".into());
    }
    let store = crate::inject::store_lock();
    let mut grants = load(dir);
    grants.retain(|g| g.name != grant.name);
    let name = grant.name.clone();
    grants.push(grant);
    save(dir, &grants)?;
    drop(store);
    crate::inject::spawn(dir, cdp_port);
    tick(dir, cdp_port);
    let error = state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .errors
        .get(&name)
        .map(|(e, _)| e.clone());
    Ok(match error {
        Some(e) => format!(
            "{{\"added\":{},\"error\":{}}}",
            json_string(&name),
            json_string(&e)
        ),
        None => format!("{{\"added\":{},\"applied\":true}}", json_string(&name)),
    })
}

pub fn remove(dir: &Path, name: &str) -> Result<String, String> {
    let _store = crate::inject::store_lock();
    let mut grants = load(dir);
    let before = grants.len();
    grants.retain(|g| g.name != name);
    if grants.len() == before {
        return Err(format!("no grant named {name}"));
    }
    save(dir, &grants)?;
    // Chromium has no per-origin revoke short of resetting everything; the
    // grant lapses when the held session or the browser goes.
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .applied
        .remove(name);
    Ok(format!("{{\"removed\":{}}}", json_string(name)))
}

pub fn list_json(dir: &Path) -> String {
    let state = state().lock().unwrap_or_else(|e| e.into_inner());
    let items: Vec<String> = load(dir)
        .iter()
        .map(|g| {
            let error = match state.errors.get(&g.name) {
                Some((e, at)) => format!("{{\"message\":{},\"at\":{at}}}", json_string(e)),
                None => "null".into(),
            };
            format!(
                "{{\"name\":{},\"origin\":{},\"permissions\":{},\"applied\":{},\"last_error\":{}}}",
                json_string(&g.name),
                json_string(&g.origin),
                g.permissions_json(),
                state.applied.get(&g.name) == Some(&g.json()),
                error
            )
        })
        .collect();
    format!("{{\"grants\":[{}]}}", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_parse_lists_and_comma_strings() {
        let g = Grant {
            name: "f".into(),
            origin: "https://www.figma.com".into(),
            permissions: vec!["localNetworkAccess".into(), "clipboardReadWrite".into()],
        };
        assert_eq!(Grant::parse(&g.json()), Some(g.clone()));
        let body = r#"{"name":"f","origin":"https://www.figma.com","permissions":"localNetworkAccess, clipboardReadWrite"}"#;
        assert_eq!(Grant::parse(body), Some(g));
    }
}
