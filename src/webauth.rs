//! WEB accounts and the base views they are DELEGATED.
//!
//! Why accounts are a separate record and not just another API key: a key is a
//! machine capability — a random 122-bit secret (rendered as 32 hex chars),
//! stored only as a digest, which
//! is exactly what a mount or an agent needs and nothing else. A human login
//! needs the opposite properties: a password the operator picks and can be
//! handed a new one out-of-band, and (the part no key column expresses) a SET
//! of base views the person owns. So this module keeps ONE thing — who may sign
//! in to the governance web UI, and over which base views they may do it — and
//! leaves "what credential does a client present to a data plane" to
//! [`crate::apikeys`].
//!
//! Where it lives: the same control plane as the keys and the view
//! definitions, because that is the deployment's one system of record. Under
//! SQLite the two tables sit in `<base>/meta/_control/web_accounts.db` beside
//! `api_keys.db`; under PostgreSQL they are `grovefs_control.web_accounts` /
//! `web_grants`, so a secondary meta-server (or a second web gateway aimed at
//! the same URI) sees the same accounts with nothing to colocate.
//!
//! **The server verifies passwords.** The web gateway
//! ([`crate::web`]) holds the ONE root admin key and forwards the credentials a
//! human typed to a `login` op; the digest comparison and the PBKDF2 cost both
//! happen here, in the process that owns the store. A stolen gateway config
//! therefore yields no password material at all — only the admin key it could
//! not avoid holding.
//!
//! Delegation model, in one sentence: an account is granted BASE views, and it
//! manages everything that hangs off them — the tree's own lifecycle, the
//! derived lenses, aliases and forks built over it (resolved through the
//! definition chain, see [`resolve_base`]), and the mount/MCP keys bound to
//! those names. The grant never has to be repeated for a fork: [
//! `resolve_base`] walks `fork → base` (and `lens/alias → base`) on every
//! gated call, so an asset created by delegation stays owned by the base view's
//! owner instead of silently escaping them.

use rusqlite::{params, Connection};

use crate::control::PgPool;
use crate::mcp::ControlStore;

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

/// PBKDF2-HMAC-SHA256 rounds. 210 000 is the current OWASP floor for SHA-256
/// and costs a login ~100 ms on one core: slow enough that an offline dump is
/// not cheap, fast enough that a human never retries into a rate limiter
/// because the page felt stuck. Deliberately built on the `sha2` the crate
/// already links — this store must not drag a new crypto dependency (and its
/// supply chain) into the server binary for a few dozen lines of standard
/// key stretching.
const PBKDF2_ITER: u32 = 210_000;
/// Salt bytes (hex-doubled in the stored string).
const SALT_BYTES: usize = 16;

/// The HMAC-SHA256 block size (RFC 2104).
const BLOCK: usize = 64;

/// A key prepared for REPEATED HMAC-SHA256.
///
/// RFC 2104 folds the key into a 64-byte block and hashes that block before
/// the message. In PBKDF2 the key is the same password for every one of the
/// ~200 000 rounds, so the pad blocks — and, because a `Sha256` handle is
/// cloneable, the inner pad compression itself — are computed ONCE here rather
/// than once per round. That is not a micro-optimization: the naive per-round
/// form costs ~2 s per login, which is an availability bug on a governance
/// entrance (a wrong password becomes a cheap way to stall a meta-server).
struct HmacKey {
    /// State after the ipad block: clone it, feed the message, finalize — that
    /// is the entire inner hash.
    inner: sha2::Sha256,
    /// The opad block, fed before the inner digest.
    outer: [u8; BLOCK],
}

impl HmacKey {
    fn new(key: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut k = [0u8; BLOCK];
        if key.len() > BLOCK {
            let d = Sha256::digest(key);
            k[..32].copy_from_slice(&d[..]);
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0u8; BLOCK];
        let mut outer = [0u8; BLOCK];
        for i in 0..BLOCK {
            ipad[i] = k[i] ^ 0x36;
            outer[i] = k[i] ^ 0x5c;
        }
        let mut inner = Sha256::new();
        inner.update(ipad);
        Self { inner, outer }
    }

    /// `HMAC_k(m)` over a message given as slices (so the PBKDF2 first round's
    /// `salt || INT(1)` needs no temporary), reusing the prepared key state.
    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        use sha2::Digest;
        let mut h = self.inner.clone();
        for p in parts {
            h.update(p);
        }
        let mut o = sha2::Sha256::new();
        o.update(self.outer);
        o.update(h.finalize());
        let out = o.finalize();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out[..]);
        buf
    }
}

/// One HMAC-SHA256 (RFC 2104): the 64-byte block shape with ipad/opad.
/// The one-shot spelling of what [`HmacKey`] does, kept for the RFC 607-style
/// vectors the tests check against — production paths hold a `HmacKey` because
/// they call it in a loop and must not rebuild the pads each round.
#[cfg(test)]
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    HmacKey::new(key).mac(&[msg])
}

/// PBKDF2 (RFC 8018 §5.2) with HMAC-SHA256, `dkLen = 32` — so exactly one
/// block, no per-block `i` loop and nothing to concatenate. The round loop
/// keeps `U` in a stack array: one hasher clone and two compressions per
/// round, zero allocation.
fn pbkdf2_sha256(pw: &[u8], salt: &[u8], iter: u32) -> [u8; 32] {
    let k = HmacKey::new(pw);
    let mut u = k.mac(&[salt, &1u32.to_be_bytes()]);
    let mut t = u;
    for _ in 1..iter {
        u = k.mac(&[&u]);
        for (a, b) in t.iter_mut().zip(u.iter()) {
            *a ^= *b;
        }
    }
    t
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode hex, refusing anything that is not lowercase hex of even length.
fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Compare two digests without leaking which byte differed through timing.
/// Hand-rolled for the same reason PBKDF2 is: another dependency for ten lines
/// of `|=` accumulation is a bad trade in a security path we want auditable.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Stretch a password into the stored form
/// `pbkdf2-sha256$<iter>$<salt-hex>$<dk-hex>`. The salt comes from two v4
/// UUIDs (the crate's already-a-dependency entropy source, getrandom-backed).
pub fn hash_password(pw: &str) -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut salt = [0u8; SALT_BYTES];
    salt.copy_from_slice(&a.as_u128().to_le_bytes()[..SALT_BYTES]);
    salt.iter_mut()
        .zip(b.as_u128().to_le_bytes().iter())
        .for_each(|(s, x)| *s ^= x);
    let dk = pbkdf2_sha256(pw.as_bytes(), &salt, PBKDF2_ITER);
    format!("pbkdf2-sha256${PBKDF2_ITER}${}${}", hex(&salt), hex(&dk))
}

