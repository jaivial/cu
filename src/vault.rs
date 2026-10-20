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
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
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
        vault.migrate(&conn)?;
        // The secret-requests DDL goes first so the migration (which may
        // have just created the table in the current shape) and a fresh
        // database end up with the same schema.
        conn.execute_batch(&format!("{SECRET_REQUESTS_DDL}{OTP_REQUESTS_DDL}{SCHEMA}"))
            .map_err(|e| format!("vault schema: {e}"))?;
        let _ = conn.execute(
            "INSERT OR REPLACE INTO vault_meta(key, value) VALUES ('schema_version', '1')",
            [],
        );
        Ok(vault)
    }

    /// Bring a database written by an older build up to the current schema.
    /// `CREATE TABLE IF NOT EXISTS` silently keeps an old shape, so a table
    /// that predates the current `secret_requests` columns is renamed and
    /// its rows copied across before the schema runs.
    fn migrate(&self, conn: &Connection) -> Result<(), String> {
        let columns: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(secret_requests)")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let current = !columns.is_empty()
            && columns.iter().any(|c| c == "requested_by")
            && columns.iter().any(|c| c == "status");
        if columns.is_empty() || current {
            return Ok(());
        }
        conn.execute(
            "ALTER TABLE secret_requests RENAME TO secret_requests_old",
            [],
        )
        .map_err(|e| e.to_string())?;
        // The rename freed the name, so the current shape can be created
        // here, before the copy — the full schema batch runs after us.
        conn.execute_batch(SECRET_REQUESTS_DDL)
            .map_err(|e| e.to_string())?;
        // Rows are only worth copying when the old shape is the one this
        // project once shipped; anything stranger is dropped with the table.
        if columns.contains(&"requester".to_string()) && columns.contains(&"state".to_string()) {
            conn.execute(
                "INSERT INTO secret_requests
                     (id, domain, reason, requested_by, status, created_at, decided_at)
                 SELECT id, domain, reason, requester, state, created_at, decided_at
                 FROM secret_requests_old",
                [],
            )
            .map_err(|e| e.to_string())?;
        }
        conn.execute("DROP TABLE secret_requests_old", [])
            .map_err(|e| e.to_string())?;
        Ok(())
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

    // ---- secret requests (an agent asks, a person approves in the UI) ----

    /// Record an agent's request for a secret covering `domain`. The request
    /// carries no secret: it is a question a person answers in the vault UI.
    pub fn create_request(
        &self,
        domain: &str,
        reason: &str,
        requested_by: &str,
    ) -> Result<i64, String> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO secret_requests(domain, reason, requested_by, status, created_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            rusqlite::params![domain, reason, requested_by, now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    /// Requests newest first, optionally filtered by status, without secrets.
    pub fn list_requests(&self, status: Option<&str>) -> Result<String, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, domain, reason, requested_by, status, credential_id, created_at, decided_at
                 FROM secret_requests ORDER BY id DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, domain, reason, requested_by, st, credential_id, created_at, decided_at) =
                row.map_err(|e| e.to_string())?;
            if let Some(want) = status
                && st != want
            {
                continue;
            }
            out.push(format!(
                "{{\"id\":{id},\"domain\":{},\"reason\":{},\"requested_by\":{},\"status\":\"{st}\",\
                 \"credential_id\":{},\"created_at\":{created_at},\"decided_at\":{}}}",
                json_string(&domain),
                json_string(&reason),
                json_string(&requested_by),
                credential_id
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "null".into()),
                decided_at
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "null".into()),
            ));
        }
        Ok(format!("{{\"requests\":[{}]}}", out.join(",")))
    }

    /// One request, expired if it sat pending too long. An approved request
    /// carries the credential with its values revealed — that reveal is the
    /// whole point of the flow, and the routes that reach here are the ones
    /// the bearer token or a live UI session already guard.
    pub fn get_request(&self, id: i64) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT domain, reason, requested_by, status, credential_id, created_at, decided_at
                 FROM secret_requests WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .ok();
        let Some((domain, reason, requested_by, mut st, credential_id, created_at, decided_at)) =
            row
        else {
            return Ok(None);
        };
        if st == "pending" && now() - created_at > REQUEST_TTL_SECS {
            conn.execute(
                "UPDATE secret_requests SET status='expired' WHERE id=?1 AND status='pending'",
                [id],
            )
            .map_err(|e| e.to_string())?;
            st = "expired".into();
        }
        let credential = match (st.as_str(), credential_id) {
            ("approved", Some(cid)) => self.get_credential(cid)?,
            _ => None,
        };
        Ok(Some(format!(
            "{{\"id\":{id},\"domain\":{},\"reason\":{},\"requested_by\":{},\"status\":\"{st}\",\
             \"credential_id\":{},\"created_at\":{created_at},\"decided_at\":{},\"credential\":{}}}",
            json_string(&domain),
            json_string(&reason),
            json_string(&requested_by),
            credential_id
                .map(|c| c.to_string())
                .unwrap_or_else(|| "null".into()),
            decided_at
                .map(|d| d.to_string())
                .unwrap_or_else(|| "null".into()),
            credential.unwrap_or_else(|| "null".into()),
        )))
    }

    /// Approve or deny a pending request. Approval must name (or imply) the
    /// credential to hand over; nothing is revealed here, only linked.
    pub fn decide_request(
        &self,
        id: i64,
        approve: bool,
        credential_id: Option<i64>,
    ) -> Result<bool, String> {
        if approve && credential_id.is_none() {
            return Err(
                "approval needs a credential_id (none was given and none is implied)".to_string(),
            );
        }
        let conn = self.connect()?;
        let n = conn
            .execute(
                "UPDATE secret_requests SET status=?2, credential_id=COALESCE(?3, credential_id), \
                 decided_at=?4 WHERE id=?1 AND status='pending'",
                rusqlite::params![
                    id,
                    if approve { "approved" } else { "denied" },
                    credential_id,
                    now()
                ],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            // Distinguish "already decided" from "never existed" for the API.
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM secret_requests WHERE id = ?1",
                    [id],
                    |r| r.get::<_, i64>(0).map(|c| c > 0),
                )
                .unwrap_or(false);
            if exists {
                return Err("request is not pending any more".into());
            }
            return Ok(false);
        }
        Ok(true)
    }

    // ---- one-time-code hand-offs (an agent waits, a person answers) ----

    /// Open a one-time-code hand-off for `domain`. Returns the id, the URL
    /// token and the expiry. The token itself is never stored — only its
    /// SHA-256 — so a database read cannot mint answer links, and the link
    /// the agent hands to the person is the only copy.
    pub fn create_otp_request(
        &self,
        domain: &str,
        reason: &str,
        kind: &str,
        requested_by: &str,
        ttl_secs: i64,
    ) -> Result<(i64, String, i64), String> {
        let kind = match kind {
            "code" | "setup" => kind,
            _ => return Err("kind must be code or setup".into()),
        };
        let ttl = ttl_secs.clamp(OTP_REQUEST_MIN_TTL_SECS, OTP_REQUEST_MAX_TTL_SECS);
        let conn = self.connect()?;
        reap_otp_requests(&conn);
        let token = format!("otp-{}", hex_encode(&rand_bytes::<24>()));
        let at = now();
        conn.execute(
            "INSERT INTO otp_requests(token_hash, domain, reason, kind, requested_by,
                                      status, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
            rusqlite::params![
                token_hash(&token),
                domain,
                reason,
                kind,
                requested_by,
                at,
                at + ttl
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok((conn.last_insert_rowid(), token, at + ttl))
    }

    /// What the temporary page shows: the metadata of the request `token`
    /// names, never its response or seed. Lazily expires a stale pending row.
    pub fn otp_request_by_token(&self, token: &str) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT id, domain, reason, kind, requested_by, status, expires_at,
                        seed IS NOT NULL
                 FROM otp_requests WHERE token_hash = ?1",
                [token_hash(token)],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, bool>(7)?,
                    ))
                },
            )
            .ok();
        let Some((id, domain, reason, kind, requested_by, status, expires_at, has_seed)) = row
        else {
            return Ok(None);
        };
        let status = expire_if_stale(&conn, "otp_requests", id, &status, expires_at);
        Ok(Some(format!(
            "{{\"id\":{id},\"domain\":{},\"reason\":{},\"kind\":{},\"requested_by\":{},\"status\":{},\"expires_in\":{},\"has_seed\":{}}}",
            json_string(&domain),
            json_string(&reason),
            json_string(&kind),
            json_string(&requested_by),
            json_string(&status),
            (expires_at - now()).max(0),
            has_seed,
        )))
    }

    /// The person's answer. Single-use and expiry are enforced in the
    /// UPDATE's WHERE, not in a check that could race: a token that is
    /// spent, expired or unknown updates zero rows.
    pub fn answer_otp_request(&self, token: &str, response: &str) -> Result<bool, String> {
        let response = response.trim();
        if response.is_empty() || response.len() > 128 {
            return Err("a code is 1-128 characters".into());
        }
        let sealed = seal(&self.key, response.as_bytes())?;
        let n = self
            .connect()?
            .execute(
                "UPDATE otp_requests SET status='answered', response=?2, answered_at=?3
                 WHERE token_hash=?1 AND status='pending' AND expires_at > ?3",
                rusqlite::params![token_hash(token), sealed, now()],
            )
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// The agent's poll. With `collect`, an answered response is returned
    /// once and only once: the row flips to `collected` and the sealed value
    /// is deleted in the same transaction, so a code does not linger at rest
    /// after the agent has it.
    pub fn poll_otp_request(&self, id: i64, collect: bool) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT status, expires_at, response FROM otp_requests WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .ok();
        let Some((status, expires_at, response)) = row else {
            return Ok(None);
        };
        let status = expire_if_stale(&conn, "otp_requests", id, &status, expires_at);
        if collect && status == "answered" {
            // Claim the row first, atomically: exactly one collector flips
            // answered -> collecting, so the code is delivered once even
            // under concurrent polls.
            let claimed = conn
                .execute(
                    "UPDATE otp_requests SET status='collecting'
                     WHERE id=?1 AND status='answered'",
                    [id],
                )
                .map_err(|e| e.to_string())?;
            if claimed == 0 {
                return Ok(Some(format!(
                    "{{\"id\":{id},\"status\":\"collected\",\"expires_in\":0}}"
                )));
            }
            let response = match response {
                Some(sealed) => unseal(&self.key, &sealed)?,
                None => return Err("answered request without a response".into()),
            };
            // Build the reply before wiping the stored value: a body that
            // failed to build must not cost the person their answer.
            let body = format!(
                "{{\"id\":{id},\"status\":\"collected\",\"expires_in\":0,\"response\":{}}}",
                json_string(&response),
            );
            conn.execute(
                "UPDATE otp_requests SET status='collected', response=NULL, seed=NULL
                 WHERE id=?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
            return Ok(Some(body));
        }
        Ok(Some(format!(
            "{{\"id\":{id},\"status\":{},\"expires_in\":{}}}",
            json_string(&status),
            (expires_at - now()).max(0),
        )))
    }

    /// Give up a pending hand-off (the agent took another path).
    pub fn cancel_otp_request(&self, id: i64) -> Result<bool, String> {
        let n = self
            .connect()?
            .execute(
                "UPDATE otp_requests SET status='cancelled', response=NULL, seed=NULL
                 WHERE id=?1 AND status='pending'",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// Recent hand-offs for the vault UI, newest first, never with secrets.
    pub fn list_otp_requests(&self) -> Result<String, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, domain, reason, kind, requested_by, status, created_at, expires_at
                 FROM otp_requests ORDER BY id DESC LIMIT 50",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, domain, reason, kind, requested_by, status, created_at, expires_at) =
                row.map_err(|e| e.to_string())?;
            let status = expire_if_stale(&conn, "otp_requests", id, &status, expires_at);
            out.push(format!(
                "{{\"id\":{id},\"domain\":{},\"reason\":{},\"kind\":{},\"requested_by\":{},\"status\":{},\"created_at\":{created_at},\"expires_at\":{expires_at}}}",
                json_string(&domain),
                json_string(&reason),
                json_string(&kind),
                json_string(&requested_by),
                json_string(&status),
            ));
        }
        Ok(format!("{{\"requests\":[{}]}}", out.join(",")))
    }

    /// The domain a hand-off names (for the capture route's attach step).
    pub fn otp_request_domain(&self, id: i64) -> Option<String> {
        self.connect()
            .ok()?
            .query_row(
                "SELECT domain FROM otp_requests WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }

    /// Attach the seed a QR capture decoded to a pending setup request, so
    /// the temporary page can render it as a QR for the person's own
    /// authenticator. Sealed like any other field.
    pub fn set_otp_request_seed(&self, id: i64, uri: &str) -> Result<(), String> {
        let sealed = seal(&self.key, uri.as_bytes())?;
        let n = self
            .connect()?
            .execute(
                "UPDATE otp_requests SET seed=?2 WHERE id=?1 AND status='pending'",
                rusqlite::params![id, sealed],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(
                "the hand-off is no longer pending (answered, expired or cancelled); seed not attached"
                    .into(),
            );
        }
        Ok(())
    }

    /// The seed of a pending, unexpired request, for the QR the temporary
    /// page renders. Only the person holding the URL token and a live UI
    /// session reaches this (the route is UI-only).
    pub fn otp_request_seed_by_token(&self, token: &str) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT seed FROM otp_requests
                 WHERE token_hash=?1 AND status='pending' AND expires_at > ?2",
                rusqlite::params![token_hash(token), now()],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .ok()
            .flatten();
        row.map(|sealed| unseal(&self.key, &sealed)).transpose()
    }

    /// Merge a TOTP seed into a credential's fields. An existing `totp`
    /// field is a live 2FA setup: replacing it needs `force` - the explicit
    /// confirmation of whoever runs the capture - otherwise this refuses
    /// instead of destroying a working setup. Returns the credential id
    /// and, when a replacement did happen, the account the previous seed
    /// named so the caller can surface it.
    ///
    /// With `credential_id: None` the domain must resolve to exactly one
    /// credential; with none at all a fresh credential holds the seed,
    /// labelled with the account the URI names.
    pub fn put_totp(
        &self,
        credential_id: Option<i64>,
        domain: &str,
        label: &str,
        uri: &str,
        force: bool,
    ) -> Result<(i64, Option<String>), String> {
        let (id, domain, label, notes, fields) = match credential_id {
            Some(id) => {
                let full = self.get_credential(id)?.ok_or("no such credential")?;
                let fields =
                    crate::server::json_array(&full, "fields").unwrap_or_else(|| "[]".into());
                (
                    Some(id),
                    crate::server::json_string_value(&full, "domain").unwrap_or_default(),
                    crate::server::json_string_value(&full, "label").unwrap_or_default(),
                    crate::server::json_string_value(&full, "notes").unwrap_or_default(),
                    fields,
                )
            }
            None => {
                let matches = self.credentials_for_domain(domain)?;
                match matches.len() {
                    0 => (
                        None,
                        domain.to_string(),
                        label.to_string(),
                        String::new(),
                        "[]".into(),
                    ),
                    1 => {
                        let full = self
                            .get_credential(matches[0].0)?
                            .ok_or("no such credential")?;
                        (
                            Some(matches[0].0),
                            crate::server::json_string_value(&full, "domain").unwrap_or_default(),
                            crate::server::json_string_value(&full, "label").unwrap_or_default(),
                            crate::server::json_string_value(&full, "notes").unwrap_or_default(),
                            crate::server::json_array(&full, "fields")
                                .unwrap_or_else(|| "[]".into()),
                        )
                    }
                    _ => {
                        return Err(
                            "several credentials cover this domain; pass credential_id".into()
                        );
                    }
                }
            }
        };
        let existing: Vec<String> = crate::server::json_objects(&fields)
            .into_iter()
            .filter(|f| {
                crate::server::json_string_value(f, "name")
                    .map(|n| n.eq_ignore_ascii_case("totp"))
                    .unwrap_or(false)
            })
            .collect();
        if !existing.is_empty() && !force {
            // Name the setup at stake (metadata only, never the seed) so the
            // caller can put the replacement in front of the user and get
            // their explicit confirmation; a flag alone is not consent.
            let account = existing
                .first()
                .and_then(|f| crate::server::json_string_value(f, "value"))
                .and_then(|old_uri| crate::otp::parse(&old_uri).ok())
                .map(|old| match (old.issuer.is_empty(), old.account.is_empty()) {
                    (false, false) => format!("{}:{}", old.issuer, old.account),
                    (false, true) => old.issuer,
                    _ => old.account,
                })
                .unwrap_or_else(|| "an unknown account".into());
            return Err(format!(
                "this credential already holds a live TOTP seed for {account}; replacing an \
                 active 2FA setup needs the user's explicit confirmation - confirm with them, \
                 then re-run with force (cu otp capture --replace)"
            ));
        }
        // The account the outgoing seed named, for the caller's report.
        // Metadata only - the seed itself is never returned.
        let previous_account = existing
            .first()
            .and_then(|f| crate::server::json_string_value(f, "value"))
            .and_then(|old_uri| crate::otp::parse(&old_uri).ok())
            .map(|old| old.account);
        let mut kept: Vec<String> = crate::server::json_objects(&fields)
            .into_iter()
            .filter(|f| {
                crate::server::json_string_value(f, "name")
                    .map(|n| !n.eq_ignore_ascii_case("totp"))
                    .unwrap_or(true)
            })
            .collect();
        kept.push(format!(
            "{{\"name\":\"totp\",\"value\":{},\"secret\":true,\"type\":\"password\"}}",
            json_string(uri)
        ));
        let id = self.put_credential(
            id,
            &domain,
            &label,
            &format!("[{}]", kept.join(",")),
            &notes,
        )?;
        Ok((id, previous_account))
    }

    /// The current code of the credential's stored `totp` field, if it has
    /// one that parses: (code, seconds left in its step).
    pub fn totp_for_credential(&self, id: i64) -> Result<Option<(String, u64)>, String> {
        let Some(full) = self.get_credential(id)? else {
            return Ok(None);
        };
        let fields = crate::server::json_array(&full, "fields").unwrap_or_default();
        let Some(uri) = crate::server::json_objects(&fields)
            .into_iter()
            .find(|f| {
                crate::server::json_string_value(f, "name")
                    .map(|n| n.eq_ignore_ascii_case("totp"))
                    .unwrap_or(false)
            })
            .and_then(|f| crate::server::json_string_value(&f, "value"))
        else {
            return Ok(None);
        };
        let totp = crate::otp::parse(&uri)?;
        Ok(Some(totp.code()))
    }

    /// The current code from the newest credential covering `domain` that
    /// holds a parseable `totp` field: (credential id, code, seconds left).
    pub fn totp_for_domain(&self, domain: &str) -> Result<Option<(i64, String, u64)>, String> {
        for (id, _, _) in self.credentials_for_domain(domain)? {
            if let Some((code, left)) = self.totp_for_credential(id)? {
                return Ok(Some((id, code, left)));
            }
        }
        Ok(None)
    }

    /// Credentials for `domain` with their field values, decrypted, most
    /// recently updated first. For the basic-auth responder and the
    /// approve-with-default: both are server-side, and both are guarded
    /// routes.
    pub fn credentials_for_domain(
        &self,
        domain: &str,
    ) -> Result<Vec<(i64, String, String)>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT id, domain, fields FROM credentials ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, stored, fields) = row.map_err(|e| e.to_string())?;
            if domain_matches(domain, &stored) {
                out.push((id, stored, unseal(&self.key, &fields).unwrap_or_default()));
            }
        }
        Ok(out)
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
        // Sessions opened with the old password do not outlive it: whoever
        // knows the new one must be the one holding a cookie, not everyone
        // who was ever logged in before the change.
        self.ui_sessions_drop_all();
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

