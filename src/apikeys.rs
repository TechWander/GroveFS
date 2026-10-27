//! API-key authentication and a call-audit log for the MCP governance server.
//!
//! The `grovefs mcp` server is a control-plane entrance onto view governance. By
//! default it trusts whoever can spawn the process (the same OS-level boundary
//! as a local `mount`). That is fine for a single operator on one machine, but
//! an agent runtime often fronts the server to MANY distinct callers, each of
//! whom should be (a) authenticated by a revocable credential and (b) audited.
//! This module adds exactly that, WITHOUT changing the default (auth stays
//! opt-in via `grovefs mcp --require-api-key`, so the plain server and every
//! existing suite behave identically).
//!
//! Design:
//! - **Keys are stored hashed.** `grovefs apikey create` mints a random secret,
//!   persists only `SHA-256(secret)`, and prints the full plaintext key
//!   (`<id>:<secret>`) exactly once. A leaked `api_keys.db` therefore hands out
//!   nothing usable. The `id` half is only a lookup index (not a secret); the
//!   `secret` half is the bearer capability.
//! - **Authentication is per-session.** The stdio transport has no header
//!   channel, so the caller presents its key in the `initialize` params
//!   (`{"apiKey": "<id>:<secret>"}`); an operator may also pre-authenticate a
//!   whole server by exporting `GROVEFS_MCP_API_KEY` (for clients that cannot
//!   customize the handshake). `initialize`/`ping` stay open; `tools/list` and
//!   `tools/call` are refused until a key authenticates.
//! - **Every tool call is appended to `mcp_calls`** (timestamp, key id/label,
//!   tool, success, a compact detail) — attribution and a replayable audit trail,
//!   queryable with `grovefs apikey calls`.
//!
//! Storage mirrors [`crate::acl::AclStore`]: one local SQLite file at
//! `<base>/meta/api_keys.db`. It is deliberately SQLite-only — the MCP server
//! is metadata-only over a LOCAL store, so there is no PG path to keep in sync
//! here; a `meta-server`-style HA deployment shares this file like `acl.db`.
//!
//! Honest boundary: this authenticates CALLERS OF THE MCP GOVERNANCE API, it is
//! not the mount/data auth path (that stays the server's token/ACL). A caller
//! admitted by an API key can still only do what `grovefs view` does — governance
//! writes — and every such write is logged against that key.

use rusqlite::{params, Connection};
use std::path::Path;

/// A successfully authenticated key's identity (never the secret or hash).
#[derive(Debug, Clone)]
pub struct AuthedKey {
    pub id: String,
    pub label: String,
}

/// Operator-facing metadata about one stored key (no secret material).
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub id: String,
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// One recorded MCP tool invocation.
#[derive(Debug, Clone)]
pub struct CallLog {
    pub ts: String,
    pub key_id: Option<String>,
    pub key_label: Option<String>,
    pub method: String,
    pub tool: String,
    pub ok: bool,
    pub detail: String,
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Lowercase hex SHA-256 of a string (the at-rest key credential digest).
fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out[..]);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 hex chars (256 bits) of secret from two v4 UUIDs (already a dependency).
fn gen_secret() -> String {
    format!(
        "{:032x}{:032x}",
        uuid::Uuid::new_v4().as_u128(),
        uuid::Uuid::new_v4().as_u128(),
    )
}

/// The per-base API-key + call-log store (one SQLite file).
pub struct ApiKeyStore {
    conn: parking_lot::Mutex<Connection>,
}

