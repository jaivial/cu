//! `cu`: a local persistent-browser daemon and CLI for AI agents.
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use cu::policy::{BrowserMode, BrowserPolicy};
use cu::server::{self, AppState};

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_CDP_PORT: u16 = 9222;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    // `--tab ID` runs any page command in another tab (a popup a click
    // opened, found with `cu tabs`); it is lifted out before dispatch.
    let mut tab: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--tab" {
            if i + 1 >= args.len() {
                eprintln!("cu: --tab needs a tab id (see `cu tabs`)");
                std::process::exit(1);
            }
            tab = Some(args.remove(i + 1));
            args.remove(i);
        } else {
            i += 1;
        }
    }
    // Append `tab=` to a page command's path, keeping any query it has.
    let on_tab = |path: &str| match &tab {
        None => path.to_string(),
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
            Some(other) => Err(format!(
                "unknown challenge command {other}; usage: cu challenge [release]"
            )),
        },
        Some("diagnostics") => request("GET", "/v1/diagnostics", None).map(|s| println!("{s}")),
        Some("targets") => request("GET", "/v1/targets", None).map(|s| println!("{s}")),
        Some("navigate") => match args.get(1) {
            Some(url) => request(
                "POST",
                &on_tab("/v1/navigate"),
                Some(&format!("{{\"url\":\"{}\"}}", server::json_escape(url))),
            )
            .map(|s| println!("{s}")),
            None => Err("usage: cu navigate <url> [--tab ID]".into()),
        },
        Some("shot" | "screenshot") => shot_query(&args[1..]).and_then(|(query, file)| {
            request("GET", &on_tab(&format!("/v1/screenshot?{query}")), None)
                .and_then(|s| write_screenshot(&s, file.as_deref()))
        }),
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
fn print_help() {
    println!(
        "cu — persistent browser for AI agents\n\n  cu start [--port N] [--data DIR] [--mode fast|compat] [--webgl] [--public-url URL]\n  cu status\n  cu diagnostics\n  cu targets\n  cu challenge [release]\n  cu navigate URL\n  cu shot [FILE] [--png] [--width N] [--height N] [--scale F]\n          [--ref REF | --selector CSS] [--padding N]\n  cu snapshot\n  cu text\n  cu downloads\n  cu tabs [close ID]\n  cu click REF\n  cu type REF TEXT [--submit]\n  cu upload [REF] FILE...\n  cu act JSON_ACTIONS   (or JSON on stdin)\n  cu login\n  cu session save|load NAME [--context CTX]\n\nPage commands take --tab ID to run in another tab (see `cu tabs`)."
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
    let mut public_url = env::var("CU_PUBLIC_URL").ok().filter(|u| !u.trim().is_empty());
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
    cu::session::claim_owner(&data, port, cdp_port)?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
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
/// `cu shot [FILE] [--png] [--quality N] [--width N] [--height N] [--scale F]
/// [--ref REF | --selector CSS] [--padding N]` as a `/v1/screenshot` query and
/// the file to write. A FILE ending in `.png` asks for PNG.
fn shot_query(args: &[String]) -> Result<(String, Option<String>), String> {
    const USAGE: &str = "usage: cu shot [FILE] [--png] [--quality N] [--width N] [--height N] [--scale F] [--ref REF | --selector CSS] [--padding N]";
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
    let name = args.get(1).ok_or("usage: cu session save|load NAME [--context CTX]")?;
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
