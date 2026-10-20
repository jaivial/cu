//! `cu`: a local persistent-browser daemon and CLI for AI agents.
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cu::policy::{BrowserMode, BrowserPolicy};
use cu::server::{self, AppState};

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_CDP_PORT: u16 = 9222;
const DAEMON_PORT: u16 = 8810;
const DAEMON_CDP_BASE: u16 = 9362;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    // `--tab ID` runs any page command in another tab (a popup a click
    // opened, found with `cu tabs`); it is lifted out before dispatch.
    let mut tab: Option<String> = None;
    // `--new-tab` asks for a page of this invocation's own, instead of the
    // tab this session is already working in.
    let mut new_tab = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--tab" {
            if i + 1 >= args.len() {
                eprintln!("cu: --tab needs a tab id (see `cu tabs`)");
                std::process::exit(1);
            }
            tab = Some(args.remove(i + 1));
            args.remove(i);
        } else if args[i] == "--new-tab" {
            new_tab = true;
            args.remove(i);
        } else {
            i += 1;
        }
    }
    // Append `tab=` to a page command's path, keeping any query it has.
    let on_tab = |path: &str| match &tab {
        None => invocation_tab(path, new_tab),
        Some(id) if path.contains('?') => format!("{path}&tab={id}"),
        Some(id) => format!("{path}?tab={id}"),
    };
    let result = match args.first().map(String::as_str) {
        Some("start") => start(&args[1..]),
        Some("status") => request("GET", "/v1/status", None).map(|s| println!("{s}")),
        Some("challenge") => match args.get(1).map(String::as_str) {
            None => request("GET", &on_tab("/v1/challenge"), None).map(|s| println!("{s}")),
            Some("release") => request("POST", &on_tab("/v1/challenge/release"), Some("{}"))
                .map(|s| println!("{s}")),
            // `cu challenge wait --timeout S`: park until the person has
            // finished, instead of polling `cu challenge` in a loop.
            Some("wait") => wait_for_handoff(&args[2..], &on_tab),
            Some(other) => Err(format!(
                "unknown challenge command {other}; usage: cu challenge [release|wait]"
            )),
        },
        // `cu handoff`: the hand-off state -- is a person needed, on which
        // tab, for how long -- and the passes already recorded.
        Some("handoff") => match args.get(1).map(String::as_str) {
            None => request("GET", &on_tab("/v1/handoff"), None).map(|s| println!("{s}")),
            Some("wait") => wait_for_handoff(&args[2..], &on_tab),
            Some(other) => Err(format!(
                "unknown handoff command {other}; usage: cu handoff [wait]"
            )),
        },
        Some("diagnostics") => request("GET", "/v1/diagnostics", None).map(|s| println!("{s}")),
        Some("targets") => request("GET", "/v1/targets", None).map(|s| println!("{s}")),
        Some("navigate") => {
            let inject = match args.iter().position(|a| a == "--inject") {
                Some(i) => match args.get(i + 1) {
                    Some(v) => v.eq_ignore_ascii_case("on"),
                    None => {
                        eprintln!("cu: --inject needs on or off; defaulting to on");
                        true
                    }
                },
                None => true,
            };
            match args.get(1) {
                Some(url) => request(
                    "POST",
                    &on_tab("/v1/navigate"),
                    Some(&format!(
                        "{{\"url\":\"{}\",\"inject\":{}}}",
                        server::json_escape(url),
                        inject
                    )),
                )
                .map(|s| println!("{s}")),
                None => Err("usage: cu navigate <url> [--inject on|off] [--tab ID]".into()),
            }
        }
        Some("shot" | "screenshot") => shot_query(&args[1..]).and_then(|(query, file)| {
            request("GET", &on_tab(&format!("/v1/screenshot?{query}")), None)
                .and_then(|s| write_screenshot(&s, file.as_deref()))
        }),
        Some("snapshot") => request("GET", &on_tab("/v1/snapshot"), None).map(|s| println!("{s}")),
        // `cu inspect REF` / `cu inspect --selector CSS`: how to find an
        // element again and which component and source file render it.
        Some("inspect") => target_query(&args[1..], "usage: cu inspect REF | --selector CSS")
            .and_then(|(query, _)| request("GET", &on_tab(&format!("/v1/inspect?{query}")), None))
            .map(|s| println!("{s}")),
        // `cu mark REF "what is wrong" [--severity S]`: a QA note pinned to
        // an element, collected by `cu marks`.
        Some("mark") => mark(&args[1..], &on_tab),
        Some("marks") => match args.get(1).map(String::as_str) {
            None => request("GET", &on_tab("/v1/marks"), None).map(|s| println!("{s}")),
            Some("--md") => {
                request("GET", &on_tab("/v1/marks?format=md"), None).map(|s| print!("{s}"))
            }
            Some("clear") => {
                request("DELETE", &on_tab("/v1/marks"), Some("{}")).map(|s| println!("{s}"))
            }
            Some(other) => Err(format!(
                "unknown marks command {other}; usage: cu marks [--md | clear]"
            )),
        },
        // `cu console [--level L] [--grep S] [--limit N] [--since N] [--clear] [--json]`
        Some("console") => console_cmd(&args[1..], &on_tab),
        Some("text") => request("GET", &on_tab("/v1/text"), None).map(|s| println!("{s}")),
        Some("downloads") => request("GET", "/v1/downloads", None).map(|s| println!("{s}")),
        Some("tabs") => match args.get(1).map(String::as_str) {
            None => request("GET", "/v1/tabs", None).map(|s| println!("{s}")),
            Some("close") => match args.get(2) {
                Some(id) => request("DELETE", &format!("/v1/tabs/{id}"), Some("{}"))
                    .map(|s| println!("{s}")),
                None => Err("usage: cu tabs close ID".into()),
            },
            Some(other) => Err(format!(
                "unknown tabs command {other}; usage: cu tabs [close ID]"
            )),
        },
        Some("click") => match args.get(1) {
            Some(r) => request(
                "POST",
                &on_tab("/v1/click"),
                Some(&format!("{{\"ref\":{}}}", server::json_string(r))),
            )
            .map(|s| println!("{s}")),
            None => Err("usage: cu click REF".into()),
        },
        Some("type") => match (args.get(1), args.get(2)) {
            (Some(r), Some(text)) => request(
                "POST",
                &on_tab("/v1/type"),
                Some(&format!(
                    "{{\"ref\":{},\"text\":{},\"submit\":{}}}",
                    server::json_string(r),
                    server::json_string(text),
                    args.iter().any(|a| a == "--submit")
                )),
            )
            .map(|s| println!("{s}")),
            _ => Err("usage: cu type REF TEXT [--submit]".into()),
        },
        // `cu upload [REF] FILE...`: REF is the file input or the control
        // that opens the picker; without it, the page's only file input.
        // Paths are made absolute here, against the caller's directory.
        Some("upload") => {
            let (reference, files) = match args.get(1) {
                Some(r) if cu::actions::valid_ref(r) => (Some(r.clone()), &args[2..]),
                _ => (None, &args[1..]),
            };
            if files.is_empty() {
                Err("usage: cu upload [REF] FILE...".into())
            } else {
                let files = files
                    .iter()
                    .map(|f| {
                        fs::canonicalize(f)
                            .map(|p| server::json_string(&p.to_string_lossy()))
                            .map_err(|e| format!("{f}: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>();
                files.and_then(|files| {
                    let reference = reference
                        .map(|r| format!("\"ref\":{},", server::json_string(&r)))
                        .unwrap_or_default();
                    request(
                        "POST",
                        &on_tab("/v1/upload"),
                        Some(&format!("{{{reference}\"files\":[{}]}}", files.join(","))),
                    )
                    .map(|s| println!("{s}"))
                })
            }
        }
        // `cu act '[{"do":"click","ref":"e3"}, ...]'`, or the JSON on stdin.
        Some("act") => {
            let actions = match args.get(1) {
                Some(a) if a != "-" => Ok(a.clone()),
                _ => {
                    let mut input = String::new();
                    std::io::stdin()
                        .read_to_string(&mut input)
                        .map(|_| input)
                        .map_err(|e| e.to_string())
                }
            };
            actions.and_then(|actions| {
                request(
                    "POST",
                    &on_tab("/v1/act"),
                    Some(&format!("{{\"actions\":{}}}", actions.trim())),
                )
                .map(|s| println!("{s}"))
            })
        }
        Some("login") => {
            println!(
                "Open {} in a browser. Passwords go directly to the server.",
                login_url()
            );
            Ok(())
        }
        Some("session") => session(&args[1..]),
        Some("daemon") => daemon(&args[1..]),
        // The global `--tab` lifter has already taken a `--tab ID` out of
        // the arguments; the vault session save wants it.
        Some("vault") => vault(&args[1..], tab.clone()),
        Some("otp") => otp(&args[1..], tab.clone()),
        _ => {
            print_help();
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("cu: {error}");
        std::process::exit(1);
    }
}
/// `cu handoff wait [--timeout S]`: ask the daemon to watch the tab until the
/// hand-off resolves, and print one clear answer.
///
/// The daemon does the waiting (bounded, with its own backoff and a ceiling),
/// so this is a single request rather than a client-side poll loop: the
/// command cannot spin, and one wait costs one daemon thread instead of one
/// request per second. A wait that ends with the person still needed exits
/// non-zero, so a script or an agent can tell "still waiting" from "carry
/// on" without parsing the JSON.
fn wait_for_handoff(args: &[String], on_tab: &dyn Fn(&str) -> String) -> Result<(), String> {
    let mut timeout = 120.0f64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--timeout" => {
                i += 1;
                timeout = args
                    .get(i)
                    .ok_or("--timeout needs seconds")?
                    .parse()
                    .map_err(|_| "--timeout takes seconds")?;
            }
            flag => return Err(format!("unknown option {flag}")),
        }
        i += 1;
    }
    let body = request(
        "GET",
        &on_tab(&format!("/v1/challenge?wait={timeout}")),
        None,
    )?;
    println!("{body}");
    match server::json_string_value(&body, "timed_out").as_deref() {
        Some("true") => Err(format!(
            "still waiting for a person after {timeout:.0}s; the tab is not usable yet"
        )),
        _ => Ok(()),
    }
}

fn print_help() {
    println!(
        "cu — persistent browser for AI agents\n\n  cu start [--port N] [--data DIR] [--mode fast|compat] [--webgl] [--public-url URL]\n  cu status\n  cu diagnostics\n  cu targets\n  cu challenge [release|wait [--timeout S]]\n  cu handoff [wait [--timeout S]]\n  cu navigate URL\n  cu shot [FILE] [--png] [--width N] [--height N] [--scale F]\n          [--ref REF | --selector CSS] [--padding N]\n          [--annotate REFS|all] [--marks]\n  cu snapshot\n  cu inspect REF | --selector CSS\n  cu mark REF|--selector CSS NOTE [--severity blocker|major|minor|nit]\n  cu marks [--md | clear]\n  cu console [--level L] [--grep S] [--limit N] [--since N] [--clear] [--json]\n          levels: debug|verbose log info trace warn error exception csp network, all, LEVEL+\n  cu text\n  cu downloads\n  cu tabs [close ID]\n  cu click REF\n  cu type REF TEXT [--submit]\n  cu upload [REF] FILE...\n  cu act JSON_ACTIONS   (or JSON on stdin)\n  cu login\n  cu session save|load NAME [--context CTX]\n  cu daemon [--port N] [--cdp-port N] [--data DIR]  always-on multi-agent daemon\n  cu vault status|sessions|session save [--tab ID] [DOMAIN]|session forget DOMAIN\n  cu vault creds [QUERY]|cred get ID|cred rm ID|cred add DOMAIN LABEL|passwd\n  cu vault request DOMAIN [--reason TEXT] [--from NAME] [--wait] [--timeout S]\n  cu vault requests [STATUS]\n  cu otp request DOMAIN [--reason TEXT] [--from NAME] [--kind code|setup]\n          [--wait] [--timeout S]\n  cu otp wait ID [--timeout S] | cancel ID | list\n  cu otp capture --tab T [--domain D] [--request ID] [--credential ID] [--replace]\n  cu otp code DOMAIN | --credential ID\n\nPage commands take --tab ID to run in another tab (see `cu tabs`),\nor --new-tab to work in a page of their own instead of the session's."
    );
}

/// CDP port requested through the environment, if it parses.
fn cdp_port_from_env() -> Option<u16> {
    env::var("CU_CDP_PORT").ok().and_then(|p| p.parse().ok())
}

fn start(args: &[String]) -> Result<(), String> {
    let mut port = DEFAULT_PORT;
    let mut mode = BrowserMode::from_env()?;
    let mut data = PathBuf::from(env::var_os("CU_DATA_DIR").unwrap_or_else(|| ".cu".into()));
    let mut webgl = false;
    let mut public_url = env::var("CU_PUBLIC_URL")
        .ok()
        .filter(|u| !u.trim().is_empty());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|_| "invalid port")?;
            }
            "--data" => {
                i += 1;
                data = PathBuf::from(args.get(i).ok_or("--data needs a value")?);
            }
            "--mode" => {
                i += 1;
                mode = BrowserMode::parse(args.get(i).ok_or("--mode needs fast or compat")?)?;
            }
            "--compat" => mode = BrowserMode::Compatibility,
            "--webgl" => webgl = true,
            "--public-url" => {
                i += 1;
                public_url = Some(args.get(i).ok_or("--public-url needs a URL")?.clone());
            }
            flag => return Err(format!("unknown option {flag}")),
        }
        i += 1;
    }
    // Decided before anything is written, so a compatibility start that has
    // no full browser to run fails here instead of after the daemon is up.
    let mut policy = BrowserPolicy::resolve(mode)?;
    policy.webgl |= webgl;
    let public = public_url
        .as_deref()
        .map(server::parse_public_url)
        .transpose()?;
    cu::scheduler::configure(cu::scheduler::Budget::for_mode(mode));
    fs::create_dir_all(data.join("profiles/default")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("sessions")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("downloads")).map_err(|e| e.to_string())?;
    // Chromium refuses a relative download directory, so the path handed to
    // the browser must be absolute whatever way `cu` was started.
    let data = fs::canonicalize(&data).unwrap_or(data);
    let cdp_port = cdp_port_from_env().unwrap_or(DEFAULT_CDP_PORT);
    // One daemon per profile: a second one would clear the first one's
    // browser locks and overwrite its server.json.
    // Already served by a live daemon (e.g. the CLI auto-started one before
    // systemd got to it)? Then this invocation is done: exit cleanly so a
    // `Restart=on-failure` unit does not fight the running process.
    if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
        let _ = stream
            .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let mut reply = String::new();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream.read_to_string(&mut reply).is_ok() && reply.contains("running") {
            eprintln!("cu: a daemon already answers on {port}; nothing to do");
            return Ok(());
        }
    }
    cu::session::claim_owner(&data, port, cdp_port)?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("port {port}: {e} (is another cu instance on it?)"))?;
    let token = server::random_token();
    fs::write(
        data.join("server.json"),
        match &public {
            Some((url, _)) => format!(
                "{{\"port\":{port},\"token\":\"{token}\",\"public_url\":\"{}\"}}\n",
                server::json_escape(url)
            ),
            None => format!("{{\"port\":{port},\"token\":\"{token}\"}}\n"),
        },
    )
    .map_err(|e| e.to_string())?;
    // Listen and publish the token before the browser is up: `cu start` then
    // returns in single-digit milliseconds instead of blocking ~0.5 s on
    // Chromium, and the browser warms up while the agent reads its first tool
    // result. Actions that need the browser wait on the readiness signal.
    server::exit_on_signal(cdp_port);
    let browser = server::BrowserState::new();
    server::spawn_browser_thread(data.clone(), cdp_port, Arc::clone(&browser), policy);
    eprintln!("cu listening on http://127.0.0.1:{port}; browser DevTools on 127.0.0.1:{cdp_port}");
    if let Some((url, _)) = &public {
        eprintln!("cu login page published at {url}/login");
    }
    let state = Arc::new(AppState {
        data_dir: data,
        token,
        cdp_port,
        browser,
        public_host: public.map(|(_, host)| host),
        vault_host: env::var("CU_VAULT_HOST")
            .ok()
            .filter(|h| !h.trim().is_empty()),
        daemon: false,
    });
    server::serve(state, listener);
    Ok(())
}
/// Where a person opens the login form: the running daemon's public URL, else
/// `CU_PUBLIC_URL`, else the daemon's loopback port.
fn login_url() -> String {
    let data = env::var_os("CU_DATA_DIR").unwrap_or_else(|| ".cu".into());
    let config = fs::read_to_string(PathBuf::from(data).join("server.json")).unwrap_or_default();
    let public = server::json_value(&config, "public_url")
        .or_else(|| env::var("CU_PUBLIC_URL").ok())
        .and_then(|u| server::parse_public_url(&u).ok());
    if let Some((url, _)) = public {
        return format!("{url}/login");
    }
    let port = server::json_value(&config, "port")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    format!("http://127.0.0.1:{port}/login")
}
/// One CLI request. Resolution order: a server started with `CU_DATA_DIR`
/// (the legacy per-instance mode other agents' services use), the canonical
/// daemon (`~/.cu/daemon`), a legacy `./.cu` in this directory — and if
/// nothing answers, the daemon is started transparently and the request is
/// retried, so a plain `cu ...` works on a fresh boot without anyone having
/// run `cu daemon` by hand.
fn request(method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    if let Some(data) = env::var_os("CU_DATA_DIR") {
        let config = fs::read_to_string(PathBuf::from(&data).join("server.json"))
            .map_err(|_| "server not running (start it with `cu start`)")?;
        return request_config(&config, method, path, body);
    }
    let daemon_config = daemon_dir().join("server.json");
    let legacy_config = PathBuf::from(".cu").join("server.json");
    for source in [&daemon_config, &legacy_config] {
        if let Ok(config) = fs::read_to_string(source)
            && let Some((port, _)) = endpoint(&config)
            && TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            return request_config(&config, method, path, body);
        }
    }
    ensure_daemon()?;
    // The freshly started daemon rewrites server.json (new token) as it
    // binds; a config read racing that write would auth against a dead
    // token, so re-read and retry once.
    for _ in 0..2 {
        let config = fs::read_to_string(&daemon_config)
            .map_err(|_| "the daemon did not come up (see ~/.cu/daemon/daemon.log)")?;
        match request_config(&config, method, path, body) {
            Ok(reply) => return Ok(reply),
            Err(_) => thread::sleep(Duration::from_millis(300)),
        }
    }
    Err("the daemon is up but refused the request (see ~/.cu/daemon/daemon.log)".into())
}

