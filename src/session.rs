//! SessionPolicy: one stable, host-coherent identity per profile, one owner
//! per profile, and saved sessions encrypted at rest.
//!
//! **Identity.** The locale, time zone and window size a browser shows are
//! read together by sites, and a set that contradicts itself (a Spanish
//! locale on a machine whose clock says UTC and whose IP says Ohio, a screen
//! that changes size on every restart) is a stronger signal than any one of
//! them. cu therefore derives the identity from the *host* the first time a
//! profile is used -- `LC_ALL`/`LC_MESSAGES`/`LANG` for the locale, `TZ`,
//! `/etc/timezone` or the `/etc/localtime` link for the zone -- writes it into
//! the profile (`cu-identity.json`, so a saved session carries it) and
//! applies the same values on every later launch. They are applied at the
//! process level (`--lang`, the browser's `TZ`, `--window-size` when
//! headless), so every frame, worker and named context of the browser agrees
//! with them; nothing is patched in JavaScript. Nothing is invented either:
//! the user agent and platform are the browser's own, and the identity
//! never claims an OS the host does not run. `CU_LOCALE`, `CU_TIMEZONE` and
//! `CU_WINDOW_SIZE` override a value on purpose, and the override is reported
//! as such.
//!
//! **Owner.** A data directory has one browser profile, and two daemons on it
//! used to clear each other's Chromium locks and overwrite each other's
//! `server.json`. The daemon now holds an exclusive `flock` on
//! `<data>/owner.lock` for its whole life; a second `cu start` on the same
//! directory is refused with the owner's pid and port.
//!
//! **Encrypted sessions.** `cu session save` used to copy the live profile --
//! cookies, local storage, the logged-in state -- into a plain directory.
//! It now writes one `sessions/NAME.cuse` file: every file of the profile
//! (caches excluded) is a separate XChaCha20-Poly1305 record, authenticated
//! with the session name and its position, and the archive ends with a
//! record that counts them, so a renamed, reordered, truncated or edited
//! archive fails to load instead of loading half a login. The key is
//! `CU_SESSION_KEY` (64 hex characters) or a key file
//! (`CU_SESSION_KEY_FILE`, default `~/.config/cu/session.key`, created
//! 0600 on first use), deliberately outside the data directory so a copied
//! `.cu` does not carry its own key. A load decrypts everything into a
//! staging directory first and touches the live profile only if all of it
//! verified. Old plain-directory sessions still load.
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::server::{json_string, json_string_value, json_value};

/// The file, inside the profile, that holds its identity.
pub const IDENTITY_FILE: &str = "cu-identity.json";

/// Where a value of the identity came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Read from this machine.
    Host,
    /// Set on purpose with an environment variable.
    Override,
    /// Neither: a neutral default (the host did not say).
    Default,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Override => "override",
            Self::Default => "default",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "host" => Self::Host,
            "override" => Self::Override,
            _ => Self::Default,
        }
    }
}

/// A profile's identity, stable across restarts.
#[derive(Clone, Debug)]
pub struct Identity {
    /// Seeds per-session behaviour (the paced input model).
    pub seed: u64,
    /// BCP 47 tag, e.g. `es-ES`.
    pub locale: String,
    pub locale_source: Source,
    /// IANA zone, e.g. `Europe/Madrid`.
    pub timezone: String,
    pub timezone_source: Source,
    /// Window size in CSS pixels, applied when headless (a headful window is
    /// the window manager's on the real screen). `None` lets the browser
    /// decide.
    pub window: Option<(u32, u32)>,
    pub window_source: Source,
    /// `std::env::consts::OS` when the identity was made.
    pub host_os: String,
    pub created_unix: u64,
}

/// `en_US.UTF-8` / `es_ES@euro` / `C` -> `en-US` / `es-ES` / None.
pub fn bcp47(posix: &str) -> Option<String> {
    let base = posix.split(['.', '@']).next()?.trim();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    let mut parts = base.split(['_', '-']);
    let language = parts.next()?;
    if !(2..=3).contains(&language.len()) || !language.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let language = language.to_ascii_lowercase();
    match parts.next() {
        Some(region)
            if (region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()))
                || (region.len() == 3 && region.chars().all(|c| c.is_ascii_digit())) =>
        {
            Some(format!("{language}-{}", region.to_ascii_uppercase()))
        }
        _ => Some(language),
    }
}

