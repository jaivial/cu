//! How the browser is launched: the `FastTest` profile cu has always used, or
//! a `Compatibility` profile that is a stock full Chrome/Chromium with nothing
//! switched off.
//!
//! `FastTest` turns off the GPU, extensions, site isolation and a dozen
//! background services. That is the right trade for owned test sites, where
//! start-up time and memory are what matter -- and the wrong one for a site
//! that scores how normal a browser looks: a browser with no GPU process, no
//! site isolation and a pile of disabled features is not a configuration any
//! real user runs. `Compatibility` launches the full browser with the few
//! flags DevTools and an unattended profile need, and refuses the separate
//! `headless-shell` build, which is a different (older) browser
//! implementation that no flag turns into the full one.
//!
//! Neither mode spoofs anything. Compatibility only stops cu from making the
//! browser *less* normal than it is on this host; whatever the host itself
//! looks like (no display, software GL, its IP) is reported by
//! [`crate::diagnostics`], not hidden.
//!
//! **A window instead of `--headless`.** A headless Chrome says so in its
//! User-Agent, in its client hints and in its own command line, and a site
//! that reads any of them answers a navigation with "verifying you are not a
//! bot" instead of the page (see [`crate::stealth`], which measures it). So
//! the launch prefers a headful browser on an X display -- the daemon's own
//! Xvfb when the machine has no display -- and only passes `--headless=new`
//! when no display could be had or when `CU_HEADLESS=1` asks for it. Nothing
//! is faked about a windowless host beyond the absence of the flag, and the
//! fallback is logged.
use std::env;
use std::path::{Path, PathBuf};

use crate::minbrowser;

/// Which launch profile the daemon uses. Chosen with `CU_MODE` or
/// `cu start --mode`; the default is `FastTest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserMode {
    FastTest,
    Compatibility,
}

impl BrowserMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "fast" | "fast-test" | "fasttest" | "test" => Ok(Self::FastTest),
            "compat" | "compatibility" => Ok(Self::Compatibility),
            other => Err(format!(
                "unknown browser mode \"{other}\"; use fast or compat"
            )),
        }
    }

    /// `CU_MODE`, or `FastTest` when it is not set.
    pub fn from_env() -> Result<Self, String> {
        Self::parse(&env::var("CU_MODE").unwrap_or_default())
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::FastTest => "fast-test",
            Self::Compatibility => "compatibility",
        }
    }
}

/// Flags the `FastTest` profile is launched with.
///
/// An agent's browser needs pages, DevTools and the profile -- not updates,
/// sync, translation, crash upload, audio or a GPU process. Each of those is a
/// process or a background timer, and on a loaded machine they compete with
/// the page for CPU. Site isolation is relaxed so same-site frames share one
/// renderer, which is most of the memory saving.
pub const FAST_TEST_ARGS: &[&str] = &[
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-dev-shm-usage",
    // Background work an agent never asked for.
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-sync",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-breakpad",
    "--disable-crash-reporter",
    "--metrics-recording-only",
    "--no-pings",
    "--mute-audio",
    "--password-store=basic",
    // Rendering: software raster. (`--in-process-gpu` would also drop the GPU
    // process, but it crashes chrome-headless-shell on cross-document
    // navigation, so it is not used.)
    "--disable-gpu",
    // No frame-rate limit, only with `CU_FAST_CAPTURE=1` (see
    // [`fast_capture`]). Chromium otherwise waits for the compositor's next
    // vsync before it hands a frame to a screenshot, and a capture then costs
    // two frame intervals (~33 ms) instead of the few milliseconds the raster
    // itself takes.
    "--disable-frame-rate-limit",
    // A tab cu is not looking at is a hidden tab, and Chromium slows a hidden
    // tab's timers to one wake-up a second (to one a minute once it has been
    // hidden for five). The page an agent just clicked on is hidden too, so
    // its own `setTimeout(.., 300)` took a second: a reaction scheduled 1.5 s
    // after a click arrived after 2.2 s, measured. These three keep a hidden
    // tab's scripts at full speed; it still paints nothing.
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    // The network service as a thread of the browser, not a process.
    "--enable-features=NetworkServiceInProcess",
    // Fewer processes: same-site frames share a renderer.
    "--disable-site-isolation-trials",
    "--renderer-process-limit=4",
    "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,\
     AutofillServerCommunication,CalculateNativeWinOcclusion,InterestFeedContentSuggestions,\
     CertificateTransparencyComponentUpdater,LensOverlay,PaintHolding,\
     SpareRendererForSitePerProcess,BackForwardCache",
];