/// The `(port, token)` a `server.json` describes, if it parses.
fn endpoint(config: &str) -> Option<(u16, String)> {
    let port = server::json_value(config, "port")?.parse().ok()?;
    let token = server::json_value(config, "token")?;
    Some((port, token))
}

/// One request against a server described by its `server.json` contents.
fn request_config(
    config: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String, String> {
    let port = server::json_value(config, "port").ok_or("invalid server state")?;
    let token = server::json_value(config, "token").ok_or("invalid server state")?;
    let mut stream = TcpStream::connect((
        "127.0.0.1",
        port.parse::<u16>().map_err(|_| "invalid port")?,
    ))
    .map_err(|e| e.to_string())?;
    let body = body.unwrap_or("");
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    let ok = response.starts_with("HTTP/1.1 2");
    let reply = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    if ok { Ok(reply) } else { Err(reply) }
}

/// The canonical daemon's data directory.
fn daemon_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CU_DAEMON_DIR") {
        return PathBuf::from(dir);
    }
    let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".cu").join("daemon")
}

/// The `(port, token)` of the canonical daemon, when one is configured and
/// this is not a legacy `CU_DATA_DIR` invocation.
fn daemon_config() -> Option<(u16, String)> {
    if env::var_os("CU_DATA_DIR").is_some() {
        return None;
    }
    let config = fs::read_to_string(daemon_dir().join("server.json")).ok()?;
    if server::json_value(&config, "daemon").as_deref() != Some("true") {
        return None;
    }
    let port = server::json_value(&config, "port")?.parse().ok()?;
    let token = server::json_value(&config, "token")?;
    Some((port, token))
}

