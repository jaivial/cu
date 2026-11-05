//! Min Browser as cu's preferred browser.
//!
//! Min (minbrowser.org) is an Electron browser: same CDP endpoint as
//! Chromium (its engine currently reports Chrome/150), it honours
//! `--remote-debugging-port` and `--user-data-dir`, but being Electron it
//! cannot run `--headless=new`. [`crate::policy`] therefore launches it
//! headful on a display — `CU_DISPLAY`, the inherited `DISPLAY`, or an
//! Xvfb the daemon starts — and never falls back to a headless flag.
//!
//! This module only decides where Min lives and, when it is missing,
//! installs it (user-space, no root): the official `min-<version>-amd64.deb`
//! is downloaded with `curl` and extracted with `ar` + `tar` into
//! `~/.local/opt/min`, whose data tarball lays out `./opt/Min/*`
//! (`--strip-components=2` puts the tree directly in the install dir).
//! `CU_MIN_AUTOINSTALL=0` turns the installer off; cu then falls back to
//! the Chromium candidates and says so.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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
    // ./opt/Min/** -> the install dir itself.
    let mut tar = Command::new("tar");
    tar.arg("xf").arg(&data).arg("-C").arg(&dir);
    if data.extension().is_some_and(|e| e == "zst") {
        tar.arg("--zstd");
    }
    tar.arg("--strip-components=2");
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
    fn deb_url_none_without_the_asset() {
        assert_eq!(deb_url_from("\"browser_download_url\": \"x.rpm\""), None);
    }
}