/// Flags the `Compatibility` profile is launched with: only what an
/// unattended DevTools-driven profile cannot do without.
///
/// - first-run and default-browser prompts would sit in front of the first
///   page of a fresh profile;
/// - `--password-store=basic` keeps Chrome from blocking start-up on a desktop
///   keyring prompt nobody is there to answer (it changes where saved
///   passwords are encrypted, nothing a page can see).
///
/// Deliberately absent: `--disable-gpu`, `--disable-extensions`, site
/// isolation, `--disable-features`, `--mute-audio`, `--enable-automation`
/// (which sets `navigator.webdriver`) and `--remote-allow-origins` (cu's
/// websocket sends no `Origin`, so it is not needed: the flag only widens
/// who may open the DevTools socket, and a browser without it still accepts
/// an `Origin`-less client, which is the only kind cu ever is).
pub const COMPATIBILITY_ARGS: &[&str] = &[
    "--no-first-run",
    "--no-default-browser-check",
    "--password-store=basic",
];

/// Flags added when WebGL is asked for (`CU_WEBGL=1` or `cu start --webgl`),
/// in either mode; `--disable-gpu` is dropped from `FastTest` at the same time.
///
/// A host without a GPU (a server under xvfb) has no hardware GL, and since
/// Chromium 137 the browser no longer falls back to SwiftShader on its own:
/// WebGL is simply unavailable and sites that need WebGL2 (CapCut's editor)
/// send the browser to an "incompatible" page. ANGLE on SwiftShader is a CPU
/// implementation of GLES 3, which is enough for WebGL2; Chromium only lets
/// web content use it when `--enable-unsafe-swiftshader` says so.
pub const WEBGL_ARGS: &[&str] = &[
    "--use-gl=angle",
    "--use-angle=swiftshader",
    "--enable-unsafe-swiftshader",
    "--ignore-gpu-blocklist",
];

/// Whether the browser may render as fast as the CPU allows: only with
/// `CU_FAST_CAPTURE=1`.
///
/// Chromium's frame-rate limit is what makes a screenshot wait for the
/// compositor's next vsync. Removing it makes a capture cost ~6 ms instead
/// of ~34 ms -- and makes a page that animates cost the cores it takes to
/// render it, 2,700 frames a second on a headless browser with software
/// rendering. It was the default from 1.8 to 1.10. Measured on a QA flow of
/// 38 commands over three apps, 8 s apart, with idle tabs parked
/// ([`crate::park`]): 82.9 CPU-seconds of browser without the limit, 27.5
/// with it, for 3.7 s of commands instead of 4.1 s. And a capture of a
/// background tab was slower without it, not faster (1.0 s each when they
/// come one after the other, 85 ms with the limit). So the limit stays
/// unless asked otherwise.
pub fn fast_capture() -> bool {
    fast_capture_from(env::var("CU_FAST_CAPTURE").ok().as_deref())
}

fn fast_capture_from(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("1") | Some("on") | Some("true") | Some("yes")
    )
}

/// Whether `CU_WEBGL` asks for WebGL (any value but empty, `0` or `false`).
pub fn webgl_from_env() -> bool {
    env::var("CU_WEBGL")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"))
        .unwrap_or(false)
}