/// A one-time-code hand-off lives ten minutes unless the agent says
/// otherwise; anything between one minute and fifteen is honoured. Short on
/// purpose: a code typed into a stale page is a code typed into a stale page.
pub const OTP_REQUEST_TTL_SECS: i64 = 10 * 60;
pub const OTP_REQUEST_MIN_TTL_SECS: i64 = 60;
pub const OTP_REQUEST_MAX_TTL_SECS: i64 = 15 * 60;

/// Flip a still-`pending` row to `expired` when its time ran out, returning
/// the status to report. Shared by the token and id lookups.
fn expire_if_stale(
    conn: &Connection,
    table: &str,
    id: i64,
    status: &str,
    expires_at: i64,
) -> String {
    if status == "pending" && now() >= expires_at {
        let _ = conn.execute(
            &format!("UPDATE {table} SET status='expired', response=NULL, seed=NULL WHERE id=?1 AND status='pending'"),
            [id],
        );
        return "expired".into();
    }
    status.to_string()
}

/// Housekeeping on each new hand-off: terminal rows an hour past expiry go,
/// and no terminal row keeps a response or seed at rest. Metadata (who
/// asked, for what, how it ended) stays as the audit trail.
fn reap_otp_requests(conn: &Connection) {
    let cutoff = now() - 3600;
    let _ = conn.execute("DELETE FROM otp_requests WHERE expires_at < ?1", [cutoff]);
    let _ = conn.execute(
        "UPDATE otp_requests SET response=NULL, seed=NULL
         WHERE status IN ('collected', 'cancelled', 'expired')
           AND (response IS NOT NULL OR seed IS NOT NULL)",
        [],
    );
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// How long a secret request may sit pending before reading it reports (and
/// marks) it expired: an approval a person never got to see should not be
/// answered by an agent that checks back the next day.
pub const REQUEST_TTL_SECS: i64 = 15 * 60;

const USER_FIELD_NAMES: &[&str] = &["username", "user", "email", "login", "login_username"];
const PASSWORD_FIELD_NAMES: &[&str] = &["password", "pass", "pwd"];

/// The `(user, password)` a fields JSON array carries, for HTTP basic auth:
/// the first field whose name reads as a username and the first whose name
/// reads as a password, case-insensitively. `None` unless both are there.
pub fn basic_auth_pair(fields: &str) -> Option<(String, String)> {
    let find = |names: &[&str]| {
        fields.split("\"name\"").skip(1).find_map(|chunk| {
            let rest = chunk.trim_start_matches(|c: char| c != '"');
            let rest = rest.strip_prefix('"')?;
            let end = string_body_end(rest)?;
            let name = rest[..end].to_ascii_lowercase();
            if !names.contains(&name.as_str()) {
                return None;
            }
            // The value follows the next `"value"` in this chunk.
            let value_part = rest[end..].split("\"value\"").nth(1)?;
            let value_rest = value_part.trim_start_matches(|c: char| c != '"');
            let value_rest = value_rest.strip_prefix('"')?;
            let value_end = string_body_end(value_rest)?;
            Some(unescape_json(&value_rest[..value_end]))
        })
    };
    let user = find(USER_FIELD_NAMES)?;
    let password = find(PASSWORD_FIELD_NAMES)?;
    Some((user, password))
}

/// Index of the closing quote of a JSON string body (the text between the
/// quotes), honouring `\"` escapes.
fn string_body_end(body: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else {
            match c {
                '\\' => escaped = true,
                '"' => return Some(i),
                _ => {}
            }
        }
    }
    None
}