/// A page command without `--tab`, talking to the daemon: work in the tab
/// this session already has, opening one only the first time (with
/// `--new-tab`, or when the session has none). `cu navigate` and the
/// `cu snapshot` an agent runs after it are two invocations of one session, so
/// the second reads the page the first opened; two sessions are two processes
/// and never share a page.
fn invocation_tab(path: &str, new_tab: bool) -> String {
    let Some((port, token)) = daemon_config() else {
        return path.to_string();
    };
    let owner = session_pid();
    let body = format!("{{\"owner_pid\":{owner},\"fresh\":{new_tab}}}");
    match request_daemon(port, &token, "POST", "/v1/tabs", Some(&body)) {
        Ok(reply) => {
            if let Some(id) = server::json_value(&reply, "id") {
                // A bare boolean, which `json_value` (string-only) cannot read.
                if reply.contains("\"reused\":true") {
                    eprintln!(
                        "cu: working in tab ({id}), the page this session already had; it is collected when the session exits"
                    );
                } else {
                    eprintln!(
                        "cu: working in a fresh tab ({id}); it is collected when this session exits"
                    );
                }
                return if path.contains('?') {
                    format!("{path}&tab={id}")
                } else {
                    format!("{path}?tab={id}")
                };
            }
            eprintln!("cu: the daemon answered without a tab id; working in its default page");
            path.to_string()
        }
        Err(e) => {
            // Never silent: a page command that could not get a tab of its own
            // would run in whatever page the daemon shows, which is how an
            // agent ends up reading another agent's tab.
            eprintln!("cu: could not open this session's tab ({e}); working in the default page");
            path.to_string()
        }
    }
}