/// The host's locale, the way glibc picks the one for messages.
pub fn host_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .and_then(|v| bcp47(&v))
}

/// An IANA zone name: letters, digits and `/_+-`, no `..`.
fn valid_zone(zone: &str) -> bool {
    !zone.is_empty()
        && !zone.contains("..")
        && !zone.starts_with('/')
        && zone
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c))
}

/// The host's zone: `TZ` (a name, or `:name`), `/etc/timezone`, or the
/// zone the `/etc/localtime` link points at.
pub fn host_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':');
        if valid_zone(tz) {
            return Some(tz.to_string());
        }
    }
    if let Ok(zone) = fs::read_to_string("/etc/timezone") {
        let zone = zone.trim();
        if valid_zone(zone) {
            return Some(zone.to_string());
        }
    }
    let link = fs::read_link("/etc/localtime").ok()?;
    let link = link.to_string_lossy();
    let zone = link.split("zoneinfo/").nth(1)?;
    valid_zone(zone).then(|| zone.to_string())
}

/// Desktop window sizes common enough not to stand out; a profile keeps the
/// one its seed picks.
const WINDOWS: &[(u32, u32)] = &[
    (1920, 1080),
    (1536, 864),
    (1440, 900),
    (1366, 768),
    (1280, 800),
];

fn parse_window(value: &str) -> Option<(u32, u32)> {
    let (w, h) = value.trim().split_once(['x', 'X', ','])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    ((320..=7680).contains(&w) && (240..=4320).contains(&h)).then_some((w, h))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fresh_seed() -> u64 {
    let mut bytes = [0u8; 8];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = (nanos ^ ((std::process::id() as u64) << 32)).to_le_bytes();
    }
    u64::from_le_bytes(bytes)
}

impl Identity {
    /// A new identity from the host and the overrides.
    fn derive() -> Self {
        let seed = fresh_seed();
        let (locale, locale_source) = match std::env::var("CU_LOCALE").ok().and_then(|l| bcp47(&l))
        {
            Some(l) => (l, Source::Override),
            None => match host_locale() {
                Some(l) => (l, Source::Host),
                None => ("en-US".into(), Source::Default),
            },
        };
        let (timezone, timezone_source) =
            match std::env::var("CU_TIMEZONE").ok().filter(|z| valid_zone(z)) {
                Some(z) => (z, Source::Override),
                None => match host_timezone() {
                    Some(z) => (z, Source::Host),
                    None => ("UTC".into(), Source::Default),
                },
            };
        let (window, window_source) = match std::env::var("CU_WINDOW_SIZE")
            .ok()
            .and_then(|w| parse_window(&w))
        {
            Some(w) => (Some(w), Source::Override),
            None => (
                Some(WINDOWS[(seed % WINDOWS.len() as u64) as usize]),
                Source::Default,
            ),
        };
        Self {
            seed,
            locale,
            locale_source,
            timezone,
            timezone_source,
            window,
            window_source,
            host_os: std::env::consts::OS.into(),
            created_unix: now_unix(),
        }
    }