/// The JSON escapes a sealed fields value may carry. The vault's hand-rolled
/// parser never unescaped, and a literal `\n` in a password would not
/// authenticate.
fn unescape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(c) = char::from_u32(code)
                {
                    out.push(c);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The current `secret_requests` shape, on its own so the migration can
/// create it mid-flight (see [`Vault::migrate`]).
const SECRET_REQUESTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS secret_requests (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  domain        TEXT NOT NULL,
  reason        TEXT NOT NULL DEFAULT '',
  requested_by  TEXT NOT NULL DEFAULT '',
  status        TEXT NOT NULL DEFAULT 'pending',
  credential_id INTEGER,
  created_at    INTEGER NOT NULL,
  decided_at    INTEGER
);
";

const OTP_REQUESTS_DDL: &str = "
CREATE TABLE IF NOT EXISTS otp_requests (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  token_hash    TEXT NOT NULL UNIQUE,
  domain        TEXT NOT NULL,
  reason        TEXT NOT NULL DEFAULT '',
  kind          TEXT NOT NULL DEFAULT 'code',
  requested_by  TEXT NOT NULL DEFAULT '',
  status        TEXT NOT NULL DEFAULT 'pending',
  response      BLOB,
  seed          BLOB,
  credential_id INTEGER,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  answered_at   INTEGER
);
";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vault_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ui_sessions (
  token_hash TEXT PRIMARY KEY,
  remember   INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
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

// ---- UI session cookies and login rate limiting ----

fn login_attempts() -> &'static Mutex<VecDeque<Instant>> {
    static Q: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());
    &Q
}