/// Verify a password against a stored digest. False for a malformed or
/// foreign-format hash (never a panic, never a silent accept), and it runs
/// PBKDF2 for any hash that parses so the refusal timing matches a wrong
/// password's.
pub fn verify_password(pw: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let Ok(iter) = parts[1].parse::<u32>() else {
        return false;
    };
    let (Some(salt), Some(want)) = (unhex(parts[2]), unhex(parts[3])) else {
        return false;
    };
    // The cost is a per-row property, so an operator RAISING `PBKDF2_ITER`
    // never locks out rows minted at the old count — they still carry (and
    // honor) it. What a row may NOT do is buy more CPU than this build is
    // configured to spend: the ceiling is the build constant, so a dumped or
    // hand-edited row claiming 4e9 rounds costs one ordinary login and then
    // fails the digest comparison. (Lowering the constant is the one
    // migration that does lock old rows out; bump it, never shrink it.)
    let iter = iter.clamp(1_000, PBKDF2_ITER);
    let dk = pbkdf2_sha256(pw.as_bytes(), &salt, iter);
    ct_eq(&dk, &want)
}

// ---------------------------------------------------------------------------
// Accounts, roles, grants
// ---------------------------------------------------------------------------

/// What an account may do in the web UI.
///
/// `Admin` is the deployment owner: every view, plus account and grant
/// management. `User` is a DELEGATED owner: nothing until a base view is
/// granted, then everything hanging off that base (its tree, its lenses, its
/// aliases, its forks, and keys bound to those names). Deliberately two values
/// and not a permission bitmask — the delegation question the UI asks is "who
/// owns this base view", and a flag pile would only let that question be
/// answered inconsistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    /// Parse a stored or supplied role. Unknown text is an ERROR, never a
    /// default: silently collapsing a typo into `User` would hand an operator a
    /// working-looking account with no rights and hide the mistake.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "admin" => Ok(Role::Admin),
            "user" => Ok(Role::User),
            other => Err(format!(
                "unknown account role '{other}' (expected admin|user)"
            )),
        }
    }
}

/// A sign-in name. Accounts are stored lowercase so `Alice` and `alice` cannot
/// become two identities holding two different grant sets.
pub fn valid_account(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-');
    !s.is_empty()
        && s.len() <= 64
        && s == s.to_lowercase()
        && s.chars().next().map(|c| c.is_ascii_alphanumeric()) == Some(true)
        && s.chars().all(ok)
}

/// Normalize and validate a supplied account name.
pub fn normalize_account(s: &str) -> Result<String, String> {
    let a = s.trim().to_lowercase();
    if !valid_account(&a) {
        return Err(format!(
            "invalid account name '{s}' (use 1-64 of a-z 0-9 . _ -, starting with a letter or digit)"
        ));
    }
    Ok(a)
}

/// One account row as the UI lists it.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub account: String,
    pub role: String,
    pub created_at: String,
    pub disabled_at: Option<String>,
    pub last_login_at: Option<String>,
    /// The base views this account is delegated (empty for an admin, which
    /// needs no grants — the UI shows the difference instead of hiding it).
    pub bases: Vec<String>,
}

/// One delegation row.
#[derive(Debug, Clone)]
pub struct GrantInfo {
    pub account: String,
    pub base_view: String,
    pub perm: String,
    pub granted_at: String,
    pub granted_by: Option<String>,
}

/// The outcome of a password check: enough to build a session without ever
/// handing the digest out.
#[derive(Debug, Clone)]
pub struct LoginOk {
    pub account: String,
    pub role: Role,
    pub bases: Vec<String>,
}

/// The two backend spellings of the same account/grant schema, exactly as
/// [`crate::apikeys::ApiKeyStore`] does it: identical columns and refusals on
/// both, dialect differences confined to the SQL strings.
enum Backend {
    Sqlite(parking_lot::Mutex<Connection>),
    /// The process-wide control-plane pool (see [`crate::control::shared`]),
    /// shared with the credential and definition stores.
    Pg(std::sync::Arc<PgPool>),
}

const ACCOUNTS_DDL_SQLITE: &str = "CREATE TABLE IF NOT EXISTS web_accounts (
    account       TEXT PRIMARY KEY,
    pw_hash       TEXT NOT NULL,
    role          TEXT NOT NULL DEFAULT 'user',
    created_at    TEXT NOT NULL,
    disabled_at   TEXT,
    last_login_at TEXT
);";
const GRANTS_DDL_SQLITE: &str = "CREATE TABLE IF NOT EXISTS web_grants (
    account    TEXT NOT NULL,
    base_view  TEXT NOT NULL,
    perm       TEXT NOT NULL DEFAULT 'manage',
    granted_at TEXT NOT NULL,
    granted_by TEXT,
    PRIMARY KEY (account, base_view)
);";

const ACCOUNTS_DDL_PG: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.web_accounts (
    account       TEXT PRIMARY KEY,
    pw_hash       TEXT NOT NULL,
    role          TEXT NOT NULL DEFAULT 'user',
    created_at    TEXT NOT NULL,
    disabled_at   TEXT,
    last_login_at TEXT
)";
const GRANTS_DDL_PG: &str = "CREATE TABLE IF NOT EXISTS grovefs_control.web_grants (
    account    TEXT NOT NULL,
    base_view  TEXT NOT NULL,
    perm       TEXT NOT NULL DEFAULT 'manage',
    granted_at TEXT NOT NULL,
    granted_by TEXT,
    PRIMARY KEY (account, base_view)
)";

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// The account + delegation store. Cheap to share (wrap in an `Arc`); the
/// SQLite handle is mutex-guarded and the PG handle is a pooled one.
pub struct WebAuthStore {
    backend: Backend,
}