    pub fn json(&self) -> String {
        format!(
            "{{\"version\":1,\"seed\":\"{:016x}\",\"locale\":{},\"locale_source\":\"{}\",\
             \"timezone\":{},\"timezone_source\":\"{}\",\"window\":{},\"window_source\":\"{}\",\
             \"host_os\":{},\"created_unix\":{}}}",
            self.seed,
            json_string(&self.locale),
            self.locale_source.name(),
            json_string(&self.timezone),
            self.timezone_source.name(),
            self.window
                .map(|(w, h)| format!("[{w},{h}]"))
                .unwrap_or_else(|| "null".into()),
            self.window_source.name(),
            json_string(&self.host_os),
            self.created_unix
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let seed = u64::from_str_radix(&json_string_value(text, "seed")?, 16).ok()?;
        let locale = json_string_value(text, "locale").and_then(|l| bcp47(&l))?;
        let timezone = json_string_value(text, "timezone").filter(|z| valid_zone(z))?;
        let window = crate::server::json_array(text, "window")
            .and_then(|w| parse_window(w.trim_start_matches('[').trim_end_matches(']')));
        Some(Self {
            seed,
            locale,
            locale_source: Source::parse(&json_string_value(text, "locale_source")?),
            timezone,
            timezone_source: Source::parse(&json_string_value(text, "timezone_source")?),
            window,
            window_source: Source::parse(
                &json_string_value(text, "window_source").unwrap_or_default(),
            ),
            host_os: json_string_value(text, "host_os").unwrap_or_default(),
            created_unix: json_value(text, "created_unix")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        })
    }

    /// The profile's identity: the stored one, with any explicit override
    /// applied on top, or a new one derived and stored. Notes say where the
    /// stored identity and the host now disagree (kept for stability).
    pub fn load_or_create(profile: &Path) -> (Self, Vec<String>) {
        let path = profile.join(IDENTITY_FILE);
        let mut notes = Vec::new();
        let stored = fs::read_to_string(&path).ok().and_then(|t| Self::parse(&t));
        let mut identity = match stored {
            Some(identity) => identity,
            None => {
                let identity = Self::derive();
                let _ = fs::create_dir_all(profile);
                let _ = fs::write(&path, identity.json() + "\n");
                identity
            }
        };
        // An explicit override always wins, and is stored so it is stable.
        let mut changed = false;
        if let Some(l) = std::env::var("CU_LOCALE").ok().and_then(|l| bcp47(&l))
            && l != identity.locale
        {
            identity.locale = l;
            identity.locale_source = Source::Override;
            changed = true;
        }
        if let Some(z) = std::env::var("CU_TIMEZONE").ok().filter(|z| valid_zone(z))
            && z != identity.timezone
        {
            identity.timezone = z;
            identity.timezone_source = Source::Override;
            changed = true;
        }
        if let Some(w) = std::env::var("CU_WINDOW_SIZE")
            .ok()
            .and_then(|w| parse_window(&w))
            && Some(w) != identity.window
        {
            identity.window = Some(w);
            identity.window_source = Source::Override;
            changed = true;
        }
        if changed {
            let _ = fs::write(&path, identity.json() + "\n");
        }
        if identity.host_os != std::env::consts::OS {
            notes.push(format!(
                "identity was made on {} but this host runs {}; the browser reports this host",
                identity.host_os,
                std::env::consts::OS
            ));
        }
        if identity.locale_source == Source::Host
            && let Some(host) = host_locale()
            && host != identity.locale
        {
            notes.push(format!(
                "host locale is now {host}; the profile keeps {} for stability (delete {IDENTITY_FILE} to re-derive)",
                identity.locale
            ));
        }
        if identity.timezone_source == Source::Host
            && let Some(host) = host_timezone()
            && host != identity.timezone
        {
            notes.push(format!(
                "host time zone is now {host}; the profile keeps {} for stability (delete {IDENTITY_FILE} to re-derive)",
                identity.timezone
            ));
        }
        for (what, source) in [
            ("locale", identity.locale_source),
            ("time zone", identity.timezone_source),
        ] {
            if source == Source::Override {
                notes.push(format!(
                    "{what} is an explicit override, not this host's: it must match the network the browser uses"
                ));
            }
        }
        (identity, notes)
    }

    /// Launch flags that carry the identity.
    pub fn args(&self, headless: bool) -> Vec<String> {
        let mut args = vec![
            format!("--lang={}", self.locale),
            format!("--accept-lang={}", accept_languages(&self.locale)),
        ];
        if headless && let Some((w, h)) = self.window {
            args.push(format!("--window-size={w},{h}"));
            // Headless has no monitor; without this `screen` reads 800x600
            // under a 1920x1080 window, which no real desktop shows.
            args.push(format!("--screen-info={{{w}x{h}}}"));
        }
        args
    }

    /// Environment for the browser process: the real process time zone (so
    /// workers and every frame agree), and the locale.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("TZ", self.timezone.clone()),
            ("LANGUAGE", self.locale.replace('-', "_")),
        ]
    }
}

/// `es-ES` -> `es-ES,es`: the region tag, then its language, as browsers
/// configured for one language send it.
pub fn accept_languages(locale: &str) -> String {
    match locale.split_once('-') {
        Some((language, _)) => format!("{locale},{language}"),
        None => locale.to_string(),
    }
}