/// A UI session that was not asked to be remembered: it lives this long from
/// the last request that used it. Twelve hours is enough to close the laptop
/// overnight without losing the vault, and short enough that a session left
/// open on a shared machine is gone by the next working day.
pub const UI_SESSION_TTL: i64 = 12 * 3600;
/// With "remember this session" ticked: a month. Sliding like the short one,
/// so an active month-long session is never cut off mid-work.
pub const UI_SESSION_TTL_REMEMBER: i64 = 30 * 24 * 3600;

const MAX_LOGIN_ATTEMPTS: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(300);

/// A live UI session, as [`Vault::ui_session_check`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiSession {
    /// Whether the person ticked "remember this session" at login.
    pub remember: bool,
    /// When this session stops being valid (unix seconds).
    pub expires_at: i64,
}

impl UiSession {
    /// Seconds left, floored at zero — what the cookie's `Max-Age` should be.
    pub fn max_age(&self) -> i64 {
        (self.expires_at - now()).max(0)
    }
}

/// The lifetime a session gets from its `remember` flag.
pub fn ui_session_ttl(remember: bool) -> i64 {
    if remember {
        UI_SESSION_TTL_REMEMBER
    } else {
        UI_SESSION_TTL
    }
}

/// A fresh UI session token (call only after the password checked out). Only
/// its hash is stored, so a database that leaks hands over no live session.
pub fn ui_session_token() -> String {
    format!("{}-{}", random_token(), hex_encode(&rand_bytes::<16>()))
}

