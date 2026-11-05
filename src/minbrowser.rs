//! Min Browser as cu's default browser.
//!
//! Min (minbrowser.org) is an Electron browser: same CDP endpoint as
//! Chromium (its engine currently reports Chrome/150), it honours
//! `--remote-debugging-port` and `--user-data-dir`, but being Electron it
//! cannot run `--headless=new`. [`crate::policy`] therefore launches it
//! headful on a display — `CU_DISPLAY`, else an Xvfb the daemon starts, the
//! inherited `DISPLAY` only as a last resort — and never with a headless
//! flag. Its profile is `profiles/min`, apart from Chrome's.
//!
//! This module only decides where Min lives and, when it is missing,
//! installs it (user-space, no root): the official `min-<version>-amd64.deb`
//! is downloaded with `curl` and extracted with `ar` + `tar` into
//! `~/.local/opt/min`, whose data tarball lays out `./opt/Min/*`
//! (`--strip-components=3` puts the tree directly in the install dir).
//! `CU_MIN_AUTOINSTALL=0` turns the installer off; cu then falls back to
//! the Chromium candidates and says so.
//!
//! **Tabs.** Electron answers `Target.createTarget` with `-32000 Not
//! supported` and has no browser contexts, so cu cannot open pages the
//! Chromium way. Min's own window UI (the `min://app/index.html` target)
//! can: [`open_tab`] replays, in that page, the `view-event`/`new-tab`
//! message Min's main process sends when a link asks for a new tab, which
//! runs Min's own `addTab`. The new page then shows up in DevTools discovery
//! like any tab. A tab Min does not show has no compositor frames, so a
//! screenshot of it never returns: [`select`] brings a tab to the front by
//! clicking its tab-bar entry, and [`crate::server`] calls it before every
//! capture. [`close_tab`] closes through `window.close()`, which Min's
//! preload turns into its own `closeTab` (a `Target.closeTarget` would kill
//! the page and leave a dead entry in Min's tab list).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::server::{Tab, http_get, json_objects, json_string, json_string_value};

/// Whether the browser this process drives is Min. Set once by the daemon
/// start, read by [`crate::server`] wherever Min differs from Chromium.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the running browser as Min (or not).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Relaxed);
}

/// Whether the running browser is Min.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// A DevTools page that is Min's own window, not a web page: the window UI
/// (`min://app/index.html`), Min's internal pages, and the helper pages Min
/// loads from its install (`file://…/resources/app/…`, e.g. the history
/// service). cu never drives these, lists them, or counts them as tabs.
pub fn is_ui_url(url: &str) -> bool {
    url.starts_with("min://") || (url.starts_with("file://") && url.contains("/resources/app/"))
}

/// CDP target id -> Min's own tab id, for the tabs cu opened. Min's ids are
/// kept as strings: they are larger than 2^53 and JSON numbers would round.
fn min_tabs() -> &'static Mutex<HashMap<String, String>> {
    static TABS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    TABS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long a tab Min was asked to open may take to show up in discovery.
const OPEN_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a closed tab may take to leave discovery.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// The `(id, url)` of every page target in DevTools discovery.
fn page_list(cdp_port: u16) -> Result<Vec<(String, String)>, String> {
    let discovery = http_get(&format!("127.0.0.1:{cdp_port}"), "/json")?;
    Ok(json_objects(&discovery)
        .into_iter()
        .filter(|o| json_string_value(o, "type").as_deref() == Some("page"))
        .filter_map(|o| {
            Some((
                json_string_value(&o, "id")?,
                json_string_value(&o, "url").unwrap_or_default(),
            ))
        })
        .collect())
}

/// The tab of Min's window UI, where the tab strip and `tabs` live.
fn ui_tab(cdp_port: u16) -> Result<Tab, String> {
    let id = page_list(cdp_port)?
        .into_iter()
        .find(|(_, url)| url.starts_with("min://app/index.html"))
        .map(|(id, _)| id)
        .ok_or("Min's window UI (min://app/index.html) is not in DevTools discovery")?;
    Ok(Tab {
        cdp_port,
        target: Some(id),
    })
}

/// Evaluate `expression` in Min's window UI and return its string value.
fn ui_eval(cdp_port: u16, expression: &str) -> Result<String, String> {
    let params = format!(
        "{{\"expression\":{},\"returnByValue\":true}}",
        json_string(expression)
    );
    let reply = ui_tab(cdp_port)?.command("Runtime.evaluate", &params)?;
    if reply.contains("\"exceptionDetails\"") {
        return Err(format!("Min UI script failed: {reply}"));
    }
    Ok(json_string_value(&reply, "value").unwrap_or_default())
}