/// The running profile's identity, the notes about it, and what a page
/// observed of it once the browser was up.
struct Current {
    identity: Identity,
    notes: Vec<String>,
    observed: Option<String>,
}

fn current() -> &'static Mutex<Option<Current>> {
    static CURRENT: OnceLock<Mutex<Option<Current>>> = OnceLock::new();
    CURRENT.get_or_init(|| Mutex::new(None))
}

/// Remember the identity the browser was launched with.
pub fn set_current(identity: &Identity, notes: Vec<String>) {
    *current().lock().unwrap_or_else(|e| e.into_inner()) = Some(Current {
        identity: identity.clone(),
        notes,
        observed: None,
    });
}

/// The running profile's identity, if the daemon has one.
pub fn identity() -> Option<Identity> {
    current()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.identity.clone())
}

/// The seed of the running identity (the paced input model uses it).
pub fn seed() -> Option<u64> {
    identity().map(|i| i.seed)
}

/// What a page reads back, checked against the identity: run once the
/// browser is up, in cu's isolated world (it reads only `navigator`,
/// `Intl`, `screen` and the viewport).
pub const OBSERVE_JS: &str = "JSON.stringify({language:navigator.language,languages:navigator.languages.join(','),timezone:Intl.DateTimeFormat().resolvedOptions().timeZone,platform:navigator.platform,ua:navigator.userAgent,screen:screen.width+'x'+screen.height,viewport:innerWidth+'x'+innerHeight})";

/// Compare what the page observed with the identity, store the result for
/// diagnostics and log any mismatch.
pub fn record_observed(observed: &str) {
    let mut guard = current().lock().unwrap_or_else(|e| e.into_inner());
    let Some(Current {
        identity,
        notes,
        observed: seen,
    }) = guard.as_mut()
    else {
        return;
    };
    *seen = Some(observed.to_string());
    let mut mismatch = Vec::new();
    if json_string_value(observed, "language").as_deref() != Some(identity.locale.as_str()) {
        mismatch.push(format!(
            "navigator.language is {:?}, identity says {}",
            json_string_value(observed, "language").unwrap_or_default(),
            identity.locale
        ));
    }
    let tz = json_string_value(observed, "timezone").unwrap_or_default();
    // `UTC` and `Etc/UTC` are the same zone under two names.
    let same_zone = tz == identity.timezone
        || (tz.trim_start_matches("Etc/") == "UTC"
            && identity.timezone.trim_start_matches("Etc/") == "UTC");
    if !same_zone {
        mismatch.push(format!(
            "Intl time zone is {tz:?}, identity says {}",
            identity.timezone
        ));
    }
    let platform = json_string_value(observed, "platform").unwrap_or_default();
    let ua = json_string_value(observed, "ua").unwrap_or_default();
    let host_family = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Mac",
        "windows" => "Win",
        other => other,
    };
    if !platform.contains(host_family) || (host_family == "Linux" && ua.contains("Windows NT")) {
        mismatch.push(format!(
            "the browser claims platform {platform:?} on a {} host",
            std::env::consts::OS
        ));
    }
    for m in &mismatch {
        eprintln!("cu: identity: {m}");
    }
    notes.retain(|n| !n.starts_with("observed: "));
    notes.extend(mismatch.into_iter().map(|m| format!("observed: {m}")));
}

/// The `session` object of `/v1/diagnostics`.
pub fn json() -> String {
    let guard = current().lock().unwrap_or_else(|e| e.into_inner());
    let Some(Current {
        identity,
        notes,
        observed: seen,
    }) = guard.as_ref()
    else {
        return "null".into();
    };
    format!(
        "{{\"identity\":{},\"observed\":{},\"owner\":{},\"encrypted_sessions\":{},\"notes\":[{}]}}",
        identity.json(),
        seen.clone().unwrap_or_else(|| "null".into()),
        owner_json(),
        key_source()
            .map(json_string)
            .unwrap_or_else(|| "null".into()),
        notes
            .iter()
            .map(|n| json_string(n))
            .collect::<Vec<_>>()
            .join(",")
    )
}

