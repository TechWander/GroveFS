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
//!   server and CLI pointing at it, with no per-node file to colocate.
//!
//! The backend is picked once at open time ([`ApiKeyStore::open`] for the
//! local file, [`ApiKeyStore::open_pg`] for a PG URI;
//! [`ApiKeyStore::open_for`] routes like the view backend). Both
//! expose identical columns, semantics and refusal strings.
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

/// The control plane in the `treedb` engine's generic tables (treedb DESIGN.md
/// §10 C 4, milestone M1). A child module so it can reuse this module's private
/// helpers (`now_iso`, `expiry_passed`, `StoredKey`, …) instead of copying them —
/// two copies of the verification inputs are exactly how two backends drift.
#[cfg(feature = "treedb")]
mod treedb_ctl;

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
    /// Free-text remark the operator attaches to a key (`apikey update
    /// --note`) — the field that makes a 200-row registry greppable.
    pub note: Option<String>,
    /// RFC 3339 deadline, `None` = never expires.
    pub expires_at: Option<String>,
    /// The SERVER's verdict on `expires_at` (the same comparison the auth path
    /// makes), so no client re-implements it and `list` cannot disagree with
    /// what actually authenticates.
    pub expired: bool,
    /// Account that MINTED the key — deliberately distinct from `owner`, who
    /// the key belongs to. An admin minting for a colleague is the common case.
    pub created_by: Option<String>,
    /// Masked fingerprint of the secret (`a1b2…c3d4`): enough to recognise one
    /// credential in a list, far too little to be a credential. See
    /// [`secret_hint`] for what is actually revealed.
    pub secret_hint: Option<String>,
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

/// 32 hex chars (128 bits nominal, 122 of them random — the v4 version/variant
/// bits are fixed) of secret from one v4 UUID (already a dependency). 122 bits
/// is comfortably past the 112-bit bar NIST asks of a symmetric credential: an
/// attacker who steals the store still faces 2^122 SHA-256 preimages, which is
/// as unreachable as 2^256 was, and the online path cannot be brute-forced at
/// any rate. The shorter half the operator actually pastes is the point.
fn gen_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The masked fingerprint stored beside the digest: FOUR hex chars from the
/// front of the secret and four from the back, joined by an ellipsis
/// (`a1b2…c3d4`). This is what the console's key list displays, so an operator
/// can recognise a credential without the store holding anything reversible.
/// What it gives away is 32 of the secret's 122 random bits — an attacker who
/// already stole the database still faces 2^90 work per key, which is the same
/// answer as facing 2^122: none. Revealing a CONTIGUOUS run would be worse
/// (a prefix is what typo-brute-forcing and grep-adjacency exploit), hence the
/// two short halves.
fn secret_hint(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return String::new();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

/// How long a remark may be. A registry field, not a document: past this the
/// column stops being scannable and starts being a place to hide payloads, so
/// the store refuses it at the door instead of truncating silently.
pub const NOTE_MAX: usize = 500;

fn check_note(note: Option<&str>) -> Result<(), String> {
    match note {
        Some(t) if t.chars().count() > NOTE_MAX => Err(format!(
            "that note is {} characters — the limit is {NOTE_MAX}, which is already more than a remark needs",
            t.chars().count()
        )),
        _ => Ok(()),
    }
}

/// Turn a TTL spelling (`30m`, `24h`, `7d`, `365d`, `never`, or empty) into the
/// instant it expires at, written in the store's own clock as the same RFC 3339
/// shape [`now_iso`] stamps everywhere else — an operator reading a row sees
/// local wall time, while [`expiry_passed`] compares the parsed instants, so the
/// display format costs nothing in correctness. `None` means "never expires"
/// and is stored as NULL.
/// The count-and-unit parser behind both duration spellings an operator types:
/// a key's deadline (`expires_from_ttl` below, measured FORWARD from now) and
/// an audit retention window (`cutoff_from_age`, measured BACKWARD from now).
/// `None` means "never". The refusal text keeps saying `bad ttl` verbatim —
/// README/USAGE/docs.html quote it character for character as CLI output.
pub fn duration_secs(ttl: &str) -> Result<Option<i64>, String> {
    let t = ttl.trim().to_lowercase();
    if t.is_empty() || t == "never" || t == "none" || t == "0" {
        return Ok(None);
    }
    let (digits, unit) = t.split_at(t.len() - 1);
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("bad ttl '{ttl}': expected a count and a unit, like 45m, 24h or 7d"))?;
    if n <= 0 {
        return Err(format!("bad ttl '{ttl}': the count must be positive (or `never`)"));
    }
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        other => {
            return Err(format!(
                "bad ttl '{ttl}': unit '{other}' is unknown — use s(econds), m(inutes), h(ours) or d(ays), or `never`"
            ))
        }
    };
    Ok(Some(secs))
}

/// Turn a TTL spelling (`30m`, `24h`, `7d`, `365d`, `never`, or empty) into the
/// instant it expires at, written in the store's own clock as the same RFC 3339
/// shape [`now_iso`] stamps everywhere else — an operator reading a row sees
/// local wall time, while [`expiry_passed`] compares the parsed instants, so the
/// display format costs nothing in correctness. `None` means "never expires"
/// and is stored as NULL.
pub fn expires_from_ttl(ttl: &str) -> Result<Option<String>, String> {
    let Some(secs) = duration_secs(ttl)? else {
        return Ok(None);
    };
    let when = chrono::Local::now()
        + chrono::Duration::try_seconds(secs).ok_or_else(|| format!("ttl '{ttl}' overflows"))?;
    Ok(Some(when.to_rfc3339()))
}

