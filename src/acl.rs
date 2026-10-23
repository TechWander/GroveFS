//! View-level advisory access control (opt-in whitelist per view).
//!
//! GroveFS's core isolation is by *view name*: a client only ever opens the view
//! it asked for, and cannot see other views' trees. That is structural isolation,
//! not a permission system — anyone who can reach the store (or holds the shared
//! `--token`) can mount *any* view by name. This module layers an optional,
//! coarse **per-uid whitelist** on top, so an operator can say "view `alice` may
//! only be mounted by uid 1000 and 1002".
//!
//! Semantics (kept minimal and backward-compatible):
//! - A view with **no ACL rows is unrestricted** — anyone who passes the token
//!   gate may mount it, exactly as before. The other regression suites never
//!   write ACL rows, so they are unaffected.
//! - A view with **one or more ACL rows is whitelisted**: only uids listed with
//!   `can_read` may open it at all; only uids listed with `can_write` may mutate
//!   it (create/unlink/rename/write/chmod/...).
//!
//! Storage backend follows the metadata store choice:
//! - Under SQLite (default/embedded): `<base>/meta/acl.db`, one local file.
//! - Under PostgreSQL (`--store postgres://…`): a shared table
//!   `grovefs_acl.view_acl` inside the same PG instance that holds the view
//!   trees. This keeps the entire metadata plane (trees + locks + ACL) in one
//!   database, simplifying backup, migration and replication: a single
//!   `pg_dump` captures everything.
//!
//! Honest security boundary: the presenting uid is self-reported by the mount
//! process (`geteuid()`, or an explicit `--as-uid`). Over a trusted LAN this is a
//! *courtesy* fence against honest-but-different users, not a hard wall — a
//! malicious client that already holds the `--token` can lie about its uid or
//! query the store directly. Enforcement therefore lives at the authoritative
//! layer (the central `meta-server`), where every open and every write-op passes
//! through one gate; a purely local `mount` only enforces the read/mount gate (its
//! writes hit the on-disk SQLite directly, guarded there only by filesystem
//! permissions). Use `--token` for real teeth; use ACL for intra-team separation.

use rusqlite::{params, Connection};
use std::path::Path;

/// One whitelist entry: a uid's read/write grant on a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclEntry {
    pub uid: u32,
    pub can_read: bool,
    pub can_write: bool,
}

/// The PG table name for the ACL when stored in PostgreSQL.
const ACL_PG_TABLE: &str = "grovefs_acl.view_acl";

/// The per-base ACL table. Dispatches to either a local SQLite file or a
/// PostgreSQL table depending on the metadata backend in use.
pub enum AclStore {
    Sqlite {
        conn: parking_lot::Mutex<Connection>,
    },
    Pg {
        client: parking_lot::Mutex<postgres::Client>,
    },
}

impl AclStore {
    // ---------------------------------------------------------------- SQLite