/// The process this CLI call belongs to: the nearest ancestor that is not a
/// one-shot shell (`sh -c ...`, the wrapper an agent runs one command in) nor
/// another `cu`. Consecutive commands of one agent share it, so they share a
/// page; a different agent is a different process and gets its own.
fn session_pid() -> u32 {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let Some(parent) = parent_of(pid) else { break };
        if parent <= 1 {
            break;
        }
        if !is_transient_shell(parent) {
            return parent;
        }
        pid = parent;
    }
    std::os::unix::process::parent_id()
}

/// The parent pid in `/proc/<pid>/stat`, read the way the kernel writes it
/// (`comm` may hold spaces and parentheses, so the fields start after the last
/// `)`).
fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.rsplit_once(')')?.1;
    fields.split_whitespace().nth(1)?.parse().ok()
}

/// A shell running one command (`bash -c 'cu ...'`) or a wrapper around one:
/// these are per-invocation processes, so the session is above them.
fn is_transient_shell(pid: u32) -> bool {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let comm = comm.trim();
    match SHELLS.contains(&comm) {
        true => fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map(|line| {
                line.split('\0').any(|arg| {
                    arg.starts_with('-')
                        && !arg.starts_with("--")
                        && arg[1..].chars().any(|c| c == 'c')
                        && arg.len() <= 4
                })
            })
            .unwrap_or(false),
        false => WRAPPERS.contains(&comm),
    }
}

/// Shells and one-shot wrappers an agent (or a person) runs a command through.
const SHELLS: [&str; 8] = ["sh", "bash", "dash", "zsh", "ash", "ksh", "fish", "busybox"];
const WRAPPERS: [&str; 6] = ["timeout", "env", "nohup", "setsid", "nice", "stdbuf"];

fn request_daemon(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let body = body.unwrap_or("");
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    let ok = response.starts_with("HTTP/1.1 2");
    let reply = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    if ok { Ok(reply) } else { Err(reply) }
}

/// Spawn the daemon detached (own process group, output into its data
/// directory) and wait for it to answer. `cu daemon` itself runs under
/// systemd in normal operation; this is the fallback for a bare CLI call.
fn ensure_daemon() -> Result<(), String> {
    let dir = daemon_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // On a host with the `cu.service` unit installed, systemd is the owner:
    // starting it there survives reboots and logouts, so prefer it and fall
    // back to a detached spawn only where systemctl is not usable.
    if std::path::Path::new("/etc/systemd/system/cu.service").exists() {
        let _ = Command::new("sudo")
            .args(["-n", "systemctl", "start", "cu"])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(config) = fs::read_to_string(dir.join("server.json"))
                && let Some((port, _)) = endpoint(&config)
                && TcpStream::connect(("127.0.0.1", port)).is_ok()
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        }
        eprintln!("cu: systemd did not bring the daemon up; spawning it directly");
    }
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdout(log).stderr(log_err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| format!("could not spawn the daemon: {e}"))?;
    eprintln!("cu: the daemon was not running; starting it (~/.cu/daemon)...");
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(config) = fs::read_to_string(dir.join("server.json"))
            && server::json_value(&config, "port").is_some()
            && TcpStream::connect((
                "127.0.0.1",
                server::json_value(&config, "port")
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(0),
            ))
            .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("the daemon did not answer within 90 s (see ~/.cu/daemon/daemon.log)".into())
}