/// Open `url` in a new Min tab and return its DevTools target id.
///
/// The tab is opened in the background (a foreground open makes Min drop
/// the current tab when that one is empty, which could be another
/// session's), then brought to the front with [`select`], so the session
/// that asked for it can capture it straight away.
pub fn open_tab(cdp_port: u16, url: &str) -> Result<String, String> {
    let before: Vec<String> = page_list(cdp_port)?.into_iter().map(|(id, _)| id).collect();
    let script = format!(
        "(() => {{ const known = new Set(tabs.get().map(t => String(t.id))); \
         require('electron').ipcRenderer.emit('view-event', {{}}, \
           {{event: 'new-tab', tabId: tabs.getSelected(), args: [{url}, false]}}); \
         const added = tabs.get().map(t => String(t.id)).filter(id => !known.has(id)); \
         return added.length ? added[0] : '' }})()",
        url = json_string(url)
    );
    let min_id = ui_eval(cdp_port, &script)?;
    if min_id.is_empty() {
        return Err("Min did not add a tab".into());
    }
    let started = Instant::now();
    let target = loop {
        let fresh: Vec<(String, String)> = page_list(cdp_port)?
            .into_iter()
            .filter(|(id, page_url)| !before.contains(id) && !is_ui_url(page_url))
            .collect();
        if let Some((id, _)) = fresh.first() {
            break id.clone();
        }
        if started.elapsed() > OPEN_TIMEOUT {
            return Err("Min added a tab, but it never showed up in DevTools discovery".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    min_tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(target.clone(), min_id);
    let _ = select(cdp_port, &target);
    Ok(target)
}

/// Bring target `target` to the front of Min's window, so it paints and a
/// screenshot of it returns. A no-op when it already is the selected tab.
pub fn select(cdp_port: u16, target: &str) -> Result<(), String> {
    let known = min_tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(target)
        .cloned();
    // A tab cu did not open (the first one, a popup) is found by its URL.
    let url = match known {
        Some(_) => String::new(),
        None => page_list(cdp_port)?
            .into_iter()
            .find(|(id, _)| id == target)
            .map(|(_, url)| url)
            .ok_or("no such tab; see GET /v1/tabs")?,
    };
    let script = format!(
        "(() => {{ let id = {known}; \
         if (!id) {{ const t = tabs.get().find(t => t.url === {url}); if (!t) return 'unknown'; id = String(t.id) }} \
         if (String(tabs.getSelected()) === id) return 'selected'; \
         const el = document.querySelector('.tab-item[data-tab=\"' + id + '\"]'); \
         if (!el) return 'unknown'; el.click(); return 'switched' }})()",
        known = json_string(&known.unwrap_or_default()),
        url = json_string(&url)
    );
    match ui_eval(cdp_port, &script)?.as_str() {
        // The view is shown on the next frame; give it that frame.
        "switched" => {
            std::thread::sleep(Duration::from_millis(120));
            Ok(())
        }
        "selected" => Ok(()),
        other => Err(format!("Min has no tab for target {target} ({other})")),
    }
}

/// Close target `target` the way Min closes a tab, so its tab list stays in
/// step; `Target.closeTarget` is the fallback when the page does not go.
pub fn close_tab(cdp_port: u16, target: &str) -> Result<(), String> {
    let tab = Tab {
        cdp_port,
        target: Some(target.to_string()),
    };
    let _ = tab.command(
        "Runtime.evaluate",
        "{\"expression\":\"window.close(); true\",\"returnByValue\":true}",
    );
    let started = Instant::now();
    while started.elapsed() < CLOSE_TIMEOUT {
        let live = page_list(cdp_port)?;
        if !live.iter().any(|(id, _)| id == target) {
            forget(target);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = crate::server::browser_command(
        cdp_port,
        "Target.closeTarget",
        &format!("{{\"targetId\":{}}}", json_string(target)),
    )?;
    forget(target);
    if reply.contains("\"error\"") {
        return Err(reply);
    }
    Ok(())
}

fn forget(target: &str) {
    min_tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(target);
}

/// Where a user-space Min is installed (`$HOME/.local/opt/min`).
pub fn install_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Min install dir needs $HOME")?;
    Ok(home.join(".local/opt/min"))
}

/// An existing Min executable, or `None`. Checked in order: `min` on the
/// `PATH` (the distro package), `/opt/Min/min` (the vendor layout), and
/// the user-space install above.
pub fn find() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("min"));
        }
    }
    candidates.push(PathBuf::from("/opt/Min/min"));
    if let Ok(dir) = install_dir() {
        candidates.push(dir.join("min"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .and_then(clear_symlink)
}

/// A candidate that is a symlink to somewhere else, as distro packages
/// ship them (`/usr/bin/min -> /opt/Min/min`): what is spawned is the
/// target, so that is what is checked.
fn clear_symlink(path: PathBuf) -> Option<PathBuf> {
    match std::fs::canonicalize(&path) {
        Ok(resolved) if resolved.is_file() => Some(resolved),
        _ if path.is_file() => Some(path),
        _ => None,
    }
}

/// Whether the automatic installer may run (`CU_MIN_AUTOINSTALL` not
/// empty, `0`, `off`, `false` or `no`).
fn autoinstall() -> bool {
    !matches!(
        std::env::var("CU_MIN_AUTOINSTALL")
            .unwrap_or_else(|_| "1".into())
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "" | "0" | "off" | "false" | "no"
    )
}

/// A usable Min executable: an existing one, or a fresh install. The
/// installer runs at most once per process; on failure cu keeps starting
/// with its Chromium candidates.
pub fn ensure() -> Result<PathBuf, String> {
    static ONCE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ONCE.get_or_init(|| match find() {
        Some(found) => Ok(found),
        None if !autoinstall() => Err("Min not installed and CU_MIN_AUTOINSTALL=0".into()),
        None => install(),
    })
    .clone()
}

/// Download and unpack the official amd64 `.deb` into the install dir.
fn install() -> Result<PathBuf, String> {
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let url = latest_deb_url()?;
    eprintln!("[min] installing {url} into {}", dir.display());
    let deb = dir.join("min.deb");
    run(
        Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(&deb)
            .arg(&url),
        "download Min",
    )?;
    let unpack = dir.join(".unpack");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack).map_err(|e| format!("mkdir {}: {e}", unpack.display()))?;
    run(
        Command::new("ar").arg("x").arg(&deb).current_dir(&unpack),
        "ar x min.deb",
    )?;
    let data = ["data.tar.xz", "data.tar.zst", "data.tar.gz"]
        .iter()
        .map(|name| unpack.join(name))
        .find(|path| path.is_file())
        .ok_or("the .deb carries no data tarball")?;
    // `./opt/Min/**` -> the install dir itself: `.`, `opt` and `Min` are
    // three path components.
    let mut tar = Command::new("tar");
    tar.arg("xf").arg(&data).arg("-C").arg(&dir);
    if data.extension().is_some_and(|e| e == "zst") {
        tar.arg("--zstd");
    }
    tar.arg("--strip-components=3");
    run(&mut tar, "unpack Min")?;
    let _ = std::fs::remove_dir_all(&unpack);
    let _ = std::fs::remove_file(&deb);
    let bin = dir.join("min");
    if !bin.is_file() {
        return Err(format!(
            "the .deb unpacked no ./opt/Min/min into {}",
            dir.display()
        ));
    }
    make_executable(&bin)?;
    eprintln!("[min] installed {}", bin.display());
    Ok(bin)
}

/// The `browser_download_url` of the newest `min-<version>-amd64.deb`,
/// read out of the GitHub releases API answer (no JSON dependency).
fn latest_deb_url() -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--retry", "2"])
        .arg("https://api.github.com/repos/minbrowser/min/releases/latest")
        .output()
        .map_err(|e| format!("curl the releases API: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    deb_url_from(&text).ok_or_else(|| "no amd64 deb in the latest release".into())
}

/// The first `min-...-amd64.deb` download URL in `json`, Min's asset
/// naming (`min-1.35.7-amd64.deb`), with the surrounding quotes kept off.
pub fn deb_url_from(json: &str) -> Option<String> {
    for line in json.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("\"browser_download_url\":") {
            // The URL is the first token after the key; whatever follows it
            // (a closing brace, a comma) is not part of it.
            let url = rest
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('"');
            if url.ends_with("-amd64.deb") {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {e}", path.display()))
}

fn run(command: &mut Command, what: &str) -> Result<(), String> {
    command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{what}: {e}"))
        .and_then(|out| {
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{what}: exit {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deb_url_picks_the_amd64_asset() {
        let json = r#"
          "assets": [
            { "name": "min-1.35.7-arm64.deb",
              "browser_download_url": "https://x/min-1.35.7-arm64.deb" },
            { "name": "min-1.35.7-amd64.deb",
              "browser_download_url": "https://x/min-1.35.7-amd64.deb" },
            { "name": "min-1.35.7-x86_64.rpm",
              "browser_download_url": "https://x/min-1.35.7-x86_64.rpm" }
          ]
        "#;
        assert_eq!(
            deb_url_from(json).as_deref(),
            Some("https://x/min-1.35.7-amd64.deb")
        );
    }

    #[test]
    fn min_window_pages_are_not_web_pages() {
        assert!(is_ui_url("min://app/index.html"));
        assert!(is_ui_url("min://newtab"));
        assert!(is_ui_url(
            "file:///opt/Min/resources/app/js/places/placesService.html"
        ));
        assert!(!is_ui_url("https://example.com/"));
        assert!(!is_ui_url("about:blank"));
        assert!(!is_ui_url("file:///home/me/page.html"));
    }

    /// Downloads ~90 MB from GitHub: run by hand with
    /// `cargo test -- --ignored installs_min_into_a_fresh_home`.
    #[test]
    #[ignore]
    fn installs_min_into_a_fresh_home() {
        let home = std::env::temp_dir().join(format!("cu-min-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: an ignored, single-threaded test run by hand.
        unsafe { std::env::set_var("HOME", &home) };
        let bin = install().expect("install Min");
        assert_eq!(bin, home.join(".local/opt/min/min"));
        let out = Command::new(&bin).arg("--version").output();
        assert!(bin.is_file() && out.is_ok());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deb_url_none_without_the_asset() {
        assert_eq!(deb_url_from("\"browser_download_url\": \"x.rpm\""), None);
    }
}