/// The stamp `ago` back from now — the boundary an audit retention window
/// keeps everything on or after. Same `to_rfc3339` shape [`log_call`] writes,
/// which is what makes comparing them as TEXT meaningful; the deadline side of
/// the store has relied on that ordering convention throughout.
///
/// [`log_call`]: Self::log_call
pub fn cutoff_from_age(ago: &str) -> Result<String, String> {
    let Some(secs) = duration_secs(ago)? else {
        return Err(format!(
            "bad retention '{ago}': audit rows need a real window like 90d, 24h or 365d (`never` would keep everything)"
        ));
    };
    let when = chrono::Local::now()
        - chrono::Duration::try_seconds(secs).ok_or_else(|| format!("retention '{ago}' overflows"))?;
    Ok(when.to_rfc3339())
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
                 owner       TEXT,
                 note        TEXT,
                 expires_at  TEXT,
                 created_by  TEXT,
                 secret_hint TEXT
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
    owner        TEXT,
    note         TEXT,
    expires_at   TEXT,
    created_by   TEXT,
    secret_hint  TEXT
)";
/// Column additions for stores minted before those columns existed. NOT a table
/// upgrade (nothing is rewritten, no row is touched): they only make the extra
/// columns visible, so the single SELECT/INSERT spelling works against an old
/// file or schema. `(column, SQLite ALTER, PG ALTER)` in application order; the
/// SQLite arm needs the `table_info` probe because SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, while PG's is idempotent on its own.
const KEY_MIGRATIONS: &[(&str, &str, &str)] = &[
    (
        "owner",
        "ALTER TABLE api_keys ADD COLUMN owner TEXT",
        "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS owner TEXT",
    ),
    (
        "note",
        "ALTER TABLE api_keys ADD COLUMN note TEXT",
        "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS note TEXT",
    ),
    (
        "expires_at",
        "ALTER TABLE api_keys ADD COLUMN expires_at TEXT",
        "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS expires_at TEXT",
    ),
    (
        "created_by",
        "ALTER TABLE api_keys ADD COLUMN created_by TEXT",
        "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS created_by TEXT",
    ),
    (
        "secret_hint",
        "ALTER TABLE api_keys ADD COLUMN secret_hint TEXT",
        "ALTER TABLE grovefs_control.api_keys ADD COLUMN IF NOT EXISTS secret_hint TEXT",
    ),
];
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
///
/// The first two arms route **two SQL dialects** (`exec2(sqlite_sql, pg_sql)`), which
/// is why the third backend is NOT a peer inside those helpers: treedb has no SQL,
/// so it dispatches one level UP, at the method layer — every verb asks
/// [`Self::treedb`] first and hands the semantics to [`treedb_ctl::TreedbKeys`].
/// The `Treedb` arms in `exec2`/`query2` are therefore unreachable by construction
/// and left as refusals rather than `unreachable!()` (the same rule the PG arm
/// follows).
enum Backend {
    Sqlite(parking_lot::Mutex<Connection>),
    Pg(PgPool),
    #[cfg(feature = "treedb")]
    Treedb(Box<treedb_ctl::TreedbKeys>),
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
    expires_at: Option<String>,
}

/// Milliseconds since the epoch, for the deadline comparison. Instant-based on
/// purpose: `expires_at` is written as a LOCAL RFC 3339 string (so an operator
/// reading the row sees their own wall clock), and comparing those strings
/// lexically would be wrong for two rows minted on either side of a DST shift.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Has this row's deadline passed on the store's clock? One place decides it,
/// so the handshake ([`ApiKeyStore::authenticate_for`]), the live-session
/// re-check ([`ApiKeyStore::live_state`]) and what `list` reports cannot
/// disagree about which keys still work. A NULL `expires_at` never expires; a
/// deadline that will NOT parse counts as EXPIRED, since failing open would make
/// a corrupted `expires_at` a way to hand a credential immortality.
fn expiry_passed(expires_at: Option<&str>, now_ms: i64) -> bool {
    let Some(raw) = expires_at else {
        return false;
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(when) => when.timestamp_millis() <= now_ms,
        Err(_) => true,
    }
}

/// The two normalisations every "edit the bookkeeping fields" write shares: a
/// blank remark is stored as `NULL` (not a zero-length string) and `Some(None)`
/// means "never expires". Written once so the SQL and engine paths cannot drift
/// about what an empty string means.
fn note_and_deadline<'a>(
    note: Option<&'a str>,
    expires_at: Option<Option<&'a str>>,
) -> (Option<&'a str>, Option<&'a str>) {
    (
        note.map(str::trim).filter(|t| !t.is_empty()),
        expires_at.unwrap_or(None),
    )
}

