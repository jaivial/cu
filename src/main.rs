//! `cu`: a local persistent-browser daemon and CLI for AI agents.
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use cu::server::{self, AppState};

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_CDP_PORT: u16 = 9222;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    // `--tab ID` runs any page command in another tab (a popup a click
    // opened, found with `cu tabs`); it is lifted out before dispatch.
    let mut tab: Option<String> = None;
    let mut context: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, slot, hint) = match args[i].as_str() {
            "--tab" => ("--tab", &mut tab, "a tab id (see `cu tabs`)"),
            "--context" => ("--context", &mut context, "a context name"),
            _ => {
                i += 1;
                continue;
            }
        };
        if i + 1 >= args.len() {
            eprintln!("cu: {flag} needs {hint}");
            std::process::exit(1);
        }
        *slot = Some(args.remove(i + 1));
        args.remove(i);
    }
    // Append `tab=`/`context=` to a page command's path, keeping any query it
    // has: `--tab ID` runs in another tab, `--context NAME` in an isolated
    // browser context of the same Chrome (giving both is an error server-side).
    let on_tab = |path: &str| {
        let mut pick = Vec::new();
        if let Some(id) = &tab {
            pick.push(format!("tab={id}"));
        }
        if let Some(name) = &context {
            pick.push(format!("context={name}"));
        }
        if pick.is_empty() {
            path.to_string()
        } else if path.contains('?') {
            format!("{path}&{}", pick.join("&"))
        } else {
            format!("{path}?{}", pick.join("&"))
        }
    };
    let result = match args.first().map(String::as_str) {
        Some("start") => start(&args[1..]),
        Some("status") => status(&args[1..]),
        Some("navigate") => match args.get(1) {
            Some(url) => request(
                "POST",
                &on_tab("/v1/navigate"),
                Some(&format!("{{\"url\":\"{}\"}}", server::json_escape(url))),
            )
            .map(|s| println!("{s}")),
            None => Err("usage: cu navigate <url> [--tab ID]".into()),
        },
        Some("shot") => request("GET", &on_tab("/v1/screenshot?format=jpeg"), None)
            .and_then(|s| write_screenshot(&s, args.get(1).map(String::as_str))),
        Some("snapshot") => request("GET", &on_tab("/v1/snapshot"), None).map(|s| println!("{s}")),
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
        Some("tab") => tab_cmd(&args[1..]),
        Some("context") => context_cmd(&args[1..]),
        Some("lease") => lease_cmd(&args[1..]),
        Some("batch") => batch(&args[1..]),
        Some("login") => {
            println!(
                "Open http://127.0.0.1:{DEFAULT_PORT}/login in a browser. Passwords go directly to the server."
            );
            Ok(())
        }
        Some("session") => session(&args[1..]),
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
/// `cu status [--short]`: full JSON, or one compact line for scripts.
fn status(args: &[String]) -> Result<(), String> {
    let s = request("GET", "/v1/status", None)?;
    if args.first().map(String::as_str) == Some("--short") {
        // JSON strings come back quoted; `true`/numbers are bare literals.
        let field = |k: &str| {
            server::json_value(&s, k)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                let needle = format!("\"{k}\":");
                s.split_once(&needle)
                    .map(|(_, rest)| {
                        rest.trim_start()
                            .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                            .next()
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default()
            })
        };
        println!(
            "running={} browser={} leases={}",
            field("running"),
            field("browser"),
            field("leases")
        );
    } else {
        println!("{s}");
    }
    Ok(())
}

/// Parse `--lease N` / `--label NAME` plus one positional argument.
fn lease_and_label(args: &[String]) -> Result<(String, String, Option<String>), String> {
    let mut positional = String::new();
    let mut lease = None;
    let mut label = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lease" => {
                i += 1;
                lease = Some(args.get(i).ok_or("--lease needs seconds")?.clone());
            }
            "--label" => {
                i += 1;
                label = args.get(i).ok_or("--label needs a name")?.clone();
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            other => positional = other.to_string(),
        }
        i += 1;
    }
    Ok((positional, label, lease))
}

