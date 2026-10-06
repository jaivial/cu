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
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("start") => start(&args[1..]),
        Some("status") => request("GET", "/v1/status", None).map(|s| println!("{s}")),
        Some("navigate") => match args.get(1) {
            Some(url) => request(
                "POST",
                "/v1/navigate",
                Some(&format!("{{\"url\":\"{}\"}}", server::json_escape(url))),
            )
            .map(|s| println!("{s}")),
            None => Err("usage: cu navigate <url>".into()),
        },
        Some("shot") => request("GET", "/v1/screenshot", None)
            .and_then(|s| write_screenshot(&s, args.get(1).map(String::as_str))),
        Some("snapshot") => request("GET", "/v1/snapshot", None).map(|s| println!("{s}")),
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
fn print_help() {
    println!(
        "cu — persistent browser for AI agents\n\n  cu start [--port N] [--data DIR]\n  cu status\n  cu navigate URL\n  cu shot [FILE]\n  cu snapshot\n  cu login\n  cu session save NAME"
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
    let image =
        server::json_value(response, "png_base64").ok_or("server did not return an image")?;
    fs::write(
        path.unwrap_or("screenshot.png"),
        server::decode_base64(&image)?,
    )
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
    fn names_cannot_escape_profile_directory() {
        assert!(!valid_name("../secrets"));
        assert!(valid_name("work_1"));
    }
}
