//! The vault: per-domain sessions and site credentials in SQLite, encrypted
//! at rest with AES-256-GCM.
//!
//! Two stores share one database (`~/.cu/daemon/vault.db` by default):
//!
//! - `sessions` — what a logged-in browser looks like to a site (cookies and
//!   `localStorage`), captured per domain from a live tab, so a new tab can
//!   be navigated straight into a logged-in state without a second login.
//! - `credentials` — what a login form asks for (user, password, OTP,
//!   headers, tokens), as a field list so the vault UI can render a form per
//!   site instead of assuming `user`/`password`.
//!
//! Secrets (cookie values, field values) are never on disk in the clear:
//! each value is sealed with AES-256-GCM under a key held in
//! `~/.cu/vault.key` (mode 0600, generated on first use). The vault UI's
//! master password is not the encryption key: it is verified against a
//! salted PBKDF2-HMAC-SHA256 hash in `vault_meta`, and only unlocks the UI
//! — the daemon itself reads the key file, as any process running as the
//! user could anyway.
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use rusqlite::Connection;

use crate::server::{json_string, random_token};

/// Where the key file lives: the parent of the data directory, so the
/// canonical daemon (`~/.cu/daemon`) uses exactly `~/.cu/vault.key`.
fn key_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir
        .parent()
        .map(|p| p.join("vault.key"))
        .unwrap_or_else(|| data_dir.join("vault.key"))
}

/// The 32-byte vault key, generated on first use (hex, mode 0600).
fn load_or_create_key(data_dir: &std::path::Path) -> Result<[u8; 32], String> {
    let path = key_path(data_dir);
    if let Ok(hex) = std::fs::read_to_string(&path) {
        let hex = hex.trim();
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(
                hex.get(i * 2..i * 2 + 2)
                    .ok_or_else(|| format!("{}: truncated key file", path.display()))?,
                16,
            )
            .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        return Ok(key);
    }
    let key: [u8; 32] = rand_bytes();
    std::fs::write(&path, hex_encode(&key))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    restrict(&path);
    Ok(key)
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use aes_gcm::aead::rand_core::RngCore;
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Seal `plaintext` under `key`: a fresh 12-byte nonce followed by the
/// AES-256-GCM ciphertext (nonce is never reused — it is random per seal).
fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce_bytes: [u8; 12] = rand_bytes();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("vault seal failed: {e}"))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a value `seal` produced.
fn unseal(key: &[u8; 32], blob: &[u8]) -> Result<String, String> {
    if blob.len() < 13 {
        return Err("vault record is truncated".into());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let pt = cipher
        .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
        .map_err(|_| "vault record failed to decrypt (wrong key?)".to_string())?;
    String::from_utf8(pt).map_err(|_| "vault record is not UTF-8".into())
}

/// PBKDF2-HMAC-SHA256 for a 32-byte derived key (single block).
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    use hmac::Mac;
    type HmacSha256 = hmac::Hmac<sha2::Sha256>;
    let mut mac =
        <HmacSha256 as hmac::Mac>::new_from_slice(password).expect("hmac accepts any key length");
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut u = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&u);
    for _ in 1..iterations {
        let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(password)
            .expect("hmac accepts any key length");
        mac.update(&u);
        u = mac.finalize().into_bytes();
        for (a, b) in out.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    out
}

const PBKDF2_ITERATIONS: u32 = 120_000;

/// The vault database. Opened per operation: one SQLite open is microseconds
/// and this rate never gets near being a bottleneck, while a connection held
/// in a static would need `Sync` gymnastics for no gain.
pub struct Vault {
    path: PathBuf,
    key: [u8; 32],
}

