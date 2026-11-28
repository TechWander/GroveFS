//! Shared plumbing for the PostgreSQL CONTROL plane — the home of API keys,
//! the call-audit log and view definitions when the metadata backend is
//! PostgreSQL (`--meta-store postgres://…`).
//!
//! Why this exists: view TREES already lived in PG (one `grovefs_v_<hex>`
//! schema per view, see [`crate::pg_store`]), but the control-plane state was
//! deliberately file/SQLite-local (`<meta-dir>/meta/_control/api_keys.db`,
//! `<meta-dir>/meta/views/*.layout`). A PG deployment then had TWO systems of
//! record: governance reads and writes had to hit the meta-server's local disk
//! as well as Postgres, and a second meta-server (or a HA standby) never saw
//! either file. Under PG everything now lives in one `grovefs_control` schema,
//! so a key minted or a lens defined anywhere is authoritative everywhere the
//! same URI is configured. Under SQLite nothing moves: keys stay in
//! `meta/_control/api_keys.db`, definitions in `meta/views/*.layout`.
//!
//! The control plane is low-frequency (auth events, ~1 revoke re-check per
//! live session per second, governance calls from a CLI/MCP agent), so this
//! pool is small and deliberately simpler than the view-tree pool in
//! [`crate::pg_store`]: one bounded pool of plain clients, no SSI wrapper
//! (single-row upserts/selects need no serializable schedule), transparent
//! reconnect on a server-side close.

use postgres::{Client, NoTls};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// The ONE control-plane pool this process uses per database, keyed by the
/// EXPANDED control URI (see [`crate::pg_store::control_store`]).
///
/// Why a registry exists: one `meta-server` with a PG control plane opens three
/// control-plane stores against the SAME database — the definition store
/// (`grovefs_control.view_defs`), the credential/audit store
/// (`api_keys`/`mcp_calls`) and the web-account store (`web_accounts`/`web_grants`) —
/// and each used to build its own pool with its own eager connect. Measured on
/// this box (2026-11-28, one sharded instance, no mount, backends counted by
/// `pg_stat_activity` filtered to a private database): **3 warm sessions** where
/// one database was being talked to. Retention then compounded it: each handle
/// kept up to [`CONTROL_POOL_SIZE`] after a burst, so a process that had served
/// governance traffic on all three tables sat on up to 12 of them.
///
/// With the registry in place the SAME probe on the SAME instance reads **1**
/// (`tests/control_pool.rs` pins the number against a live PostgreSQL, three
/// stores and one fourth consumer included).
///
/// What sharing is ALLOWED to do is exactly what these three consumers have in
/// common: a plain session, no per-session `SET`, and idempotent
/// `CREATE … IF NOT EXISTS` DDL serialized by [`DDL_LOCK_CLASS`]. Each consumer
/// still runs its own DDL through the shared handle (that is why [`shared`] takes
/// `ddl` and does not just cache-and-return), so opening order stays irrelevant.
///
/// What is deliberately NOT in here is the shard path's pool
/// (`remote::meta::shard`): its sessions are shaped at connect with
/// `statement_timeout` and `idle_in_transaction_session_timeout` of 2 s
/// (`SHARD_DB_TIMEOUT` there) so a wedged control plane cannot park a meta
/// worker (ADR-0005). Handing those sessions to the credential store would put a
/// 2 s ceiling under audit inserts and DDL, and taking them off would need a
/// `RESET` per use — more round trips than the sharing saves. Two shapes, one
/// contract each, is the honest result; the numbers above are why the other three
/// collapsed into one.
static SHARED_POOLS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<PgPool>>>> =
    OnceLock::new();

fn registry() -> &'static parking_lot::Mutex<HashMap<String, Arc<PgPool>>> {
    SHARED_POOLS.get_or_init(Default::default)
}