impl Vault {
    /// Open a UI session for `remember` and return its token.
    pub fn ui_session_create(&self, remember: bool) -> Result<String, String> {
        let token = ui_session_token();
        let at = now();
        let ttl = ui_session_ttl(remember);
        self.connect()?
            .execute(
                "INSERT INTO ui_sessions(token_hash, remember, created_at, last_seen, expires_at)
                 VALUES (?1, ?2, ?3, ?3, ?4)",
                rusqlite::params![token_hash(&token), i64::from(remember), at, at + ttl],
            )
            .map_err(|e| e.to_string())?;
        Ok(token)
    }

    /// Whether `token` names a live session, sliding its expiry forward.
    ///
    /// The renewal is the point: someone working in the vault all afternoon
    /// was being logged out at the twelve-hour mark with no warning, because
    /// the lifetime was measured from login rather than from last use. Every
    /// authenticated request moves the deadline instead, so a session dies
    /// after a *period of inactivity* — the behaviour every other session
    /// cookie in the world has — and not after a fixed span of wall clock.
    pub fn ui_session_check(&self, token: Option<&str>) -> Option<UiSession> {
        let token = token.filter(|t| !t.is_empty())?;
        let hash = token_hash(token);
        let at = now();
        let conn = self.connect().ok()?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT remember, expires_at FROM ui_sessions WHERE token_hash = ?1",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (remember, expires_at) = row?;
        if expires_at <= at {
            // Expired: drop it here rather than waiting for a sweep, so a dead
            // token cannot be told apart from one that never existed.
            let _ = conn.execute("DELETE FROM ui_sessions WHERE token_hash = ?1", [&hash]);
            return None;
        }
        let ttl = ui_session_ttl(remember != 0);
        let _ = conn.execute(
            "UPDATE ui_sessions SET last_seen = ?2, expires_at = ?3 WHERE token_hash = ?1",
            rusqlite::params![hash, at, at + ttl],
        );
        Some(UiSession {
            remember: remember != 0,
            expires_at: at + ttl,
        })
    }