/// `cu shot [FILE] [--png] [--quality N] [--width N] [--height N] [--scale F]
/// [--ref REF | --selector CSS] [--padding N]` as a `/v1/screenshot` query and
/// the file to write. A FILE ending in `.png` asks for PNG.
fn shot_query(args: &[String]) -> Result<(String, Option<String>), String> {
    const USAGE: &str = "usage: cu shot [FILE] [--png] [--quality N] [--width N] [--height N] [--scale F] [--ref REF | --selector CSS] [--padding N] [--annotate REFS|all] [--marks]";
    let mut file = None;
    let mut png = false;
    let mut pairs: Vec<(&str, String)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let key = match args[i].as_str() {
            "--png" => {
                png = true;
                i += 1;
                continue;
            }
            "--quality" => "quality",
            "--width" => "width",
            "--height" => "height",
            "--scale" => "scale",
            "--ref" => "ref",
            "--selector" => "selector",
            "--padding" => "padding",
            "--annotate" => "annotate",
            "--marks" => {
                pairs.push(("marks", "1".into()));
                i += 1;
                continue;
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}; {USAGE}"));
            }
            path if file.is_none() => {
                file = Some(path.to_string());
                i += 1;
                continue;
            }
            _ => return Err(USAGE.into()),
        };
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{} needs a value; {USAGE}", args[i]))?;
        pairs.push((key, url_encode(value)));
        i += 2;
    }
    png |= file
        .as_deref()
        .is_some_and(|f| f.to_ascii_lowercase().ends_with(".png"));
    let mut query = format!("format={}", if png { "png" } else { "jpeg" });
    for (key, value) in pairs {
        query.push_str(&format!("&{key}={value}"));
    }
    Ok((query, file))
}

/// `REF` or `--selector CSS` at the head of `args`, as a `ref=`/`selector=`
/// query, and the arguments after it.
fn target_query<'a>(args: &'a [String], usage: &str) -> Result<(String, &'a [String]), String> {
    match args.first().map(String::as_str) {
        Some("--selector") => match args.get(1) {
            Some(css) => Ok((format!("selector={}", url_encode(css)), &args[2..])),
            None => Err(usage.into()),
        },
        Some(r) if cu::actions::valid_ref(r) => Ok((format!("ref={r}"), &args[1..])),
        _ => Err(usage.into()),
    }
}

fn mark(args: &[String], on_tab: &dyn Fn(&str) -> String) -> Result<(), String> {
    const USAGE: &str =
        "usage: cu mark REF|--selector CSS NOTE [--severity blocker|major|minor|nit]";
    let (target, rest) = target_query(args, USAGE)?;
    let mut note = None;
    let mut severity = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--severity" => {
                severity = Some(rest.get(i + 1).ok_or(USAGE)?.clone());
                i += 2;
            }
            text if note.is_none() => {
                note = Some(text.to_string());
                i += 1;
            }
            _ => return Err(USAGE.into()),
        }
    }
    let note = note.ok_or(USAGE)?;
    let (key, value) = target.split_once('=').unwrap_or_default();
    let value = server::url_decode(value);
    let severity = severity
        .map(|s| format!(",\"severity\":{}", server::json_string(&s)))
        .unwrap_or_default();
    request(
        "POST",
        &on_tab("/v1/marks"),
        Some(&format!(
            "{{\"{key}\":{},\"note\":{}{severity}}}",
            server::json_string(&value),
            server::json_string(&note)
        )),
    )
    .map(|s| println!("{s}"))
}

/// Percent-encode a query value (a CSS selector has `#`, `&`, spaces...).
fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
fn write_screenshot(response: &str, path: Option<&str>) -> Result<(), String> {
    // The format is chosen by the server, so the client just saves what came
    // back under the name that matches it.
    let (field, default_path) = if response.contains("\"format\":\"png\"") {
        ("png_base64", "screenshot.png")
    } else {
        ("jpeg_base64", "screenshot.jpg")
    };
    // A failed capture comes back as an error object; show the server's
    // words instead of a generic "no image" that hides why.
    let image = server::json_value(response, field).ok_or_else(|| {
        server::json_value(response, "error")
            .unwrap_or_else(|| "server did not return an image".into())
    })?;
    fs::write(path.unwrap_or(default_path), server::decode_base64(&image)?)
        .map_err(|e| e.to_string())
}
fn session(args: &[String]) -> Result<(), String> {
    let action = args
        .first()
        .map(String::as_str)
        .ok_or("usage: cu session save|load NAME")?;
    if !matches!(action, "save" | "load") {
        return Err("usage: cu session save|load NAME".into());
    }
    let name = args
        .get(1)
        .ok_or("usage: cu session save|load NAME [--context CTX]")?;
    let mut path = if action == "save" {
        format!("/v1/session/{name}")
    } else {
        format!("/v1/session/{name}/load")
    };
    // `--context CTX`: the cookie jar of a named context (`?context=CTX`)
    // instead of the persistent profile.
    if let Some(i) = args.iter().position(|a| a == "--context") {
        let context = args.get(i + 1).ok_or("--context needs a context name")?;
        path.push_str(&format!("?context={context}"));
    }
    request("POST", &path, Some("{}")).map(|s| println!("{s}"))
}

/// Canonical always-on daemon: one process, one persistent headless Chrome,
/// one tab per CLI invocation. The port defaults to 8810 and walks upward
/// while something else holds it; the DevTools port is picked free the same
/// way, so two daemons (this one and any legacy instance) never collide.
fn daemon(args: &[String]) -> Result<(), String> {
    let mut port = DAEMON_PORT;
    let mut cdp_port = None;
    let mut data = daemon_dir();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|_| "invalid port")?;
            }
            "--cdp-port" => {
                i += 1;
                cdp_port = Some(
                    args.get(i)
                        .ok_or("--cdp-port needs a value")?
                        .parse()
                        .map_err(|_| "invalid port")?,
                );
            }
            "--data" => {
                i += 1;
                data = PathBuf::from(args.get(i).ok_or("--data needs a value")?);
            }
            flag => return Err(format!("unknown option {flag}")),
        }
        i += 1;
    }
    let mode = BrowserMode::from_env()?;
    // Headless is the point of the daemon: no display is looked for, no
    // window is opened, whatever the host has (`CU_HEADLESS` said it too).
    let mut policy = BrowserPolicy::resolve(mode)?;
    // `CU_HEADLESS=0` opts the daemon into a windowed browser on a virtual
    // display (`CU_DISPLAY`): closer to a real browser, fewer bot checks.
    let windowed = std::env::var("CU_HEADLESS")
        .map(|v| v.trim() == "0")
        .unwrap_or(false);
    if !(windowed && policy.display.is_some() && !policy.headless) {
        policy.headless = true;
        policy.display = None;
    }
    cu::scheduler::configure(cu::scheduler::Budget::for_mode(mode));
    fs::create_dir_all(data.join("profiles/default")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("sessions")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("downloads")).map_err(|e| e.to_string())?;
    // The vault must open (key file writable, schema sound) before anything
    // listens: a daemon that cannot keep its vault honest should not start.
    cu::vault::Vault::open(&data).map_err(|e| format!("vault: {e}"))?;
    let data = fs::canonicalize(&data).unwrap_or(data);
    let port = free_port_from(port);
    let cdp_port = match cdp_port {
        Some(port) => port,
        None => free_port_from(DAEMON_CDP_BASE),
    };
    // Already served by a live daemon (e.g. the CLI auto-started one before
    // systemd got to it)? Then this invocation is done: exit cleanly so a
    // `Restart=on-failure` unit does not fight the running process.
    if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
        let _ = stream
            .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let mut reply = String::new();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream.read_to_string(&mut reply).is_ok() && reply.contains("running") {
            eprintln!("cu: a daemon already answers on {port}; nothing to do");
            return Ok(());
        }
    }
    cu::session::claim_owner(&data, port, cdp_port)?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("port {port}: {e} (is another cu instance on it?)"))?;
    let token = server::random_token();
    fs::write(
        data.join("server.json"),
        format!("{{\"port\":{port},\"token\":\"{token}\",\"daemon\":\"true\"}}\n"),
    )
    .map_err(|e| e.to_string())?;
    server::exit_on_signal(cdp_port);
    let browser = server::BrowserState::new();
    server::spawn_browser_thread(data.clone(), cdp_port, Arc::clone(&browser), policy);
    server::spawn_tab_gc(cdp_port);
    let vault_host = env::var("CU_VAULT_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty());
    eprintln!(
        "cu daemon on http://127.0.0.1:{port}; browser DevTools on 127.0.0.1:{cdp_port}; vault at {}{}",
        data.join("vault.db").display(),
        vault_host
            .as_deref()
            .map(|h| format!("; vault host {h}"))
            .unwrap_or_default()
    );
    let state = Arc::new(server::AppState {
        data_dir: data,
        token,
        cdp_port,
        browser,
        public_host: None,
        vault_host,
        daemon: true,
    });
    server::serve(state, listener);
    Ok(())
}