/// Whether the post-launch hardening runs: on unless `CU_STEALTH` says
/// empty, `0`, `off` or `false`.
pub fn stealth_from_env() -> bool {
    !matches!(
        env::var("CU_STEALTH")
            .unwrap_or_else(|_| "1".into())
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Executables tried, in order, when `CU_BROWSER` is not set and no Min
/// Browser is available.
///
/// `FastTest` keeps cu's historical default first. `Compatibility` prefers a
/// stock Google Chrome install (a package-managed full browser) and falls back
/// to distribution Chromium. An installed (or auto-installed) Min is preferred
/// over both lists: see [`minbrowser`].
fn candidates(mode: BrowserMode) -> &'static [&'static str] {
    match mode {
        BrowserMode::FastTest => &[
            "chromium",
            "chromium-browser",
            "google-chrome-stable",
            "google-chrome",
        ],
        BrowserMode::Compatibility => &[
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
        ],
    }
}

/// Everything about the launch that is decided before the browser runs.
#[derive(Clone, Debug)]
pub struct BrowserPolicy {
    pub mode: BrowserMode,
    /// What the executable was asked as (`CU_BROWSER` or a default name).
    pub requested: String,
    /// The file that is spawned: `requested` resolved through `PATH`.
    pub executable: PathBuf,
    /// `executable` with symlinks followed, for the log: `google-chrome`
    /// is usually a link into `/opt`, and which build ran is what matters.
    pub resolved: PathBuf,
    pub headless: bool,
    /// The executable is a `headless-shell` build (Chrome for Testing's
    /// `chrome-headless-shell`, Playwright's `headless_shell`).
    pub headless_shell: bool,
    /// Software WebGL ([`WEBGL_ARGS`]) instead of `FastTest`'s `--disable-gpu`.
    pub webgl: bool,
    /// X display the browser gets a window on: `CU_DISPLAY`, an existing
    /// `DISPLAY`, or an Xvfb this daemon started. `None` means the browser
    /// runs headless.
    pub display: Option<String>,
    /// Harden the running browser after launch ([`crate::stealth`]): a
    /// windowed User-Agent, no `navigator.webdriver`, no DevTools command
    /// line, no `SwiftShader` in the GL renderer. Off with `CU_STEALTH=0`.
    pub stealth: bool,
    /// The browser is Min (Electron): same CDP, honours
    /// `--remote-debugging-port` and `--user-data-dir`, but it has no
    /// `--headless` mode, so it always runs on [`Self::display`].
    pub min: bool,
}

impl BrowserPolicy {
    /// Decide the launch from the mode and the environment (`CU_BROWSER`,
    /// `CU_HEADLESS`). Fails, rather than quietly downgrading, when
    /// `Compatibility` would end up on a headless-shell build.
    pub fn resolve(mode: BrowserMode) -> Result<Self, String> {
        // Chrome/Chromium is the default. Min Browser is opt-in
        // (`CU_MIN_DEFAULT=1`, or `CU_BROWSER` pointing at it): an Electron
        // browser cannot be driven as fully as Chromium (no
        // `Target.createTarget`, no browser contexts, views Min never loaded
        // stay detached from its window and paint nothing), so it is not
        // what cu picks on its own. With the opt-in, a missing Min is
        // auto-installed (`CU_MIN_AUTOINSTALL=0` turns that off).
        let prefer_min = matches!(env::var("CU_MIN_DEFAULT"), Ok(v)
            if matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"));
        let min_default = || match minbrowser::find() {
            Some(path) => Some(path.display().to_string()),
            None => match minbrowser::ensure() {
                Ok(path) => Some(path.display().to_string()),
                Err(why) => {
                    eprintln!("[min] staying on the Chromium candidates: {why}");
                    None
                }
            },
        };
        let requested = match env::var("CU_BROWSER") {
            Ok(binary) if !binary.trim().is_empty() => binary,
            _ => if prefer_min { min_default() } else { None }.unwrap_or_else(|| {
                candidates(mode)
                    .iter()
                    .find(|name| find_executable(name).is_some())
                    .unwrap_or(&candidates(mode)[0])
                    .to_string()
            }),
        };
        let executable = find_executable(&requested).unwrap_or_else(|| PathBuf::from(&requested));
        let resolved = std::fs::canonicalize(&executable).unwrap_or_else(|_| executable.clone());
        let headless_shell = is_headless_shell(&executable) || is_headless_shell(&resolved);
        if mode == BrowserMode::Compatibility && headless_shell {
            return Err(format!(
                "compatibility mode needs a full Chrome/Chromium, but {} is a headless-shell build; \
                 set CU_BROWSER to a full browser or use CU_MODE=fast",
                resolved.display()
            ));
        }
        let min = is_min(&executable) || is_min(&resolved);
        let stealth = stealth_from_env();
        // A headful browser needs a display, and a headless-shell build is
        // headless whatever it is told, so one of the two decides on its own.
        // `CU_HEADLESS` still has the last word: `1` means headless even when a
        // display is free, `0` means a window even when none is.
        let asked = env::var("CU_HEADLESS")
            .ok()
            .map(|v| v.trim() != "0")
            .filter(|headless| *headless || env::var("CU_HEADLESS").is_ok());
        if min && asked == Some(true) {
            return Err(format!(
                "{} is Min (Electron) and cannot run headless; unset CU_HEADLESS, \
                 or point CU_BROWSER at a Chrome/Chromium",
                resolved.display()
            ));
        }
        let mut display = None;
        let mut headless = true;
        if !headless_shell && asked != Some(true) {
            // Min always has a window: on a private Xvfb rather than the
            // desktop the daemon inherited, so its windows never appear on
            // the user's screen.
            let found = if min {
                crate::stealth::private_display()
            } else {
                crate::stealth::display()
            };
            match found {
                Some(found) => {
                    let name = found.name.clone();
                    crate::stealth::remember_display(found);
                    display = Some(name);
                    headless = false;
                }
                None => {}
            }
        }
        if asked == Some(true) {
            headless = true;
            display = None;
        }
        // Min has no headless mode: a window is not an optimisation there
        // but a requirement, so a missing display is a hard error instead of
        // a silent `--headless=new` that Electron cannot honour.
        if min {
            match &display {
                Some(_) => headless = false,
                None => {
                    return Err(format!(
                        "{} is Min (Electron) and needs a display: set CU_DISPLAY or DISPLAY, \
                         or have Xvfb installed so the daemon can start one",
                        resolved.display()
                    ));
                }
            }
        }
        Ok(Self {
            mode,
            requested,
            executable,
            resolved,
            headless,
            headless_shell,
            webgl: webgl_from_env(),
            display,
            stealth,
            min,
        })
    }

    /// The full argument list, DevTools port and profile included.
    pub fn args(&self, profile: &Path, cdp_port: u16) -> Vec<String> {
        // Anything a page could read out of `/proc/<pid>/cmdline` is left
        // out: `--remote-debugging-port` on its own is the one an
        // unprivileged page cannot read, but `--remote-allow-origins=*` and
        // `--enable-automation` (which is what switches on the
        // `AutomationControlled` blink feature, and so `navigator.webdriver`)
        // are never given.
        let mut args = vec![format!("--remote-debugging-port={cdp_port}")];
        let base = match self.mode {
            BrowserMode::FastTest => FAST_TEST_ARGS,
            BrowserMode::Compatibility => COMPATIBILITY_ARGS,
        };
        args.extend(
            base.iter()
                .filter(|a| !(self.webgl && **a == "--disable-gpu"))
                .filter(|a| !(!fast_capture() && **a == "--disable-frame-rate-limit"))
                .map(|a| a.to_string()),
        );
        if self.webgl {
            args.extend(WEBGL_ARGS.iter().map(|a| a.to_string()));
        }
        // One argument: Chromium only parses `--user-data-dir=PATH` here, and
        // treats a separate PATH as a second target ("Multiple targets are not
        // supported in headless mode", exit 13).
        args.push(format!("--user-data-dir={}", profile.display()));
        // A headless-shell build is headless whatever it is told; the full
        // browser gets the unified (`new`) headless mode, the same browser as
        // headed Chrome without a window.
        if self.headless && !self.headless_shell {
            args.push("--headless=new".into());
            // The User-Agent override `crate::stealth` installs lives only as
            // long as the DevTools connection that made it, so a headless
            // daemon's later requests went out as `HeadlessChrome` and
            // sites such as x.com answered 403. The launch flag is what
            // every request and every new tab carries.
            if self.stealth {
                if let Some(agent) = windowed_agent_for(&self.executable) {
                    args.push(format!("--user-agent={agent}"));
                }
            }
        }
        args.push("about:blank".into());
        args
    }
}

/// The User-Agent a windowed build of `executable` sends, from its own
/// `--version` (`Google Chrome 145.0.7632.67` -> `Chrome/145.0.0.0`): the
/// reduced form current Chrome sends. `None` off Linux or when the version
/// cannot be read, in which case the browser keeps its own.
fn windowed_agent_for(executable: &Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let out = std::process::Command::new(executable)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_windowed_agent(&text)
}

fn parse_windowed_agent(version_output: &str) -> Option<String> {
    let major: u32 = version_output.split_whitespace().find_map(|word| {
        word.split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .filter(|_| word.contains('.'))
    })?;
    Some(format!(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

fn is_headless_shell(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("headless_shell") || n.contains("headless-shell"))
}

/// Whether `path` is the Min browser: its executable is called `min` and
/// lives in a `Min` install dir (distro package, `/opt/Min`, or the
/// user-space install of [`crate::minbrowser`]).
fn is_min(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("min"))
        || path.ancestors().any(|dir| {
            dir.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("min"))
        })
}

/// `name` as an executable path: as given if it has a slash, else the first
/// match on `PATH`.
fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_rate_limit_stays_unless_asked_otherwise() {
        for keep in [
            None,
            Some(""),
            Some("0"),
            Some("off"),
            Some("no"),
            Some("maybe"),
        ] {
            assert!(!fast_capture_from(keep), "{keep:?}");
        }
        for remove in ["1", "on", "true", "yes", " ON "] {
            assert!(fast_capture_from(Some(remove)), "{remove}");
        }
    }
}