/// The engine arm a SQL-shaped `match` can never reach: every verb asks
/// [`ApiKeyStore::treedb`] first and returns there. A refusal (not a panic) keeps
/// the rule the PG arm already follows — a future verb that forgets the
/// delegation answers an error instead of taking a server down.
#[cfg(feature = "treedb")]
fn missed_delegation<T>() -> Result<T, String> {
    Err("treedb control: this verb has no engine path (a delegation was missed)".to_string())
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
        // A store from before the bookkeeping columns (`owner`, `note`,
        // `expires_at`, `created_by`, `secret_hint`) lacks them; add each in
        // place, once. Fresh stores get them from the DDL above, so this loop
        // is a no-op there.
        let mut names = conn
            .prepare("PRAGMA table_info(api_keys)")
            .and_then(|mut s| {
                let names = s
                    .query_map([], |r| r.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names)
            })
            .map_err(|e| e.to_string())?;
        for (column, sqlite, _) in KEY_MIGRATIONS {
            if !names.iter().any(|n| n == column) {
                conn.execute(sqlite, []).map_err(|e| e.to_string())?;
                names.push((*column).to_string());
            }
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
        // PG's ALTERs are idempotent, so the DDL and the migrations go in one
        // ordered batch (see [`PgPool::open`]).
        let mut ddl: Vec<&str> = vec![KEYS_DDL_PG, CALLS_DDL_PG];
        ddl.extend(KEY_MIGRATIONS.iter().map(|(_, _, pg)| *pg));
        let pool = PgPool::open(uri, &ddl)?;
        Ok(Self {
            backend: Backend::Pg(pool),
        })
    }

    /// 把控制面写进 treedb 引擎的通用表需要显式带上这个开关（见 [`Self::open_treedb`]）。
    #[cfg(feature = "treedb")]
    pub const TREEDB_CONTROL_ENV: &'static str = "GROVEFS_TREEDB_CONTROL";

    /// Whether this process has been told to keep its control plane in the
    /// treedb engine (`GROVEFS_TREEDB_CONTROL=engine`). One reader, so the route
    /// decision (`open_treedb`) and the address the servers pass in
    /// (`StoreKind::control_tables_uri`) cannot disagree about what the switch says.
    #[cfg(feature = "treedb")]
    pub fn treedb_control_enabled() -> bool {
        std::env::var(Self::TREEDB_CONTROL_ENV)
            .map(|v| v == "engine")
            .unwrap_or(false)
    }

    /// The control plane in the treedb engine's generic tables
    /// ([`treedb_ctl`]). Opt-in per process: `GROVEFS_TREEDB_CONTROL=engine`.
    ///
    /// Why a switch and not just "route on the URI": until the control tables have
    /// been FILLED (the migration milestone), they are empty — routing there
    /// unconditionally would report every key and account as unknown, which an
    /// operator would read as "nobody can log in", not as "the migration was not
    /// run". The switch is deleted once the migration lands.
    #[cfg(feature = "treedb")]
    pub fn open_treedb(uri: &str) -> Result<Self, String> {
        let on = Self::treedb_control_enabled();
        if !on {
            return Err(format!(
                "`{uri}` is a treedb tree store: its control plane lives in the engine's generic \
                 tables, which are only read when `{}` is set to `engine` (and only after they have \
                 been filled); without it, mint and check keys against the LOCAL SQLite beside the \
                 meta-server's --meta-dir",
                Self::TREEDB_CONTROL_ENV
            ));
        }
        let addr = crate::treedb_store::parse_addr(uri)?;
        let keys = treedb_ctl::TreedbKeys::open(&addr)?;
        Ok(Self {
            backend: Backend::Treedb(Box::new(keys)),
        })
    }

    /// The store for a metadata-backend selection: a `postgres[ql]://` URI
    /// routes to the PG control schema, anything else (including `None`) to the
    /// local file under `base` — the same routing rule the view definitions
    /// follow ([`crate::mcp::control_store_for`]). A `scheme://` URI neither
    /// backend serves is refused rather than quietly read from a local file.
    pub fn open_for(base: &Path, store_uri: Option<&str>) -> Result<Self, String> {
        match store_uri {
            // `postgres://…` — or a `file:` instance registry listing them
            // ([`crate::pg_store::is_pg_store`]); the pool binds the registry's
            // FIRST entry, where the control schema lives.
            Some(u) if crate::pg_store::is_pg_store(u) => Self::open_pg(u),
            // A `treedb://` tree store: the control plane moves into the engine's
            // generic tables, behind [`Self::TREEDB_CONTROL_ENV`] (see `open_treedb`).
            #[cfg(feature = "treedb")]
            Some(u) if crate::treedb_store::is_treedb_store(u) => Self::open_treedb(u),
            Some(u) if u.contains("://") => Err(format!(
                "`{u}` is not a --meta-store scheme GroveFS supports: credentials live in a local SQLite file or PostgreSQL (`postgres://…`)"
            )),
            _ => Self::open(base),
        }
    }

    /// Read one credential row by id, whichever backend holds it. The
    /// verification rules themselves live in [`Self::authenticate_for`] and
    /// [`Self::live_state`], so they cannot drift per backend.
    fn fetch_key(&self, id: &str) -> Result<Option<StoredKey>, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.fetch_key(id);
        }
        let mut rows = self.query2(
            "SELECT label, secret_hash, scope, revoked_at, view, ro, owner, expires_at FROM api_keys WHERE id = ?1",
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
                    cell::<String>(r, 7)?,
                ))
            },
            "SELECT label, secret_hash, scope, revoked_at, view_name, ro, owner, expires_at FROM grovefs_control.api_keys WHERE id = $1",
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
                    cell_str(r, 7),
                )
            },
        )?;
        Ok(rows.pop().map(
            |(label, secret_hash, scope, revoked_at, view, ro, owner, expires_at)| StoredKey {
                label,
                secret_hash,
                scope,
                revoked_at,
                view,
                ro: ro != 0,
                owner,
                expires_at,
            },
        ))
    }

    /// Record that `id` just authenticated. A no-op for an id that vanished
    /// between the read and this stamp (the SQL UPDATE matches no row), which is
    /// the same behaviour a revoke racing an authentication has always had.
    fn stamp_used(&self, id: &str) -> Result<(), String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.stamp_used(id);
        }
        let ts = now_iso();
        self.exec2(
            "UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2",
            params![ts, id],
            "UPDATE grovefs_control.api_keys SET last_used_at = $1 WHERE id = $2",
            &[&ts, &id],
        )
    }

    /// The engine backend's handle; `None` under SQLite/PG. Every verb's first
    /// line is this question, which is what keeps the treedb arm out of the SQL
    /// dispatchers (see [`Backend`]).
    #[cfg(feature = "treedb")]
    fn treedb(&self) -> Option<&treedb_ctl::TreedbKeys> {
        match &self.backend {
            Backend::Treedb(k) => Some(k),
            _ => None,
        }
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
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => Err(
                "treedb control: this statement has no engine spelling (a verb missed its \
                 delegation)"
                    .to_string(),
            ),
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
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => Err(
                "treedb control: this SELECT has no engine spelling (a verb missed its \
                 delegation)"
                    .to_string(),
            ),
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
    /// it. Keys minted without the bookkeeping fields have no note, no deadline
    /// and no recorded minter — see [`Self::create_extended`] for those.
    pub fn create_bound(
        &self,
        label: &str,
        scope: Scope,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
    ) -> Result<(String, String), String> {
        // One minting path only: the extra fields are simply absent, which is
        // exactly how a key minted before `--ttl`/`--note` existed looked.
        self.create_extended(label, scope, view, ro, owner, None, None, None)
    }

    /// Mint a new key with the full bookkeeping set (see [`Self::create_bound`]
    /// for the identity fields, plus): `note` (free-text remark), `expires_at`
    /// (an [`expires_from_ttl`] instant, `None` = never) and `created_by` — the
    /// ACCOUNT that minted it, which is NOT the same as `owner` (an admin
    /// minting for a colleague). The plaintext secret still never reaches the
    /// store: only its digest and the masked [`secret_hint`] do.
    pub fn create_extended(
        &self,
        label: &str,
        scope: Scope,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
        note: Option<&str>,
        expires_at: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<(String, String), String> {
        check_note(note)?;
        // The id is a lookup index, not a secret (auth rests on the digest
        // comparison), so 64 random bits — with the PRIMARY KEY as the
        // collision net — keeps it unguessable and short enough to paste.
        let id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
        let secret = gen_secret();
        let hash = sha256_hex(&secret);
        let hint = secret_hint(&secret);
        let full_key = format!("{id}:{secret}");
        let sc = scope.as_str();
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            // 引擎档：一行就是整行，`PUT_IF_ABSENT` 把「id 撞了」变成报错而不是覆盖别人的凭据。
            k.insert_key(
                &id,
                label,
                &hash,
                &hint,
                sc,
                view,
                ro,
                owner,
                note,
                expires_at,
                created_by,
            )?;
            return Ok((id, full_key));
        }
        let ts = now_iso();
        self.exec2(
            "INSERT INTO api_keys (id, label, secret_hash, created_at, scope, view, ro, owner, note, expires_at, created_by, secret_hint) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![id, label, hash, ts, sc, view, ro as i64, owner, note, expires_at, created_by, hint],
            "INSERT INTO grovefs_control.api_keys (id, label, secret_hash, created_at, scope, view_name, ro, owner, note, expires_at, created_by, secret_hint) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &[
                &id,
                &label,
                &hash,
                &ts,
                &sc,
                &view,
                &ro,
                &owner,
                &note,
                &expires_at,
                &created_by,
                &hint,
            ],
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
        // Checked before the digest, exactly like `revoked_at`: the deadline is
        // a property of the row the caller already proved to own, and naming it
        // is the difference between "rotate your credential" and a mystery.
        if expiry_passed(k.expires_at.as_deref(), now_ms()) {
            return Err(format!(
                "authentication failed: this api key expired at {} (extend it with `grovefs apikey update --id {id} --ttl <dur>`, or mint a fresh one)",
                k.expires_at.as_deref().unwrap_or("?")
            ));
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
    /// not wall-clock. Unknown id, revoked row, a row whose deadline has passed
    /// or a scope that no longer fits the entrance all report `None` — so an
    /// `--ttl` cuts a live mount off on this same path, not only at the next
    /// handshake.
    pub fn live_state(&self, id: &str, want: Scope) -> Option<LiveState> {
        let k = self.fetch_key(id).ok()??;
        if k.revoked_at.is_some()
            || expiry_passed(k.expires_at.as_deref(), now_ms())
            || !Scope::parse(&k.scope).unwrap_or(Scope::Any).admits(want)
        {
            return None;
        }
        Some(LiveState {
            live: true,
            ro: k.ro,
        })
    }

    /// Why an id that WAS accepted at handshake is now refused by the
    /// re-check tick, in the same words [`Self::authenticate_for`] uses for a
    /// fresh attempt. The two mount servers used to answer every mid-session
    /// cut with "has been revoked", which sent operators off to re-mint a
    /// credential that only needed `apikey update --ttl` — an expired row and a
    /// revoked row are different repairs, and the sentence a live mount prints
    /// on stderr is often the only one they see.
    ///
    /// Reads the row a second time, right after [`Self::live_state`] said
    /// `None`: one extra primary-key lookup on a path that only runs when a
    /// session is already being closed, never on the per-op hot path. The two
    /// reads are not one transaction, so a `--ttl` pushed forward between them
    /// can make the row look live again; that case says so plainly instead of
    /// claiming a cause that no longer holds.
    pub fn dead_reason(&self, id: &str, want: Scope) -> String {
        let k = match self.fetch_key(id) {
            Ok(Some(k)) => k,
            // Revoked rows are never deleted, so an absent id means the store
            // itself changed under the session (a fresh file, a wiped table).
            _ => return "authentication failed: unknown api key id".to_string(),
        };
        if k.revoked_at.is_some() {
            return "authentication failed: this api key has been revoked".to_string();
        }
        if expiry_passed(k.expires_at.as_deref(), now_ms()) {
            return format!(
                "authentication failed: this api key expired at {} (extend it with `grovefs apikey update --id {id} --ttl <dur>`, or mint a fresh one)",
                k.expires_at.as_deref().unwrap_or("?")
            );
        }
        let scope = Scope::parse(&k.scope).unwrap_or(Scope::Any);
        if !scope.admits(want) {
            return format!(
                "authentication failed: this api key is scoped to '{}', not '{}'",
                scope.as_str(),
                want.as_str()
            );
        }
        "authentication failed: this api key was refused by the live-credential re-check"
            .to_string()
    }

    /// Re-verify a credential a LIVE connection already pinned, and stamp it as
    /// used. The control plane needs both halves: it stopped presenting the key
    /// on every request (see `control_remote::GovPool`), so the store has to be
    /// consulted on the connection instead — otherwise a pinned session would
    /// sail through a revoke, and `apikey list` would keep reporting a key
    /// constantly in use as never used again.
    ///
    /// Two control-plane queries per [`KEY_RECHECK`] window per connection, on
    /// arriving traffic — never per request, and never for an idle socket.
    ///
    /// The digest is deliberately NOT re-compared, and that is what makes a
    /// rotation a rotation: `apikey rotate` replaces the credential for the NEXT
    /// handshake and leaves established sessions alone (its own help says so),
    /// while `revoke` and an elapsed `--ttl` are columns this reads, so they cut
    /// a pinned session off within the window exactly as they cut a live mount.
    pub fn revalidate_live(&self, id: &str, want: Scope) -> Result<(), String> {
        if self.live_state(id, want).is_none() {
            return Err(self.dead_reason(id, want));
        }
        self.stamp_used(id)
    }

    /// Flip the read-only flag of an ACTIVE key in place (`apikey set-ro`).
    /// Returns false if no active row matched `id` — a revoked key is NOT
    /// resurrected and never authenticates again, and silently flipping a
    /// dead row would make `list` lie about what revoke did. The new value is
    /// picked up by every live session on its next [`KEY_RECHECK`] tick
    /// ([`Self::live_state`]); a session established AFTER this call reads
    /// the flag at `authenticate_for` as usual.
    pub fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            // 引擎档：读 → 改 → 带「revoked_at 为空」的 CAS 写回（`RowPut` 整行覆盖）。
            return k.set_ro(id, ro);
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
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => return missed_delegation(),
        };
        Ok(n > 0)
    }

    /// Every key, newest first. No secret material is in the row set: what
    /// `list` can show about a secret is the masked [`secret_hint`] fingerprint.
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.list();
        }
        let rows = self.query2(
            "SELECT id, label, scope, view, ro, created_at, last_used_at, revoked_at, owner,
                    note, expires_at, created_by, secret_hint
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
                    note: cell(r, 9)?,
                    expires_at: cell(r, 10)?,
                    created_by: cell(r, 11)?,
                    secret_hint: cell(r, 12)?,
                    expired: false,
                })
            },
            "SELECT id, label, scope, view_name, ro, created_at, last_used_at, revoked_at, owner,
                    note, expires_at, created_by, secret_hint
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
                note: cell_str(r, 9),
                expires_at: cell_str(r, 10),
                created_by: cell_str(r, 11),
                secret_hint: cell_str(r, 12),
                expired: false,
            },
        )?;
        // `expired` is the SERVER's verdict, stamped from one clock reading for
        // the whole pass, so what a list claims and what the handshake enforces
        // cannot drift — and no client has to compare timestamp strings written
        // in another machine's timezone.
        let now = now_ms();
        Ok(rows
            .into_iter()
            .map(|mut k| {
                k.expired = expiry_passed(k.expires_at.as_deref(), now);
                k
            })
            .collect())
    }

    /// Rotate the secret of an ACTIVE key in place (`apikey rotate`): mint a
    /// new secret, store only its digest and the new masked fingerprint, return
    /// the new plaintext `"<id>:<secret>"` for one-time display — same contract
    /// as [`Self::create_bound`]. Everything else (id, label, scope, view
    /// binding, ro, owner, `created_at`) survives untouched, so audit rows and
    /// view bindings keep their attribution. Returns `None` if no ACTIVE row
    /// matched `id` — a revoked key stays dead, the same rule [`Self::set_ro`]
    /// follows. The OLD secret fails its next handshake (the digest no longer
    /// matches); a session already established on it lives until its next
    /// reconnect — [`Self::live_state`] re-checks liveness and `ro`, not the
    /// secret. To kill a credential NOW rather than replace it, revoke.
    pub fn rotate(&self, id: &str) -> Result<Option<String>, String> {
        let secret = gen_secret();
        let hash = sha256_hex(&secret);
        // The fingerprint follows the secret in the same statement: a list that
        // still showed the retired key's hint would point at a credential that
        // no longer exists.
        let hint = secret_hint(&secret);
        let full_key = format!("{id}:{secret}");
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return Ok(if k.rotate(id, &hash, &hint)? { Some(full_key) } else { None });
        }
        let n = match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(
                    "UPDATE api_keys SET secret_hash = ?1, secret_hint = ?2 WHERE id = ?3 AND revoked_at IS NULL",
                    params![hash, hint, id],
                )
                .map_err(|e| e.to_string())?,
            Backend::Pg(pool) => pool
                .execute(
                    "UPDATE grovefs_control.api_keys SET secret_hash = $1, secret_hint = $2 WHERE id = $3 AND revoked_at IS NULL",
                    &[&hash, &hint, &id],
                )? as usize,
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => return missed_delegation(),
        };
        Ok(if n > 0 { Some(full_key) } else { None })
    }

    /// Edit the bookkeeping fields of an ACTIVE key in place (`apikey update`):
    /// the remark and/or the deadline. `note` is written verbatim when given (an
    /// empty string clears it); `expires_at` distinguishes three cases —
    /// `None` leave alone, `Some(None)` remove the deadline ("never"),
    /// `Some(Some(instant))` set it (from [`expires_from_ttl`]). Returns false
    /// when no ACTIVE row matched: a REVOKED key is not editable, because
    /// editing a credential that can never authenticate again would make `list`
    /// advertise a live deadline on a dead key. Revoking is terminal by design
    /// (see [`Self::set_ro`] for the same rule).
    pub fn update_fields(
        &self,
        id: &str,
        note: Option<&str>,
        expires_at: Option<Option<&str>>,
    ) -> Result<bool, String> {
        check_note(note)?;
        let mut sets: Vec<&str> = Vec::new();
        if note.is_some() {
            sets.push("note");
        }
        if expires_at.is_some() {
            sets.push("expires_at");
        }
        if sets.is_empty() {
            return Err("nothing to update: pass --note and/or --ttl".to_string());
        }
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.update_fields(id, note, expires_at);
        }
        // Built from the same column list for both dialects, so the two
        // parameter vectors stay aligned by position (`exec2`'s contract) and a
        // new field costs one `push`, not two hand-written statements.
        let mut sql_s = String::from("UPDATE api_keys SET ");
        let mut sql_p = String::from("UPDATE grovefs_control.api_keys SET ");
        for (i, column) in sets.iter().enumerate() {
            if i > 0 {
                sql_s.push_str(", ");
                sql_p.push_str(", ");
            }
            let n = i + 1;
            sql_s.push_str(&format!("{column} = ?{n}"));
            sql_p.push_str(&format!("{column} = ${n}"));
        }
        // Bound at function scope: the parameter vectors hold references into
        // these, so they must outlive the statement, not an `if` arm.
        // An empty remark is stored as NULL — the shape `create_extended` writes
        // for a key without one — so a reader never has to tell "no note" apart
        // from "a note of zero length".
        let (note_text, deadline) = note_and_deadline(note, expires_at);
        let mut ps: Vec<&dyn rusqlite::ToSql> = Vec::new();
        let mut pp: Vec<&(dyn postgres::types::ToSql + Sync)> = Vec::new();
        if note.is_some() {
            ps.push(&note_text);
            pp.push(&note_text);
        }
        if expires_at.is_some() {
            ps.push(&deadline);
            pp.push(&deadline);
        }
        let n = sets.len() + 1;
        sql_s.push_str(&format!(" WHERE id = ?{n} AND revoked_at IS NULL"));
        sql_p.push_str(&format!(" WHERE id = ${n} AND revoked_at IS NULL"));
        ps.push(&id);
        pp.push(&id);
        let rows = match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(&sql_s, ps.as_slice())
                .map_err(|e| e.to_string())?,
            Backend::Pg(pool) => pool.execute(&sql_p, pp.as_slice())? as usize,
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => return missed_delegation(),
        };
        Ok(rows > 0)
    }

    /// Revoke a key by id (idempotent: returns false if no ACTIVE row matched,
    /// so the caller can distinguish "revoked now" from "already revoked /
    /// never existed"). Revoked keys fail authentication but stay in the table
    /// so historical `mcp_calls` rows keep their attribution.
    pub fn revoke(&self, id: &str) -> Result<bool, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.revoke(id);
        }
        let ts = now_iso();
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
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => return missed_delegation(),
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
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.log_call(
                key_id.as_deref(),
                key_label.as_deref(),
                method,
                tool,
                ok,
                detail,
            );
        }
        let ts = now_iso();
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
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.recent_calls(limit);
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

    /// How many audit rows are strictly older than `cutoff` (a stamp in the
    /// shape [`cutoff_from_age`] produces). This is the dry-run half of
    /// retention: an operator sees the size of the cut before taking it.
    pub fn count_calls_older_than(&self, cutoff: &str) -> Result<usize, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.count_calls_older_than(cutoff);
        }
        let mut rows = self.query2(
            "SELECT count(*) FROM mcp_calls WHERE ts < ?1",
            params![cutoff],
            |r| r.get::<_, i64>(0),
            "SELECT count(*) FROM grovefs_control.mcp_calls WHERE ts < $1",
            &[&cutoff],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(rows.pop().map(|n| n.max(0) as usize).unwrap_or(0))
    }

    /// Drop the audit rows older than `cutoff` and report how many went.
    ///
    /// Deliberately blunt: an audit trail is the one table where a partial or
    /// reordered delete would be worse than none, so the predicate is exactly
    /// what it says (`ts < cutoff`, no sampling, no cap) and the answer is a
    /// count the caller has to print. Rows are never updated in place, so a
    /// `ts` written at insert time is the row's own age for the rest of its
    /// life; comparing them as TEXT is the same convention the key deadlines in
    /// this table already rely on.
    ///
    /// The MCP/audit surface is shared by every tenant in the deployment, so
    /// this is a SUPERUSER verb — see the gate in the control plane.
    pub fn purge_calls_older_than(&self, cutoff: &str) -> Result<usize, String> {
        #[cfg(feature = "treedb")]
        if let Some(k) = self.treedb() {
            return k.purge_calls_older_than(cutoff);
        }
        match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute("DELETE FROM mcp_calls WHERE ts < ?1", params![cutoff])
                .map_err(|e| e.to_string()),
            Backend::Pg(pool) => pool
                .execute(
                    "DELETE FROM grovefs_control.mcp_calls WHERE ts < $1",
                    &[&cutoff],
                )
                .map(|n| n as usize),
            #[cfg(feature = "treedb")]
            Backend::Treedb(_) => missed_delegation(),
        }
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
        };
        assert_eq!(stored.len(), 64);
        assert_ne!(stored, key.split_once(':').unwrap().1);
    }

    /// The short format: 16-hex id + 32-hex secret = a 49-char key an operator
    /// can actually eyeball and paste.
    #[test]
    fn minted_keys_use_the_short_format() {
        let s = store();
        let (id, key) = s.create("short").unwrap();
        assert_eq!(id.len(), 16, "id is 16 hex chars");
        let (kid, secret) = key.split_once(':').unwrap();
        assert_eq!(kid, id);
        assert_eq!(secret.len(), 32, "secret is 32 hex chars");
        assert_eq!(key.len(), 49);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A legacy long key (32-hex id, 64-hex secret) keeps authenticating:
    /// the digest compare never looked at lengths, so shortening the mint
    /// format is zero-migration — old and new keys coexist in one table.
    #[test]
    fn legacy_long_keys_still_authenticate() {
        let s = store();
        let secret = format!(
            "{:032x}{:032x}",
            uuid::Uuid::new_v4().as_u128(),
            uuid::Uuid::new_v4().as_u128(),
        );
        let ts = now_iso();
        s.exec2(
            "INSERT INTO api_keys (id, label, secret_hash, created_at, scope, view, ro, owner) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params!["deadbeefcafe0123456789abcdef0123", "legacy", sha256_hex(&secret), ts, "any", None::<String>, 0i64, None::<String>],
            "INSERT INTO grovefs_control.api_keys (id, label, secret_hash, created_at, scope, view_name, ro, owner) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&"deadbeefcafe0123456789abcdef0123", &"legacy", &sha256_hex(&secret), &ts, &"any", &None::<String>, &0i64, &None::<String>],
        )
        .unwrap();
        let authed = s
            .authenticate(&format!("deadbeefcafe0123456789abcdef0123:{secret}"))
            .unwrap();
        assert_eq!(authed.label, "legacy");
    }

    /// `rotate` replaces only the secret: the id (and with it every audit row,
    /// view binding and the label/scope/ro/owner columns) survives, the new
    /// plaintext authenticates and the old one stops to.
    #[test]
    fn rotate_swaps_the_secret_and_keeps_the_identity() {
        let s = store();
        let (id, old_key) = s
            .create_bound("rotator", Scope::Mcp, Some("team"), true, Some("alice"))
            .unwrap();
        let new_key = s.rotate(&id).unwrap().expect("active row rotated");
        let (nid, _) = new_key.split_once(':').unwrap();
        assert_eq!(nid, id, "the id is stable across rotation");
        assert_ne!(new_key, old_key);
        // New secret lives, old secret dies at the next handshake…
        assert_eq!(s.authenticate(&new_key).unwrap().id, id);
        assert!(s.authenticate(&old_key).is_err());
        // …while identity and policy are untouched.
        let authed = s.authenticate(&new_key).unwrap();
        assert_eq!(authed.label, "rotator");
        assert!(authed.ro);
        assert_eq!(authed.owner.as_deref(), Some("alice"));
        assert!(authed.binds_to("team") && !authed.binds_to("other"));
        // Revoked stays revoked — rotate never half-resurrects a key.
        s.revoke(&id).unwrap();
        assert!(s.rotate(&id).unwrap().is_none());
        assert!(s.rotate("nosuchid").unwrap().is_none());
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
        let (id, _k) = s
            .create_bound("toggler", Scope::Mount, None, true, None)
            .unwrap();
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

    /// A pinned session is re-verified without ever presenting the secret, so
    /// this is the one function standing between connection reuse and a
    /// credential that outlived its revoke. Three things it must get right, all
    /// asserted here: a live row passes, a revoked or expired row refuses with
    /// the SAME sentence a fresh handshake would use (an operator reading an
    /// audit line cannot tell one wording from another by repair cost — one says
    /// `--ttl`, the other says re-mint), and a pass stamps `last_used_at`,
    /// because a gateway using a key all day is using it.
    #[test]
    fn revalidating_a_pinned_session_refuses_the_dead_and_stamps_the_live() {
        let s = store();
        let (id, _key) = s.create_for("governor", Scope::Admin).unwrap();
        let stamp = |s: &ApiKeyStore| {
            s.list()
                .unwrap()
                .into_iter()
                .find(|k| k.id == id)
                .unwrap()
                .last_used_at
        };
        // Never presented, never used — and still revalidatable, which is the
        // whole point: the connection is the thing that authenticated.
        assert_eq!(stamp(&s), None, "a minted key has not been used yet");
        s.revalidate_live(&id, Scope::Admin).unwrap();
        assert!(stamp(&s).is_some(), "a passing re-check IS a use");
        // Revoked: refused, in the revoke's own words.
        s.revoke(&id).unwrap();
        let why = s.revalidate_live(&id, Scope::Admin).unwrap_err();
        assert!(why.contains("revoked") && !why.contains("expired"), "{why}");
        // Expired: the opposite repair, and the opposite sentence.
        let (eid, _) = s.create_for("shortlived", Scope::Admin).unwrap();
        s.update_fields(&eid, None, Some(Some(&deadline_from_now(-1))))
            .unwrap();
        let why = s.revalidate_live(&eid, Scope::Admin).unwrap_err();
        assert!(why.contains("expired at") && why.contains("--ttl"), "{why}");
        // Scoped elsewhere: an `mcp` key cannot hold a governance session, even
        // one established before the scope mattered.
        let (mid, _) = s.create_for("mcp only", Scope::Mcp).unwrap();
        let why = s.revalidate_live(&mid, Scope::Admin).unwrap_err();
        assert!(why.contains("'mcp'") && why.contains("'admin'"), "{why}");
        // An id the store has never heard of.
        assert_eq!(
            s.revalidate_live("ffffffffffffffff", Scope::Admin)
                .unwrap_err(),
            "authentication failed: unknown api key id"
        );
    }

    /// `set_ro` only touches an ACTIVE row: it reports false (and changes
    /// nothing) for a revoked or unknown id — a revoke is never silently
    /// reversed by a permission flip.
    #[test]
    fn set_ro_refuses_revoked_and_unknown_ids() {
        let s = store();
        let (id, _k) = s
            .create_bound("gone", Scope::Mount, None, false, None)
            .unwrap();
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
        let (_mid, mkey) = s
            .create_bound("phone", Scope::Mount, None, false, Some("bob"))
            .unwrap();
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

    /// A store plus the DB FILE it wrote to, for the tests that check what is
    /// actually on disk: the column lists are the store's own description of
    /// itself, the bytes are the evidence.
    fn store_on_disk() -> (ApiKeyStore, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        std::mem::forget(dir);
        (ApiKeyStore::open(&p).unwrap(), p.join("meta").join("api_keys.db"))
    }

    /// An RFC 3339 stamp `secs` from now on the store's own clock — what
    /// [`expires_from_ttl`] writes for a TTL of that length (negative for a
    /// deadline already passed, which no TTL can produce but a waiting operator
    /// certainly will).
    fn deadline_from_now(secs: i64) -> String {
        let d = chrono::Duration::try_seconds(secs).unwrap();
        (chrono::Local::now() + d).to_rfc3339()
    }

    /// What the store keeps about a secret: a 9-character mask, and nothing that
    /// assembles into a credential. The plaintext is searched for in the raw
    /// file bytes (and the WAL, where a fresh write actually lands) — the one
    /// assertion that means "recoverable" rather than "not selected by `list`".
    #[test]
    fn the_store_holds_a_mask_never_a_secret() {
        let (s, path) = store_on_disk();
        let (id, key) = s
            .create_extended(
                "laptop",
                Scope::Any,
                None,
                false,
                Some("alice"),
                Some("back on the 3rd"),
                None,
                Some("admin"),
            )
            .unwrap();
        let secret = key.split_once(':').unwrap().1;
        let mut bytes = std::fs::read(&path).unwrap();
        // WAL keeps a recent write out of the main file; check both, so this
        // cannot pass just because the row had not been checkpointed yet.
        if let Ok(wal) = std::fs::read(path.with_extension("db-wal")) {
            bytes.extend_from_slice(&wal);
        }
        let hay = String::from_utf8_lossy(&bytes);
        assert!(!hay.contains(secret), "the plaintext secret reached the file");
        assert!(!hay.contains(&key), "nor the assembled key");
        assert!(
            hay.contains(&sha256_hex(secret)),
            "the digest is what is stored — the test would be vacuous without this"
        );
        let row = s.list().unwrap().into_iter().find(|k| k.id == id).unwrap();
        let hint = row.secret_hint.clone().unwrap();
        assert_eq!(hint.chars().count(), 9, "4 hex, an ellipsis, 4 hex: {hint}");
        let head: String = hint.chars().take(4).collect();
        let tail: String = hint.chars().skip(5).collect();
        assert!(secret.starts_with(&head) && secret.ends_with(&tail));
        // The middle stays hidden: 96 hex digits' worth of it, which is why the
        // mask is safe to print in a list.
        assert!(!hint.contains(&secret[4..secret.len() - 4]));
        assert!(!row.expired, "no deadline set is not a deadline passed");
        assert_eq!(row.note.as_deref(), Some("back on the 3rd"));
        assert_eq!(row.created_by.as_deref(), Some("admin"));
        assert_eq!(
            row.owner.as_deref(),
            Some("alice"),
            "whose key it is and who minted it are different columns"
        );
    }

    /// A key with a deadline is refused AFTER it and the refusal says so —
    /// "expired" and "revoked" need different repairs, and an operator who gets
    /// the wrong one of those words wastes the afternoon.
    #[test]
    fn an_expired_key_is_refused_and_says_so() {
        let s = store();
        let (id, key) = s
            .create_extended(
                "temp",
                Scope::Any,
                None,
                false,
                None,
                None,
                Some(&deadline_from_now(3600)),
                Some("admin"),
            )
            .unwrap();
        assert!(s.authenticate(&key).is_ok(), "a FUTURE deadline changes nothing");
        let listed = s.list().unwrap();
        assert!(!listed[0].expired);
        assert!(listed[0].expires_at.is_some());
        // Move the deadline into the past the only way there is from here.
        s.update_fields(&id, None, Some(Some(&deadline_from_now(-60))))
            .unwrap();
        let err = s.authenticate(&key).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert!(
            err.contains("--ttl"),
            "the refusal should name the repair: {err}"
        );
        assert!(
            s.list().unwrap()[0].expired,
            "`list`'s verdict and the handshake's must agree"
        );
    }

    /// The deadline is not only a handshake rule: a mount that authenticated
    /// while the key was valid loses its ESTABLISHED session on the re-check
    /// path, on the same ≤1s latency `revoke` advertises. `--ttl never` puts it
    /// back — an expired key is deferred, not dead (a revoked one is not).
    #[test]
    fn expiry_cuts_a_live_session_off() {
        let s = store();
        let (id, key) = s.create_for("mounter", Scope::Mount).unwrap();
        s.authenticate_mount(&key).unwrap();
        assert!(s.is_live(&id, Scope::Mount));
        s.update_fields(&id, None, Some(Some(&deadline_from_now(-1))))
            .unwrap();
        assert!(!s.is_live(&id, Scope::Mount));
        assert!(s.live_state(&id, Scope::Mount).is_none());
        s.update_fields(&id, None, Some(None)).unwrap();
        assert!(s.is_live(&id, Scope::Mount));
        assert!(s.authenticate_mount(&key).is_ok());
    }

    /// The sentence a mount prints when the re-check cuts it off must name the
    /// cause, because the two repairs are opposites: an expired key is extended
    /// with `apikey update --ttl`, a revoked one is gone and needs re-minting.
    /// Both used to print "has been revoked".
    #[test]
    fn the_mid_session_cut_names_the_real_reason() {
        let s = store();
        // Expired: the deadline and the repair both belong in the refusal.
        let (eid, _) = s.create_for("mounter", Scope::Mount).unwrap();
        s.update_fields(&eid, None, Some(Some(&deadline_from_now(-1))))
            .unwrap();
        assert!(s.live_state(&eid, Scope::Mount).is_none());
        let why = s.dead_reason(&eid, Scope::Mount);
        assert!(why.contains("expired at"), "{why}");
        assert!(
            why.contains("--ttl") && !why.contains("revoked"),
            "an expired key is extended, not re-minted: {why}"
        );
        // Revoked: the opposite wording, and no expiry invented for it.
        let (rid, _) = s.create_for("mounter", Scope::Mount).unwrap();
        s.revoke(&rid).unwrap();
        let why = s.dead_reason(&rid, Scope::Mount);
        assert!(why.contains("revoked"), "{why}");
        assert!(!why.contains("expired"), "{why}");
        // Re-scoped away from this entrance.
        let (mid, _) = s.create_for("mcp only", Scope::Mcp).unwrap();
        let why = s.dead_reason(&mid, Scope::Mount);
        assert!(why.contains("'mcp'") && why.contains("'mount'"), "{why}");
        // A row the re-check never found at all.
        assert_eq!(
            s.dead_reason("ffffffffffffffff", Scope::Mount),
            "authentication failed: unknown api key id"
        );
        // Still live (the two reads raced an `--ttl` pushed forward): do not
        // claim a cause the row does not have.
        let (lid, _) = s.create_for("mounter", Scope::Mount).unwrap();
        assert!(s
            .dead_reason(&lid, Scope::Mount)
            .contains("live-credential re-check"));
    }

    /// A typo in a TTL must not read as "no deadline": the whole point of the
    /// column is that a key can be given a finite life, so an unparseable
    /// spelling is an error at the door.
    #[test]
    fn ttl_spelling_is_validated_at_the_door() {
        // The empty string is the one blank allowed, and it means "no deadline"
        // (it is what an unset form field arrives as).
        assert!(expires_from_ttl("").unwrap().is_none());
        for never in ["never", "NEVER", " none ", "0"] {
            assert!(
                expires_from_ttl(never).unwrap().is_none(),
                "'{never}' means never"
            );
        }
        for dur in ["45m", "24h", "7d", "365d", "30s"] {
            assert!(expires_from_ttl(dur).unwrap().is_some(), "'{dur}' is a deadline");
        }
        for bad in ["7", "7x", "1w", "-3d", "0d", "d", "h", "1.5d", "some"] {
            assert!(
                expires_from_ttl(bad).is_err(),
                "'{bad}' must not silently mean 'never'"
            );
        }
    }

    /// `apikey update` edits the bookkeeping and NOTHING else: it refuses an
    /// empty call and an over-long remark instead of pretending, leaves the
    /// fields it was not given alone, stores a blank remark as NULL (no "blank
    /// but present" state for a reader to distinguish), and will not touch a
    /// revoked row.
    #[test]
    fn update_edits_the_bookkeeping_columns_only() {
        let s = store();
        let (id, key) = s
            .create_bound("keep", Scope::Mcp, Some("team1"), true, Some("alice"))
            .unwrap();
        let row = || {
            s.list()
                .unwrap()
                .into_iter()
                .find(|k| k.id == id)
                .unwrap()
        };
        assert!(
            s.update_fields(&id, None, None).is_err(),
            "an update that changes nothing should say so"
        );
        assert!(s.update_fields(&id, Some("hand over on the 3rd"), None).unwrap());
        assert_eq!(row().note.as_deref(), Some("hand over on the 3rd"));
        assert_eq!(row().scope, "mcp");
        assert_eq!(row().view.as_deref(), Some("team1"));
        assert!(row().ro);
        assert_eq!(row().owner.as_deref(), Some("alice"));
        assert!(
            s.authenticate(&key).is_ok(),
            "a remark is metadata, not a new credential"
        );
        // Setting one field leaves the other alone.
        s.update_fields(&id, None, Some(Some(&deadline_from_now(86400))))
            .unwrap();
        assert_eq!(row().note.as_deref(), Some("hand over on the 3rd"));
        assert!(!row().expired);
        // A blank remark is no remark.
        s.update_fields(&id, Some("   "), None).unwrap();
        assert_eq!(row().note, None);
        // Over-long is refused, not silently truncated — and a refused update
        // must not have written half a row.
        let long = "x".repeat(NOTE_MAX + 1);
        assert!(s.update_fields(&id, Some(&long), None).is_err());
        assert!(row().expires_at.is_some(), "the earlier deadline survived a refused note");
        // Revoke is final.
        s.revoke(&id).unwrap();
        assert!(!s.update_fields(&id, Some("x"), None).unwrap());
        assert!(!s.update_fields("nosuchid", Some("x"), None).unwrap());
    }

    /// A store written before the bookkeeping columns existed: opening it ADDS
    /// them in place and reads the old rows as unknown (`None`), rather than
    /// failing or inventing a value. This is the zero-migration promise the PG
    /// path shares with the file path, asserted on the file the operator has.
    #[test]
    fn a_pre_bookkeeping_store_is_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta").join("api_keys.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE api_keys (id TEXT PRIMARY KEY, label TEXT NOT NULL,
                        secret_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                        last_used_at TEXT, revoked_at TEXT,
                        scope TEXT NOT NULL DEFAULT 'any', view TEXT,
                        ro INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO api_keys (id, label, secret_hash, created_at, scope)
                        VALUES ('legacyid', 'old', 'hash', '2026-01-01T00:00:00+08:00', 'any');",
            )
            .unwrap();
        }
        let s = ApiKeyStore::open(dir.path()).unwrap();
        let row = &s.list().unwrap()[0];
        assert_eq!(row.id, "legacyid");
        assert_eq!(row.owner, None);
        assert_eq!(row.note, None);
        assert_eq!(row.expires_at, None);
        assert_eq!(row.secret_hint, None);
        assert!(!row.expired, "a row with no deadline is not an expired one");
        // Opening it a second time is a no-op, not a duplicate-column failure.
        assert!(ApiKeyStore::open(dir.path()).is_ok());
    }
}