/// Hand out the process's single pool for `uri`'s control database, after making
/// sure the caller's tables exist in it.
///
/// This is the ONLY entry point for a control-plane pool: the eager-connect-
/// plus-DDL shape used to live in a public `PgPool::open`, and three stores
/// calling it against one URI is how one process came to sit on three warm
/// sessions (see [`SHARED_POOLS`]).
///
/// `uri` may be any spelling the deployment uses (a bare instance, a sharded
/// `uri,uri` spec, a `file:` registry); it is normalized through
/// [`crate::pg_store::control_store`] first, which is also what makes two
/// differently-spelled consumers land on one pool.
pub fn shared(uri: &str, ddl: &[&str]) -> Result<Arc<PgPool>, String> {
    let expanded = crate::pg_store::control_store(uri)?;
    let hot = registry().lock().get(&expanded).cloned();
    let pool = match hot {
        Some(p) => p,
        None => {
            // The connect happens OUTSIDE the registry lock: a slow or unreachable
            // control plane must not block another database's open behind it. Two
            // consumers racing a cold URI both connect; the one that loses the
            // insert is dropped, and its single warm session closes with it — a
            // transient extra connect at startup, never a second live pool.
            let fresh = Arc::new(PgPool::connect_only(&expanded)?);
            let mut reg = registry().lock();
            match reg.get(&expanded).cloned() {
                Some(winner) => winner,
                None => {
                    reg.insert(expanded.clone(), Arc::clone(&fresh));
                    fresh
                }
            }
        }
    };
    if !ddl.is_empty() {
        pool.init_schema(ddl)?;
    }
    Ok(pool)
}

/// How many control-plane databases this process holds pools for. Test-only
/// evidence for [`shared`]: it must not grow when a fourth store opens the same
/// URI.
#[cfg(test)]
fn shared_pool_count() -> usize {
    registry().lock().len()
}

/// The single schema every control-plane table lives in.
pub const CONTROL_SCHEMA: &str = "grovefs_control";
/// The API-key + call-audit table pair (see [`crate::apikeys`]).
pub const KEYS_TABLE: &str = "grovefs_control.api_keys";
pub const CALLS_TABLE: &str = "grovefs_control.mcp_calls";
/// The named-view definition table (see [`crate::mcp::ControlStore`]).
pub const DEFS_TABLE: &str = "grovefs_control.view_defs";

/// Connections RETAINED per control-plane POOL — which, since [`SHARED_POOLS`],
/// means per PROCESS per control database, because the definition, credential
/// and web-account stores all hand their traffic through the one handle.
///
/// This is a RETENTION bound, not a pre-fill: the pool goes to PostgreSQL with
/// one session (see [`PgPool::connect_only`]) and grows to this only under a
/// burst, each returned session kept instead of closed. The number is worth
/// bounding because the same `max_connections` budget also pays the per-view
/// tree pools of [`crate::pg_store`] (write floor + read floor per active view)
/// — which is what a `53300 remaining connection slots are reserved…` refusal
/// is actually about, and why an idle control plane must cost one session and
/// not one per table it happens to use.
pub(crate) const CONTROL_POOL_SIZE: usize = 4;

/// Global serializer for control-schema DDL. Two `CREATE SCHEMA IF NOT EXISTS`
/// statements from different sessions can deadlock in PG (a known catalog
/// race), so initializer processes take a transactionless advisory lock on a
/// fixed class number first — released unconditionally in `finally` style
/// below (every exit path executes the unlock). The view-placement DDL in
/// [`crate::pg_store`] shares this class: it creates a table in the SAME
/// schema, so the same race applies and the same lock must serialize it.
pub(crate) const DDL_LOCK_CLASS: i64 = 7100_0001;

/// Lowercase hex of the raw view bytes — the same encoding
/// [`crate::pg_store`] uses for per-view schema names (`schema_for`), exposed
/// here so the control plane can ask "does view X have a tree?" without
/// guessing string formats.
pub fn schema_name_for(view: &str) -> String {
    let mut hex = String::with_capacity(view.len() * 2);
    for b in view.as_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("grovefs_v_{hex}")
}

/// Decode a `grovefs_v_<hex>` schema name back to the view name it encodes.
/// `None` for anything that is not this toolchain's own deterministic
/// encoding, so an unrelated schema sharing the prefix is never surfaced as a
/// mountable view.
pub fn view_from_schema_name(schema: &str) -> Option<String> {
    let hex = schema.strip_prefix("grovefs_v_")?;
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(s, 16).ok()?);
    }
    let view = String::from_utf8(bytes).ok()?;
    crate::layout::valid_view_name(&view).then_some(view)
}