// ── owner lock ───────────────────────────────────────────────────────────

fn owner() -> &'static Mutex<Option<(fs::File, String)>> {
    static OWNER: OnceLock<Mutex<Option<(fs::File, String)>>> = OnceLock::new();
    OWNER.get_or_init(|| Mutex::new(None))
}

fn owner_json() -> String {
    owner()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, info)| info.clone())
        .unwrap_or_else(|| "null".into())
}

/// Become the one owner of `data`'s profile for this process's life, or say
/// who owns it.
pub fn claim_owner(data: &Path, port: u16, cdp_port: u16) -> Result<(), String> {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    use std::os::fd::AsRawFd;
    fs::create_dir_all(data).map_err(|e| e.to_string())?;
    let path = data.join("owner.lock");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: a plain syscall on a descriptor this function owns.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        let mut held = String::new();
        let _ = file.read_to_string(&mut held);
        return Err(format!(
            "{} is already owned by another cu daemon ({}); one daemon per profile -- \
             stop it or use another --data directory",
            data.display(),
            held.trim()
        ));
    }
    let info = format!(
        "{{\"pid\":{},\"port\":{port},\"cdp_port\":{cdp_port},\"since_unix\":{}}}",
        std::process::id(),
        now_unix()
    );
    file.set_len(0).map_err(|e| e.to_string())?;
    file.write_all(info.as_bytes()).map_err(|e| e.to_string())?;
    *owner().lock().unwrap_or_else(|e| e.into_inner()) = Some((file, info));
    Ok(())
}

// ── encrypted sessions ───────────────────────────────────────────────────

const MAGIC: &[u8; 8] = b"CUSESS1\n";
/// Profile entries that are caches or per-run state, not session state.
const SKIP: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "ShaderCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "component_crx_cache",
    "Crashpad",
    "BrowserMetrics",
    "optimization_guide_model_store",
];
const KIND_FILE: u8 = 0;
const KIND_DIR: u8 = 1;
const KIND_LINK: u8 = 2;
const KIND_END: u8 = 3;

/// Where the key comes from, for diagnostics (never the key itself).
pub fn key_source() -> Option<&'static str> {
    if std::env::var("CU_SESSION_PLAINTEXT").as_deref() == Ok("1") {
        return Some("off (CU_SESSION_PLAINTEXT=1)");
    }
    Some(if std::env::var("CU_SESSION_KEY").is_ok() {
        "CU_SESSION_KEY"
    } else if std::env::var("CU_SESSION_KEY_FILE").is_ok() {
        "CU_SESSION_KEY_FILE"
    } else {
        "~/.config/cu/session.key"
    })
}

fn decode_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn key_file() -> PathBuf {
    if let Some(p) = std::env::var_os("CU_SESSION_KEY_FILE") {
        return PathBuf::from(p);
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config.join("cu").join("session.key")
}

/// The session key: `CU_SESSION_KEY`, or the key file, created on first
/// use when `create` is set.
fn key(create: bool) -> Result<XChaCha20Poly1305, String> {
    if let Ok(hex) = std::env::var("CU_SESSION_KEY") {
        let k = decode_hex(&hex).ok_or("CU_SESSION_KEY must be 64 hex characters")?;
        return Ok(XChaCha20Poly1305::new(&k.into()));
    }
    let path = key_file();
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mode = fs::metadata(&path)
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} is readable by other users (mode {:o}); chmod 600 it",
                    path.display(),
                    mode & 0o777
                ));
            }
            let k = decode_hex(&text).ok_or_else(|| {
                format!("{} does not hold a 64-hex-character key", path.display())
            })?;
            Ok(XChaCha20Poly1305::new(&k.into()))
        }
        Err(_) if create => {
            let k = XChaCha20Poly1305::generate_key(&mut OsRng);
            let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            f.write_all(format!("{hex}\n").as_bytes())
                .map_err(|e| e.to_string())?;
            eprintln!("cu: created session key {}", path.display());
            Ok(XChaCha20Poly1305::new(&k))
        }
        Err(e) => Err(format!("no session key ({}: {e})", path.display())),
    }
}