    /// Open (and if necessary initialize) `<base>/meta/acl.db`. Creating the
    /// table is idempotent; an empty table means "no view is restricted".
    pub fn open(base: &Path) -> rusqlite::Result<Self> {
        let path = base.join("meta").join("acl.db");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS view_acl (
                 view       TEXT    NOT NULL,
                 uid        INTEGER NOT NULL,
                 can_read   INTEGER NOT NULL,
                 can_write  INTEGER NOT NULL,
                 PRIMARY KEY (view, uid)
             );",
        )?;
        Ok(Self::Sqlite {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    // ------------------------------------------------------------- PostgreSQL

    /// Open (and if necessary initialize) the ACL table inside PG.
    /// Creates schema `grovefs_acl` and table `view_acl` idempotently.
    pub fn open_pg(conn_uri: &str) -> Result<Self, String> {
        let client = postgres::Client::connect(conn_uri, postgres::NoTls)
            .map_err(|e| format!("AclStore::open_pg connect: {e}"))?;
        let me = Self::Pg {
            client: parking_lot::Mutex::new(client),
        };
        // Idempotent DDL — safe to call on every startup.
        {
            let mut c = me.pg_lock();
            c.batch_execute(
                "CREATE SCHEMA IF NOT EXISTS grovefs_acl;\n\
                 CREATE TABLE IF NOT EXISTS grovefs_acl.view_acl (\n\
                     view       TEXT    NOT NULL,\n\
                     uid        BIGINT  NOT NULL,\n\
                     can_read   BOOLEAN NOT NULL,\n\
                     can_write  BOOLEAN NOT NULL,\n\
                     PRIMARY KEY (view, uid)\n\
                 );"
            ).map_err(|e| format!("AclStore::open_pg DDL: {e}"))?;
        }
        Ok(me)
    }

    fn pg_lock(&self) -> parking_lot::MutexGuard<'_, postgres::Client> {
        match self {
            Self::Pg { client } => client.lock(),
            Self::Sqlite { .. } => panic!("pg_lock called on SQLite AclStore"),
        }
    }

    // ---------------------------------------------------------- Dispatch API

    /// Whether `view` has any whitelist rows (i.e. is restricted).
    pub fn is_restricted(&self, view: &str) -> bool {
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM view_acl WHERE view=?1)",
                    params![view],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(true)
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                c.query(
                    &format!("SELECT EXISTS(SELECT 1 FROM {ACL_PG_TABLE} WHERE view=$1)"),
                    &[&view],
                )
                .map(|rows| rows.first().map(|r| r.get::<_, bool>(0)).unwrap_or(true))
                .unwrap_or(true)
            }
        }
    }

    /// Whether `uid` may open (mount/read) `view`.
    pub fn check_read(&self, view: &str, uid: u32) -> bool {
        let uid_i = uid as i64;
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                c.query_row(
                    "SELECT (NOT EXISTS (SELECT 1 FROM view_acl WHERE view=?1))\n             \n                    OR EXISTS (SELECT 1 FROM view_acl\n             \n                               WHERE view=?2 AND uid=?3 AND can_read=1)",
                    params![view, view, uid_i],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                c.query(
                    &format!(
                        "SELECT (NOT EXISTS (SELECT 1 FROM {ACL_PG_TABLE} WHERE view=$1))\n             \n                            OR EXISTS (SELECT 1 FROM {ACL_PG_TABLE}\n             \n                                      WHERE view=$2 AND uid=$3 AND can_read)"
                    ),
                    &[&view, &view, &uid_i],
                )
                .map(|rows| rows.first().map(|r| r.get::<_, bool>(0)).unwrap_or(false))
                .unwrap_or(false)
            }
        }
    }

    /// Whether `uid` may mutate `view`.
    pub fn check_write(&self, view: &str, uid: u32) -> bool {
        let uid_i = uid as i64;
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                c.query_row(
                    "SELECT (NOT EXISTS (SELECT 1 FROM view_acl WHERE view=?1))\n             \n                    OR EXISTS (SELECT 1 FROM view_acl\n             \n                               WHERE view=?2 AND uid=?3 AND can_write=1)",
                    params![view, view, uid_i],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                c.query(
                    &format!(
                        "SELECT (NOT EXISTS (SELECT 1 FROM {ACL_PG_TABLE} WHERE view=$1))\n             \n                            OR EXISTS (SELECT 1 FROM {ACL_PG_TABLE}\n             \n                                      WHERE view=$2 AND uid=$3 AND can_write)"
                    ),
                    &[&view, &view, &uid_i],
                )
                .map(|rows| rows.first().map(|r| r.get::<_, bool>(0)).unwrap_or(false))
                .unwrap_or(false)
            }
        }
    }

    /// Grant (or replace) `uid`'s read/write access to `view`.
    pub fn allow(&self, view: &str, uid: u32, read: bool, write: bool) -> Result<(), AclError> {
        let uid_i = uid as i64;
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                c.execute(
                    "INSERT INTO view_acl (view,uid,can_read,can_write) VALUES (?1,?2,?3,?4)\n             \n                     ON CONFLICT(view,uid) DO UPDATE SET can_read=excluded.can_read, can_write=excluded.can_write",
                    params![view, uid_i, read as i64, write as i64],
                )?;
                Ok(())
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                c.execute(
                    &format!(
                        "INSERT INTO {ACL_PG_TABLE} (view,uid,can_read,can_write) VALUES ($1,$2,$3,$4)\n             \n                         ON CONFLICT(view,uid) DO UPDATE SET can_read=EXCLUDED.can_read, can_write=EXCLUDED.can_write"
                    ),
                    &[&view, &uid_i, &read, &write],
                )
                .map_err(|e| AclError(e.to_string()))?;
                Ok(())
            }
        }
    }

    /// Remove `uid`'s grant from `view`.
    pub fn revoke(&self, view: &str, uid: u32) -> Result<(), AclError> {
        let uid_i = uid as i64;
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                c.execute(
                    "DELETE FROM view_acl WHERE view=?1 AND uid=?2",
                    params![view, uid_i],
                )?;
                Ok(())
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                c.execute(
                    &format!("DELETE FROM {ACL_PG_TABLE} WHERE view=$1 AND uid=$2"),
                    &[&view, &uid_i],
                )
                .map_err(|e| AclError(e.to_string()))?;
                Ok(())
            }
        }
    }

    /// List grants for one view (empty = unrestricted).
    pub fn list(&self, view: &str) -> Vec<AclEntry> {
        match self {
            Self::Sqlite { conn } => {
                let c = conn.lock();
                let mut stmt = match c.prepare(
                    "SELECT uid,can_read,can_write FROM view_acl WHERE view=?1 ORDER BY uid",
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                let rows = match stmt.query_map(params![view], |r| {
                    Ok(AclEntry {
                        uid: r.get::<_, i64>(0)? as u32,
                        can_read: r.get::<_, i64>(1)? == 1,
                        can_write: r.get::<_, i64>(2)? == 1,
                    })
                }) {
                    Ok(it) => it,
                    Err(_) => return Vec::new(),
                };
                rows.filter_map(|x| x.ok()).collect()
            }
            Self::Pg { .. } => {
                let mut c = self.pg_lock();
                match c.query(
                    &format!("SELECT uid,can_read,can_write FROM {ACL_PG_TABLE} WHERE view=$1 ORDER BY uid"),
                    &[&view],
                ) {
                    Ok(rows) => rows
                        .iter()
                        .map(|r| AclEntry {
                            uid: r.get::<_, i64>(0) as u32,
                            can_read: r.get::<_, bool>(1),
                            can_write: r.get::<_, bool>(2),
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                }
            }
        }
    }
}

/// Thin error wrapper so `allow`/`revoke` can be uniform across backends.
#[derive(Debug)]
pub struct AclError(pub String);

impl std::fmt::Display for AclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for AclError {}
impl From<rusqlite::Error> for AclError {
    fn from(e: rusqlite::Error) -> Self {
        AclError(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_until_first_row() {
        let dir = tempfile::tempdir().unwrap();
        let acl = AclStore::open(dir.path()).unwrap();
        // No rows: everyone may read and write (backward-compatible default).
        assert!(acl.check_read("alice", 0));
        assert!(acl.check_write("alice", 12345));
        assert!(!acl.is_restricted("alice"));

        // Adding a row for uid 1000 whitelists the view: only 1000 gets in.
        acl.allow("alice", 1000, true, true).unwrap();
        assert!(acl.is_restricted("alice"));
        assert!(acl.check_read("alice", 1000));
        assert!(acl.check_write("alice", 1000));
        assert!(!acl.check_read("alice", 1001));
        assert!(!acl.check_write("alice", 1001));
    }

    #[test]
    fn read_only_grant_blocks_write() {
        let dir = tempfile::tempdir().unwrap();
        let acl = AclStore::open(dir.path()).unwrap();
        acl.allow("bob", 2000, true, false).unwrap();
        assert!(acl.check_read("bob", 2000));
        assert!(!acl.check_write("bob", 2000));
    }

    #[test]
    fn revoke_last_row_returns_to_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        let acl = AclStore::open(dir.path()).unwrap();
        acl.allow("carol", 3000, true, true).unwrap();
        assert!(acl.is_restricted("carol"));
        acl.revoke("carol", 3000).unwrap();
        assert!(!acl.is_restricted("carol"));
        assert!(acl.check_read("carol", 9999));
    }

    #[test]
    fn allow_is_idempotent_update() {
        let dir = tempfile::tempdir().unwrap();
        let acl = AclStore::open(dir.path()).unwrap();
        acl.allow("d", 4000, true, false).unwrap();
        acl.allow("d", 4000, true, true).unwrap(); // upgrade to rw
        assert!(acl.check_write("d", 4000));
        let listed = acl.list("d");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].can_write);
    }
}
