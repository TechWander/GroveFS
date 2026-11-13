//! API-key authentication and a call-audit log for the MCP governance server
//! and for `meta-server`/`data-server` mount admission.
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
//! The same credential store also gates the MOUNT plane: a `meta-server` or
//! `data-server` started with `--require-api-key` admits only connections that
//! present a live key whose [`Scope`] fits the plane, so mount access becomes
//! per-caller (mintable, revocable, attributable) instead of one static
//! cluster-wide `--token` baked into the server's command line. Revocation
//! reaches ESTABLISHED connections too: both servers re-verify the key behind a
//! live session at most once per second and then refuse it (see
//! [`ApiKeyStore::is_live`]).
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
//! Storage FOLLOWS the metadata backend — one system of record, never two:
//! - under SQLite (the default) keys live in one local file,
//!   `<base>/meta/api_keys.db`;
//! - under PostgreSQL (`--meta-store postgres://…`) the SAME two tables live in the
//!   shared `grovefs_control` schema ([`crate::control`]) — a key minted or
//!   revoked against the URI is authoritative for every meta-server, MCP
//!   server and CLI pointing at it, with no per-node file to colocate;
//! - under FoundationDB (`--meta-store fdb://…`) they live IN that cluster, under
//!   the `(root,'ctl',…)` sibling of the view prefixes
//!   ([`crate::fdb_store::CtlKeyTable`]) — the credential plane travels with the
//!   trees it governs, so a key minted against the URI admits a mount, an MCP
//!   session and the governance port on every node aimed at that cluster, and a
//!   revoke is one row edit for all of them.
//!
//! The backend is picked once at open time ([`ApiKeyStore::open`] for the
//! local file, [`ApiKeyStore::open_pg`] for a PG URI, [`ApiKeyStore::open_fdb`]
//! for `fdb://`; [`ApiKeyStore::open_for`] routes like the view backend). All
//! three expose identical columns, semantics and refusal strings.
//! Mount gating reuses one store per metadata root (or per URI); a
//! `data-server` on a node without the control plane sets `--meta-dir
//! <base>` (see the CLI help).
//!
//! Honest boundary: a key answers WHO may connect and — via its optional
//! `view` binding — WHICH single tree they may open (`None` = any view, a
//! system-level key). A key's `ro` flag is enforced server-side: the meta- and
//! data-servers refuse every write-intent op from a read-only key, so "read-
//! only" no longer depends on the client honouring `mount -o ro`. The flag is
//! not frozen at mint — `apikey set-ro` flips it in place, and the live-session
//! re-check ([`ApiKeyStore::live_state`]) reads the CURRENT row each
//! [`KEY_RECHECK`] window, so the write verdict hot-swaps on an established
//! mount (the kernel-side `-o ro` an `--ro` handshake nudged an honest client
//! into still needs a remount to lift, but the authoritative server gate
//! refuses or admits the op either way). Both access
//! axes — WHICH single tree a key may open (`view` binding) and whether it may
//! write at all (`ro`) — live in the credential itself; the self-reported uid
//! stays advisory, and the key is the real gate against a malicious peer. The
//! store holds only SHA-256 digests, so a leaked `api_keys.db` hands out
//! nothing usable.

use rusqlite::{params, Connection};
use std::path::Path;

use crate::control::PgPool;

/// A successfully authenticated key's identity (never the secret or hash).
///
/// Beyond *who* connected, a mount key can now carry *which* view it is bound
/// to and whether it is read-only — both encoded directly in the credential.
/// `view: None` means "any view" (a
/// system-level key); `Some(v)` locks the key to that one physical view. `ro`
/// marks a key read-only: the server refuses every write-intent op from it, so
/// "read-only" is enforced server-side rather than by asking the client to mount
/// `ro`.
#[derive(Debug, Clone)]
pub struct AuthedKey {
    pub id: String,
    /// The ACCOUNT this credential belongs to (human-chosen, e.g. a person or
    /// a team). `None` for keys minted before accounts existed — such rows keep
    /// authenticating and are attributed by `label` alone.
    pub owner: Option<String>,
    pub label: String,
    pub view: Option<String>,
    pub ro: bool,
}

impl AuthedKey {
    /// Does this key's view binding admit `view`? An unbound key (`view: None`,
    /// a system-level credential) admits any view; a bound key only its own.
    pub fn binds_to(&self, view: &str) -> bool {
        self.view.as_deref().is_none_or(|v| v == view)
    }
}

/// The live-session re-check verdict for one authenticated id: still
/// admitted, plus the key's CURRENT `ro` flag — so `apikey set-ro` reaches an
/// established mount within the re-check window instead of only the next
/// handshake.
#[derive(Debug, Clone, Copy)]
pub struct LiveState {
    pub live: bool,
    pub ro: bool,
}