    /// Whether `token` names a live session, without renewing it: the check
    /// for surfaces that ask "am I logged in?" and must not quietly extend
    /// the life of a session nobody is using.
    pub fn ui_session_valid(&self, token: Option<&str>) -> bool {
        self.ui_session_peek(token).is_some()
    }

    /// The stored session without touching its expiry.
    fn ui_session_peek(&self, token: Option<&str>) -> Option<UiSession> {
        let hash = token_hash(token?);
        let at = now();
        self.connect()
            .ok()?
            .query_row(
                "SELECT remember, expires_at FROM ui_sessions WHERE token_hash = ?1",
                [&hash],
                |r| {
                    Ok(UiSession {
                        remember: r.get::<_, i64>(0)? != 0,
                        expires_at: r.get::<_, i64>(1)?,
                    })
                },
            )
            .ok()
            .filter(|s| s.expires_at > at)
    }

    /// End the session `token` names (logging out).
    pub fn ui_session_drop(&self, token: Option<&str>) {
        let Some(hash) = token.filter(|t| !t.is_empty()).map(token_hash) else {
            return;
        };
        if let Ok(conn) = self.connect() {
            let _ = conn.execute("DELETE FROM ui_sessions WHERE token_hash = ?1", [&hash]);
        }
    }

    /// Forget every session — for when the master password changes.
    pub fn ui_sessions_drop_all(&self) {
        if let Ok(conn) = self.connect() {
            let _ = conn.execute("DELETE FROM ui_sessions", []);
        }
    }

    /// Delete sessions that expired more than a day ago, so a long-lived
    /// vault does not accumulate rows nobody will ever look up again.
    pub fn ui_sessions_reap(&self) {
        if let Ok(conn) = self.connect() {
            let _ = conn.execute(
                "DELETE FROM ui_sessions WHERE expires_at < ?1",
                [now() - 24 * 3600],
            );
        }
    }
}