fn aad(name: &str, index: u64) -> Vec<u8> {
    let mut aad = MAGIC.to_vec();
    aad.extend_from_slice(name.as_bytes());
    aad.push(0);
    aad.extend_from_slice(&index.to_le_bytes());
    aad
}

fn record(kind: u8, path: &str, mode: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 + path.len() + body.len());
    out.push(kind);
    out.extend_from_slice(&(path.len() as u16).to_le_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&mode.to_le_bytes());
    out.extend_from_slice(body);
    out
}

struct Writer<'a> {
    out: fs::File,
    cipher: &'a XChaCha20Poly1305,
    name: &'a str,
    index: u64,
}

impl Writer<'_> {
    fn put(&mut self, plain: &[u8]) -> Result<(), String> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: &aad(self.name, self.index),
                },
            )
            .map_err(|_| "encryption failed")?;
        self.out
            .write_all(&(sealed.len() as u32).to_le_bytes())
            .and_then(|_| self.out.write_all(&nonce))
            .and_then(|_| self.out.write_all(&sealed))
            .map_err(|e| e.to_string())?;
        self.index += 1;
        Ok(())
    }

    fn walk(&mut self, root: &Path, rel: &str) -> Result<(), String> {
        let dir = root.join(rel);
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // Per-process locks (see `copy_dir`) and caches are not state.
            if name.starts_with("Singleton") || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            let mode = meta.permissions().mode() & 0o777;
            if meta.file_type().is_symlink() {
                let link = fs::read_link(entry.path()).map_err(|e| e.to_string())?;
                self.put(&record(
                    KIND_LINK,
                    &path,
                    0,
                    link.to_string_lossy().as_bytes(),
                ))?;
            } else if meta.is_dir() {
                self.put(&record(KIND_DIR, &path, mode, &[]))?;
                self.walk(root, &path)?;
            } else if meta.is_file() {
                // A file the browser deletes mid-walk (a journal) is skipped.
                let Ok(body) = fs::read(entry.path()) else {
                    continue;
                };
                self.put(&record(KIND_FILE, &path, mode, &body))?;
            }
        }
        Ok(())
    }
}

/// The live cookie jar a save captured, as `Storage.getCookies` returned
/// it; it lands in the profile on load and is consumed at once.
pub const COOKIES_FILE: &str = "cu-cookies.json";

/// The cookies a loaded session left for the browser, removed from the
/// profile as they are taken (they are also in the browser's own store).
pub fn take_pending_cookies(profile: &Path) -> Option<String> {
    let path = profile.join(COOKIES_FILE);
    let cookies = fs::read_to_string(&path).ok();
    let _ = fs::remove_file(&path);
    cookies.filter(|c| c.trim_start().starts_with('['))
}

/// The archive of session `name`.
pub fn archive_path(sessions: &Path, name: &str) -> PathBuf {
    sessions.join(format!("{name}.cuse"))
}

/// Save `profile` as session `name`: encrypted unless
/// `CU_SESSION_PLAINTEXT=1`. Written to a temporary file and renamed, so a
/// failed save never replaces a good one.
pub fn save(
    profile: &Path,
    sessions: &Path,
    name: &str,
    live_cookies: Option<&str>,
) -> Result<&'static str, String> {
    if !profile.exists() {
        return Err("browser profile does not exist".into());
    }
    if std::env::var("CU_SESSION_PLAINTEXT").as_deref() == Ok("1") {
        crate::server::copy_dir(profile, &sessions.join(name))?;
        if let Some(cookies) = live_cookies {
            fs::write(sessions.join(name).join(COOKIES_FILE), cookies)
                .map_err(|e| e.to_string())?;
        }
        return Ok("plaintext");
    }
    let cipher = key(true)?;
    fs::create_dir_all(sessions).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(sessions, fs::Permissions::from_mode(0o700));
    let tmp = sessions.join(format!(".{name}.cuse.tmp"));
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    out.write_all(MAGIC).map_err(|e| e.to_string())?;
    let mut writer = Writer {
        out,
        cipher: &cipher,
        name,
        index: 0,
    };
    // Chromium writes cookies to disk in batches (about every 30 s), so a
    // login made just before the save may not be in the profile yet; the
    // browser's live cookie jar goes in as its own record and is put back
    // into the browser after a load (see [`take_pending_cookies`]).
    let walked = writer.walk(profile, "").and_then(|_| match live_cookies {
        Some(cookies) => writer.put(&record(KIND_FILE, COOKIES_FILE, 0o600, cookies.as_bytes())),
        None => Ok(()),
    });
    let result = walked.and_then(|_| {
        let count = writer.index;
        writer.put(&record(KIND_END, "", 0, &count.to_le_bytes()))?;
        writer.out.sync_all().map_err(|e| e.to_string())
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, archive_path(sessions, name)).map_err(|e| e.to_string())?;
    // A plaintext copy from before encryption would leave the login readable
    // next to the encrypted one.
    let legacy = sessions.join(name);
    if legacy.is_dir() {
        let _ = fs::remove_dir_all(&legacy);
    }
    Ok("encrypted")
}