impl WebAuthStore {
    /// Open (creating the schema on first use) the LOCAL store at
    /// `<base>/meta/_control/web_accounts.db`.
    pub fn open(base: &std::path::Path) -> Result<Self, String> {
        let path = crate::meta::control_db_path(base, "web_accounts");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             {ACCOUNTS_DDL_SQLITE}
             {GRANTS_DDL_SQLITE}"
        ))
        .map_err(|e| e.to_string())?;
        Ok(Self {
            backend: Backend::Sqlite(parking_lot::Mutex::new(conn)),
        })
    }

    /// Open the PostgreSQL control-plane tables. The DDL goes through
    /// [`crate::control::shared`], which serializes `CREATE` with the shared
    /// advisory lock — the same catalog race that the keys/definitions tables
    /// guard — and hands it the process's ONE pool for this database.
    pub fn open_pg(uri: &str) -> Result<Self, String> {
        let pool = crate::control::shared(uri, &[ACCOUNTS_DDL_PG, GRANTS_DDL_PG])?;
        Ok(Self {
            backend: Backend::Pg(pool),
        })
    }

    /// Route by metadata backend, the same rule the keys and definitions
    /// follow: a PostgreSQL URI (inline list or `file:` registry) uses the
    /// shared schema, anything else the local file. An unknown `scheme://` is
    /// refused rather than quietly falling back to a file the deployment never
    /// reads.
    pub fn open_for(base: &std::path::Path, store_uri: Option<&str>) -> Result<Self, String> {
        match store_uri {
            Some(u) if crate::pg_store::is_pg_store(u) => Self::open_pg(u),
            // treedb serves trees only; web accounts stay in the LOCAL SQLite,
            // exactly like the api keys beside them (see
            // [`crate::apikeys::ApiKeyStore::open_for`] for the same arm).
            #[cfg(feature = "treedb")]
            Some(u) if crate::treedb_store::is_treedb_store(u) => Self::open(base),
            Some(u) if u.contains("://") => Err(format!(
                "`{u}` is not a --meta-store scheme GroveFS supports: web accounts live in a local SQLite file or PostgreSQL (`postgres://…`)"
            )),
            _ => Self::open(base),
        }
    }

    /// One parameterized statement, backend-routed (see
    /// [`crate::apikeys::ApiKeyStore::exec2`] for the same shape).
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
        }
    }

    /// Is there no account yet? This is what makes the install-time bootstrap
    /// safe to leave on the command line: it can only ever fill an EMPTY store,
    /// never overwrite or add a second owner.
    pub fn is_empty(&self) -> Result<bool, String> {
        let n: Vec<i64> = self.query2(
            "SELECT COUNT(*) FROM web_accounts",
            &[],
            |r| r.get::<_, i64>(0),
            "SELECT COUNT(*) FROM grovefs_control.web_accounts",
            &[],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(n.first().copied().unwrap_or(0) == 0)
    }

    /// The bases delegated to one account, sorted (the UI and the ACL both want
    /// a stable order).
    pub fn bases_of(&self, account: &str) -> Result<Vec<String>, String> {
        let mut v = self.query2(
            "SELECT base_view FROM web_grants WHERE account = ?1 ORDER BY base_view",
            params![account],
            |r| r.get::<_, String>(0),
            "SELECT base_view FROM grovefs_control.web_grants WHERE account = $1 ORDER BY base_view",
            &[&account],
            |r| r.get::<_, String>(0),
        )?;
        v.sort();
        v.dedup();
        Ok(v)
    }

    /// Every account, with its grants folded in, sorted by name.
    pub fn list(&self) -> Result<Vec<AccountInfo>, String> {
        let rows = self.query2(
            "SELECT account, role, created_at, disabled_at, last_login_at FROM web_accounts ORDER BY account",
            &[],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
            "SELECT account, role, created_at, disabled_at, last_login_at FROM grovefs_control.web_accounts ORDER BY account",
            &[],
            |r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    r.get::<_, String>(2),
                    r.get::<_, Option<String>>(3),
                    r.get::<_, Option<String>>(4),
                )
            },
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for (account, role, created_at, disabled_at, last_login_at) in rows {
            let bases = self.bases_of(&account)?;
            out.push(AccountInfo {
                account,
                role,
                created_at,
                disabled_at,
                last_login_at,
                bases,
            });
        }
        Ok(out)
    }

    /// How many accounts can still administer this deployment. Two governance
    /// verbs consult it — demoting and disabling (or deleting) — because both
    /// can strand a store with no owner. A LOCKED-OUT admin group has no
    /// self-service way back: recovering it means editing the table by hand
    /// under the meta-server, which is a worse outage than the one the operator
    /// asked for. The last admin can still hand the keys over deliberately by
    /// creating a second admin first.
    pub fn active_admins(&self) -> Result<usize, String> {
        let n = self.query2(
            "SELECT COUNT(*) FROM web_accounts WHERE role = 'admin' AND disabled_at IS NULL",
            &[],
            |r| r.get::<_, i64>(0),
            "SELECT COUNT(*) FROM grovefs_control.web_accounts WHERE role = 'admin' AND disabled_at IS NULL",
            &[],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(n.first().copied().unwrap_or(0) as usize)
    }

    pub fn get(&self, account: &str) -> Result<Option<AccountInfo>, String> {
        Ok(self.list()?.into_iter().find(|a| a.account == account))
    }

    /// The stored digest + liveness for one account, in the shape the login
    /// path needs. Kept private so no caller can print a hash.
    fn fetch(&self, account: &str) -> Result<Option<(String, String, Option<String>)>, String> {
        let mut rows = self.query2(
            "SELECT pw_hash, role, disabled_at FROM web_accounts WHERE account = ?1",
            params![account],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
            "SELECT pw_hash, role, disabled_at FROM grovefs_control.web_accounts WHERE account = $1",
            &[&account],
            |r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    r.get::<_, Option<String>>(2),
                )
            },
        )?;
        Ok(rows.pop())
    }

    /// Create an account with an already-stretched digest (see
    /// [`hash_password`]). Fails if the name exists — a create that quietly
    /// resets a password would be a login bypass with the wrong name.
    pub fn create(&self, account: &str, pw_hash: &str, role: Role) -> Result<(), String> {
        let ts = now_iso();
        let r = role.as_str();
        self.exec2(
            "INSERT INTO web_accounts (account, pw_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![account, pw_hash, r, ts],
            "INSERT INTO grovefs_control.web_accounts (account, pw_hash, role, created_at) VALUES ($1, $2, $3, $4)",
            &[&account, &pw_hash, &r, &ts],
        )
    }

    /// Set a new password. The caller must have established it may (an admin,
    /// or the account's own holder through a verified login) — this store does
    /// not decide, it only refuses an unknown name.
    pub fn set_password(&self, account: &str, pw_hash: &str) -> Result<bool, String> {
        let n = self.changes2(
            "UPDATE web_accounts SET pw_hash = ?1 WHERE account = ?2",
            params![pw_hash, account],
            "UPDATE grovefs_control.web_accounts SET pw_hash = $1 WHERE account = $2",
            &[&pw_hash, &account],
        )?;
        Ok(n > 0)
    }

    pub fn set_role(&self, account: &str, role: Role) -> Result<bool, String> {
        let r = role.as_str();
        let n = self.changes2(
            "UPDATE web_accounts SET role = ?1 WHERE account = ?2",
            params![r, account],
            "UPDATE grovefs_control.web_accounts SET role = $1 WHERE account = $2",
            &[&r, &account],
        )?;
        Ok(n > 0)
    }

    /// Disable (or re-enable) without deleting: the account's grants and its
    /// audit history stay attributable, exactly like a revoked key keeps its
    /// row. A disabled account fails every login and every gated call.
    pub fn set_disabled(&self, account: &str, disabled: bool) -> Result<bool, String> {
        let ts = if disabled { Some(now_iso()) } else { None };
        let n = self.changes2(
            "UPDATE web_accounts SET disabled_at = ?1 WHERE account = ?2",
            params![ts, account],
            "UPDATE grovefs_control.web_accounts SET disabled_at = $1 WHERE account = $2",
            &[&ts, &account],
        )?;
        Ok(n > 0)
    }

    /// Drop the account and its grants. Deliberate about ordering: the grants
    /// go first, so an error between the two cannot leave an owner-less grant
    /// row that a later re-create would silently inherit. Keys this account
    /// minted are NOT touched — they are machine credentials that may be
    /// serving live mounts; the UI tells the operator to revoke them.
    pub fn delete(&self, account: &str) -> Result<bool, String> {
        self.exec2(
            "DELETE FROM web_grants WHERE account = ?1",
            params![account],
            "DELETE FROM grovefs_control.web_grants WHERE account = $1",
            &[&account],
        )?;
        let n = self.changes2(
            "DELETE FROM web_accounts WHERE account = ?1",
            params![account],
            "DELETE FROM grovefs_control.web_accounts WHERE account = $1",
            &[&account],
        )?;
        Ok(n > 0)
    }

    fn stamp_login(&self, account: &str) -> Result<(), String> {
        let ts = now_iso();
        self.exec2(
            "UPDATE web_accounts SET last_login_at = ?1 WHERE account = ?2",
            params![ts, account],
            "UPDATE grovefs_control.web_accounts SET last_login_at = $1 WHERE account = $2",
            &[&ts, &account],
        )
    }

    /// Grant a base view to an account. Idempotent (a re-grant refreshes the
    /// record rather than erroring), and it validates the NAME shape only —
    /// whether the tree exists is the caller's business, because a deployment
    /// may grant a view it is about to create, and refusing here would make
    /// provisioning a two-phase dance with no upside.
    pub fn grant(
        &self,
        account: &str,
        base_view: &str,
        granted_by: Option<&str>,
    ) -> Result<(), String> {
        let ts = now_iso();
        self.exec2(
            "INSERT INTO web_grants (account, base_view, perm, granted_at, granted_by) VALUES (?1, ?2, 'manage', ?3, ?4)
             ON CONFLICT(account, base_view) DO UPDATE SET granted_at = excluded.granted_at, granted_by = excluded.granted_by",
            params![account, base_view, ts, granted_by],
            "INSERT INTO grovefs_control.web_grants (account, base_view, perm, granted_at, granted_by) VALUES ($1, $2, 'manage', $3, $4)
             ON CONFLICT(account, base_view) DO UPDATE SET granted_at = EXCLUDED.granted_at, granted_by = EXCLUDED.granted_by",
            &[&account, &base_view, &ts, &granted_by],
        )
    }

    pub fn ungrant(&self, account: &str, base_view: &str) -> Result<bool, String> {
        let n = self.changes2(
            "DELETE FROM web_grants WHERE account = ?1 AND base_view = ?2",
            params![account, base_view],
            "DELETE FROM grovefs_control.web_grants WHERE account = $1 AND base_view = $2",
            &[&account, &base_view],
        )?;
        Ok(n > 0)
    }

    pub fn list_grants(&self) -> Result<Vec<GrantInfo>, String> {
        self.query2(
            "SELECT account, base_view, perm, granted_at, granted_by FROM web_grants ORDER BY account, base_view",
            &[],
            |r| {
                Ok(GrantInfo {
                    account: r.get::<_, String>(0)?,
                    base_view: r.get::<_, String>(1)?,
                    perm: r.get::<_, String>(2)?,
                    granted_at: r.get::<_, String>(3)?,
                    granted_by: r.get::<_, Option<String>>(4)?,
                })
            },
            "SELECT account, base_view, perm, granted_at, granted_by FROM grovefs_control.web_grants ORDER BY account, base_view",
            &[],
            |r| {
                GrantInfo {
                    account: r.get::<_, String>(0),
                    base_view: r.get::<_, String>(1),
                    perm: r.get::<_, String>(2),
                    granted_at: r.get::<_, String>(3),
                    granted_by: r.get::<_, Option<String>>(4),
                }
            },
        )
    }

    /// One UPDATE/DELETE that must report how many rows it hit.
    fn changes2(
        &self,
        sql_s: &str,
        ps: &[&dyn rusqlite::ToSql],
        sql_p: &str,
        pp: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<u64, String> {
        match &self.backend {
            Backend::Sqlite(m) => m
                .lock()
                .execute(sql_s, ps)
                .map(|n| n as u64)
                .map_err(|e| e.to_string()),
            Backend::Pg(pool) => pool.execute(sql_p, pp),
        }
    }

    /// Verify a sign-in. Every failure path returns the SAME wording — an
    /// unknown account and a wrong password must not be distinguishable to
    /// whoever holds the gateway's URL. Runs PBKDF2 for any existing row (even
    /// a disabled one) so timing does not leak liveness either.
    pub fn login(&self, account: &str, password: &str) -> Result<LoginOk, String> {
        let bad = || "invalid account or password".to_string();
        let Some((pw_hash, role, disabled_at)) = self.fetch(account)? else {
            // Burn a comparable amount of work on the no-row path.
            let _ = pbkdf2_sha256(password.as_bytes(), b"grovefs-timing-pad", PBKDF2_ITER);
            return Err(bad());
        };
        let ok = verify_password(password, &pw_hash);
        if !ok {
            return Err(bad());
        }
        if disabled_at.is_some() {
            return Err("this account is disabled — ask an administrator to enable it".into());
        }
        let role = Role::parse(&role).unwrap_or(Role::User);
        let bases = self.bases_of(account)?;
        self.stamp_login(account)?;
        Ok(LoginOk {
            account: account.to_string(),
            role,
            bases,
        })
    }
}

