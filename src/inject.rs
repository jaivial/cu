//! Persistent registered scripts (`cu inject`).
//!
//! A registration names a URL glob and a script. A daemon thread (started
//! once there is at least one registration, see [`spawn`]) lists the page
//! targets every 2 s; on each matching tab it registers the script with
//! `Page.addScriptToEvaluateOnNewDocument` on the tab's keeper (once per
//! keeper generation) and evaluates it right away. With `retry_ms` it keeps
//! evaluating until the result starts with `ok`, `started` or `already`.
//! With `keep_open` a URL is opened in a background tab when nothing
//! matches (at most once a minute). Grants ([`crate::grant`]) ride the same
//! thread.

use crate::server::{json_number, json_objects, json_string, json_string_value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    pub name: String,
    pub matcher: String,
    pub js: String,
    pub retry_ms: u64,
    pub keep_open: String,
}

impl Script {
    pub fn json(&self) -> String {
        format!(
            "{{\"name\":{},\"match\":{},\"js\":{},\"retry_ms\":{},\"keep_open\":{}}}",
            json_string(&self.name),
            json_string(&self.matcher),
            json_string(&self.js),
            self.retry_ms,
            json_string(&self.keep_open)
        )
    }

    pub fn parse(object: &str) -> Option<Script> {
        let name = json_string_value(object, "name").filter(|n| !n.is_empty())?;
        Some(Script {
            name,
            matcher: json_string_value(object, "match").unwrap_or_default(),
            js: json_string_value(object, "js").unwrap_or_default(),
            retry_ms: json_number(object, "retry_ms").unwrap_or(0.0).max(0.0) as u64,
            keep_open: json_string_value(object, "keep_open").unwrap_or_default(),
        })
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("inject.json")
}

pub fn load(dir: &Path) -> Vec<Script> {
    std::fs::read_to_string(path(dir))
        .map(|s| json_objects(&s).iter().filter_map(|o| Script::parse(o)).collect())
        .unwrap_or_default()
}

fn save(dir: &Path, scripts: &[Script]) -> Result<(), String> {
    let body: Vec<String> = scripts.iter().map(Script::json).collect();
    write_private(&path(dir), &format!("[{}]\n", body.join(",")))
}

/// Write a file only its owner can read (the scripts may carry tokens).
pub fn write_private(path: &Path, body: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `*` matches any run of characters, `?` one; everything else literally.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// A result that means the script is in place.
pub fn settled(result: &str) -> bool {
    ["ok", "started", "already"]
        .iter()
        .any(|p| result.starts_with(p))
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}


/// What happened on one tab for one registration.
#[derive(Clone, Debug)]
struct TabState {
    generation: u64,
    done: bool,
    last_eval: Instant,
    url: String,
    result: String,
    at: u64,
}

fn tabs() -> &'static Mutex<HashMap<(String, String), TabState>> {
    static TABS: OnceLock<Mutex<HashMap<(String, String), TabState>>> = OnceLock::new();
    TABS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn opened() -> &'static Mutex<HashMap<String, Instant>> {
    static OPENED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    OPENED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The value (or the exception) a `Runtime.evaluate` reply carries, as text.
pub fn eval_result(reply: &str) -> String {
    if let Some(error) = crate::server::json_object(reply, "error") {
        return format!(
            "error: {}",
            json_string_value(error, "message").unwrap_or_default()
        );
    }
    if let Some(details) = crate::server::json_object(reply, "exceptionDetails") {
        let text = crate::server::json_object(details, "exception")
            .and_then(|e| json_string_value(e, "description"))
            .or_else(|| json_string_value(details, "text"))
            .unwrap_or_default();
        return format!("error: {}", text.chars().take(300).collect::<String>());
    }
    let inner = crate::server::json_object(reply, "result")
        .and_then(|outer| crate::server::json_object(outer, "result"));
    match inner {
        Some(r) => json_string_value(r, "value")
            .or_else(|| crate::server::json_value(r, "value"))
            .or_else(|| json_string_value(r, "type"))
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn evaluate(keeper: &crate::keeper::Keeper, js: &str) -> String {
    let params = format!(
        "{{\"expression\":{},\"returnByValue\":true,\"awaitPromise\":true}}",
        json_string(js)
    );
    match keeper.call("Runtime.evaluate", &params, Duration::from_secs(15)) {
        Ok(reply) => eval_result(&reply),
        Err(e) => format!("error: {e}"),
    }
}

/// Page targets as `(id, url)`.
pub fn pages(cdp_port: u16) -> Vec<(String, String)> {
    let Ok(list) = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/list") else {
        return Vec::new();
    };
    json_objects(&list)
        .iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter_map(|o| Some((json_string_value(o, "id")?, json_string_value(o, "url")?)))
        .collect()
}

/// One registration on one tab: register on a new keeper, evaluate, retry.
fn apply(cdp_port: u16, script: &Script, target: &str, url: &str, force: bool) -> Option<String> {
    let keeper = crate::keeper::watch(cdp_port, target).ok()?;
    let key = (script.name.clone(), target.to_string());
    let prior = tabs().lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
    let fresh = prior.as_ref().is_none_or(|p| p.generation != keeper.generation);
    if fresh {
        let params = format!(
            "{{\"source\":{},\"runImmediately\":false}}",
            json_string(&script.js)
        );
        let _ = keeper.call(
            "Page.addScriptToEvaluateOnNewDocument",
            &params,
            Duration::from_secs(5),
        );
    }
    let due = match &prior {
        _ if fresh || force => true,
        Some(p) => {
            !p.done
                && script.retry_ms > 0
                && p.last_eval.elapsed() >= Duration::from_millis(script.retry_ms)
        }
        None => true,
    };
    if !due {
        return None;
    }
    let result = evaluate(&keeper, &script.js);
    let done = script.retry_ms == 0 || settled(&result);
    tabs().lock().unwrap_or_else(|e| e.into_inner()).insert(
        key,
        TabState {
            generation: keeper.generation,
            done,
            last_eval: Instant::now(),
            url: url.to_string(),
            result: result.clone(),
            at: now_secs(),
        },
    );
    Some(result)
}


/// Open `url` in a background tab, at most once a minute per registration.
fn keep_open(cdp_port: u16, script: &Script) {
    let mut opened = opened().lock().unwrap_or_else(|e| e.into_inner());
    if opened
        .get(&script.name)
        .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
    {
        return;
    }
    opened.insert(script.name.clone(), Instant::now());
    drop(opened);
    let params = format!(
        "{{\"url\":{},\"background\":true}}",
        json_string(&script.keep_open)
    );
    if let Err(e) = crate::grant::browser_call(cdp_port, "Target.createTarget", &params) {
        eprintln!("cu inject: {}: keep_open failed: {e}", script.name);
    }
}

/// One pass over every registration.
pub fn tick(dir: &Path, cdp_port: u16) {
    let scripts = load(dir);
    let pages = if scripts.is_empty() { Vec::new() } else { pages(cdp_port) };
    for script in &scripts {
        let matching: Vec<_> = pages.iter().filter(|(_, url)| glob(&script.matcher, url)).collect();
        if matching.is_empty() && !script.keep_open.is_empty() {
            keep_open(cdp_port, script);
        }
        for (id, url) in matching {
            apply(cdp_port, script, id, url, false);
        }
    }
    // Forget tabs that are gone and registrations that were removed.
    let live: Vec<&String> = pages.iter().map(|(id, _)| id).collect();
    tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(name, id), _| live.contains(&id) && scripts.iter().any(|s| &s.name == name));
    crate::grant::tick(dir, cdp_port);
}

/// Start the watch thread once, if anything is registered.
pub fn spawn(dir: &Path, cdp_port: u16) {
    if load(dir).is_empty() && crate::grant::load(dir).is_empty() {
        return;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    let dir = dir.to_path_buf();
    STARTED.get_or_init(|| {
        std::thread::spawn(move || {
            loop {
                tick(&dir, cdp_port);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    });
}

/// The registrations with what happened on each matching tab.
pub fn list_json(dir: &Path) -> String {
    let tabs = tabs().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let items: Vec<String> = load(dir)
        .iter()
        .map(|s| {
            let on: Vec<String> = tabs
                .iter()
                .filter(|((name, _), _)| name == &s.name)
                .map(|((_, id), t)| {
                    format!(
                        "{{\"tab\":{},\"url\":{},\"result\":{},\"at\":{},\"done\":{}}}",
                        json_string(id),
                        json_string(&t.url),
                        json_string(&t.result),
                        t.at,
                        t.done
                    )
                })
                .collect();
            let js = if s.js.len() > 80 { format!("{}...", s.js.chars().take(80).collect::<String>()) } else { s.js.clone() };
            format!(
                "{{\"name\":{},\"match\":{},\"retry_ms\":{},\"keep_open\":{},\"js_bytes\":{},\"js\":{},\"tabs\":[{}]}}",
                json_string(&s.name),
                json_string(&s.matcher),
                s.retry_ms,
                json_string(&s.keep_open),
                s.js.len(),
                json_string(&js),
                on.join(",")
            )
        })
        .collect();
    format!("{{\"scripts\":[{}]}}", items.join(","))
}


pub fn add(dir: &Path, cdp_port: u16, script: Script) -> Result<String, String> {
    if script.name.is_empty() || script.matcher.is_empty() || script.js.is_empty() {
        return Err("inject add needs a name, a match glob and a script".into());
    }
    let mut scripts = load(dir);
    scripts.retain(|s| s.name != script.name);
    let name = script.name.clone();
    scripts.push(script);
    save(dir, &scripts)?;
    // A re-registration starts over on every tab.
    tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(n, _), _| n != &name);
    spawn(dir, cdp_port);
    Ok(format!("{{\"added\":{}}}", json_string(&name)))
}

pub fn remove(dir: &Path, name: &str) -> Result<String, String> {
    let mut scripts = load(dir);
    let before = scripts.len();
    scripts.retain(|s| s.name != name);
    if scripts.len() == before {
        return Err(format!("no inject registration named {name}"));
    }
    save(dir, &scripts)?;
    Ok(format!("{{\"removed\":{}}}", json_string(name)))
}

/// Evaluate a registration now on every matching tab.
pub fn run(dir: &Path, cdp_port: u16, name: &str) -> Result<String, String> {
    let script = load(dir)
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no inject registration named {name}"))?;
    let results: Vec<String> = pages(cdp_port)
        .iter()
        .filter(|(_, url)| glob(&script.matcher, url))
        .map(|(id, url)| {
            let result = apply(cdp_port, &script, id, url, true).unwrap_or_default();
            format!(
                "{{\"tab\":{},\"url\":{},\"result\":{}}}",
                json_string(id),
                json_string(url),
                json_string(&result)
            )
        })
        .collect();
    Ok(format!("{{\"name\":{},\"tabs\":[{}]}}", json_string(name), results.join(",")))
}

type Reply = (&'static str, &'static str, String);

fn reply(result: Result<String, String>) -> Reply {
    match result {
        Ok(body) => ("200 OK", "application/json", body),
        Err(e) => (
            "400 Bad Request",
            "application/json",
            format!("{{\"error\":{}}}", json_string(&e)),
        ),
    }
}

/// The `/v1/inject` and `/v1/grant` routes.
pub fn route(method: &str, path: &str, body: &str, dir: &Path, cdp_port: u16) -> Option<Reply> {
    let name = || json_string_value(body, "name").unwrap_or_default();
    Some(match (method, path) {
        ("GET", "/v1/inject") => ("200 OK", "application/json", list_json(dir)),
        ("POST", "/v1/inject") => reply(add(
            dir,
            cdp_port,
            Script {
                name: name(),
                matcher: json_string_value(body, "match").unwrap_or_default(),
                js: json_string_value(body, "js").unwrap_or_default(),
                retry_ms: json_number(body, "retry_ms").unwrap_or(0.0).max(0.0) as u64,
                keep_open: json_string_value(body, "keep_open").unwrap_or_default(),
            },
        )),
        ("POST", "/v1/inject/rm") => reply(remove(dir, &name())),
        ("POST", "/v1/inject/run") => reply(run(dir, cdp_port, &name())),
        ("GET", "/v1/grant") => ("200 OK", "application/json", crate::grant::list_json(dir)),
        ("POST", "/v1/grant") => reply(crate::grant::add(dir, cdp_port, body)),
        ("POST", "/v1/grant/rm") => reply(crate::grant::remove(dir, &name())),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_a_shell() {
        assert!(glob("https://www.figma.com/design/*", "https://www.figma.com/design/abc/x"));
        assert!(glob("*figma.com*", "https://figma.com/"));
        assert!(!glob("https://a.com/*", "https://b.com/x"));
        assert!(glob("a?c", "abc"));
    }

    #[test]
    fn settled_results_stop_the_retry() {
        assert!(settled("started in File (port 1996)"));
        assert!(settled("already running"));
        assert!(!settled("figma not ready"));
    }

    #[test]
    fn eval_results_read_value_and_exceptions() {
        let ok = r#"{"id":1,"result":{"result":{"type":"string","value":"ok"}}}"#;
        assert_eq!(eval_result(ok), "ok");
        let ex = r#"{"id":1,"result":{"result":{"type":"object"},"exceptionDetails":{"text":"Uncaught","exception":{"description":"ReferenceError: x"}}}}"#;
        assert_eq!(eval_result(ex), "error: ReferenceError: x");
    }

    #[test]
    fn scripts_round_trip_through_json() {
        let s = Script { name: "f".into(), matcher: "*".into(), js: "\"x\"\n".into(), retry_ms: 5, keep_open: String::new() };
        assert_eq!(Script::parse(&s.json()), Some(s));
    }
}