/// A relative archive path that stays inside the destination.
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Decrypt session `name` into `dest`, which must not exist. Every record is
/// verified; any failure leaves `dest` removed.
fn decrypt_into(archive: &Path, name: &str, dest: &Path) -> Result<(), String> {
    let cipher = key(false)?;
    let bytes = fs::read(archive).map_err(|e| e.to_string())?;
    let fail = |e: &str| {
        let _ = fs::remove_dir_all(dest);
        Err(format!("session {name} cannot be loaded: {e}"))
    };
    if !bytes.starts_with(MAGIC) {
        return fail("not a cu session archive");
    }
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut at = MAGIC.len();
    let mut index = 0u64;
    loop {
        if at + 4 + 24 > bytes.len() {
            return fail("archive is truncated");
        }
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default()) as usize;
        let nonce = XNonce::from_slice(&bytes[at + 4..at + 28]);
        at += 28;
        if at + len > bytes.len() {
            return fail("archive is truncated");
        }
        let Ok(plain) = cipher.decrypt(
            nonce,
            Payload {
                msg: &bytes[at..at + len],
                aad: &aad(name, index),
            },
        ) else {
            return fail("wrong key, or the archive was modified or renamed");
        };
        at += len;
        if plain.len() < 7 {
            return fail("malformed record");
        }
        let kind = plain[0];
        let plen = u16::from_le_bytes([plain[1], plain[2]]) as usize;
        if plain.len() < 7 + plen {
            return fail("malformed record");
        }
        let path = String::from_utf8_lossy(&plain[3..3 + plen]).into_owned();
        let mode = u32::from_le_bytes(plain[3 + plen..7 + plen].try_into().unwrap_or_default());
        let body = &plain[7 + plen..];
        if kind == KIND_END {
            let count = u64::from_le_bytes(body.try_into().unwrap_or_default());
            if count != index || at != bytes.len() {
                return fail("record count does not match");
            }
            return Ok(());
        }
        if !safe_relative(&path) {
            return fail("unsafe path in archive");
        }
        let target = dest.join(&path);
        let written = match kind {
            KIND_DIR => fs::create_dir_all(&target).and_then(|_| {
                fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o700))
            }),
            KIND_FILE => fs::write(&target, body).and_then(|_| {
                fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o600))
            }),
            KIND_LINK => {
                std::os::unix::fs::symlink(String::from_utf8_lossy(body).as_ref(), &target)
            }
            _ => return fail("unknown record kind"),
        };
        if let Err(e) = written {
            return fail(&e.to_string());
        }
        index += 1;
    }
}

/// Load session `name` over `profile`: the encrypted archive if there is
/// one, else a plain directory saved before encryption.
pub fn load(profile: &Path, sessions: &Path, name: &str) -> Result<&'static str, String> {
    let archive = archive_path(sessions, name);
    if archive.is_file() {
        let staging = sessions.join(format!(".{name}.loading"));
        let _ = fs::remove_dir_all(&staging);
        decrypt_into(&archive, name, &staging)?;
        let copied = crate::server::copy_dir(&staging, profile);
        let _ = fs::remove_dir_all(&staging);
        copied?;
        return Ok("encrypted");
    }
    let legacy = sessions.join(name);
    if legacy.is_dir() {
        crate::server::copy_dir(&legacy, profile)?;
        return Ok("plaintext");
    }
    Err(format!("no saved session named {name}"))
}