impl ApiKeyStore {
    /// Open (and if necessary initialize) `<base>/meta/api_keys.db`. Creating
    /// the tables is idempotent; an empty `api_keys` table means "no caller has
    /// been issued a key yet" (which, under `--require-api-key`, locks out all
    /// gated calls until the operator mints one).
    pub fn open(base: &Path) -> rusqlite::Result<Self> {
        let path = base.join("meta").join("api_keys.db");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS api_keys (
                 id          TEXT PRIMARY KEY,
                 label       TEXT NOT NULL,
                 secret_hash TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 last_used_at TEXT,
                 revoked_at  TEXT
             );
             CREATE TABLE IF NOT EXISTS mcp_calls (
                 ts        TEXT NOT NULL,
                 key_id    TEXT,
                 key_label TEXT,
                 method    TEXT NOT NULL,
                 tool      TEXT NOT NULL,
                 ok        INTEGER NOT NULL,
                 detail    TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    /// Mint a new key for `label`. Returns `(id, full_plaintext_key)`; the
    /// plaintext (`"<id>:<secret>"`) is produced here and NEVER stored — only
    /// its SHA-256 goes to disk. The caller must show it once and tell the
    /// operator to save it.
    pub fn create(&self, label: &str) -> rusqlite::Result<(String, String)> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let secret = gen_secret();
        let hash = sha256_hex(&secret);
        let full_key = format!("{id}:{secret}");
        {
            let c = self.conn.lock();
            c.execute(
                "INSERT INTO api_keys (id, label, secret_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, label, hash, now_iso()],
            )?;
        }
        Ok((id, full_key))
    }

    /// Authenticate a presented `"<id>:<secret>"` key: look up the id, reject
    /// revoked rows, then constant-length compare the secret's digest. On
    /// success stamps `last_used_at` and returns the identity; any failure
    /// returns a distinct human-readable refusal (so the operator can tell an
    /// unknown id from a wrong secret or a revoked key).
    pub fn authenticate(&self, key: &str) -> Result<AuthedKey, String> {
        let (id, secret) = key
            .split_once(':')
            .ok_or_else(|| "malformed api key (expected '<id>:<secret>')".to_string())?;
        let c = self.conn.lock();
        let row: Option<(String, String, Option<String>)> = c
            .query_row(
                "SELECT label, secret_hash, revoked_at FROM api_keys WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let (label, stored_hash, revoked) =
            row.ok_or_else(|| "authentication failed: unknown api key id".to_string())?;
        if revoked.is_some() {
            return Err("authentication failed: this api key has been revoked".to_string());
        }
        if sha256_hex(secret) != stored_hash {
            return Err("authentication failed: invalid api key secret".to_string());
        }
        c.execute(
            "UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2",
            params![now_iso(), id],
        )
        .map_err(|e| e.to_string())?;
        Ok(AuthedKey {
            id: id.to_string(),
            label,
        })
    }

    /// Every key, newest first (secrets never leave the store).
    pub fn list(&self) -> rusqlite::Result<Vec<KeyInfo>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, label, created_at, last_used_at, revoked_at
             FROM api_keys ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(KeyInfo {
                id: r.get(0)?,
                label: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
                revoked_at: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Revoke a key by id (idempotent: returns false if no ACTIVE row matched,
    /// so the caller can distinguish "revoked now" from "already revoked /
    /// never existed"). Revoked keys fail authentication but stay in the table
    /// so historical `mcp_calls` rows keep their attribution.
    pub fn revoke(&self, id: &str) -> rusqlite::Result<bool> {
        let c = self.conn.lock();
        let n = c.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now_iso(), id],
        )?;
        Ok(n > 0)
    }

    /// Append one audit row. `key` is the authenticated identity when the call
    /// was authenticated (an unauthenticated/failed attempt logs `None` with an
    /// `ok=false` refusal in `detail`).
    pub fn log_call(
        &self,
        key: Option<&AuthedKey>,
        method: &str,
        tool: &str,
        ok: bool,
        detail: &str,
    ) -> rusqlite::Result<()> {
        let key_id = key.map(|k| k.id.clone());
        let key_label = key.map(|k| k.label.clone());
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO mcp_calls (ts, key_id, key_label, method, tool, ok, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![now_iso(), key_id, key_label, method, tool, ok as i64, detail],
        )?;
        Ok(())
    }

    /// Most recent `limit` audit rows (newest first).
    pub fn recent_calls(&self, limit: usize) -> rusqlite::Result<Vec<CallLog>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT ts, key_id, key_label, method, tool, ok, detail
             FROM mcp_calls ORDER BY rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(CallLog {
                ts: r.get(0)?,
                key_id: r.get(1)?,
                key_label: r.get(2)?,
                method: r.get(3)?,
                tool: r.get(4)?,
                ok: r.get::<_, i64>(5)? == 1,
                detail: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ApiKeyStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the temp dir so the file handle outlives this call (test-only).
        let p = dir.path().to_path_buf();
        std::mem::forget(dir);
        ApiKeyStore::open(&p).unwrap()
    }

    #[test]
    fn create_then_authenticate_round_trip() {
        let s = store();
        let (id, key) = s.create("claude-desktop").unwrap();
        let authed = s.authenticate(&key).unwrap();
        assert_eq!(authed.id, id);
        assert_eq!(authed.label, "claude-desktop");
        // Only the hash is on disk — the plaintext secret is never recoverable.
        let c = s.conn.lock();
        let stored: String = c
            .query_row("SELECT secret_hash FROM api_keys WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored.len(), 64);
        assert!(!stored.contains(&key.split_once(':').unwrap().1));
    }

    #[test]
    fn wrong_secret_and_unknown_id_are_distinct_failures() {
        let s = store();
        let (_id, key) = s.create("agent").unwrap();
        let (_id2, key2) = s.create("other").unwrap();
        let good_secret = key.split_once(':').unwrap().1;
        // Right id, wrong secret.
        let bad = format!("{}:{}", _id, "deadbeef");
        assert!(s.authenticate(&bad)
            .unwrap_err()
            .contains("invalid api key secret"));
        // An id that exists but with another key's secret.
        let cross = format!("{}:{}", key2.split_once(':').unwrap().0, good_secret);
        assert!(s.authenticate(&cross).is_err());
        // A made-up id.
        assert!(s
            .authenticate("nosuchid:somesecret")
            .unwrap_err()
            .contains("unknown api key id"));
    }

    #[test]
    fn revoked_key_is_rejected_but_stays_listed() {
        let s = store();
        let (id, key) = s.create("temp").unwrap();
        assert!(s.revoke(&id).unwrap());
        assert!(s.authenticate(&key).unwrap_err().contains("revoked"));
        // Revoking twice is a no-op reported as false (not an active row).
        assert!(!s.revoke(&id).unwrap());
        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked_at.is_some());
    }

    #[test]
    fn calls_are_logged_and_attributed() {
        let s = store();
        let (_id, key) = s.create("worker").unwrap();
        let a = s.authenticate(&key).unwrap();
        s.log_call(Some(&a), "tools/call", "define_derived_view", true, "name=release")
            .unwrap();
        s.log_call(None, "tools/call", "list_views", false, "unauthorized")
            .unwrap();
        let recent = s.recent_calls(10).unwrap();
        assert_eq!(recent.len(), 2);
        // newest first
        assert_eq!(recent[0].tool, "list_views");
        assert!(!recent[0].ok);
        assert!(recent[0].key_id.is_none());
        assert_eq!(recent[1].tool, "define_derived_view");
        assert_eq!(recent[1].key_label.as_deref(), Some("worker"));
    }
}