/// A small bounded pool of PG clients plus the shared control-schema DDL.
/// Cloned handles (inside one `Arc`) share the pool.
pub struct PgPool {
    uri: String,
    pool: parking_lot::Mutex<Vec<Client>>,
}

impl PgPool {
    /// One eager connect against an already-expanded control URI, WITHOUT the
    /// DDL step. Not public on purpose: the only way to hold a control-plane
    /// pool is [`shared`], so that a process's sessions per database cannot
    /// depend on how many stores it happens to use.
    fn connect_only(expanded: &str) -> Result<Self, String> {
        // ONE warm session, not a poolful: opening a store must still fail fast
        // on an unreachable or badly-credentialed control plane (that is what
        // the eager first connect is for), but an idle process must not hold one
        // backend per control TABLE of a shared `max_connections`.
        // [`PgPool::checkout`] opens the rest on demand and [`PgPool::requeue`]
        // keeps up to [`CONTROL_POOL_SIZE`] afterwards, so a busy plane still
        // converges on a bounded shape and a quiet one costs one session per
        // database, however many stores use it.
        let mut pool = Vec::new();
        // A total outage is not survived here either way: the caller's DDL step
        // in [`shared`] checks out (and so reconnects) and reports the real
        // error, which arrives with the server's reason instead of as an empty
        // pool.
        if let Ok(c) = Client::connect(expanded, NoTls) {
            pool.push(c);
        }
        Ok(Self {
            uri: expanded.to_string(),
            pool: parking_lot::Mutex::new(pool),
        })
    }

    /// One connection: prefer a warm pooled client (skipping any closed by an
    /// idle timeout / server restart), else a fresh connect.
    pub fn checkout(&self) -> Result<Client, String> {
        loop {
            let pooled = self.pool.lock().pop();
            match pooled {
                None => break,
                Some(c) if c.is_closed() => continue,
                Some(c) => return Ok(c),
            }
        }
        Client::connect(&self.uri, NoTls).map_err(|e| {
            // The URI embeds the password, and this text leaves the process: a
            // control-plane reply carries it to a terminal AND to the web
            // console, a refused mount shows it to the mounting user, and the
            // audit row stores it. Name the database, not the credential — the
            // rule [`crate::pg_store::redact_uri`] exists to make one place.
            format!(
                "postgres connect {}: {e}",
                crate::pg_store::redact_uri(&self.uri)
            )
        })
    }

    /// Hand a used client back (bounded; a closed socket is dropped instead of
    /// leaking a dead handle into the pool).
    pub fn requeue(&self, c: Client) {
        if c.is_closed() {
            return;
        }
        let mut p = self.pool.lock();
        if p.len() < CONTROL_POOL_SIZE {
            p.push(c);
        }
    }

    /// Run one statement batch on a checked-out client.
    pub fn batch(&self, sql: &str) -> Result<(), String> {
        let mut c = self.checkout()?;
        let r = c.batch_execute(sql).map_err(|e| format!("postgres {e}"));
        self.requeue(c);
        r
    }

    /// Run one parameterized query, mapping rows through `map`.
    pub fn query<F, T>(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
        mut map: F,
    ) -> Result<Vec<T>, String>
    where
        F: FnMut(&postgres::Row) -> T,
    {
        let mut c = self.checkout()?;
        let r = c
            .query(sql, params)
            .map(|rows| rows.iter().map(&mut map).collect())
            .map_err(|e| format!("postgres {e}"));
        self.requeue(c);
        r
    }

