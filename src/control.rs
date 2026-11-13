//! Shared plumbing for the PostgreSQL CONTROL plane — the home of API keys,
//! the call-audit log and view definitions when the metadata backend is
//! PostgreSQL (`--meta-store postgres://…`).
//!
//! Why this exists: view TREES already lived in PG (one `grovefs_v_<hex>`
//! schema per view, see [`crate::pg_store`]), but the control-plane state was
//! deliberately file/SQLite-local (`<meta-dir>/meta/api_keys.db`,
//! `<meta-dir>/meta/views/*.layout`). A PG deployment then had TWO systems of
//! record: governance reads and writes had to hit the meta-server's local disk
//! as well as Postgres, and a second meta-server (or a HA standby) never saw
//! either file. Under PG everything now lives in one `grovefs_control` schema,
//! so a key minted or a lens defined anywhere is authoritative everywhere the
//! same URI is configured. Under SQLite nothing moves: keys stay in
//! `meta/api_keys.db`, definitions in `meta/views/*.layout`.
//!
//! The control plane is low-frequency (auth events, ~1 revoke re-check per
//! live session per second, governance calls from a CLI/MCP agent), so this
//! pool is small and deliberately simpler than the view-tree pool in
//! [`crate::pg_store`]: one bounded pool of plain clients, no SSI wrapper
//! (single-row upserts/selects need no serializable schedule), transparent
//! reconnect on a server-side close.

use postgres::{Client, NoTls};

/// The single schema every control-plane table lives in.
pub const CONTROL_SCHEMA: &str = "grovefs_control";
/// The API-key + call-audit table pair (see [`crate::apikeys`]).
pub const KEYS_TABLE: &str = "grovefs_control.api_keys";
pub const CALLS_TABLE: &str = "grovefs_control.mcp_calls";
/// The named-view definition table (see [`crate::mcp::ControlStore`]).
pub const DEFS_TABLE: &str = "grovefs_control.view_defs";

/// Connections held per control-plane store handle. Keys are checked on every
/// mount handshake and once per second per live session; definitions are read
/// on a bounded hot-reload probe — a handful of pooled sessions covers a busy
/// control plane without meaningfully touching PG's `max_connections`.
const CONTROL_POOL_SIZE: usize = 4;

/// Global serializer for control-schema DDL. Two `CREATE SCHEMA IF NOT EXISTS`
/// statements from different sessions can deadlock in PG (a known catalog
/// race), so initializer processes take a transactionless advisory lock on a
/// fixed class number first — released unconditionally in `finally` style
/// below (every exit path executes the unlock).
const DDL_LOCK_CLASS: i64 = 7100_0001;

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
    /// Build a pool for `uri` and make sure the control schema and its tables
    /// exist (idempotent, cross-process safe via an advisory lock). The DDL is
    /// passed in by the consuming module because keys and definitions live in
    /// ONE schema owned by two stores; each store hands the statements it
    /// needs at open time.
    pub fn open(uri: &str, ddl: &[&str]) -> Result<Self, String> {
        // A sharded store spec (`uri,uri,...`) names several instances for the
        // view trees; the control schema is ONE authoritative record and stays
        // on the first instance (see `pg_store::control_store`).
        let uri = crate::pg_store::control_store(uri);
        let mut pool = Vec::new();
        for _ in 0..CONTROL_POOL_SIZE {
            match Client::connect(uri, NoTls) {
                Ok(c) => pool.push(c),
                // Fewer warm connections is fine: `checkout` opens on demand,
                // and a total outage surfaces on the first real query.
                Err(_) => break,
            }
        }
        let this = Self {
            uri: uri.to_string(),
            pool: parking_lot::Mutex::new(pool),
        };
        if !ddl.is_empty() {
            this.init_schema(ddl)?;
        }
        Ok(this)
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
        Client::connect(&self.uri, NoTls)
            .map_err(|e| format!("postgres connect {}: {e}", self.uri))
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
    fn init_schema(&self, ddl: &[&str]) -> Result<(), String> {
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