impl Vault {
    /// Open (creating schema and key file if needed).
    pub fn open(data_dir: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let key = load_or_create_key(data_dir)?;
        let vault = Self {
            path: data_dir.join("vault.db"),
            key,
        };
        let conn = vault.connect()?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("vault schema: {e}"))?;
        let _ = conn.execute(
            "INSERT OR REPLACE INTO vault_meta(key, value) VALUES ('schema_version', '1')",
            [],
        );
        Ok(vault)
    }

    fn connect(&self) -> Result<Connection, String> {
        Connection::open(&self.path)
            .map_err(|e| format!("could not open {}: {e}", self.path.display()))
    }

    // ---- sessions ----

    /// Store a captured session for `domain`, replacing the previous one.
    pub fn save_session(
        &self,
        domain: &str,
        cookies: &str,
        local_storage: &str,
        expires_at: Option<i64>,
    ) -> Result<(), String> {
        let cookies = seal(&self.key, cookies.as_bytes())?;
        let storage = seal(&self.key, local_storage.as_bytes())?;
        let now = now();
        self.connect()?
            .execute(
                "INSERT INTO sessions(domain, profile, cookies, local_storage, expires_at, updated_at)
                 VALUES (?1, 'default', ?2, ?3, ?4, ?5)
                 ON CONFLICT(domain, profile) DO UPDATE SET
                   cookies=excluded.cookies, local_storage=excluded.local_storage,
                   expires_at=excluded.expires_at, updated_at=excluded.updated_at",
                rusqlite::params![domain, cookies, storage, expires_at, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The stored session for `domain`, or for the registrable parent a
    /// site was reached through (`a.example.com` finds `example.com`).
    pub fn get_session(&self, domain: &str) -> Result<Option<StoredSession>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT domain, cookies, local_storage, expires_at, updated_at FROM sessions")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (stored, cookies, storage, expires_at, updated_at) =
                row.map_err(|e| e.to_string())?;
            if domain_matches(domain, &stored) {
                return Ok(Some(StoredSession {
                    domain: stored,
                    cookies: unseal(&self.key, &cookies)?,
                    local_storage: unseal(&self.key, &storage).unwrap_or_default(),
                    expires_at,
                    updated_at,
                }));
            }
        }
        Ok(None)
    }

    /// Every session, without any secret value.
    pub fn list_sessions(&self) -> Result<String, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT domain, expires_at, updated_at FROM sessions ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (domain, expires_at, updated_at) = row.map_err(|e| e.to_string())?;
            out.push(format!(
                "{{\"domain\":{},\"expires_at\":{},\"updated_at\":{}}}",
                json_string(&domain),
                expires_at
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "null".into()),
                updated_at
            ));
        }
        Ok(format!("{{\"sessions\":[{}]}}", out.join(",")))
    }

    pub fn forget_session(&self, domain: &str) -> Result<bool, String> {
        let n = self
            .connect()?
            .execute("DELETE FROM sessions WHERE domain = ?1", [domain])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    // ---- credentials ----

    /// Create or update a credential; `fields` is a JSON array of
    /// `{"name":...,"value":...,"secret":bool,"type":...}` objects, stored
    /// sealed as one blob.
    pub fn put_credential(
        &self,
        id: Option<i64>,
        domain: &str,
        label: &str,
        fields: &str,
        notes: &str,
    ) -> Result<i64, String> {
        let sealed = seal(&self.key, fields.as_bytes())?;
        let conn = self.connect()?;
        let now = now();
        match id {
            Some(id) => {
                conn.execute(
                    "UPDATE credentials SET domain=?1, label=?2, fields=?3, notes=?4, updated_at=?5 WHERE id=?6",
                    rusqlite::params![domain, label, sealed, notes, now, id],
                )
                .map_err(|e| e.to_string())?;
                Ok(id)
            }
            None => {
                conn.execute(
                    "INSERT INTO credentials(domain, label, fields, notes, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![domain, label, sealed, notes, now],
                )
                .map_err(|e| e.to_string())?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    /// Every credential matching `query` (case-insensitive over domain,
    /// label and notes), without secret values.
    pub fn list_credentials(&self, query: &str) -> Result<String, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT id, domain, label, notes, created_at, updated_at, fields FROM credentials ORDER BY domain, label")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let needle = query.to_ascii_lowercase();
        let mut out = Vec::new();
        for row in rows {
            let (id, domain, label, notes, created_at, updated_at, fields) =
                row.map_err(|e| e.to_string())?;
            if !needle.is_empty()
                && ![&domain, &label, &notes]
                    .iter()
                    .any(|v| v.to_ascii_lowercase().contains(&needle))
            {
                continue;
            }
            // Field names are not secrets and the UI wants them for its
            // generated forms; values stay sealed here.
            let names = field_names(&unseal(&self.key, &fields).unwrap_or_default());
            out.push(format!(
                "{{\"id\":{},\"domain\":{},\"label\":{},\"notes\":{},\"fields\":{},\"created_at\":{},\"updated_at\":{}}}",
                id,
                json_string(&domain),
                json_string(&label),
                json_string(&notes),
                json_string(&names.join(",")),
                created_at,
                updated_at
            ));
        }
        Ok(format!("{{\"credentials\":[{}]}}", out.join(",")))
    }

    /// One credential with its secrets, decrypted. Only for an authorised
    /// requester (bearer token or an authenticated UI session).
    pub fn get_credential(&self, id: i64) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT domain, label, notes, fields, created_at, updated_at FROM credentials WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        let row = stmt
            .query_row([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .ok();
        let Some((domain, label, notes, fields, created_at, updated_at)) = row else {
            return Ok(None);
        };
        let fields = unseal(&self.key, &fields)?;
        Ok(Some(format!(
            "{{\"id\":{},\"domain\":{},\"label\":{},\"notes\":{},\"fields\":{fields},\"created_at\":{},\"updated_at\":{}}}",
            id,
            json_string(&domain),
            json_string(&label),
            json_string(&notes),
            created_at,
            updated_at
        )))
    }

    pub fn delete_credential(&self, id: i64) -> Result<bool, String> {
        let n = self
            .connect()?
            .execute("DELETE FROM credentials WHERE id = ?1", [id])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// How many credentials exist for `domain` (for the navigate hint).
    pub fn count_credentials(&self, domain: &str) -> usize {
        let Ok(conn) = self.connect() else {
            return 0;
        };
        let Ok(mut stmt) = conn.prepare("SELECT domain FROM credentials") else {
            return 0;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return 0;
        };
        rows.filter_map(|r| r.ok())
            .filter(|stored| domain_matches(domain, stored))
            .count()
    }

    // ---- master password ----

    pub fn has_password(&self) -> bool {
        self.meta("pw_hash").is_some_and(|v| !v.is_empty())
    }

    /// Set (first run) or change (with the old password) the master password.
    pub fn set_password(&self, old: Option<&str>, new: &str) -> Result<(), String> {
        if self.has_password() {
            let Some(old) = old else {
                return Err("vault already has a master password; give the old one".into());
            };
            if !self.verify_password(old) {
                return Err("old master password is wrong".into());
            }
        }
        let salt: [u8; 16] = rand_bytes();
        let hash = pbkdf2(new.as_bytes(), &salt, PBKDF2_ITERATIONS);
        let conn = self.connect()?;
        for (k, v) in [
            ("pw_salt", hex_encode(&salt)),
            ("pw_hash", hex_encode(&hash)),
            ("pw_iters", PBKDF2_ITERATIONS.to_string()),
        ] {
            conn.execute(
                "INSERT INTO vault_meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![k, v],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn verify_password(&self, password: &str) -> bool {
        let (Some(salt), Some(hash)) = (self.meta("pw_salt"), self.meta("pw_hash")) else {
            return false;
        };
        let iterations = self
            .meta("pw_iters")
            .and_then(|v| v.parse().ok())
            .unwrap_or(PBKDF2_ITERATIONS);
        let mut salt_bytes = [0u8; 16];
        for (i, b) in salt_bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(salt.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
        }
        let mut hash_bytes = [0u8; 32];
        for (i, b) in hash_bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(hash.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
        }
        pbkdf2(password.as_bytes(), &salt_bytes, iterations) == hash_bytes
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.connect()
            .ok()?
            .query_row(
                "SELECT value FROM vault_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .ok()
    }
}

#[derive(Clone, Debug)]
pub struct StoredSession {
    pub domain: String,
    /// JSON array of DevTools cookies, ready for `Storage.setCookies`.
    pub cookies: String,
    /// JSON object of `localStorage` key/values.
    pub local_storage: String,
    pub expires_at: Option<i64>,
    pub updated_at: i64,
}

/// Whether a URL's host `host` is covered by a record stored under `stored`:
/// exact match, or a subdomain of it.
fn domain_matches(host: &str, stored: &str) -> bool {
    let host = host.trim_start_matches("www.");
    let stored = stored.trim_start_matches("www.");
    host == stored || host.ends_with(&format!(".{stored}")) || stored.ends_with(&format!(".{host}"))
}

/// Field names of a fields JSON array, best effort with the hand-rolled
/// parser (`"name":"..."` occurrences in order).
fn field_names(fields: &str) -> Vec<String> {
    fields
        .split("\"name\"")
        .skip(1)
        .filter_map(|chunk| {
            let rest = chunk.trim_start_matches(|c: char| c != '"');
            let rest = rest.strip_prefix('"')?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vault_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  domain        TEXT NOT NULL,
  profile       TEXT NOT NULL DEFAULT 'default',
  cookies       BLOB NOT NULL,
  local_storage BLOB NOT NULL DEFAULT x'',
  tokens        TEXT,
  expires_at    INTEGER,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (domain, profile)
);
CREATE TABLE IF NOT EXISTS credentials (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  domain     TEXT NOT NULL,
  label      TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'login',
  fields     BLOB NOT NULL,
  notes      TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
";

// ---- UI session cookies and login rate limiting (in-memory, daemon-lifetime) ----

fn ui_sessions() -> &'static Mutex<HashSet<(String, Instant)>> {
    static SET: OnceLock<Mutex<HashSet<(String, Instant)>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

fn login_attempts() -> &'static Mutex<VecDeque<Instant>> {
    static Q: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());
    &Q
}

const UI_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_LOGIN_ATTEMPTS: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(300);

/// A fresh UI session token (call only after the password checked out).
pub fn ui_session_create() -> String {
    let token = format!("{}-{}", random_token(), hex_encode(&rand_bytes::<16>()));
    ui_sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((token.clone(), Instant::now()));
    token
}

pub fn ui_session_valid(token: Option<&str>) -> bool {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return false;
    };
    let mut sessions = ui_sessions().lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|(_t, at)| at.elapsed() < UI_SESSION_TTL);
    sessions.iter().any(|(t, _)| t == token)
}

pub fn ui_session_drop(token: Option<&str>) {
    if let Some(token) = token {
        ui_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(t, _)| t != token);
    }
}

/// Whether another login attempt is allowed right now, and if it is, record
/// this one. Naive global limiter: the UI is behind nginx and one person
/// uses it, so global is the honest scope.
pub fn login_allowed() -> bool {
    let mut attempts = login_attempts().lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = Instant::now() - LOGIN_WINDOW;
    attempts.retain(|at| *at > cutoff);
    if attempts.len() >= MAX_LOGIN_ATTEMPTS {
        return false;
    }
    attempts.push_back(Instant::now());
    true
}

/// The `cu_vault_session` value out of a `Cookie:` header, if any.
pub fn cookie_token(cookie_header: &str) -> Option<String> {
    for pair in cookie_header.split(';') {
        let (name, value) = pair.trim().split_once('=')?;
        if name == "cu_vault_session" {
            return Some(value.to_string());
        }
    }
    None
}

/// Standard base64, for carrying JSON through a `Runtime.evaluate` script
/// without quoting hazards.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("cu-vault-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open(&dir).expect("opens")
    }

    #[test]
    fn seal_roundtrips_and_differs_per_call() {
        let key: [u8; 32] = rand_bytes();
        let a = seal(&key, b"secret value").unwrap();
        let b = seal(&key, b"secret value").unwrap();
        assert_ne!(a, b, "a fresh nonce must change the ciphertext");
        assert_eq!(unseal(&key, &a).unwrap(), "secret value");
    }

    #[test]
    fn credentials_roundtrip_without_leaking_values() {
        let vault = tmp_vault("creds");
        let fields = r#"[{"name":"user","value":"jaime","secret":false,"type":"text"},
                          {"name":"pass","value":"hunter2","secret":true,"type":"password"}]"#;
        let id = vault
            .put_credential(None, "example.com", "main", fields, "")
            .unwrap();
        let listed = vault.list_credentials("").unwrap();
        assert!(!listed.contains("hunter2"), "the list never shows values");
        assert!(listed.contains("user"), "field names are listed");
        let revealed = vault.get_credential(id).unwrap().unwrap();
        assert!(revealed.contains("hunter2"));
    }

    #[test]
    fn sessions_match_subdomains() {
        assert!(domain_matches("app.example.com", "example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("notexample.com", "example.com"));
    }

    #[test]
    fn master_password_verifies_and_rejects() {
        let vault = tmp_vault("pw");
        assert!(!vault.has_password());
        vault.set_password(None, "correct horse").unwrap();
        assert!(vault.has_password());
        assert!(vault.verify_password("correct horse"));
        assert!(!vault.verify_password("wrong"));
        assert!(vault.set_password(None, "second").is_err());
        vault
            .set_password(Some("correct horse"), "battery")
            .unwrap();
        assert!(vault.verify_password("battery"));
    }

    #[test]
    fn ui_sessions_expire_and_drop() {
        let token = ui_session_create();
        assert!(ui_session_valid(Some(&token)));
        ui_session_drop(Some(&token));
        assert!(!ui_session_valid(Some(&token)));
        assert!(!ui_session_valid(Some("bogus")));
        assert!(!ui_session_valid(None));
    }
}