/// The first free port at or after `from` (25 candidates, then give up with
/// the original, whose bind will produce the honest error).
fn free_port_from(from: u16) -> u16 {
    for candidate in from..from.saturating_add(25) {
        if TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
    from
}

/// `cu otp`: one-time codes — hand-offs a person answers from the phone,
/// setup-QR capture, and codes minted from a stored seed.
fn otp(args: &[String], tab: Option<String>) -> Result<(), String> {
    let usage = "usage: cu otp request DOMAIN [--reason TEXT] [--from NAME] [--kind code|setup] [--wait] [--timeout S] | wait ID [--timeout S] | cancel ID | list | capture --tab T [--domain D] [--request ID] [--credential ID] [--replace] | code DOMAIN | code --credential ID";
    let action = args.first().ok_or(usage)?;
    match action.as_str() {
        "request" => {
            let domain = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: cu otp request DOMAIN [--reason TEXT] [--from NAME] [--kind code|setup] [--wait] [--timeout S]")?;
            let mut reason = String::new();
            let mut from = String::new();
            let mut kind = "code".to_string();
            let mut wait = false;
            let mut timeout = 600.0f64;
            let mut rest = &args[2..];
            while let [flag, value, tail @ ..] = rest {
                match flag.as_str() {
                    "--reason" => {
                        reason = value.clone();
                        rest = tail;
                    }
                    "--from" => {
                        from = value.clone();
                        rest = tail;
                    }
                    "--kind" => {
                        kind = value.clone();
                        rest = tail;
                    }
                    "--timeout" => {
                        timeout = value.parse().map_err(|_| "--timeout takes seconds")?;
                        rest = tail;
                    }
                    _ => break,
                }
            }
            if rest.first().map(String::as_str) == Some("--wait") {
                wait = true;
            }
            let created = request(
                "POST",
                "/v1/vault/otp/requests",
                Some(&format!(
                    "{{\"domain\":{},\"reason\":{},\"kind\":{},\"requested_by\":{}}}",
                    server::json_string(domain),
                    server::json_string(&reason),
                    server::json_string(&kind),
                    server::json_string(&from),
                )),
            )?;
            println!("{created}");
            if wait {
                let id = server::json_string_value(&created, "id")
                .or_else(|| server::json_number(&created, "id").map(|n| (n as i64).to_string()))
                .ok_or("no id in reply")?;
                otp_wait(&id, timeout)?;
            }
            Ok(())
        }
        "wait" => {
            let id = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: cu otp wait ID [--timeout S]")?;
            let mut timeout = 600.0f64;
            let mut rest = &args[2..];
            while let [flag, value, tail @ ..] = rest {
                if flag.as_str() == "--timeout" {
                    timeout = value.parse().map_err(|_| "--timeout takes seconds")?;
                    rest = tail;
                } else {
                    break;
                }
            }
            otp_wait(id, timeout)
        }
        "cancel" => {
            let id = args.get(1).ok_or("usage: cu otp cancel ID")?;
            request(
                "DELETE",
                &format!("/v1/vault/otp/requests/{id}"),
                Some("{}"),
            )
            .map(|s| println!("{s}"))
        }
        "list" => request("GET", "/v1/vault/otp/requests", None).map(|s| println!("{s}")),
        "capture" => {
            // Replacing a credential's live TOTP seed needs this explicit
            // flag - the operator's confirmation, never an inference.
            let force = args.iter().any(|a| a == "--replace");
            let cleaned: Vec<String> = args[1..]
                .iter()
                .filter(|a| a.as_str() != "--replace")
                .cloned()
                .collect();
            let mut domain = String::new();
            let mut request_id = String::new();
            let mut credential_id = String::new();
            let mut rest = &cleaned[..];
            while let [flag, value, tail @ ..] = rest {
                match flag.as_str() {
                    "--domain" => {
                        domain = value.clone();
                        rest = tail;
                    }
                    "--request" => {
                        request_id = value.clone();
                        rest = tail;
                    }
                    "--credential" => {
                        credential_id = value.clone();
                        rest = tail;
                    }
                    _ => return Err(format!("unknown option {flag}")),
                }
            }
            if request_id.is_empty() && domain.is_empty() {
                return Err("cu otp capture needs --domain D or --request ID".into());
            }
            if tab.is_none() {
                return Err(
                    "cu otp capture needs --tab T naming the tab on the service's setup page"
                        .into(),
                );
            }
            let path = if request_id.is_empty() {
                "/v1/vault/otp/capture".to_string()
            } else {
                format!("/v1/vault/otp/requests/{request_id}/capture")
            };
            let path = match &tab {
                Some(id) => format!("{path}?tab={id}"),
                None => path,
            };
            let mut fields = Vec::new();
            if force {
                fields.push("\"force\":true".to_string());
            }
            if !domain.is_empty() {
                fields.push(format!("\"domain\":{}", server::json_string(&domain)));
            }
            if !credential_id.is_empty() {
                fields.push(format!(
                    "\"credential_id\":{}",
                    server::json_string(&credential_id)
                ));
            }
            let body = format!("{{{}}}", fields.join(","));
            // The reply names the issuer and account the seed was stored
            // for; the seed itself never leaves the vault.
            request("POST", &path, Some(&body)).map(|s| println!("{s}"))
        }
        "code" => {
            let domain = args.get(1).filter(|a| !a.starts_with("--"));
            let mut credential_id = String::new();
            let mut rest = if domain.is_some() {
                &args[2..]
            } else {
                &args[1..]
            };
            while let [flag, value, tail @ ..] = rest {
                if flag.as_str() == "--credential" {
                    credential_id = value.clone();
                    rest = tail;
                } else {
                    return Err(format!("unknown option {flag}"));
                }
            }
            let body = if !credential_id.is_empty() {
                format!(
                    "{{\"credential_id\":{}}}",
                    server::json_string(&credential_id)
                )
            } else if let Some(domain) = domain {
                format!("{{\"domain\":{}}}", server::json_string(domain))
            } else {
                return Err("usage: cu otp code DOMAIN | code --credential ID".into());
            };
            request("POST", "/v1/vault/otp/code", Some(&body)).map(|s| println!("{s}"))
        }
        _ => Err(usage.into()),
    }
}

/// Park on a hand-off until the person answers, it expires or the local
/// budget runs out; the server-side wait caps each poll, so loop it.
///
/// Collection is a two-step hand-over: the answer arrives with a one-owner
/// collect token, and only the ACK - or the vault's collect window closing
/// - wipes the stored code. The code is printed only after the ACK lands:
/// a code the vault still holds for redelivery never reaches this
/// transcript. A cut connection is retried by simply running
/// `cu otp wait ID` again - the vault re-delivers to the same owner token
/// as long as the window is open... except this process is gone by then, so
/// the retry loop below covers the immediate failures instead.
fn otp_wait(id: &str, timeout: f64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout);
    let mut collect_token = String::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(format!("no answer after {timeout:.0}s; giving up"));
        }
        let mut path = format!("/v1/vault/otp/requests/{id}?wait={}", left.as_secs().max(1));
        if !collect_token.is_empty() {
            path.push_str(&format!("&collect_token={collect_token}"));
        }
        let body = request("GET", &path, None)?;
        match server::json_string_value(&body, "status").as_deref() {
            Some("pending") => continue,
            Some("collecting") => {
                if let Some(token) = server::json_string_value(&body, "collect_token") {
                    collect_token = token;
                }
                if server::json_string_value(&body, "response").is_none() {
                    if collect_token.is_empty() {
                        return Err("this hand-off is being collected by another consumer".into());
                    }
                    continue;
                }
                // ACK first, print after: the vault wipes the stored code
                // only once delivery is confirmed. Bounded immediate retries
                // cover a cut connection; after those, fail WITHOUT printing
                // so the code never leaves the vault unacknowledged.
                let ack_body = format!(
                    "{{\"collect_token\":{}}}",
                    server::json_string(&collect_token)
                );
                let ack_path = format!("/v1/vault/otp/requests/{id}/ack");
                let mut acked = request("POST", &ack_path, Some(&ack_body)).is_ok();
                for _ in 0..2 {
                    if acked {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    acked = request("POST", &ack_path, Some(&ack_body)).is_ok();
                }
                if !acked {
                    return Err(format!(
                        "could not confirm delivery; the code stays in the vault - \
                         run `cu otp wait {id}` again within its collection window to retry"
                    ));
                }
                let response = server::json_string_value(&body, "response").unwrap_or_default();
                println!(
                    "{{\"id\":{},\"status\":\"collected\",\"response\":{}}}",
                    server::json_string(id),
                    server::json_string(&response),
                );
                return Ok(());
            }
            Some("collected") => return Ok(()),
            Some(other) => return Err(format!("hand-off ended as {other}")),
            None => return Err("unexpected reply from the daemon".into()),
        }
    }
}