/// Which entrance(s) a key may open. Stored as a lowercase string.
///
/// `Any` is the default and what every pre-existing row carries, so minting
/// mount-gated credentials never invalidates an MCP deployment (and vice
/// versa). Narrowing is opt-in: `--for mount` hands a client a key that cannot
/// touch the governance API, `--for mcp` the opposite — least privilege for
/// agents that should only reshape views, or mounts that should only read data.
///
/// `Admin` is the governance entrance of a `meta-server --control-listen`: it
/// lets an operator on ANY machine drive view/key/ledger changes with a
/// revocable account credential instead of a database password baked into a
/// URI, while the server process stays the only holder of the backend
/// connection. It is deliberately NOT implied by `Any` (see [`Scope::admits`]):
/// otherwise every pre-existing `--for any` credential would silently become a
/// cluster administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Any,
    Mcp,
    Mount,
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Any => "any",
            Scope::Mcp => "mcp",
            Scope::Mount => "mount",
            Scope::Admin => "admin",
        }
    }

    /// Parse a user-supplied scope (`--for <SCOPE>`); unknown words are an
    /// error so a typo cannot silently widen a credential.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "any" => Ok(Scope::Any),
            "mcp" => Ok(Scope::Mcp),
            "mount" => Ok(Scope::Mount),
            "admin" => Ok(Scope::Admin),
            other => Err(format!(
                "unknown api key scope '{other}' (expected any|mcp|mount|admin)"
            )),
        }
    }

    /// Does a key stored with `self` qualify for an entrance wanting `want`?
    ///
    /// Explicit table, on purpose: `Any` covers the two per-caller entrances it
    /// has always covered (mount + MCP) and STOPS there, so adding `Admin`
    /// cannot widen an existing credential. `Admin` is a superset — an
    /// administrator may of course mount and use MCP — while every narrower
    /// scope only admits its own entrance.
    pub fn admits(self, want: Scope) -> bool {
        match (self, want) {
            (Scope::Any, Scope::Mount | Scope::Mcp) => true,
            (Scope::Admin, _) => true,
            (own, want) => own == want,
        }
    }
}

/// Operator-facing metadata about one stored key (no secret material).
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub id: String,
    /// Account the credential belongs to (`None` on pre-account rows).
    pub owner: Option<String>,
    pub label: String,
    pub scope: String,
    pub view: Option<String>,
    pub ro: bool,
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

/// One row reader shared by both backends' map closures (column access lives
/// in the per-backend `query`, this normalizes the error plumbing).
fn cell<T>(row: &rusqlite::Row, i: usize) -> Result<Option<T>, rusqlite::Error>
where
    Option<T>: rusqlite::types::FromSql,
{
    row.get::<_, Option<T>>(i)
}

/// The per-base API-key + call-log store: one local SQLite file under the
/// SQLite backend, the shared `grovefs_control` tables under PostgreSQL.
pub struct ApiKeyStore {
    backend: Backend,
}

/// The two backend spellings of the same key/audit schema. SQLite keeps the
/// `rowid`; PG gets a serial identity so `ORDER BY <identity> DESC`
/// matches the file's `rowid DESC` audit ordering.
const KEYS_DDL_SQLITE: &str = "CREATE TABLE IF NOT EXISTS api_keys (
                 id          TEXT PRIMARY KEY,
                 label       TEXT NOT NULL,
                 secret_hash TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 last_used_at TEXT,
                 revoked_at  TEXT,
                 scope       TEXT NOT NULL DEFAULT 'any',
                 view        TEXT,
                 ro          INTEGER NOT NULL DEFAULT 0,
                 owner       TEXT
             );";
const CALLS_DDL_SQLITE: &str = "CREATE TABLE IF NOT EXISTS mcp_calls (
                 ts        TEXT NOT NULL,
                 key_id    TEXT,
                 key_label TEXT,
                 method    TEXT NOT NULL,
                 tool      TEXT NOT NULL,
                 ok        INTEGER NOT NULL,
                 detail    TEXT NOT NULL
             );";

const KEYS_DDL_PG: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.api_keys (
    seq          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id           TEXT NOT NULL UNIQUE,
    label        TEXT NOT NULL,
    secret_hash  TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at   TEXT,
    scope        TEXT NOT NULL DEFAULT 'any',
    view_name    TEXT,
    ro           BOOLEAN NOT NULL DEFAULT false,
    owner        TEXT
)";
/// Idempotent column addition for stores minted before accounts existed. NOT a
/// table upgrade (nothing is rewritten, no row is touched): it only makes the
/// `owner` column visible so the single SELECT/INSERT spelling above works
/// against an old file/schema.
const OWNER_MIGRATE_SQLITE: &str =
    "ALTER TABLE api_keys ADD COLUMN owner TEXT";
const OWNER_MIGRATE_PG: &str =
    "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS owner TEXT";
const CALLS_DDL_PG: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.mcp_calls (
    seq       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    ts        TEXT NOT NULL,
    key_id    TEXT,
    key_label TEXT,
    method    TEXT NOT NULL,
    tool      TEXT NOT NULL,
    ok        BOOLEAN NOT NULL,
    detail    TEXT NOT NULL
)";

/// Where this credential store physically lives.
enum Backend {
    Sqlite(parking_lot::Mutex<Connection>),
    Pg(PgPool),
    /// The cluster-side tables of an `fdb://` deployment.
    #[cfg(feature = "fdb")]
    Fdb(crate::fdb_store::CtlKeyTable),
}

