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
use std::env;
use std::path::{Path, PathBuf};

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
    "--remote-allow-origins=*",
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
/// websocket sends no `Origin`, so it is not needed).
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

/// Whether `CU_WEBGL` asks for WebGL (any value but empty, `0` or `false`).
pub fn webgl_from_env() -> bool {
    env::var("CU_WEBGL")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"))
        .unwrap_or(false)
}

/// Executables tried, in order, when `CU_BROWSER` is not set.
///
/// `FastTest` keeps cu's historical default first. `Compatibility` prefers a
/// stock Google Chrome install (a package-managed full browser) and falls back
/// to distribution Chromium.
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
}

impl BrowserPolicy {
    /// Decide the launch from the mode and the environment (`CU_BROWSER`,
    /// `CU_HEADLESS`). Fails, rather than quietly downgrading, when
    /// `Compatibility` would end up on a headless-shell build.
    pub fn resolve(mode: BrowserMode) -> Result<Self, String> {
        let requested = match env::var("CU_BROWSER") {
            Ok(binary) if !binary.trim().is_empty() => binary,
            _ => candidates(mode)
                .iter()
                .find(|name| find_executable(name).is_some())
                .unwrap_or(&candidates(mode)[0])
                .to_string(),
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
        let headless = env::var("CU_HEADLESS")
            .ok()
            .map(|v| v != "0")
            .unwrap_or(true);
        Ok(Self {
            mode,
            requested,
            executable,
            resolved,
            headless,
            headless_shell,
            webgl: webgl_from_env(),
        })
    }

    /// The full argument list, DevTools port and profile included.
    pub fn args(&self, profile: &Path, cdp_port: u16) -> Vec<String> {
        let mut args = vec![format!("--remote-debugging-port={cdp_port}")];
        let base = match self.mode {
            BrowserMode::FastTest => FAST_TEST_ARGS,
            BrowserMode::Compatibility => COMPATIBILITY_ARGS,
        };
        args.extend(
            base.iter()
                .filter(|a| !(self.webgl && **a == "--disable-gpu"))
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
        }
        args.push("about:blank".into());
        args
    }
}

fn is_headless_shell(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("headless_shell") || n.contains("headless-shell"))
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