/// `cu vault`: the credential and session store, from the CLI.
fn vault(args: &[String], tab: Option<String>) -> Result<(), String> {
    let usage = "usage: cu vault status | sessions | session save [--tab ID] [DOMAIN] | session forget DOMAIN | creds [QUERY] | cred get ID | cred rm ID | cred add DOMAIN LABEL (fields JSON on stdin) | passwd | request DOMAIN [--reason TEXT] [--from NAME] [--wait] [--timeout S] | requests [STATUS]";
    let action = args.first().ok_or(usage)?;
    match action.as_str() {
        "status" => request("GET", "/v1/vault/status", None).map(|s| println!("{s}")),
        "sessions" => request("GET", "/v1/vault/sessions", None).map(|s| println!("{s}")),
        "session" => match args.get(1).map(String::as_str) {
            Some("save") => {
                let mut domain = None;
                for arg in args[2..].iter() {
                    if arg.as_str() != "--tab" {
                        domain = Some(arg.to_string());
                    }
                }
                let mut path = "/v1/vault/sessions/save".to_string();
                if let Some(tab) = tab {
                    path.push_str(&format!("?tab={}", url_encode(&tab)));
                }
                if let Some(domain) = domain {
                    path.push_str(&if path.contains('?') { "&" } else { "?" });
                    path.push_str(&format!("domain={}", url_encode(&domain)));
                }
                request("POST", &path, Some("{}")).map(|s| println!("{s}"))
            }
            Some("forget") => {
                let domain = args.get(2).ok_or("usage: cu vault session forget DOMAIN")?;
                request(
                    "DELETE",
                    &format!("/v1/vault/sessions/{}", url_encode(domain)),
                    Some("{}"),
                )
                .map(|s| println!("{s}"))
            }
            _ => Err(
                "usage: cu vault session save [--tab ID] [DOMAIN] | session forget DOMAIN".into(),
            ),
        },
        "creds" => {
            let query = args
                .get(1)
                .map(|q| format!("?query={}", url_encode(q)))
                .unwrap_or_default();
            request("GET", &format!("/v1/vault/credentials{query}"), None).map(|s| println!("{s}"))
        }
        "cred" => match args.get(1).map(String::as_str) {
            Some("get") => {
                let id = args.get(2).ok_or("usage: cu vault cred get ID")?;
                request("GET", &format!("/v1/vault/credentials/{id}"), None)
                    .map(|s| println!("{s}"))
            }
            Some("rm") => {
                let id = args.get(2).ok_or("usage: cu vault cred rm ID")?;
                request("DELETE", &format!("/v1/vault/credentials/{id}"), Some("{}"))
                    .map(|s| println!("{s}"))
            }
            Some("add") => {
                let domain = args.get(2).ok_or("usage: cu vault cred add DOMAIN LABEL")?;
                let label = args.get(3).ok_or("usage: cu vault cred add DOMAIN LABEL")?;
                let mut fields = String::new();
                std::io::stdin()
                    .read_to_string(&mut fields)
                    .map_err(|e| e.to_string())?;
                let fields = fields.trim();
                if !fields.starts_with('[') {
                    return Err("fields must be a JSON array on stdin".into());
                }
                request(
                    "POST",
                    "/v1/vault/credentials",
                    Some(&format!(
                        "{{\"domain\":{},\"label\":{},\"fields\":{fields}}}",
                        server::json_string(domain),
                        server::json_string(label)
                    )),
                )
                .map(|s| println!("{s}"))
            }
            _ => Err("usage: cu vault cred get ID | rm ID | add DOMAIN LABEL".into()),
        },
        "requests" => {
            let status = args.get(1).map(|s| format!("?status={}", url_encode(s)));
            let path = match status {
                Some(q) => format!("/v1/vault/requests{q}"),
                None => "/v1/vault/requests".to_string(),
            };
            request("GET", &path, None).map(|s| println!("{s}"))
        }
        "request" => {
            let domain = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: cu vault request DOMAIN [--reason TEXT] [--from NAME] [--wait] [--timeout S]")?;
            let mut reason = String::new();
            let mut from = String::new();
            let mut wait = false;
            let mut timeout = 600.0f64;
            let mut rest = &args[2..];
            while let [flag, value, tail @ ..] = rest {
                match flag.as_str() {
                    "--reason" => {
                        reason = value.clone();
                        rest = tail;
                    }
                    "--from" => {
                        from = value.clone();
                        rest = tail;
                    }
                    "--timeout" => {
                        timeout = value.parse().map_err(|_| "--timeout takes seconds")?;
                        rest = tail;
                    }
                    _ => break,
                }
            }
            if rest.first().map(String::as_str) == Some("--wait") {
                wait = true;
            }
            let created = request(
                "POST",
                "/v1/vault/requests",
                Some(&format!(
                    "{{\"domain\":{},\"reason\":{},\"requested_by\":{}}}",
                    server::json_string(domain),
                    server::json_string(&reason),
                    server::json_string(&from)
                )),
            )?;
            let id = server::json_value(&created, "id")
                .and_then(|v| v.parse::<i64>().ok())
                .ok_or("daemon did not return a request id")?;
            println!("{created}");
            if !wait {
                return Ok(());
            }
            // Poll until a person decides (or the request expires): what
            // comes back on approval is the credential with its values —
            // the only path a secret ever takes to an agent.
            let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout);
            loop {
                if std::time::Instant::now() > deadline {
                    return Err(format!("request {id} is still pending after {timeout:.0}s"));
                }
                thread::sleep(Duration::from_secs(2));
                let body = request("GET", &format!("/v1/vault/requests/{id}"), None)?;
                match server::json_string_value(&body, "status")
                    .unwrap_or_default()
                    .as_str()
                {
                    "approved" => {
                        println!("{body}");
                        return Ok(());
                    }
                    "denied" | "expired" => {
                        println!("{body}");
                        let status = server::json_string_value(&body, "status").unwrap_or_default();
                        return Err(format!("request {id} was {status}"));
                    }
                    _ => continue,
                }
            }
        }
        "passwd" => {
            eprint!("new master password: ");
            let mut new = String::new();
            std::io::stdin()
                .read_line(&mut new)
                .map_err(|e| e.to_string())?;
            let new = new.trim().to_string();
            if new.len() < 8 {
                return Err("the master password must be at least 8 characters".into());
            }
            eprint!("old master password (empty on first run): ");
            let mut old = String::new();
            std::io::stdin()
                .read_line(&mut old)
                .map_err(|e| e.to_string())?;
            request(
                "POST",
                "/v1/vault/passwd",
                Some(&format!(
                    "{{\"old\":{},\"new\":{}}}",
                    server::json_string(old.trim()),
                    server::json_string(&new)
                )),
            )
            .map(|s| println!("{s}"))
        }
        _ => Err(usage.into()),
    }
}