/// One credential row as the VERIFICATION path reads it: the column subset
/// [`ApiKeyStore::authenticate_for`] and [`ApiKeyStore::live_state`] need, in one
/// shape so that "which rows authenticate" is decided in one place instead of
/// once per backend.
struct StoredKey {
    label: String,
    secret_hash: String,
    scope: String,
    revoked_at: Option<String>,
    view: Option<String>,
    ro: bool,
    owner: Option<String>,
}

/// One row reader for both backends: fetch column `i` as `Option<String>`.
fn cell_str(row: &postgres::Row, i: usize) -> Option<String> {
    row.get::<_, Option<String>>(i)
}

impl ApiKeyStore {
    /// Open the LOCAL SQLite store at `<base>/meta/api_keys.db` (see
    /// [`ApiKeyStore::open_for`] for backend-routing).
    pub fn open(base: &Path) -> Result<Self, String> {
        let path = base.join("meta").join("api_keys.db");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             {KEYS_DDL_SQLITE}
             {CALLS_DDL_SQLITE}"
        ))
        .map_err(|e| e.to_string())?;
        // Pre-account stores (`meta/api_keys.db` from an older build) lack the
        // `owner` column. Add it in place; the only other in-place change this
        // module ever made is nothing — everything else is part of the DDL.
        let has_owner = conn
            .prepare("PRAGMA table_info(api_keys)")
            .and_then(|mut s| {
                let names = s
                    .query_map([], |r| r.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names.iter().any(|n| n == "owner"))
            })
            .map_err(|e| e.to_string())?;
        if !has_owner {
            conn.execute(OWNER_MIGRATE_SQLITE, [])
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            backend: Backend::Sqlite(parking_lot::Mutex::new(conn)),
        })
    }

    /// Open the PostgreSQL control-plane store: the same two tables in the
    /// shared `grovefs_control` schema (idempotent DDL, cross-process safe —
    /// see [`PgPool::open`]). A PG deployment keeps ONE credential/audit
    /// system of record: every meta-server, MCP server and CLI aimed at the
    /// same URI authenticates against the same rows, and a revoke reaches all
    /// of them within the re-check window.
    pub fn open_pg(uri: &str) -> Result<Self, String> {
        let pool = PgPool::open(uri, &[KEYS_DDL_PG, OWNER_MIGRATE_PG, CALLS_DDL_PG])?;
        Ok(Self {
            backend: Backend::Pg(pool),
        })
    }

    /// Open the CLUSTER-side store of an `fdb://` deployment: the same two
    /// tables as `(root,'ctl',…)` in the cluster the trees live in
    /// ([`crate::fdb_store::CtlKeyTable`]). Same contract as the PG arm — a key
    /// minted or revoked against the URI is authoritative for every node aimed at
    /// it, and there is no per-node file to colocate.
    #[cfg(feature = "fdb")]
    pub fn open_fdb(uri: &str) -> Result<Self, String> {
        Ok(Self {
            backend: Backend::Fdb(crate::fdb_store::CtlKeyTable::open(uri)?),
        })
    }

    /// The store for a metadata-backend selection: a `postgres[ql]://` URI
    /// routes to the PG control schema, an `fdb://` URI to the cluster's own
    /// credential tables, anything else (including `None`) to the local file
    /// under `base` — the same routing rule the view definitions follow
    /// ([`crate::mcp::control_store_for`]).
    pub fn open_for(base: &Path, store_uri: Option<&str>) -> Result<Self, String> {
        match store_uri {
            Some(u) if u.starts_with("postgres://") || u.starts_with("postgresql://") => {
                Self::open_pg(u)
            }
            // An `fdb://` deployment keeps ONE system of record too. Without the
            // `fdb` feature such a store cannot have been opened at all, so the
            // local file arm below is what remains for every buildable case.
            #[cfg(feature = "fdb")]
            Some(u) if u.starts_with("fdb://") => Self::open_fdb(u),
            _ => Self::open(base),
        }
    }

    /// Read one credential row by id, whichever backend holds it. The
    /// verification rules themselves live in [`Self::authenticate_for`] and
    /// [`Self::live_state`], so they cannot drift per backend.
    fn fetch_key(&self, id: &str) -> Result<Option<StoredKey>, String> {
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return Ok(t.lookup(id)?.map(|r| StoredKey {
                label: r.label,
                secret_hash: r.secret_hash,
                scope: r.scope,
                revoked_at: r.revoked_at,
                view: r.view,
                ro: r.ro,
                owner: r.owner,
            }));
        }
        let mut rows = self.query2(
            "SELECT label, secret_hash, scope, revoked_at, view, ro, owner FROM api_keys WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    cell::<String>(r, 3)?,
                    cell::<String>(r, 4)?,
                    r.get::<_, i64>(5)?,
                    cell::<String>(r, 6)?,
                ))
            },
            "SELECT label, secret_hash, scope, revoked_at, view_name, ro, owner FROM grovefs_control.api_keys WHERE id = $1",
            &[&id],
            |r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    r.get::<_, String>(2),
                    cell_str(r, 3),
                    cell_str(r, 4),
                    r.get::<_, bool>(5) as i64,
                    cell_str(r, 6),
                )
            },
        )?;
        Ok(
            rows.pop()
                .map(|(label, secret_hash, scope, revoked_at, view, ro, owner)| StoredKey {
                    label,
                    secret_hash,
                    scope,
                    revoked_at,
                    view,
                    ro: ro != 0,
                    owner,
                }),
        )
    }

    /// Record that `id` just authenticated. A no-op for an id that vanished
    /// between the read and this stamp (the SQL UPDATE matches no row), which is
    /// the same behaviour a revoke racing an authentication has always had.
    fn stamp_used(&self, id: &str) -> Result<(), String> {
        let ts = now_iso();
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return t.stamp_used(id, &ts);
        }
        self.exec2(
            "UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2",
            params![ts, id],
            "UPDATE grovefs_control.api_keys SET last_used_at = $1 WHERE id = $2",
            &[&ts, &id],
        )
    }

    /// One parameterized statement, backend-routed: the SQLite and PG spellings
    /// of the same op are passed side by side (identical text where the
    /// dialects agree). Parameters are aligned by position across both lists.
    fn exec2(
        &self,
        sql_s: &str,
        ps: &[&dyn rusqlite::ToSql],
        sql_p: &str,
        pp: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<(), String> {
        match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(sql_s, ps)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Backend::Pg(pool) => pool.execute(sql_p, pp).map(|_| ()),
            // Unreachable by construction: every public verb delegates to the
            // cluster table before it reaches a SQL spelling. Reported instead of
            // `unreachable!()` so a future method that forgot the delegation is a
            // refusal rather than a panic inside a server.
            #[cfg(feature = "fdb")]
            Backend::Fdb(_) => {
                Err("internal: the cluster key table does not speak SQL".to_string())
            }
        }
    }

    /// One parameterized SELECT, backend-routed, with per-dialect row mappers.
    fn query2<T, FS, FP>(
        &self,
        sql_s: &str,
        ps: &[&dyn rusqlite::ToSql],
        ms: FS,
        sql_p: &str,
        pp: &[&(dyn postgres::types::ToSql + Sync)],
        mp: FP,
    ) -> Result<Vec<T>, String>
    where
        FS: Fn(&rusqlite::Row) -> Result<T, rusqlite::Error>,
        FP: Fn(&postgres::Row) -> T + Clone,
    {
        match &self.backend {
            Backend::Sqlite(m) => {
                let c = m.lock();
                let mut stmt = c.prepare(sql_s).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(ps, ms)
                    .map_err(|e| e.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                Ok(rows)
            }
            Backend::Pg(pool) => pool.query(sql_p, pp, mp),
            // See `exec2`: the public verbs route around this.
            #[cfg(feature = "fdb")]
            Backend::Fdb(_) => {
                Err("internal: the cluster key table does not speak SQL".to_string())
            }
        }
    }

    /// Mint a new unrestricted key for `label` (see [`Self::create_for`]).
    pub fn create(&self, label: &str) -> Result<(String, String), String> {
        self.create_for(label, Scope::Any)
    }

    /// Mint a new key for `label` limited to `scope` (unbound view, read-write).
    /// See [`Self::create_bound`] for the full view/ro knobs.
    pub fn create_for(&self, label: &str, scope: Scope) -> Result<(String, String), String> {
        self.create_bound(label, scope, None, false, None)
    }

    /// Mint a new key: `label`, entrance `scope`, optional `view` binding
    /// (`None` = any view), a `ro` read-only flag and an optional `owner`
    /// ACCOUNT (a human-chosen name such as a person or a team; one account may
    /// hold several keys). Returns `(id, full_plaintext_key)`; the plaintext
    /// (`"<id>:<secret>"`) is produced here and NEVER stored — only its SHA-256
    /// goes to disk. The caller must show it once and tell the operator to save
    /// it.
    pub fn create_bound(
        &self,
        label: &str,
        scope: Scope,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
    ) -> Result<(String, String), String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let secret = gen_secret();
        let hash = sha256_hex(&secret);
        let full_key = format!("{id}:{secret}");
        let ts = now_iso();
        let sc = scope.as_str();
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            t.insert_key(&crate::fdb_store::CtlKeyRow {
                id: id.clone(),
                label: label.to_string(),
                secret_hash: hash,
                created_at: ts,
                last_used_at: None,
                revoked_at: None,
                scope: sc.to_string(),
                view: view.map(str::to_string),
                ro,
                owner: owner.map(str::to_string),
            })?;
            return Ok((id, full_key));
        }
        self.exec2(
            "INSERT INTO api_keys (id, label, secret_hash, created_at, scope, view, ro, owner) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, label, hash, ts, sc, view, ro as i64, owner],
            "INSERT INTO grovefs_control.api_keys (id, label, secret_hash, created_at, scope, view_name, ro, owner) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&id, &label, &hash, &ts, &sc, &view, &ro, &owner],
        )?;
        Ok((id, full_key))
    }

    /// Authenticate a key presented to the MCP governance entrance.
    pub fn authenticate(&self, key: &str) -> Result<AuthedKey, String> {
        self.authenticate_for(key, Scope::Mcp)
    }

    /// Authenticate a key presented by a mount (`meta-server`/`data-server`).
    pub fn authenticate_mount(&self, key: &str) -> Result<AuthedKey, String> {
        self.authenticate_for(key, Scope::Mount)
    }

    /// Authenticate a key presented to a `meta-server --control-listen`
    /// governance entrance (the remote control plane).
    pub fn authenticate_admin(&self, key: &str) -> Result<AuthedKey, String> {
        self.authenticate_for(key, Scope::Admin)
    }

    /// Authenticate a presented `"<id>:<secret>"` key for an entrance wanting
    /// `want`: look up the id, reject revoked rows and rows whose [`Scope`]
    /// does not admit this entrance, then compare the secret's digest. On
    /// success stamps `last_used_at` and returns the identity; any failure
    /// returns a distinct human-readable refusal (so the operator can tell an
    /// unknown id from a wrong secret, a revoked key or a wrong-scope one).
    pub fn authenticate_for(&self, key: &str, want: Scope) -> Result<AuthedKey, String> {
        if key.is_empty() {
            // The most common operator-visible case: a client that was never
            // configured for a gated server. Name the missing knob instead of a
            // format complaint, so the fix is one command away.
            return Err(format!(
                "no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for {}`)",
                want.as_str()
            ));
        }
        let (id, secret) = key
            .split_once(':')
            .ok_or_else(|| "malformed api key (expected '<id>:<secret>')".to_string())?;
        let k = self
            .fetch_key(id)?
            .ok_or_else(|| "authentication failed: unknown api key id".to_string())?;
        if k.revoked_at.is_some() {
            return Err("authentication failed: this api key has been revoked".to_string());
        }
        if sha256_hex(secret) != k.secret_hash {
            return Err("authentication failed: invalid api key secret".to_string());
        }
        let scope = Scope::parse(&k.scope).unwrap_or(Scope::Any);
        if !scope.admits(want) {
            return Err(format!(
                "authentication failed: this api key is scoped to '{}', not '{}'",
                scope.as_str(),
                want.as_str()
            ));
        }
        self.stamp_used(id)?;
        Ok(AuthedKey {
            id: id.to_string(),
            owner: k.owner,
            label: k.label,
            view: k.view,
            ro: k.ro,
        })
    }

    /// Is a previously authenticated id STILL usable? Used by the mount
    /// servers to re-verify an established connection (at most once per
    /// [`KEY_RECHECK`] per session, on arriving traffic), so a revoke reaches
    /// live mounts instead of only the next handshake. Read-only (no
    /// `last_used_at` stamp: this runs on a periodic path, not an
    /// authentication event).
    ///
    /// Kept as the boolean shim over [`Self::live_state`] for callers that
    /// only gate on liveness.
    pub fn is_live(&self, id: &str, want: Scope) -> bool {
        self.live_state(id, want).map(|s| s.live).unwrap_or(false)
    }

    /// The re-check result for a live session: still admitted, and the
    /// key's CURRENT read-only flag. Same primary-key lookup as
    /// [`Self::is_live`] plus the `ro` column, so `apikey set-ro` hot-swaps a
    /// live mount's write verdict within the re-check window WITHOUT a
    /// reconnect: the mount servers refresh their cached gate from this row
    /// each `KEY_RECHECK` tick, not just the revoke verdict.
    ///
    /// Deliberately op-driven rather than timer-driven: the re-check runs when
    /// a request reaches the server, so a session that serves nothing at all is
    /// never probed (an idle socket grants no access by itself). Reads answered
    /// purely from a client's kernel cache likewise land before the next
    /// round-trip — the bound is "first server round-trip after the window",
    /// not wall-clock. Unknown id, revoked row or a scope that no longer fits
    /// the entrance all report `None`.
    pub fn live_state(&self, id: &str, want: Scope) -> Option<LiveState> {
        let k = self.fetch_key(id).ok()??;
        if k.revoked_at.is_some() || !Scope::parse(&k.scope).unwrap_or(Scope::Any).admits(want) {
            return None;
        }
        Some(LiveState {
            live: true,
            ro: k.ro,
        })
    }

    /// Flip the read-only flag of an ACTIVE key in place (`apikey set-ro`).
    /// Returns false if no active row matched `id` — a revoked key is NOT
    /// resurrected and never authenticates again, and silently flipping a
    /// dead row would make `list` lie about what revoke did. The new value is
    /// picked up by every live session on its next [`KEY_RECHECK`] tick
    /// ([`Self::live_state`]); a session established AFTER this call reads
    /// the flag at `authenticate_for` as usual.
    pub fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return t.set_ro(id, ro);
        }
        // Same rows-affected path shape as `revoke`: one conditional UPDATE
        // whose count distinguishes "flipped now" from "no active row".
        let n = match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(
                    "UPDATE api_keys SET ro = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                    params![ro as i64, id],
                )
                .map_err(|e| e.to_string())?,
            Backend::Pg(pool) => pool.execute(
                "UPDATE grovefs_control.api_keys SET ro = $1 WHERE id = $2 AND revoked_at IS NULL",
                &[&ro, &id],
            )? as usize,
            #[cfg(feature = "fdb")]
            Backend::Fdb(_) => {
                return Err("internal: the cluster key table does not speak SQL".to_string())
            }
        };
        Ok(n > 0)
    }

    /// Every key, newest first (secrets never leave the store).
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return Ok(t
                .list()?
                .into_iter()
                .map(|r| KeyInfo {
                    id: r.id,
                    owner: r.owner,
                    label: r.label,
                    scope: r.scope,
                    view: r.view,
                    ro: r.ro,
                    created_at: r.created_at,
                    last_used_at: r.last_used_at,
                    revoked_at: r.revoked_at,
                })
                .collect());
        }
        let rows = self.query2(
            "SELECT id, label, scope, view, ro, created_at, last_used_at, revoked_at, owner
             FROM api_keys ORDER BY created_at DESC",
            &[],
            |r| {
                Ok(KeyInfo {
                    id: r.get(0)?,
                    owner: cell(r, 8)?,
                    label: r.get(1)?,
                    scope: r.get(2)?,
                    view: cell(r, 3)?,
                    ro: r.get::<_, i64>(4)? != 0,
                    created_at: r.get(5)?,
                    last_used_at: cell(r, 6)?,
                    revoked_at: cell(r, 7)?,
                })
            },
            "SELECT id, label, scope, view_name, ro, created_at, last_used_at, revoked_at, owner
             FROM grovefs_control.api_keys ORDER BY created_at DESC, seq DESC",
            &[],
            |r| KeyInfo {
                id: r.get(0),
                owner: cell_str(r, 8),
                label: r.get(1),
                scope: r.get(2),
                view: cell_str(r, 3),
                ro: r.get(4),
                created_at: r.get(5),
                last_used_at: cell_str(r, 6),
                revoked_at: cell_str(r, 7),
            },
        )?;
        Ok(rows)
    }

    /// Revoke a key by id (idempotent: returns false if no ACTIVE row matched,
    /// so the caller can distinguish "revoked now" from "already revoked /
    /// never existed"). Revoked keys fail authentication but stay in the table
    /// so historical `mcp_calls` rows keep their attribution.
    pub fn revoke(&self, id: &str) -> Result<bool, String> {
        let ts = now_iso();
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return t.revoke(id, &ts);
        }
        let n = match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(
                    "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                    params![ts, id],
                )
                .map_err(|e| e.to_string())?,
            Backend::Pg(pool) => pool
                .execute(
                    "UPDATE grovefs_control.api_keys SET revoked_at = $1 WHERE id = $2 AND revoked_at IS NULL",
                    &[&ts, &id],
                )? as usize,
            #[cfg(feature = "fdb")]
            Backend::Fdb(_) => {
                return Err("internal: the cluster key table does not speak SQL".to_string())
            }
        };
        Ok(n > 0)
    }

    /// Append one audit row. `key` is the authenticated identity when the call
    /// was authenticated (an unauthenticated/failed attempt logs `None` with an
    /// `ok=false` refusal in `detail`). The table (`mcp_calls`) is the generic
    /// key-call audit: the MCP server logs `method`/`tool` pairs, and the mount
    /// servers log their handshakes as `method="meta"|"data"`,
    /// `tool="M_OPEN"|"OP_OPEN"` (plus a `"key revoked"` row when they drop a
    /// live session), all visible through `grovefs apikey calls`.
    pub fn log_call(
        &self,
        key: Option<&AuthedKey>,
        method: &str,
        tool: &str,
        ok: bool,
        detail: &str,
    ) -> Result<(), String> {
        let key_id = key.map(|k| k.id.clone());
        let key_label = key.map(|k| k.label.clone());
        let ts = now_iso();
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return t.append_call(&crate::fdb_store::CtlCallRow {
                ts,
                key_id,
                key_label,
                method: method.to_string(),
                tool: tool.to_string(),
                ok,
                detail: detail.to_string(),
            });
        }
        self.exec2(
            "INSERT INTO mcp_calls (ts, key_id, key_label, method, tool, ok, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts, key_id, key_label, method, tool, ok as i64, detail],
            "INSERT INTO grovefs_control.mcp_calls (ts, key_id, key_label, method, tool, ok, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[&ts, &key_id, &key_label, &method, &tool, &ok, &detail],
        )
    }

    /// Most recent `limit` audit rows (newest first — insertion order, which
    /// is the storage identity on both backends: SQLite `rowid`, PG `seq`).
    pub fn recent_calls(&self, limit: usize) -> Result<Vec<CallLog>, String> {
        #[cfg(feature = "fdb")]
        if let Backend::Fdb(t) = &self.backend {
            return Ok(t
                .recent_calls(limit)?
                .into_iter()
                .map(|r| CallLog {
                    ts: r.ts,
                    key_id: r.key_id,
                    key_label: r.key_label,
                    method: r.method,
                    tool: r.tool,
                    ok: r.ok,
                    detail: r.detail,
                })
                .collect());
        }
        let rows = self.query2(
            "SELECT ts, key_id, key_label, method, tool, ok, detail
             FROM mcp_calls ORDER BY rowid DESC LIMIT ?1",
            params![limit as i64],
            |r| {
                Ok(CallLog {
                    ts: r.get(0)?,
                    key_id: cell(r, 1)?,
                    key_label: cell(r, 2)?,
                    method: r.get(3)?,
                    tool: r.get(4)?,
                    ok: r.get::<_, i64>(5)? == 1,
                    detail: r.get(6)?,
                })
            },
            "SELECT ts, key_id, key_label, method, tool, ok, detail
             FROM grovefs_control.mcp_calls ORDER BY seq DESC LIMIT $1",
            &[&(limit as i64)],
            |r| CallLog {
                ts: r.get(0),
                key_id: cell_str(r, 1),
                key_label: cell_str(r, 2),
                method: r.get(3),
                tool: r.get(4),
                ok: r.get(5),
                detail: r.get(6),
            },
        )?;
        Ok(rows)
    }
}