// ---------------------------------------------------------------------------
// Install-time owner bootstrap
// ---------------------------------------------------------------------------

/// Shortest password the console accepts. Below this an owner account on a
/// network-reachable port is a brute-force target with a name already printed
/// on it; the check is cheap and it is the only policy that can be enforced
/// without locking out a legitimate operator's password manager output.
pub const MIN_PASSWORD: usize = 10;

/// Check a supplied password, returning the refusal when it is too weak to
/// guard a governance entrance.
pub fn check_password(account: &str, password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(format!(
            "password too short: use at least {MIN_PASSWORD} characters"
        ));
    }
    if password.trim() != password {
        return Err("password must not start or end with whitespace (a shell or paste artifact is usually the cause)".into());
    }
    if !account.is_empty() && password.eq_ignore_ascii_case(account) {
        return Err("password must not be the account name".into());
    }
    Ok(())
}

/// Seed the deployment's FIRST owner account. Returns a line for the server
/// banner; every outcome but `Ok` is a start-up failure, and the refusal wording
/// names the command that instead applies.
///
/// The rule this implements is the one the control plane already keeps for
/// admin keys — a governance entrance must never be able to mint its own
/// administrator — adapted to the fact that an install flag stays in a systemd
/// unit and is therefore re-read on EVERY start:
///
/// - empty store → create `account` as `admin` (the install-time moment);
/// - non-empty store where `account` already exists as an admin → ignored with
///   a note, so the second boot of the same unit is not an outage;
/// - non-empty store where `account` does NOT exist → refused: adding another
///   owner through a flag would bypass the attributable path (the console, or
///   a bootstrap the operator can see), which is exactly what the gate is for.
pub fn bootstrap_owner(
    store: &WebAuthStore,
    account: &str,
    password: &str,
) -> Result<String, String> {
    let account = normalize_account(account)?;
    if store.is_empty()? {
        check_password(&account, password)?;
        store.create(&account, &hash_password(password), Role::Admin)?;
        return Ok(format!(
            "created the first web account '{account}' (role admin) from the install-time flags — sign in with it, then delegate base views"
        ));
    }
    match store.get(&account)? {
        Some(a) if a.role == Role::Admin.as_str() && a.disabled_at.is_none() => Ok(format!(
            "install-time web account '{account}' already exists — nothing seeded (change its password in the console or with `web passwd`), so a restart re-reading the flag is not a lockout"
        )),
        Some(a) if a.disabled_at.is_some() => Err(format!(
            "the install-time account '{account}' EXISTS but is disabled — enable it (`web enable --account {account}` or the console) instead of resetting it from a start-up flag"
        )),
        Some(other) => Err(format!(
            "the install-time account '{account}' already exists with role '{}'; this flag only seeds the FIRST owner — promote it in the console (`account set-role {account} admin`) if that is intended",
            other.role
        )),
        None => Err(format!(
            "the web account store is not empty, so refusing to add '{account}' from a start-up flag: an administrator must create accounts through the console (audited) — pass --admin-user only on the deployment's first start"
        )),
    }
}