#[cfg(test)]
mod tests {
    use cu::server::*;

    #[test]
    fn json_values_are_extracted() {
        assert_eq!(
            json_value(r#"{"url":"https://example.test"}"#, "url").as_deref(),
            Some("https://example.test")
        );
    }
    #[test]
    fn login_page_has_password_input_but_no_value() {
        assert!(login_page().contains("type=password"));
        assert!(!login_page().contains("password="));
    }
    #[test]
    fn base64_decodes_png_prefix() {
        assert_eq!(decode_base64("iVBORw0KGgo=").unwrap(), b"\x89PNG\r\n\x1a\n");
    }
    #[test]
    fn names_cannot_escape_profile_directory() {
        assert!(!valid_name("../secrets"));
        assert!(valid_name("work_1"));
    }
}

/// `cu console`: the page's console output, filtered by level.
fn console_cmd(args: &[String], on_tab: &dyn Fn(&str) -> String) -> Result<(), String> {
    let usage = "usage: cu console [--level L[,L]|warn+|all] [--grep TEXT] [--limit N] [--since N] [--clear] [--json]";
    let (mut level, mut grep, mut limit, mut since) =
        (String::new(), String::new(), String::new(), String::new());
    let (mut clear, mut json) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--level" | "-l" => level = it.next().ok_or(usage)?.clone(),
            "--grep" | "-g" => grep = it.next().ok_or(usage)?.clone(),
            "--limit" | "-n" => limit = it.next().ok_or(usage)?.clone(),
            "--since" => since = it.next().ok_or(usage)?.clone(),
            "--clear" => clear = true,
            "--json" => json = true,
            other => return Err(format!("unknown option {other}; {usage}")),
        }
    }
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b',' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    let mut q = format!(
        "level={}&grep={}&limit={}&since={}",
        enc(&level),
        enc(&grep),
        enc(&limit),
        enc(&since)
    );
    if clear {
        q.push_str("&clear=1");
    }
    if !json {
        q.push_str("&format=text");
    }
    request("GET", &on_tab(&format!("/v1/console?{q}")), None).map(|s| println!("{}", s.trim_end()))
}