    /// Run one parameterized statement that returns no rows.
    pub fn execute(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<u64, String> {
        let mut c = self.checkout()?;
        let r = c.execute(sql, params).map_err(|e| format!("postgres {e}"));
        self.requeue(c);
        r
    }

    /// Create the control schema and the tables named by `ddl`, serialized
    /// across processes with an advisory lock so two simultaneous
    /// initializers cannot hit PG's concurrent-`CREATE` catalog deadlock.
    pub(crate) fn init_schema(&self, ddl: &[&str]) -> Result<(), String> {
        let mut c = self.checkout()?;
        let lock = format!("SELECT pg_advisory_lock({DDL_LOCK_CLASS})");
        c.batch_execute(&lock)
            .map_err(|e| format!("postgres {e}"))?;
        let body = ddl.join(";") + ";";
        let created = c.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {CONTROL_SCHEMA};{body}"
        ));
        // Always release, even when the DDL failed, so a later process is not
        // blocked behind a stuck lock.
        let unlock = format!("SELECT pg_advisory_unlock({DDL_LOCK_CLASS})");
        let _ = c.batch_execute(&unlock);
        created.map_err(|e| format!("postgres control schema init: {e}"))?;
        self.requeue(c);
        Ok(())
    }

    /// The connection URI (kept for error messages that name the backend).
    pub fn uri(&self) -> &str {
        &self.uri
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed connect must name the database it could not reach without
    /// naming the credential that was trying: the URI embeds a password, and
    /// [`PgPool::checkout`]'s error text reaches replies, logs and audit rows.
    #[test]
    fn a_failed_connect_names_the_host_and_not_the_password() {
        let pool = PgPool {
            uri: "postgres://grove:s3cret@127.0.0.1:1/grovefs".to_string(),
            pool: parking_lot::Mutex::new(Vec::new()),
        };
        let err = pool.checkout().err().expect("a closed port cannot connect");
        assert!(!err.contains("s3cret"), "the password travelled into the error: {err}");
        assert!(
            err.contains("127.0.0.1:1/grovefs"),
            "the error stopped naming the database it failed on: {err}"
        );
    }

    /// What the registry is FOR, pinned without a PostgreSQL to talk to: one
    /// control database yields one pool, however many stores ask for it and
    /// however they spell the spec. Uses a refused port on purpose —
    /// [`PgPool::connect_only`] keeps the fail-fast error for the DDL step, so a
    /// dead URI still registers a handle, and the point here is that the SECOND
    /// caller reuses it instead of opening another warm session.
    #[test]
    fn one_control_database_gets_one_pool_whatever_the_caller_asks_for() {
        let dead = format!("postgres://grove:x@127.0.0.1:1/grovefs_reg_{}", std::process::id());
        let before = shared_pool_count();
        let first = shared(&dead, &[]).expect("a dead URI still hands out a handle");
        // A different spelling of the SAME database (a shard spec whose control
        // instance is this one) must land on the same pool, not a second.
        let spec = format!("{dead},{dead}");
        let second = shared(&spec, &[]).expect("a spec still hands out a handle");
        assert!(
            Arc::ptr_eq(&first, &second),
            "two stores asking for one control database got two pools"
        );
        assert_eq!(
            shared_pool_count(),
            before + 1,
            "the registry grew for a URI it already had"
        );
        // And the pool really is bound to the EXPANDED URI, so error messages
        // name one canonical database whatever the deployment configured.
        assert_eq!(first.uri(), dead);
    }

    #[test]
    fn schema_name_round_trip() {
        // The hex encoding matches pg_store's schema_for exactly (same byte
        // order, lowercase), so the control plane resolves the SAME schemas
        // the view backend created.
        let s = schema_name_for("alice");
        assert_eq!(s, format!("grovefs_v_{}", hex_of(b"alice")));
        assert_eq!(view_from_schema_name(&s).as_deref(), Some("alice"));
        // A view name with a multi-byte UTF-8 char still round-trips.
        let s2 = schema_name_for("队a");
        assert_eq!(view_from_schema_name(&s2).as_deref(), Some("队a"));
    }

    #[test]
    fn foreign_schema_names_are_rejected() {
        assert!(view_from_schema_name("public").is_none());
        assert!(view_from_schema_name("grovefs_v_").is_none());
        assert!(view_from_schema_name("grovefs_v_zz").is_none());
        assert!(view_from_schema_name("grovefs_v_616").is_none()); // odd length
                                                                   // A valid hex decode that is NOT a legal view name (contains '/') is
                                                                   // never surfaced as a mountable view.
        let bad = schema_name_for_for_test(b"a/b");
        assert!(view_from_schema_name(&bad).is_none());
    }

    fn schema_name_for_for_test(bytes: &[u8]) -> String {
        let mut hex = String::new();
        for b in bytes {
            hex.push_str(&format!("{b:02x}"));
        }
        format!("grovefs_v_{hex}")
    }

    fn hex_of(s: &[u8]) -> String {
        s.iter().map(|b| format!("{b:02x}")).collect()
    }
}