/// `cu tab open [URL] [--lease S] [--label NAME]`, `close ID`, `renew ID [S]`.
fn tab_cmd(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("open") => {
            let (url, label, lease) = lease_and_label(&args[1..])?;
            let mut body = format!(
                "{{\"url\":{},\"label\":{}}}",
                server::json_string(&url),
                server::json_string(&label)
            );
            if let Some(seconds) = lease {
                body.insert(body.len() - 1, ',');
                body.push_str(&format!("\"lease\":{seconds}"));
            }
            request("POST", "/v1/tabs", Some(&body)).map(|s| println!("{s}"))
        }
        Some("close") => match args.get(1) {
            Some(id) => request("DELETE", &format!("/v1/tabs/{id}"), Some("{}")).map(|s| println!("{s}")),
            None => Err("usage: cu tab close ID".into()),
        },
        Some("renew") => match args.get(1) {
            Some(id) => {
                let seconds = args.get(2).map(String::as_str).unwrap_or("600");
                request(
                    "POST",
                    "/v1/lease",
                    Some(&format!(
                        "{{\"key\":{},\"seconds\":{}}}",
                        server::json_string(&format!("tab:{id}")),
                        seconds
                    )),
                )
                .map(|s| println!("{s}"))
            }
            None => Err("usage: cu tab renew ID [SECONDS]".into()),
        },
        _ => Err("usage: cu tab open [URL] [--lease S] [--label NAME] | close ID | renew ID [SECONDS]".into()),
    }
}

/// `cu context open NAME [--lease S] [--label NAME]`, `close NAME`.
fn context_cmd(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("open") => {
            let (name, label, lease) = lease_and_label(&args[1..])?;
            if name.is_empty() {
                return Err("usage: cu context open NAME [--lease S] [--label NAME]".into());
            }
            let mut body = format!(
                "{{\"name\":{},\"label\":{}}}",
                server::json_string(&name),
                server::json_string(&label)
            );
            if let Some(seconds) = lease {
                body.insert(body.len() - 1, ',');
                body.push_str(&format!("\"lease\":{seconds}"));
            }
            request("POST", "/v1/contexts", Some(&body)).map(|s| println!("{s}"))
        }
        Some("close") => match args.get(1) {
            Some(name) => {
                request("DELETE", &format!("/v1/contexts/{name}"), Some("{}")).map(|s| println!("{s}"))
            }
            None => Err("usage: cu context close NAME".into()),
        },
        _ => Err("usage: cu context open NAME [--lease S] [--label NAME] | close NAME".into()),
    }
}

/// `cu lease` lists held leases; `cu lease KEY [SECONDS]` sets or extends one.
fn lease_cmd(args: &[String]) -> Result<(), String> {
    match args.first() {
        None => request("GET", "/v1/leases", None).map(|s| println!("{s}")),
        Some(key) => {
            let seconds = args.get(1).map(String::as_str).unwrap_or("600");
            request(
                "POST",
                "/v1/lease",
                Some(&format!(
                    "{{\"key\":{},\"seconds\":{}}}",
                    server::json_string(key),
                    seconds
                )),
            )
            .map(|s| println!("{s}"))
        }
    }
}

/// Split one `cu batch` line into argv words: spaces, with "double" and
/// 'single' quotes for arguments that contain spaces.
fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// `cu batch 'CMD' 'CMD' ...` (or command lines on stdin): many commands, one
/// tool call. One compact JSON line per command; the exit code is 0 only when
/// every command succeeded.
fn batch(args: &[String]) -> Result<(), String> {
    let lines: Vec<String> = if args.is_empty() {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| e.to_string())?;
        input
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    } else {
        args.to_vec()
    };
    if lines.is_empty() {
        return Err("usage: cu batch 'CMD [ARGS...]' ... (or command lines on stdin)".into());
    }
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    let mut failed = 0usize;
    for line in &lines {
        let argv = split_command_line(line);
        if argv.is_empty() {
            continue;
        }
        let out = std::process::Command::new(&exe)
            .args(&argv)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!(
                "{{\"cmd\":{},\"ok\":true,\"out\":{}}}",
                server::json_string(line),
                server::json_string(&stdout)
            );
        } else {
            failed += 1;
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let error = stderr.strip_prefix("cu: ").unwrap_or(&stderr);
            println!(
                "{{\"cmd\":{},\"ok\":false,\"error\":{}}}",
                server::json_string(line),
                server::json_string(error)
            );
        }
    }
    if failed > 0 {
        Err(format!("{failed} of {} commands failed", lines.len()))
    } else {
        Ok(())
    }
}