/// What a UI session token is stored as. The token itself is a bearer
/// credential: a copy of `vault.db` must not be a set of live sessions.
fn token_hash(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(token.as_bytes());
    hex_encode(&hasher.finalize())
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
        let vault = tmp_vault("sess");
        let token = vault.ui_session_create(false).expect("session opens");
        assert!(vault.ui_session_valid(Some(&token)));
        vault.ui_session_drop(Some(&token));
        assert!(!vault.ui_session_valid(Some(&token)));
        assert!(!vault.ui_session_valid(Some("bogus")));
        assert!(!vault.ui_session_valid(None));
    }

    #[test]
    fn remember_buys_a_month_and_the_default_buys_twelve_hours() {
        let vault = tmp_vault("remember");
        let short = vault.ui_session_create(false).expect("session");
        let long = vault.ui_session_create(true).expect("session");
        let short = vault.ui_session_check(Some(&short)).expect("live");
        let long = vault.ui_session_check(Some(&long)).expect("live");
        assert!(!short.remember);
        assert!(long.remember);
        // Roughly the two lifetimes, not exactly: the check slid each one.
        assert!(
            (ui_session_ttl(false) - 60..=ui_session_ttl(false)).contains(&short.max_age()),
            "short session had {}s left",
            short.max_age()
        );
        assert!(
            (ui_session_ttl(true) - 60..=ui_session_ttl(true)).contains(&long.max_age()),
            "remembered session had {}s left",
            long.max_age()
        );
    }

    #[test]
    fn a_session_slides_forward_so_working_in_the_vault_never_expires() {
        let vault = tmp_vault("slide");
        let token = vault.ui_session_create(false).expect("session");
        // Wind the stored expiry back to almost nothing: a session created a
        // whole lifetime ago, used a moment ago.
        vault
            .connect()
            .unwrap()
            .execute(
                "UPDATE ui_sessions SET expires_at = ?1 WHERE token_hash = ?2",
                rusqlite::params![now() + 5, token_hash(&token)],
            )
            .unwrap();
        let session = vault.ui_session_check(Some(&token)).expect("still live");
        assert!(
            session.max_age() > ui_session_ttl(false) / 2,
            "the deadline moved forward instead of running out: {}s left",
            session.max_age()
        );
        // A peeking check, by contrast, leaves the deadline alone.
        vault
            .connect()
            .unwrap()
            .execute(
                "UPDATE ui_sessions SET expires_at = ?1 WHERE token_hash = ?2",
                rusqlite::params![now() + 5, token_hash(&token)],
            )
            .unwrap();
        assert!(vault.ui_session_valid(Some(&token)));
        let after = vault.ui_session_peek(Some(&token)).expect("live").max_age();
        assert!(after < 60, "peek must not extend: {after}s left");
    }

    #[test]
    fn an_expired_session_is_refused_and_erased() {
        let vault = tmp_vault("expired");
        let token = vault.ui_session_create(false).expect("session");
        vault
            .connect()
            .unwrap()
            .execute(
                "UPDATE ui_sessions SET expires_at = ?1 WHERE token_hash = ?2",
                rusqlite::params![now() - 1, token_hash(&token)],
            )
            .unwrap();
        assert!(vault.ui_session_check(Some(&token)).is_none());
        let left: i64 = vault
            .connect()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM ui_sessions WHERE token_hash = ?1",
                [&token_hash(&token)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "an expired session is not left lying in the table");
    }

    #[test]
    fn sessions_survive_a_restart_and_the_raw_token_is_never_stored() {
        let dir = std::env::temp_dir().join(format!(
            "cu-vault-persist-{}-{}",
            std::process::id(),
            ui_session_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let token = Vault::open(&dir)
            .expect("opens")
            .ui_session_create(true)
            .expect("session");
        // A brand-new `Vault` over the same directory is what a daemon restart
        // looks like: the session has to still be there.
        let reopened = Vault::open(&dir).expect("reopens");
        let session = reopened
            .ui_session_check(Some(&token))
            .expect("live after restart");
        assert!(session.remember);
        // And the database holds the hash, never the bearer token itself.
        let stored: String = reopened
            .connect()
            .unwrap()
            .query_row("SELECT token_hash FROM ui_sessions LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_ne!(stored, token);
        assert_eq!(stored, token_hash(&token));
        assert!(!stored.contains(&token));
    }

    #[test]
    fn changing_the_master_password_ends_every_session() {
        let vault = tmp_vault("pwrot");
        vault.set_password(None, "first").unwrap();
        let a = vault.ui_session_create(false).expect("session");
        let b = vault.ui_session_create(true).expect("session");
        vault.set_password(Some("first"), "second").unwrap();
        assert!(
            !vault.ui_session_valid(Some(&a)),
            "old cookie must not survive"
        );
        assert!(
            !vault.ui_session_valid(Some(&b)),
            "old cookie must not survive"
        );
    }

    #[test]
    fn reaping_only_removes_rows_that_died_over_a_day_ago() {
        let vault = tmp_vault("reap");
        let fresh = vault.ui_session_create(false).expect("session");
        let ancient = vault.ui_session_create(false).expect("session");
        vault
            .connect()
            .unwrap()
            .execute(
                "UPDATE ui_sessions SET expires_at = ?1 WHERE token_hash = ?2",
                rusqlite::params![now() - 48 * 3600, token_hash(&ancient)],
            )
            .unwrap();
        vault.ui_sessions_reap();
        assert!(vault.ui_session_valid(Some(&fresh)));
        assert!(!vault.ui_session_valid(Some(&ancient)));
    }

    #[test]
    fn an_old_secret_requests_table_is_migrated() {
        let dir = std::env::temp_dir().join(format!("cu-vault-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // What an unreleased build left behind: other column names.
        {
            let conn = Connection::open(dir.join("vault.db")).unwrap();
            conn.execute(
                "CREATE TABLE secret_requests (
                   id         INTEGER PRIMARY KEY AUTOINCREMENT,
                   domain     TEXT NOT NULL,
                   reason     TEXT NOT NULL DEFAULT '',
                   requester  TEXT NOT NULL DEFAULT '',
                   state      TEXT NOT NULL DEFAULT 'pending',
                   created_at INTEGER NOT NULL,
                   decided_at INTEGER
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO secret_requests(domain, reason, requester, state, created_at)
                 VALUES ('old.example.com', 'left over', 'agent-x', 'pending', 1)",
                [],
            )
            .unwrap();
        }
        let vault = Vault::open(&dir).expect("opens over the old shape");
        let listed = vault.list_requests(None).unwrap();
        assert!(listed.contains("old.example.com"), "{listed}");
        assert!(listed.contains("agent-x"));
        assert!(listed.contains("pending"));
        // And the migrated table behaves like a fresh one.
        let id = vault.create_request("example.com", "", "").unwrap();
        assert!(id > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const QA_FIELDS: &str = r#"[{"name":"user","value":"jaime","secret":false,"type":"text"},
                                {"name":"Password","value":"hunter2","secret":true,"type":"password"}]"#;

    #[test]
    fn secret_requests_roundtrip_and_gate_the_reveal() {
        let vault = tmp_vault("requests");
        let id = vault
            .create_request("example.com", "QA of the basic-auth flow", "agent-a")
            .unwrap();
        let listed = vault.list_requests(None).unwrap();
        assert!(listed.contains("example.com"));
        assert!(listed.contains("pending"));
        // Filtering works, and a list never carries a secret value.
        assert_eq!(
            vault
                .list_requests(Some("pending"))
                .unwrap()
                .matches("example.com")
                .count(),
            1
        );
        assert!(
            !vault
                .list_requests(Some("denied"))
                .unwrap()
                .contains("example.com")
        );

        // A pending request reveals nothing.
        let pending = vault.get_request(id).unwrap().unwrap();
        assert!(pending.contains("\"status\":\"pending\""));
        assert!(pending.contains("\"credential\":null"));

        // Approval without a credential is refused; with one it links it.
        assert!(vault.decide_request(id, true, None).is_err());
        let cid = vault
            .put_credential(None, "example.com", "main", QA_FIELDS, "")
            .unwrap();
        assert!(vault.decide_request(id, true, Some(cid)).unwrap());
        let approved = vault.get_request(id).unwrap().unwrap();
        assert!(approved.contains("\"status\":\"approved\""));
        assert!(
            approved.contains("hunter2"),
            "an approved request hands the credential over"
        );
        // Deciding twice is refused, and the first decision stands.
        assert!(
            vault
                .decide_request(id, false, None)
                .unwrap_err()
                .contains("not pending")
        );
        // Denial never reveals anything.
        let did = vault.create_request("other.com", "", "agent-b").unwrap();
        assert!(vault.decide_request(did, false, None).unwrap());
        let denied = vault.get_request(did).unwrap().unwrap();
        assert!(denied.contains("\"status\":\"denied\""));
        assert!(!vault.get_request(9999).unwrap().is_some());
    }

    #[test]
    fn an_approved_request_is_the_only_one_that_reveals() {
        let vault = tmp_vault("requests-reveal");
        let cid = vault
            .put_credential(None, "example.com", "main", QA_FIELDS, "")
            .unwrap();
        let id = vault.create_request("example.com", "", "agent").unwrap();
        assert!(vault.decide_request(id, true, Some(cid)).unwrap());
        let body = vault.get_request(id).unwrap().unwrap();
        // The request names its domain and the credential's domain agrees.
        assert!(body.contains("\"domain\":\"example.com\""));
    }

    #[test]
    fn basic_auth_pair_reads_user_and_password_fields() {
        let (user, pass) = basic_auth_pair(QA_FIELDS).unwrap();
        assert_eq!(user, "jaime");
        assert_eq!(pass, "hunter2");
        // JSON escapes in a stored value are decoded for the challenge.
        let escaped = r#"[{"name":"user","value":"jaime","secret":false},
                           {"name":"password","value":"h\u0075nter2","secret":true}]"#;
        assert_eq!(basic_auth_pair(escaped).unwrap().1, "hunter2");
        assert!(basic_auth_pair(r#"[{"name":"user","value":"x","secret":false}]"#).is_none());
        assert!(basic_auth_pair(r#"[{"name":"password","value":"x","secret":true}]"#).is_none());
        assert!(basic_auth_pair("not json").is_none());
        assert!(basic_auth_pair(
            r#"[{"name":"token","value":"abc","secret":true},{"name":"api_key","value":"k","secret":true}]"#
        )
        .is_none());
    }

    #[test]
    fn credentials_for_domain_match_subdomains() {
        let vault = tmp_vault("for-domain");
        vault
            .put_credential(None, "example.com", "main", QA_FIELDS, "")
            .unwrap();
        assert_eq!(
            vault
                .credentials_for_domain("app.example.com")
                .unwrap()
                .len(),
            1
        );
        assert!(
            vault
                .credentials_for_domain("notexample.com")
                .unwrap()
                .is_empty()
        );
    }
}