// ---------------------------------------------------------------------------
// Delegation resolution: name -> owning base view
// ---------------------------------------------------------------------------

/// How many definition hops a name may be chased before it is called broken.
/// A lens/alias over a fork is the deepest real shape (fork -> base, alias ->
/// fork); anything longer is a cycle or a hand-edited definition that no
/// caller may read as a grant.
const MAX_BASE_HOPS: usize = 4;

/// The question "does this name own a physical tree?", as one specific caller
/// answers it.
///
/// It is a parameter and not a `ControlStore` method because the control store
/// CANNOT always answer it. Under `treedb://` the trees live inside the engine
/// and the control plane is local SQLite, whose `has_physical` looks for a
/// `meta/<name>/<name>.db` file that this deployment never writes — so a store
/// read on its own reports "no tree" for a base that is mounted, written and
/// listed by `view list` at the same instant. The meta-server passes its own
/// enumerator (`server_ext::physical_tree_exists`), the one `view list` answers
/// from, so name resolution can never disagree with the listing the operator is
/// looking at. Every other caller keeps [`ControlStore::has_physical`].
pub type TreeProbe<'a> = &'a dyn Fn(&str) -> Result<bool, String>;

/// Resolve any view name to the BASE (physical, un-delegated-by-definition)
/// view that owns it:
///
/// - a bare physical view is its own base;
/// - `derived(b)` / `fork(b)` / `alias(r)` resolve through to `b`/`r`, and so on
///   until a base appears.
///
/// This is what makes a grant mean "manage this tree AND everything shaped out
/// of it" without a second bookkeeping table: the definition chain already
/// records the parent, so re-deriving it cannot drift from what a mount
/// resolves. An unknown name, a chain with no physical end, or a cycle is an
/// error — the ACL call site treats an error as a refusal, never as "assume
/// allowed".
pub fn resolve_base(store: &dyn ControlStore, name: &str) -> Result<String, String> {
    resolve_base_tree(store, name, &|n| store.has_physical(n))
}

/// [`resolve_base`] with the tree question supplied from outside. A caller
/// whose trees do not live in the control store MUST use this — see
/// [`TreeProbe`] for why `store.has_physical` is not a safe default there.
pub fn resolve_base_tree(
    store: &dyn ControlStore,
    name: &str,
    has_tree: TreeProbe<'_>,
) -> Result<String, String> {
    let mut cur = name.to_string();
    for _ in 0..MAX_BASE_HOPS {
        let def = match store.get_def(&cur)? {
            Some(d) => d,
            None => {
                // No definition: a base only if its tree actually exists (the
                // same gate `view_provider` applies to a mount).
                if has_tree(&cur)? {
                    return Ok(cur);
                }
                return Err(format!("view '{cur}' has neither a definition nor a tree"));
            }
        };
        let parent = match def {
            crate::layout::ViewDef::Derived(b, _) => b,
            crate::layout::ViewDef::Fork(b) => b,
            crate::layout::ViewDef::Alias(r) => r,
        };
        if parent == cur {
            return Err(format!("view '{cur}' is defined over itself"));
        }
        cur = parent;
    }
    Err(format!(
        "view '{name}' does not resolve to a base view within {MAX_BASE_HOPS} definition hops (a cycle?)"
    ))
}