fn print_help() {
    println!(
        "cu \u{2014} persistent browser for AI agents\n\n  cu start [--port N] [--data DIR]\n  cu status [--short]\n  cu navigate URL\n  cu shot [FILE]\n  cu snapshot\n  cu text\n  cu downloads\n  cu tabs [close ID]\n  cu tab open [URL] [--lease S] [--label NAME]\n  cu tab close ID | cu tab renew ID [SECONDS]\n  cu context open NAME [--lease S] [--label NAME]\n  cu context close NAME\n  cu lease [KEY [SECONDS]]\n  cu click REF\n  cu type REF TEXT [--submit]\n  cu act JSON_ACTIONS   (or JSON on stdin)\n  cu batch 'CMD' 'CMD' ...   (or command lines on stdin)\n  cu login\n  cu session save NAME\n\nPage commands take --tab ID or --context NAME to run in another tab or an\nisolated context. Leased tabs and contexts are closed by the daemon when the\nlease expires, so a test that dies leaves nothing behind.\n\nExit codes: 0 ok, 1 error (the message says what and why)."
    );
}

/// CDP port requested through the environment, if it parses.
fn cdp_port_from_env() -> Option<u16> {
    env::var("CU_CDP_PORT").ok().and_then(|p| p.parse().ok())
}

fn start(args: &[String]) -> Result<(), String> {
    let mut port = DEFAULT_PORT;
    let mut data = PathBuf::from(env::var_os("CU_DATA_DIR").unwrap_or_else(|| ".cu".into()));
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
            flag => return Err(format!("unknown option {flag}")),
        }
        i += 1;
    }
    fs::create_dir_all(data.join("profiles/default")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("sessions")).map_err(|e| e.to_string())?;
    fs::create_dir_all(data.join("downloads")).map_err(|e| e.to_string())?;
    // Chromium refuses a relative download directory, so the path handed to
    // the browser must be absolute whatever way `cu` was started.
    let data = fs::canonicalize(&data).unwrap_or(data);
    let token = server::random_token();
    fs::write(
        data.join("server.json"),
        format!("{{\"port\":{port},\"token\":\"{token}\"}}\n"),
    )
    .map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let cdp_port = cdp_port_from_env().unwrap_or(DEFAULT_CDP_PORT);
    // Listen and publish the token before the browser is up: `cu start` then
    // returns in single-digit milliseconds instead of blocking ~0.5 s on
    // Chromium, and the browser warms up while the agent reads its first tool
    // result. Actions that need the browser wait on the readiness signal.
    server::exit_on_signal(cdp_port);
    let browser = server::BrowserState::new();
    server::spawn_browser_thread(data.clone(), cdp_port, Arc::clone(&browser));
    eprintln!("cu listening on http://127.0.0.1:{port}; browser DevTools on 127.0.0.1:{cdp_port}");
    let state = Arc::new(AppState {
        data_dir: data,
        token,
        cdp_port,
        browser,
    });
    server::serve(state, listener);
    Ok(())
}
fn request(method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let data = env::var_os("CU_DATA_DIR").unwrap_or_else(|| ".cu".into());
    let config = fs::read_to_string(PathBuf::from(data).join("server.json"))
        .map_err(|_| "server not running (start it with `cu start`)")?;
    let port = server::json_value(&config, "port").ok_or("invalid server state")?;
    let token = server::json_value(&config, "token").ok_or("invalid server state")?;
    let mut stream = TcpStream::connect((
        "127.0.0.1",
        port.parse::<u16>().map_err(|_| "invalid port")?,
    ))
    .map_err(|e| e.to_string())?;
    let body = body.unwrap_or("");
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    Ok(response.split("\r\n\r\n").nth(1).unwrap_or("").into())
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
    let name = args.get(1).ok_or("usage: cu session save|load NAME")?;
    let path = if action == "save" {
        format!("/v1/session/{name}")
    } else {
        format!("/v1/session/{name}/load")
    };
    request("POST", &path, Some("{}")).map(|s| println!("{s}"))
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
    fn batch_lines_split_like_a_shell() {
        use super::split_command_line;
        assert_eq!(
            split_command_line("navigate https://x.test --tab t1"),
            vec!["navigate", "https://x.test", "--tab", "t1"]
        );
        assert_eq!(
            split_command_line("type e2 \"hello world\" --submit"),
            vec!["type", "e2", "hello world", "--submit"]
        );
        assert_eq!(split_command_line("click 'e 3'"), vec!["click", "e 3"]);
    }
    #[test]
    fn names_cannot_escape_profile_directory() {
        assert!(!valid_name("../secrets"));
        assert!(valid_name("work_1"));
    }
}
