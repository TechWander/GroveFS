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
//!   gate may mount it, exactly as before. The 14 regression suites never write
//!   ACL rows, so they are unaffected.
//! - A view with **one or more ACL rows is whitelisted**: only uids listed with
//!   `can_read` may open it at all; only uids listed with `can_write` may mutate
//!   it (create/unlink/rename/write/chmod/...).
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

/// The per-base ACL table, stored in a dedicated `<base>/meta/acl.db` so it is
/// resolved *before* any individual view db is opened (authorization precedes
/// selecting the view's storage). Serialized behind a fast mutex like the
/// metadata engine; a meta-server shares one instance across all connections.
pub struct AclStore {
    conn: parking_lot::Mutex<Connection>,
}

impl AclStore {
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
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    /// A view is "restricted" once it has any whitelist rows; unrestricted views
    /// are open to everyone (backward-compatible default).
    pub fn is_restricted(&self, view: &str) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM view_acl WHERE view=?1",
            params![view],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    /// Whether `uid` may open (mount/read) `view`. Unrestricted → always true.
    pub fn check_read(&self, view: &str, uid: u32) -> bool {
        let conn = self.conn.lock();
        let restricted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM view_acl WHERE view=?1",
                params![view],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if restricted == 0 {
            return true;
        }
        conn.query_row(
            "SELECT can_read FROM view_acl WHERE view=?1 AND uid=?2",
            params![view, uid as i64],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            == 1
    }

    /// Whether `uid` may mutate `view`. Unrestricted → true (no write policy).
    /// Restricted → requires an explicit `can_write` row.
    pub fn check_write(&self, view: &str, uid: u32) -> bool {
        let conn = self.conn.lock();
        let restricted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM view_acl WHERE view=?1",
                params![view],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if restricted == 0 {
            return true;
        }
        conn.query_row(
            "SELECT can_write FROM view_acl WHERE view=?1 AND uid=?2",
            params![view, uid as i64],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            == 1
    }

    /// Grant (or replace) `uid`'s read/write access to `view`. At least one of
    /// read/write should be set for the row to be meaningful, but a row with both
    /// false is legal: it reserves the whitelist slot (view becomes restricted,
    /// and this uid is denied) so later `revoke` of others does not reopen it.
    pub fn allow(&self, view: &str, uid: u32, read: bool, write: bool) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO view_acl (view,uid,can_read,can_write) VALUES (?1,?2,?3,?4)
             ON CONFLICT(view,uid) DO UPDATE SET can_read=excluded.can_read, can_write=excluded.can_write",
            params![view, uid as i64, read as i64, write as i64],
        )?;
        Ok(())
    }

    /// Remove `uid`'s grant from `view`. When the last row for a view is removed
    /// the view returns to unrestricted (the whitelist is defined by its rows).
    pub fn revoke(&self, view: &str, uid: u32) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM view_acl WHERE view=?1 AND uid=?2",
            params![view, uid as i64],
        )?;
        Ok(())
    }

    /// List grants for one view (empty = unrestricted).
    pub fn list(&self, view: &str) -> Vec<AclEntry> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
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