/// The caller's authority for one governance call, computed from the store on
/// every request (a grant revoked mid-session must bite on the NEXT call, not
/// at the next login — the same principle as the mount plane's key re-check).
#[derive(Debug, Clone)]
pub struct Principal {
    /// The account the call is attributed to.
    pub account: String,
    pub role: Role,
    /// The base views this account may manage. Ignored when
    /// [`Self::is_super`].
    pub bases: Vec<String>,
}

impl Principal {
    /// The principal for an actor named on a governance call.
    ///
    /// Re-read from the account store on EVERY request rather than carried in a
    /// token: a disabled account or a revoked delegation must bite on the next
    /// call, the same way a revoked api key bites on the mount plane's re-check.
    /// A `None` row and a disabled row are refused with different wording on
    /// purpose — this path is never reachable by an unauthenticated caller (the
    /// gateway has already verified the sign-in), so telling the operator
    /// "that account is disabled" is information they are entitled to.
    pub fn lookup(store: &WebAuthStore, account: &str) -> Result<Self, String> {
        let account = normalize_account(account)?;
        let Some((_, role, disabled_at)) = store.fetch(&account)? else {
            return Err(format!(
                "no web account '{account}' — the console's session outlived the account (it was deleted); sign in again"
            ));
        };
        if disabled_at.is_some() {
            return Err(format!(
                "web account '{account}' is disabled — it cannot govern anything until an admin enables it"
            ));
        }
        let role = Role::parse(&role)?;
        let bases = store.bases_of(&account)?;
        Ok(Principal {
            account,
            role,
            bases,
        })
    }

    /// The caller behind a governance connection that names NO actor: an
    /// operator or MCP server holding an `admin`-scope account key. That key
    /// already means "full governance" everywhere else in GroveFS, so this
    /// principal is `Admin` with no base list, and its calls are attributed to
    /// the key's owner rather than to a web account.
    pub fn operator(label: &str) -> Self {
        Principal {
            account: if label.is_empty() {
                "-".to_string()
            } else {
                label.to_string()
            },
            role: Role::Admin,
            bases: Vec::new(),
        }
    }

    pub fn is_super(&self) -> bool {
        self.role == Role::Admin
    }

    /// May this principal manage the base view `base`?
    pub fn manages_base(&self, base: &str) -> bool {
        self.is_super() || self.bases.iter().any(|b| b == base)
    }

    /// May this principal manage the view `name`, whatever it is? Resolves the
    /// ownership chain first, so a lens/fork/alias is authorized by the base it
    /// hangs off.
    pub fn manages(&self, store: &dyn ControlStore, name: &str) -> Result<bool, String> {
        self.manages_tree(store, name, &|n| store.has_physical(n))
    }

    /// [`Self::manages`] with the tree question supplied by the caller. See
    /// [`TreeProbe`]: a server whose trees sit in an external engine must ask
    /// through it, or a delegated account governing a live treedb base reads as
    /// governing nothing.
    pub fn manages_tree(
        &self,
        store: &dyn ControlStore,
        name: &str,
        has_tree: TreeProbe<'_>,
    ) -> Result<bool, String> {
        if self.is_super() {
            return Ok(true);
        }
        let base = resolve_base_tree(store, name, has_tree)?;
        Ok(self.manages_base(&base))
    }

    /// The refusal an operator sees when the answer is no. Names the base it
    /// resolved to rather than saying "forbidden", so the fix (`grant <account>
    /// <base>`) is obvious from the message alone.
    pub fn refusal(&self, store: &dyn ControlStore, name: &str) -> String {
        self.refusal_tree(store, name, &|n| store.has_physical(n))
    }

