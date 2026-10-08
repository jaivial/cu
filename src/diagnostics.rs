//! What browser actually ran, how, and how the protocol is being used.
//!
//! cu's benchmarks ran Chrome for Testing's headless shell while the launcher
//! defaulted to a distribution `chromium --headless=new`: two different
//! browsers behind one name. The launch record says which executable,
//! version and mode a daemon really has, and the counters show how the
//! DevTools protocol is used (how many commands, how many evaluations in the
//! page's own world, whether `Runtime.enable` was ever sent). Notes are plain
//! observations about the setup; nothing here rates or promises anything.
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::policy::{BrowserMode, BrowserPolicy};
use crate::server::{json_string, json_string_value};

/// The launch as decided and as the browser then described itself.
#[derive(Clone, Debug, Default)]
pub struct LaunchRecord {
    pub mode: String,
    pub requested: String,
    pub executable: String,
    pub resolved: String,
    pub headless: bool,
    pub headless_shell: bool,
    pub args: Vec<String>,
    pub started_unix: u64,
    /// From `/json/version` once DevTools answers.
    pub product: Option<String>,
    pub protocol: Option<String>,
    pub user_agent: Option<String>,
    pub v8: Option<String>,
    /// GL renderer the GPU process reported (`SystemInfo.getInfo`).
    pub gl_renderer: Option<String>,
    pub ready_ms: Option<u128>,
    pub error: Option<String>,
}

fn launch() -> &'static Mutex<Option<LaunchRecord>> {
    static LAUNCH: OnceLock<Mutex<Option<LaunchRecord>>> = OnceLock::new();
    LAUNCH.get_or_init(|| Mutex::new(None))
}

/// Record the launch decision before the browser is spawned.
pub fn record_launch(policy: &BrowserPolicy, args: &[String]) {
    let record = LaunchRecord {
        mode: policy.mode.name().into(),
        requested: policy.requested.clone(),
        executable: policy.executable.display().to_string(),
        resolved: policy.resolved.display().to_string(),
        headless: policy.headless,
        headless_shell: policy.headless_shell,
        // The profile path is the only argument that names the user's
        // machine layout; it is kept, there is no secret in it.
        args: args.to_vec(),
        started_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        ..Default::default()
    };
    *launch().lock().unwrap_or_else(|e| e.into_inner()) = Some(record);
}

/// Fill in what the running browser says about itself, log one line and
/// write `browser.json` next to `server.json`.
pub fn record_ready(
    data: &Path,
    version: Option<&str>,
    system_info: Option<&str>,
    ready: Duration,
) {
    let mut guard = launch().lock().unwrap_or_else(|e| e.into_inner());
    let Some(record) = guard.as_mut() else {
        return;
    };
    if let Some(version) = version {
        record.product = json_string_value(version, "Browser");
        record.protocol = json_string_value(version, "Protocol-Version");
        record.user_agent = json_string_value(version, "User-Agent");
        record.v8 = json_string_value(version, "V8-Version");
    }
    record.gl_renderer = system_info
        .and_then(|info| json_string_value(info, "glRenderer"))
        .filter(|gl| !gl.is_empty());
    record.ready_ms = Some(ready.as_millis());
    let record = record.clone();
    drop(guard);
    eprintln!(
        "cu: browser up in {} ms: mode={} headless={} executable={} ({}) product={} protocol={} gl={}",
        ready.as_millis(),
        record.mode,
        headless_name(&record),
        record.executable,
        record.resolved,
        record.product.as_deref().unwrap_or("unknown"),
        record.protocol.as_deref().unwrap_or("unknown"),
        record.gl_renderer.as_deref().unwrap_or("unknown"),
    );
    for note in notes(&record) {
        eprintln!("cu: note: {note}");
    }
    let _ = std::fs::write(data.join("browser.json"), launch_json(&record) + "\n");
}

/// Record that the launch failed, and why.
pub fn record_failure(error: &str) {
    if let Some(record) = launch().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        record.error = Some(error.into());
    }
}

fn headless_name(record: &LaunchRecord) -> &'static str {
    match (record.headless, record.headless_shell) {
        (_, true) => "shell",
        (true, false) => "new",
        (false, false) => "headful",
    }
}

/// Observations about the setup that change how normal the browser is.
fn notes(record: &LaunchRecord) -> Vec<String> {
    let mut notes = Vec::new();
    if record.mode == BrowserMode::FastTest.name() {
        notes.push(
            "fast-test profile: GPU, extensions, site isolation and several features are off; \
             meant for owned test sites"
                .into(),
        );
    }
    if record.headless_shell {
        notes.push("headless-shell build: a separate, older browser implementation".into());
    }
    if record
        .user_agent
        .as_deref()
        .is_some_and(|ua| ua.contains("HeadlessChrome"))
    {
        notes.push("the User-Agent says HeadlessChrome: sites can see headless mode".into());
    }
    if let Some(gl) = &record.gl_renderer {
        let lower = gl.to_ascii_lowercase();
        if lower.contains("swiftshader") || lower.contains("llvmpipe") || lower.contains("software")
        {
            notes.push(format!("software rendering ({gl}): no hardware GPU in use"));
        }
    } else if record.product.is_some() {
        notes.push("GPU renderer unknown (GPU process disabled or not reported)".into());
    }
    if !record.headless
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        notes.push("headful requested but no DISPLAY/WAYLAND_DISPLAY is set".into());
    }
    notes
}

