//! What the browser is running besides the tabs cu drives: popups, out-of-
//! process iframes, dedicated/shared/service workers.
//!
//! The daemon's browser-level control connection turns on target discovery
//! (`Target.setDiscoverTargets`) and keeps this registry from the
//! `targetCreated` / `targetInfoChanged` / `targetDestroyed` events. That
//! is observation only: nothing is attached and no domain is enabled in a
//! page or worker, so nothing changes for the page. A browser-level
//! `Target.setAutoAttach` was tried first; it only attaches top-level pages,
//! and reaching their frames and workers would mean holding a session on
//! every page for its whole life -- discovery gives the same picture without
//! one. Where cu has to *act* in an out-of-process iframe (site isolation,
//! the compatibility profile) it talks to that iframe target's own endpoint,
//! the same thing an auto-attached flat session is (see
//! [`crate::server::oopif_targets`]).
//!
//! URLs are cut to origin and path ([`crate::challenge::redact`]).
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::server::{json_object, json_string, json_string_value};

#[derive(Clone, Debug)]
pub struct TargetInfo {
    pub kind: String,
    pub url: String,
    pub opener: Option<String>,
    pub context: Option<String>,
    pub seen: Instant,
}

#[derive(Default)]
struct Registry {
    live: HashMap<String, TargetInfo>,
    created: HashMap<String, u64>,
    popups: u64,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Feed one browser-level event; anything that is not a target event is
/// ignored.
pub fn see(event: &str) {
    let Some(method) = json_string_value(event, "method") else {
        return;
    };
    if !method.starts_with("Target.target") {
        return;
    }
    let Some(info) = json_object(event, "targetInfo") else {
        if method == "Target.targetDestroyed"
            && let Some(id) = json_string_value(event, "targetId")
        {
            registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .live
                .remove(&id);
        }
        return;
    };
    let Some(id) = json_string_value(info, "targetId") else {
        return;
    };
    let target = TargetInfo {
        kind: json_string_value(info, "type").unwrap_or_default(),
        url: crate::challenge::redact(&json_string_value(info, "url").unwrap_or_default()),
        opener: json_string_value(info, "openerId").filter(|o| !o.is_empty()),
        context: json_string_value(info, "browserContextId"),
        seen: Instant::now(),
    };
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    match method.as_str() {
        "Target.targetCreated" => {
            *r.created.entry(target.kind.clone()).or_default() += 1;
            if target.kind == "page" && target.opener.is_some() {
                r.popups += 1;
                eprintln!(
                    "cu: popup {} opened by {} ({})",
                    id,
                    target.opener.as_deref().unwrap_or("-"),
                    target.url
                );
            }
            r.live.insert(id, target);
        }
        "Target.targetInfoChanged" => {
            let seen = r.live.get(&id).map(|t| t.seen).unwrap_or(target.seen);
            r.live.insert(id, TargetInfo { seen, ..target });
        }
        _ => {}
    }
}

/// The page that opened `id`, if it is a popup.
pub fn opener_of(id: &str) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .live
        .get(id)
        .and_then(|t| t.opener.clone())
}

/// `GET /v1/targets`: every live target, grouped counts and lifetime totals.
pub fn json() -> String {
    let r = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut live: Vec<(&String, &TargetInfo)> = r.live.iter().collect();
    live.sort_by(|a, b| a.1.kind.cmp(&b.1.kind).then(a.1.seen.cmp(&b.1.seen)));
    let mut counts: HashMap<&str, u64> = HashMap::new();
    for (_, t) in &live {
        *counts.entry(t.kind.as_str()).or_default() += 1;
    }
    let map = |m: &HashMap<&str, u64>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort();
        v.iter()
            .map(|(k, n)| format!("{}:{n}", json_string(k)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let created: HashMap<&str, u64> = r.created.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    format!(
        "{{\"live\":{{{}}},\"created\":{{{}}},\"popups_opened\":{},\"targets\":[{}]}}",
        map(&counts),
        map(&created),
        r.popups,
        live.iter()
            .map(|(id, t)| format!(
                "{{\"id\":{},\"type\":{},\"url\":{},\"opener\":{},\"context\":{},\"age_ms\":{}}}",
                json_string(id),
                json_string(&t.kind),
                json_string(&t.url),
                t.opener.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
                t.context.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
                t.seen.elapsed().as_millis()
            ))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The short form for `/v1/diagnostics`.
pub fn summary_json() -> String {
    let r = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut counts: HashMap<&str, u64> = HashMap::new();
    for t in r.live.values() {
        *counts.entry(t.kind.as_str()).or_default() += 1;
    }
    let mut v: Vec<_> = counts.iter().collect();
    v.sort();
    format!(
        "{{\"live\":{{{}}},\"popups_opened\":{}}}",
        v.iter()
            .map(|(k, n)| format!("{}:{n}", json_string(k)))
            .collect::<Vec<_>>()
            .join(","),
        r.popups
    )
}