    /// [`Self::refusal`] resolved against the caller's own tree question, so the
    /// sentence an operator reads cannot blame them for a tree the backend can
    /// see but the control store cannot.
    pub fn refusal_tree(&self, store: &dyn ControlStore, name: &str, has_tree: TreeProbe<'_>) -> String {
        match resolve_base_tree(store, name, has_tree) {
            Ok(base) => format!(
                "account '{}' is not delegated base view '{base}' (the name '{name}' resolves to it) — an admin grants it with `grant {account} {base}`",
                self.account,
                account = self.account
            ),
            Err(e) => format!(
                "account '{}' cannot govern '{name}': {e}",
                self.account
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_the_independent_reference_vectors() {
        // Vectors produced by Python's hashlib.pbkdf2_hmac('sha256', …) — an
        // implementation with nothing shared with this one, so a broken HMAC
        // cannot agree with itself.
        let d = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex(&d),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let salt3: Vec<u8> = (0u8..16).collect();
        let d = pbkdf2_sha256(b"hunter2", &salt3, 1000);
        assert_eq!(
            hex(&d),
            "f5550e89119f593cd3662c6d7da5bd3f7a90e25cd2dc221f58a6166c772753d3"
        );
        // The production round count itself, against the same independent
        // implementation — so a changed `PBKDF2_ITER` still has to stretch to
        // the digest Python derives for it.
        let d = pbkdf2_sha256(b"passphrase0123456789", b"THIsBeASalt_THisBeASalt", 210_000);
        assert_eq!(
            hex(&d),
            "3b6550387c9591b9c48fb0155c06499552252224391efef57ac703107190ffb1"
        );
        // HMAC-SHA256 itself, RFC 4231 test case 2 (key "Jefe", data "what do
        // ya want for nothing?") — and a 200-byte key, which must be hashed
        // down to fit the block first. Both reference digests come from
        // Python's hmac, an implementation sharing nothing with this one.
        let h = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&h),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long = b"SS".repeat(100);
        assert_eq!(long.len(), 200);
        let h = hmac_sha256(
            &long,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&h),
            "b36a871d7b3c66c2d8878ce987918f3603abcff3c0f1cf04d7522284a6f8ef86"
        );
    }

    #[test]
    fn hash_round_trips_and_rejects_wrong_passwords() {
        let h = hash_password("correct horse battery staple");
        assert!(h.starts_with("pbkdf2-sha256$210000$"));
        assert!(verify_password("correct horse battery staple", &h));
        assert!(!verify_password("Correct horse battery staple", &h));
        assert!(!verify_password("", &h));
        // Two hashes of the same password differ (random salt), and each still
        // verifies its own password.
        let h2 = hash_password("correct horse battery staple");
        assert_ne!(h, h2);
        assert!(verify_password("correct horse battery staple", &h2));
    }

    #[test]
    fn malformed_digests_are_refused_not_panicked() {
        for bad in [
            "",
            "plain-text-password",
            "md5$1$aa$bb",
            "pbkdf2-sha256$notanumber$aa$bb",
            "pbkdf2-sha256$1000$zz$bb",
            "pbkdf2-sha256$1000$61$bbcc",
            "pbkdf2-sha256$1000$61",
        ] {
            assert!(!verify_password("x", bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn iter_clamp_keeps_a_hostile_row_from_burning_a_core() {
        // A row carrying 4e9 rounds must not be honored: verification derives
        // at the build's own ceiling, so it costs one ordinary login and then
        // refuses. `hash_password` below is the reference for that cost.
        let fake = "pbkdf2-sha256$4000000000$6162636465666768696a6b6c6d6e6f70$00";
        let started = std::time::Instant::now();
        assert!(!verify_password("whatever", fake));
        let hostile = started.elapsed();
        let started = std::time::Instant::now();
        let _ = hash_password("whatever");
        let honest = started.elapsed();
        assert!(
            hostile <= honest * 3 + std::time::Duration::from_millis(50),
            "clamped verify cost {hostile:?} against an ordinary hash {honest:?}"
        );
    }

    /// The property the ceiling is only interesting for: one login must be a
    /// rounding error for a human and a real cost for an offline attacker. If
    /// this ever fails, the HMAC fast path regressed (it did once: the naive
    /// per-round pad re-compression cost ~2 s).
    #[test]
    fn one_login_is_bounded() {
        // The bound is build-profile dependent (an unoptimized PBKDF2 loop is
        // ~10x slower), so state which profile is being promised instead of
        // asserting a number that would only hold in `--release`.
        let budget = if cfg!(debug_assertions) {
            std::time::Duration::from_secs(8)
        } else {
            std::time::Duration::from_millis(800)
        };
        let h = hash_password("x");
        let started = std::time::Instant::now();
        assert!(verify_password("x", &h));
        let took = started.elapsed();
        assert!(
            took < budget,
            "PBKDF2 verify of {PBKDF2_ITER} rounds took {took:?} (budget {budget:?})"
        );
    }

    #[test]
    fn account_names_are_normalized_and_bounded() {
        assert_eq!(normalize_account(" Alice ").unwrap(), "alice");
        assert_eq!(normalize_account("obs_2026-a.b").unwrap(), "obs_2026-a.b");
        assert!(normalize_account("").is_err());
        assert!(normalize_account("-lead").is_err());
        assert!(normalize_account("a/b").is_err());
        assert!(normalize_account("alice@corp.example").is_err());
        assert!(normalize_account(&"x".repeat(65)).is_err());
    }

    #[test]
    fn role_parse_refuses_unknown_text() {
        assert_eq!(Role::parse("ADMIN").unwrap(), Role::Admin);
        assert_eq!(Role::parse("user").unwrap(), Role::User);
        assert!(Role::parse("root").is_err());
        assert!(Role::parse("").is_err());
    }

    #[test]
    fn store_creates_logins_and_delegates() {
        let dir = tempfile::tempdir().unwrap();
        let s = WebAuthStore::open(dir.path()).unwrap();
        assert!(s.is_empty().unwrap());

        s.create("alice", &hash_password("pw-alice"), Role::User)
            .unwrap();
        s.create("root", &hash_password("pw-root"), Role::Admin)
            .unwrap();
        assert!(!s.is_empty().unwrap());
        // A create that would overwrite a password is refused, not performed.
        assert!(s
            .create("alice", &hash_password("attacker-pw"), Role::Admin)
            .is_err());

        // Wrong password and unknown account say the same thing.
        let unknown = s.login("nobody", "x").unwrap_err();
        let wrong = s.login("alice", "x").unwrap_err();
        assert_eq!(unknown, wrong);
        assert!(!unknown.contains("unknown"));

        let ok = s.login("alice", "pw-alice").unwrap();
        assert_eq!(ok.role, Role::User);
        // A brand-new `user` owns NOTHING until a base view is granted.
        assert!(s.bases_of("alice").unwrap().is_empty());

        s.grant("alice", "team", Some("root")).unwrap();
        s.grant("alice", "team", Some("root")).unwrap(); // idempotent
        s.grant("alice", "lab", Some("root")).unwrap();
        let ok = s.login("alice", "pw-alice").unwrap();
        assert_eq!(ok.bases, vec!["lab".to_string(), "team".to_string()]);
        assert_eq!(s.list_grants().unwrap().len(), 2);
        assert!(s.ungrant("alice", "lab").unwrap());
        assert!(!s.ungrant("alice", "lab").unwrap());

        // Password change takes effect immediately; disable fails logins but
        // keeps the row and the grants attributable.
        assert!(s.set_password("alice", &hash_password("new-pw")).unwrap());
        assert!(s.login("alice", "pw-alice").is_err());
        assert!(s.login("alice", "new-pw").is_ok());
        assert!(s.set_disabled("alice", true).unwrap());
        assert_eq!(
            s.login("alice", "new-pw").unwrap_err(),
            "this account is disabled — ask an administrator to enable it"
        );
        assert!(s.set_disabled("alice", false).unwrap());
        assert!(s.login("alice", "new-pw").is_ok());

        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].account, "alice");
        assert!(listed[0].last_login_at.is_some());
        assert_eq!(listed[1].role, "admin");
        assert!(s.delete("alice").unwrap());
        assert!(s.list_grants().unwrap().is_empty());
        assert!(!s.delete("alice").unwrap());
        assert_eq!(s.login("alice", "new-pw").unwrap_err(), wrong);
    }

    #[test]
    fn super_principal_manages_everything_without_consulting_grants() {
        let p = Principal {
            account: "root".into(),
            role: Role::Admin,
            bases: vec![],
        };
        assert!(p.is_super());
        assert!(p.manages_base("anything"));
    }

    #[test]
    fn delegated_principal_is_bound_to_its_own_bases() {
        let p = Principal {
            account: "alice".into(),
            role: Role::User,
            bases: vec!["team".into()],
        };
        assert!(!p.is_super());
        assert!(p.manages_base("team"));
        assert!(!p.manages_base("team2"));
    }

    /// A control plane that holds DEFINITIONS but no trees — the exact shape of
    /// a `treedb://` deployment, where the local SQLite store keeps the fork /
    /// lens / alias slots while every physical tree lives inside the engine.
    struct DefsOnly;

    impl ControlStore for DefsOnly {
        fn backend(&self) -> &'static str {
            "sqlite"
        }
        fn describe(&self) -> String {
            "defs-only test store".into()
        }
        fn list_defs(&self) -> Result<Vec<(String, String)>, String> {
            Ok(vec![
                ("lens1".into(), "base team".into()),
                ("fork2".into(), "fork team2".into()),
            ])
        }
        fn get_def_raw(&self, name: &str) -> Result<Option<String>, String> {
            Ok(match name {
                "lens1" => Some("base team".into()),
                "fork2" => Some("fork team2".into()),
                _ => None,
            })
        }
        fn put_def(&self, _name: &str, _text: &str) -> Result<(), String> {
            unreachable!("resolution reads only")
        }
        fn remove_def(&self, _name: &str) -> Result<bool, String> {
            unreachable!("resolution reads only")
        }
        fn stamp(&self, _name: &str) -> Option<String> {
            None
        }
        // The trap this whole test exists for: no local `meta/<name>/<name>.db`,
        // so the control store alone answers "no tree" for a treedb base that
        // the engine reports as present.
        fn list_physical(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn has_physical(&self, _name: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn store_for_db_path(&self, _db: &std::path::Path) -> std::sync::Arc<dyn ControlStore> {
            unreachable!("resolution never opens another store")
        }
        fn has_fork_tree(&self, _name: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn create_fork_tree(
            &self,
            _base: &str,
            _name: &str,
            _seed: bool,
        ) -> Result<crate::meta::ForkStats, String> {
            unreachable!("resolution never writes")
        }
        fn graft_tree(
            &self,
            _dst: &str,
            _src: &str,
            _items: &[crate::meta::GraftItem],
            _dest: &str,
            _skip_existing: bool,
        ) -> Result<crate::meta::GraftStats, String> {
            unreachable!("resolution never writes")
        }
        fn destroy_fork_tree(&self, _name: &str) {}
        fn drop_fork_tree(&self, _name: &str) -> Result<(), String> {
            unreachable!("resolution never writes")
        }
        fn open_ro_view(
            &self,
            _view: &str,
        ) -> Result<std::sync::Arc<dyn crate::meta::MetaProvider>, String> {
            unreachable!("resolution never opens a tree")
        }
        fn base_tree_missing_note(&self, _view: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    #[test]
    fn a_tree_outside_the_control_store_needs_the_probe_to_resolve() {
        let store = DefsOnly;
        // Without the probe the base is invisible: the 2026-11-26 live failure,
        // where granting a real treedb view died on "neither a definition nor a
        // tree" because nothing outside the control store was ever asked.
        assert!(resolve_base(&store, "team").is_err());
        // With it, the name resolves and the definition chain still chases.
        let engine_has = |n: &str| -> Result<bool, String> { Ok(n == "team" || n == "team2") };
        assert_eq!(resolve_base_tree(&store, "team", &engine_has).unwrap(), "team");
        assert_eq!(resolve_base_tree(&store, "lens1", &engine_has).unwrap(), "team");
        // A name neither the defs nor the engine know stays an error — the probe
        // widens the answer to the truth, not to "assume allowed".
        assert!(resolve_base_tree(&store, "ghost", &engine_has).is_err());
        // And the ACL follows it: the same delegate is refused on the blind read
        // and admitted through the engine's answer.
        let p = Principal {
            account: "alice".into(),
            role: Role::User,
            bases: vec!["team".into()],
        };
        let store_ref: &dyn ControlStore = &store;
        let ask: TreeProbe = &engine_has;
        // The blind read does not answer "no", it ANSWERS "that view does not
        // exist" — which is the sentence the console rendered for a mounted view.
        assert!(p.manages(store_ref, "team").is_err());
        assert!(p.manages_tree(store_ref, "team", ask).unwrap());
        assert!(p.manages_tree(store_ref, "lens1", ask).unwrap());
        assert!(!p.manages_tree(store_ref, "fork2", ask).unwrap());
        // A name nothing reports is still an error, and every ACL call site turns
        // an error into a refusal (`unwrap_or(false)`), never into a grant.
        assert!(p.manages_tree(store_ref, "ghost", ask).is_err());
        // The refusal a caller sees must not quote the blind answer either: the
        // probe turns "that view does not exist" into the true "not delegated".
        assert!(p.refusal(store_ref, "team").contains("neither a definition"));
        assert!(p.refusal_tree(store_ref, "lens1", ask).contains("not delegated"));
    }

    #[test]
    fn password_policy_refuses_the_obvious_weaknesses() {
        assert!(check_password("alice", "a-long-enough-pw").is_ok());
        assert!(check_password("alice", "short").is_err());
        assert!(check_password("alice", " trailing space").is_err());
        assert!(check_password("alice", "ALICE123456789").is_ok());
        assert!(check_password("alice12345", "ALICE12345").is_err());
        // Unicode count, not bytes: 10 wide chars are 10 characters of entropy
        // budget, which is what the rule is about.
        assert!(check_password("alice", "").is_err());
    }

    #[test]
    fn bootstrap_seeds_only_the_first_owner_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let s = WebAuthStore::open(dir.path()).unwrap();

        // First start: the flag creates the owner.
        let line = bootstrap_owner(&s, "Alice", "first-owner-pw").unwrap();
        assert!(
            line.contains("created the first web account 'alice'"),
            "{line}"
        );
        assert_eq!(s.get("alice").unwrap().unwrap().role, "admin");
        // A restart of the SAME unit re-reads the flag: ignored, not an error,
        // and it must not touch the existing password.
        let line = bootstrap_owner(&s, "alice", "rotated-by-restart").unwrap();
        assert!(line.contains("already exists"), "{line}");
        assert!(s.login("alice", "first-owner-pw").is_ok());
        assert!(s.login("alice", "rotated-by-restart").is_err());
        // A second owner cannot be smuggled in through the flag.
        let e = bootstrap_owner(&s, "mallory", "another-owner-pw").unwrap_err();
        assert!(e.contains("not empty"), "{e}");
        // A weak install password on the very first start is refused outright.
        let dir2 = tempfile::tempdir().unwrap();
        let s2 = WebAuthStore::open(dir2.path()).unwrap();
        assert!(bootstrap_owner(&s2, "alice", "short").is_err());
        assert!(s2.is_empty().unwrap());
        // A disabled owner makes the flag fail loudly rather than reset it.
        s.set_disabled("alice", true).unwrap();
        assert!(bootstrap_owner(&s, "alice", "first-owner-pw")
            .unwrap_err()
            .contains("disabled"));
    }
}