fn opt(value: &Option<String>) -> String {
    value
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "null".into())
}

fn launch_json(record: &LaunchRecord) -> String {
    format!(
        "{{\"mode\":{},\"headless\":{},\"requested\":{},\"executable\":{},\"resolved\":{},\
         \"product\":{},\"protocol\":{},\"user_agent\":{},\"v8\":{},\"gl_renderer\":{},\
         \"ready_ms\":{},\"started_unix\":{},\"error\":{},\"args\":[{}],\"notes\":[{}]}}",
        json_string(&record.mode),
        json_string(headless_name(record)),
        json_string(&record.requested),
        json_string(&record.executable),
        json_string(&record.resolved),
        opt(&record.product),
        opt(&record.protocol),
        opt(&record.user_agent),
        opt(&record.v8),
        opt(&record.gl_renderer),
        record
            .ready_ms
            .map(|m| m.to_string())
            .unwrap_or_else(|| "null".into()),
        record.started_unix,
        opt(&record.error),
        record
            .args
            .iter()
            .map(|a| json_string(a))
            .collect::<Vec<_>>()
            .join(","),
        notes(record)
            .iter()
            .map(|n| json_string(n))
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// DevTools protocol counters for the daemon's whole life.
struct Protocol {
    commands: AtomicU64,
    errors: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
    events: AtomicU64,
    /// `Runtime.evaluate` with no `contextId`: runs in the page's own world.
    main_world_evals: AtomicU64,
    /// `Runtime.evaluate` / `callFunctionOn` into a context cu chose.
    isolated_evals: AtomicU64,
    isolated_worlds: AtomicU64,
    /// Should stay 0: cu never subscribes to `Runtime` events.
    runtime_enable: AtomicU64,
}

static PROTOCOL: Protocol = Protocol {
    commands: AtomicU64::new(0),
    errors: AtomicU64::new(0),
    total_us: AtomicU64::new(0),
    max_us: AtomicU64::new(0),
    events: AtomicU64::new(0),
    main_world_evals: AtomicU64::new(0),
    isolated_evals: AtomicU64::new(0),
    isolated_worlds: AtomicU64::new(0),
    runtime_enable: AtomicU64::new(0),
};

/// Count one DevTools command and how long its reply took.
pub fn record_command(method: &str, params: &str, elapsed: Duration, ok: bool) {
    let p = &PROTOCOL;
    p.commands.fetch_add(1, Ordering::Relaxed);
    if !ok {
        p.errors.fetch_add(1, Ordering::Relaxed);
    }
    let us = elapsed.as_micros().min(u64::MAX as u128) as u64;
    p.total_us.fetch_add(us, Ordering::Relaxed);
    p.max_us.fetch_max(us, Ordering::Relaxed);
    match method {
        "Runtime.evaluate" if params.contains("\"contextId\"") => {
            p.isolated_evals.fetch_add(1, Ordering::Relaxed);
        }
        "Runtime.evaluate" => {
            p.main_world_evals.fetch_add(1, Ordering::Relaxed);
        }
        "Runtime.callFunctionOn" => {
            p.isolated_evals.fetch_add(1, Ordering::Relaxed);
        }
        "Page.createIsolatedWorld" => {
            p.isolated_worlds.fetch_add(1, Ordering::Relaxed);
        }
        "Runtime.enable" => {
            p.runtime_enable.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// Count one DevTools event read off a connection.
pub fn record_event() {
    PROTOCOL.events.fetch_add(1, Ordering::Relaxed);
}

fn protocol_json() -> String {
    let p = &PROTOCOL;
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let commands = get(&p.commands);
    format!(
        "{{\"commands\":{commands},\"errors\":{},\"mean_us\":{},\"max_us\":{},\"events\":{},\
         \"main_world_evals\":{},\"isolated_evals\":{},\"isolated_worlds\":{},\"runtime_enable\":{}}}",
        get(&p.errors),
        get(&p.total_us).checked_div(commands).unwrap_or(0),
        get(&p.max_us),
        get(&p.events),
        get(&p.main_world_evals),
        get(&p.isolated_evals),
        get(&p.isolated_worlds),
        get(&p.runtime_enable),
    )
}

/// The `GET /v1/diagnostics` body.
pub fn json() -> String {
    let launch = launch()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(launch_json)
        .unwrap_or_else(|| "null".into());
    format!(
        "{{\"browser\":{launch},\"protocol\":{},\"note\":{}}}",
        protocol_json(),
        json_string("observations about this setup, not a measure of how sites will treat it")
    )
}