/// How often a live mount session re-verifies its API key against the store.
/// Bounds how long a revoked credential can keep working on an ESTABLISHED
/// connection: the refusal lands on the first op after this window. One
/// primary-key `SELECT` per session per interval (a control-plane query, not a
/// hot-path cost), which is why the gate can be honest about revoke latency.
pub const KEY_RECHECK: std::time::Duration = std::time::Duration::from_millis(1000);

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
        // Only the hash is on disk — the plaintext secret is never recoverable
        // (the store hands out no secret column at all, which IS the proof).
        let stored = match &s.backend {
            Backend::Sqlite(m) => m
                .lock()
                .query_row(
                    "SELECT secret_hash FROM api_keys WHERE id=?1",
                    params![id],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
            Backend::Pg(_) => unreachable!("unit store is the file backend"),
            #[cfg(feature = "fdb")]
            Backend::Fdb(_) => unreachable!("unit store is the file backend"),
        };
        assert_eq!(stored.len(), 64);
        assert_ne!(stored, key.split_once(':').unwrap().1);
    }

    #[test]
    fn wrong_secret_and_unknown_id_are_distinct_failures() {
        let s = store();
        let (_id, key) = s.create("agent").unwrap();
        let (_id2, key2) = s.create("other").unwrap();
        let good_secret = key.split_once(':').unwrap().1;
        // Right id, wrong secret.
        let bad = format!("{}:{}", _id, "deadbeef");
        assert!(s
            .authenticate(&bad)
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

    /// Scope is the least-privilege axis: a `mount` key cannot open the MCP
    /// governance entrance and an `mcp` key cannot gate a mount, while the
    /// default `any` (what every pre-scope row carries) is admitted by both.
    #[test]
    fn scope_narrows_which_entrance_a_key_opens() {
        let s = store();
        let (_mid, mkey) = s.create_for("mounter", Scope::Mount).unwrap();
        let (_aid, akey) = s.create_for("governor", Scope::Mcp).unwrap();
        let (_did, dkey) = s.create("unscoped").unwrap();
        assert_eq!(s.list().unwrap()[0].scope, "any", "default is unrestricted");
        // Right entrance: authenticates.
        assert_eq!(s.authenticate_mount(&mkey).unwrap().label, "mounter");
        assert_eq!(s.authenticate(&akey).unwrap().label, "governor");
        assert!(s.authenticate_mount(&dkey).is_ok());
        assert!(s.authenticate(&dkey).is_ok());
        // Wrong entrance: refused with a scope-specific message (so an
        // operator can tell this from a wrong secret or a revoke).
        assert!(s
            .authenticate(&mkey)
            .unwrap_err()
            .contains("scoped to 'mount', not 'mcp'"));
        assert!(s
            .authenticate_mount(&akey)
            .unwrap_err()
            .contains("scoped to 'mcp', not 'mount'"));
    }

    /// The live-session re-check: `is_live` follows a revoke (and an unknown
    /// id) without re-proving the secret, and honors scope both ways.
    #[test]
    fn is_live_tracks_revoke_and_scope() {
        let s = store();
        let (id, _k) = s.create_for("mounter", Scope::Mount).unwrap();
        let (anyid, _) = s.create("anyone").unwrap();
        assert!(s.is_live(&id, Scope::Mount));
        assert!(
            !s.is_live(&id, Scope::Mcp),
            "scope still enforced on re-check"
        );
        assert!(s.is_live(&anyid, Scope::Mcp));
        assert!(s.revoke(&id).unwrap());
        assert!(
            !s.is_live(&id, Scope::Mount),
            "revoke reaches live sessions"
        );
        assert!(!s.is_live("nosuchid", Scope::Mount));
    }

    /// `live_state` is `is_live` plus the CURRENT `ro` column: the mount
    /// servers refresh their cached write gate from it every `KEY_RECHECK`
    /// tick, so `set_ro` hot-swaps a live session without a reconnect.
    #[test]
    fn live_state_carries_the_current_ro_flag() {
        let s = store();
        let (id, _k) = s.create_bound("toggler", Scope::Mount, None, true, None).unwrap();
        assert!(s.live_state(&id, Scope::Mount).unwrap().ro, "minted --ro");
        assert!(s.set_ro(&id, false).unwrap());
        assert!(
            !s.live_state(&id, Scope::Mount).unwrap().ro,
            "re-check sees the flip to rw"
        );
        // A new handshake reads the same value the re-check path reports.
        assert!(!s.authenticate_mount(&_k).unwrap().ro);
        assert!(s.set_ro(&id, true).unwrap());
        assert!(
            s.live_state(&id, Scope::Mount).unwrap().ro,
            "and back to ro"
        );
        // Liveness semantics match `is_live` exactly (scope + revoke).
        assert!(
            s.live_state(&id, Scope::Mcp).is_none(),
            "scope still gates re-check"
        );
        assert!(s.revoke(&id).unwrap());
        assert!(
            s.live_state(&id, Scope::Mount).is_none(),
            "revoke reaches live sessions"
        );
    }

    /// `set_ro` only touches an ACTIVE row: it reports false (and changes
    /// nothing) for a revoked or unknown id — a revoke is never silently
    /// reversed by a permission flip.
    #[test]
    fn set_ro_refuses_revoked_and_unknown_ids() {
        let s = store();
        let (id, _k) = s.create_bound("gone", Scope::Mount, None, false, None).unwrap();
        assert!(!s.set_ro("nosuchid", true).unwrap());
        assert!(s.revoke(&id).unwrap());
        assert!(
            !s.set_ro(&id, true).unwrap(),
            "a revoked row is not flipped"
        );
        assert!(!s.list().unwrap()[0].ro, "ro stayed as it was");
    }

    /// A key can now carry a view binding and a read-only flag. An unbound key
    /// admits any view (`binds_to` always true); a bound one only its own. The
    /// round-trip through `authenticate_mount` must carry both fields back so
    /// the server can enforce them at M_OPEN and on every write-intent op.
    #[test]
    fn view_binding_and_ro_travel_with_the_identity() {
        let s = store();
        let (_sid, sys) = s
            .create_bound("system", Scope::Mount, None, false, None)
            .unwrap();
        let (_vid, vw) = s
            .create_bound("viewer", Scope::Mount, Some("alice"), true, Some("alice"))
            .unwrap();
        let sys = s.authenticate_mount(&sys).unwrap();
        assert_eq!(sys.view, None, "system key is unbound");
        assert!(!sys.ro, "system key is read-write");
        assert!(
            sys.binds_to("alice") && sys.binds_to("bob"),
            "unbound admits every view"
        );
        let vw = s.authenticate_mount(&vw).unwrap();
        assert_eq!(vw.view.as_deref(), Some("alice"));
        assert!(vw.ro, "read-only flag survives auth");
        assert!(vw.binds_to("alice"), "bound key opens its view");
        assert!(!vw.binds_to("bob"), "bound key refused on another view");
    }

    /// The scope admission table is what keeps the governance entrance from
    /// being reachable with a credential that was issued for something else.
    /// `Any` is the dangerous one: every key minted before scopes existed
    /// carries it, so if `Any` admitted `Admin` an upgrade would silently
    /// promote a fleet of mount credentials into administrators.
    #[test]
    fn admits_is_a_table_so_any_never_means_administrator() {
        assert!(Scope::Any.admits(Scope::Mount), "any admits mount");
        assert!(Scope::Any.admits(Scope::Mcp), "any admits mcp");
        assert!(
            !Scope::Any.admits(Scope::Admin),
            "any must NEVER admit admin (that would promote pre-scope keys)"
        );
        assert!(Scope::Admin.admits(Scope::Admin));
        // An administrator is a superset of a user: it may mount and use MCP,
        // which also means one key can operate a deployment end to end.
        assert!(Scope::Admin.admits(Scope::Mount));
        assert!(Scope::Admin.admits(Scope::Mcp));
        assert!(!Scope::Mount.admits(Scope::Admin));
        assert!(!Scope::Mcp.admits(Scope::Admin));
        assert!(!Scope::Mount.admits(Scope::Mcp));
        assert!(!Scope::Mcp.admits(Scope::Mount));
    }

    /// An admin key is attributed to an ACCOUNT and admits every entrance, and
    /// the account survives the round trip through storage (that is what makes
    /// `apikey list` and the control-plane audit name a person rather than a
    /// label). A mount-scope key is still refused at the governance gate.
    #[test]
    fn admin_key_carries_its_account_and_gates_the_control_plane() {
        let s = store();
        let (id, key) = s
            .create_bound("ops-laptop", Scope::Admin, None, false, Some("alice"))
            .unwrap();
        let a = s.authenticate_admin(&key).unwrap();
        assert_eq!(a.owner.as_deref(), Some("alice"));
        assert_eq!(a.id, id);
        // One credential, every entrance: the same key mounts and speaks MCP.
        assert!(s.authenticate_for(&key, Scope::Mount).is_ok());
        assert!(s.authenticate_for(&key, Scope::Mcp).is_ok());
        let row = s.list().unwrap().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.owner.as_deref(), Some("alice"));
        assert_eq!(row.scope, "admin");
        // …and the reverse direction is closed.
        let (_mid, mkey) = s.create_bound("phone", Scope::Mount, None, false, Some("bob")).unwrap();
        assert!(
            s.authenticate_admin(&mkey).is_err(),
            "a mount key must not open the governance entrance"
        );
        // A revoke closes the control plane immediately (the same row the
        // server re-checks per call).
        assert!(s.revoke(&id).unwrap());
        assert!(s.authenticate_admin(&key).is_err());
    }

    #[test]
    fn calls_are_logged_and_attributed() {
        let s = store();
        let (_id, key) = s.create("worker").unwrap();
        let a = s.authenticate(&key).unwrap();
        s.log_call(
            Some(&a),
            "tools/call",
            "define_derived_view",
            true,
            "name=release",
        )
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
