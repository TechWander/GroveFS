//! PostgreSQL metadata backend — a second authoritative store beside the
//! embedded SQLite [`crate::meta::MetaEngine`].
//!
//! It implements the *same* [`crate::meta::MetaProvider`] surface the FUSE layer
//! and the central `meta-server` consume, so switching a `meta-server` to PG (or
//! a local `mount` to PG) is invisible to clients. The view tree, hard links,
//! refcount GC and POSIX advisory locks behave exactly as on SQLite; only the
//! storage engine differs.
//!
//! Mapping from the SQLite schema:
//! - one `<home>/meta/<view>.db` file  ->  one schema `grovefs_v_<hex(view)>`
//!   (a deterministic, injection-safe identifier; the per-view sequence keeps
//!   `id`/`ino` small exactly like the file-per-view model, and the root row is
//!   pinned at id 1 so [`crate::meta::ROOT_INO`] stays valid);
//! - `INTEGER PRIMARY KEY` (rowid)      ->  `BIGSERIAL PRIMARY KEY`;
//! - `INSERT OR IGNORE`                 ->  `ON CONFLICT DO NOTHING`;
//! - `TransactionBehavior::Immediate`   ->  an explicit `BEGIN`/`COMMIT` (the
//!   engine holds one connection behind a mutex, so a whole logical op is
//!   serialized the way SQLite's single-writer model already serializes it);
//! - `busy_timeout` cross-process wait  ->  the mutex + PG's own lock waits.
//!
//! Honest boundary: the GroveFS-native HA (`--role/--peer/--witness` snapshot
//! shipping, built on SQLite `VACUUM INTO`) is NOT offered on the PG path —
//! Postgres replicates itself (streaming replication / PITR), which is strictly
//! better than the file-snapshot mechanism. So [`PgEngine::backup_to_bytes`]
//! reports unsupported and a `meta-server --store postgres://...` refuses the HA
//! flags. Everything else is a drop-in.

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind, ROOT_INO};
use postgres::{Client, NoTls};
use std::path::Path;

/// Default lease for a lock row (seconds), matching [`crate::meta::LockLease`].
fn lease_secs() -> i64 {
    crate::meta::LockLease::secs()
}

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Deterministic, injection-safe PG schema name for a view: `grovefs_v_` +
/// lowercase hex of the raw view bytes. Hex guarantees only `[0-9a-f]` appear,
/// so the identifier never needs quoting and can't be forged with quotes/dots.
/// A view name longer than 25 bytes (-> schema name > 63) is Postgres's
/// identifier-length limit; such names are rejected at open with a clear error.
fn schema_for(view: &str) -> Option<String> {
    if view.is_empty() || view.len() > 25 {
        return None;
    }
    let mut hex = String::with_capacity(view.len() * 2);
    for b in view.as_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    Some(format!("grovefs_v_{hex}"))
}

/// One row of the `node` table before nlink/size are computed (the `finalize`
/// step). Mirrors the SQLite engine's private RawNode.
struct RawNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    is_dir: i64,
    data_ref: Option<String>,
    link_target: Option<String>,
    mode: i64,
    uid: i64,
    gid: i64,
    mtime: i64,
    atime: i64,
    ctime: i64,
    ino: i64,
}

const COLS: &str = "id, parent_id, name, is_dir, data_ref, link_target, mode, uid, gid, mtime, atime, ctime, ino";

fn parse_node(r: &postgres::Row) -> RawNode {
    RawNode {
        id: r.get(0),
        parent_id: r.get(1),
        name: r.get(2),
        is_dir: r.get(3),
        data_ref: r.get(4),
        link_target: r.get(5),
        mode: r.get(6),
        uid: r.get(7),
        gid: r.get(8),
        mtime: r.get(9),
        atime: r.get(10),
        ctime: r.get(11),
        ino: r.get::<_, Option<i64>>(12).unwrap_or(0),
    }
}

/// A [`MetaProvider`] backed by PostgreSQL. Serialized behind one connection
/// (`parking_lot::Mutex<Client>`) — a whole metadata op runs under that lock,
/// exactly as the SQLite engine serializes on its single connection. For
/// cross-node concurrency the central `meta-server` is the single writer; a
/// direct `mount` to a shared PG database gets the same behavior as two SQLite
/// processes sharing a file.
pub struct PgEngine {
    view: String,
    schema: String,
    client: parking_lot::Mutex<Client>,
}

impl PgEngine {
    /// Connect to a PostgreSQL server (from a `postgres://user:pw@host/db`
    /// URI), ensure this view's schema exists (idempotent), and seed the root
    /// row (id=1). The URI is passed through verbatim to [`postgres::Client`].
    ///
    /// `readonly` is accepted for API parity with the SQLite engine but has no
    /// effect on the PG path (GroveFS-native HA is disabled on PG, so a
    /// "standby reads only" mode is meaningless here).
    pub fn open(conn_uri: &str, view: &str, _readonly: bool) -> std::io::Result<Self> {
        let schema = schema_for(view).ok_or_else(|| {
            std::io::Error::other(format!(
                "view name '{view}' incompatible with PG schema naming (empty, or longer than 25 bytes)"
            ))
        })?;
        let client = Client::connect(conn_uri, NoTls)
            .map_err(|e| std::io::Error::other(format!("postgres connect: {e}")))?;
        let this = Self {
            view: view.to_string(),
            schema,
            client: parking_lot::Mutex::new(client),
        };
        this.ensure_schema()?;
        Ok(this)
    }

    /// Convenience for tests/CLI: build the URI from pieces (host:port, user,
    /// password, database). Same effect as [`Self::open`] with a URI string.
    pub fn open_parts(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
        view: &str,
    ) -> std::io::Result<Self> {
        let uri = format!("postgres://{user}:{password}@{host}:{port}/{database}");
        Self::open(&uri, view, false)
    }

    /// The view this engine serves (used by the central server's per-view log
    /// lines / error reporting).
    pub fn view(&self) -> &str {
        &self.view
    }

    /// The path this engine would use if it were SQLite — used by the server
    /// for cache-eviction bookkeeping when a fresh HA snapshot lands. Not
    /// meaningful on PG (no per-view file), but present so callers can log it.
    pub fn pseudo_path(&self, base: &Path) -> std::path::PathBuf {
        base.join("meta").join(format!("{}.pg", self.view))
    }

    fn t(&self, table: &str) -> String {
        format!("{}.{}", self.schema, table)
    }

    fn ensure_schema(&self) -> std::io::Result<()> {
        let mut c = self.client.lock();
        // CREATE SCHEMA takes the raw identifier (no dots).
        c.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", self.schema))
            .map_err(io_e)?;
        let node = self.t("node");
        let locks = self.t("locks");
        let ctrl = self.t("ctrl");
        c.batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {node} (
                 id           BIGSERIAL   PRIMARY KEY,
                 parent_id    BIGINT,
                 name         TEXT        NOT NULL,
                 is_dir       BIGINT      NOT NULL,
                 data_ref     TEXT,
                 link_target  TEXT,
                 mode         BIGINT      NOT NULL,
                 uid          BIGINT      NOT NULL,
                 gid          BIGINT      NOT NULL,
                 mtime        BIGINT      NOT NULL,
                 atime        BIGINT      NOT NULL,
                 ctime        BIGINT      NOT NULL,
                 ino          BIGINT,
                 UNIQUE(parent_id, name)
             );
             CREATE INDEX IF NOT EXISTS node_parent  ON {node}(parent_id);
             CREATE INDEX IF NOT EXISTS node_dataref ON {node}(data_ref);
             CREATE TABLE IF NOT EXISTS {ctrl} (k TEXT PRIMARY KEY, v BIGINT NOT NULL);
             CREATE TABLE IF NOT EXISTS {locks} (
                 id          BIGSERIAL PRIMARY KEY,
                 ino         BIGINT NOT NULL,
                 owner       BIGINT NOT NULL,
                 sid         BIGINT NOT NULL DEFAULT 0,
                 l_start     BIGINT NOT NULL,
                 l_end       BIGINT NOT NULL,
                 pid         BIGINT NOT NULL,
                 ltype       BIGINT NOT NULL,
                 expires_at  BIGINT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS locks_ino    ON {locks}(ino, l_start, l_end);
             CREATE INDEX IF NOT EXISTS locks_owner  ON {locks}(owner);
             CREATE INDEX IF NOT EXISTS locks_sid    ON {locks}(sid);
             CREATE INDEX IF NOT EXISTS locks_expire ON {locks}(expires_at);"
        ))
        .map_err(io_e)?;
        // Seed the root (id 1) if absent, then align the BIGSERIAL sequence so
        // the next INSERT starts at max(id)+1 (the explicit id=1 didn't advance
        // the sequence and would otherwise collide with the next default).
        let t = now();
        c.execute(
            &format!(
                "INSERT INTO {node} (id, parent_id, name, is_dir, data_ref, link_target, mode, uid, gid, mtime, atime, ctime, ino)
                 VALUES (1, NULL, '/', 1, NULL, NULL, 511, 0, 0, $1, $1, $1, 1)
                 ON CONFLICT (id) DO NOTHING",
            ),
            &[&t],
        )
        .map_err(io_e)?;
        c.execute(
            &format!(
                "SELECT setval(pg_get_serial_sequence('{node}','id'), (SELECT COALESCE(MAX(id),1) FROM {node}))"
            ),
            &[],
        )
        .map_err(io_e)?;
        Ok(())
    }

    // ---- read paths -------------------------------------------------------

    /// Compute nlink/size for a raw row and produce the NodeInfo. Runs on the
    /// caller's already-held connection to avoid re-locking.
    fn finalize(&self, c: &mut Client, raw: RawNode) -> NodeInfo {
        let kind = if raw.is_dir == 1 {
            NodeKind::Dir
        } else if raw.link_target.is_some() {
            NodeKind::Symlink
        } else {
            NodeKind::Regular
        };
        let node = self.t("node");
        let (nlink, size) = match kind {
            NodeKind::Dir => {
                let subdirs: i64 = c
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1 AND is_dir=1"),
                        &[&raw.id],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                    .unwrap_or(0);
                (2 + subdirs as u64, 4096u64)
            }
            NodeKind::Symlink => (
                1,
                raw.link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0),
            ),
            NodeKind::Regular => {
                let links: i64 = c
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE ino=$1"),
                        &[&raw.ino],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(1))
                    .unwrap_or(1);
                (links.max(1) as u64, 0u64)
            }
        };
        NodeInfo {
            id: raw.ino,
            parent_id: raw.parent_id.unwrap_or(0),
            name: raw.name,
            kind,
            data_ref: raw.data_ref,
            link_target: raw.link_target,
            mode: raw.mode as u32,
            uid: raw.uid as u32,
            gid: raw.gid as u32,
            mtime: raw.mtime,
            atime: raw.atime,
            ctime: raw.ctime,
            nlink,
            size,
        }
    }

    fn query_one(&self, sql: &str, params: &[&(dyn postgres::types::ToSql + Sync)]) -> Option<RawNode> {
        let mut c = self.client.lock();
        c.query(sql, params).ok().and_then(|rows| {
            rows.into_iter().next().map(|r| parse_node(&r))
        })
    }

    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE parent_id=$1 AND name=$2");
        let raw = self.query_one(&sql, &[&parent, &name])?;
        let mut c = self.client.lock();
        Some(self.finalize(&mut c, raw))
    }

    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE ino=$1 LIMIT 1");
        let raw = self.query_one(&sql, &[&ino])?;
        let mut c = self.client.lock();
        Some(self.finalize(&mut c, raw))
    }

    pub fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let node = self.t("node");
        let sql = format!("SELECT {COLS} FROM {node} WHERE parent_id=$1 ORDER BY name");
        let mut c = self.client.lock();
        let rows = match c.query(&sql, &[&parent]) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let raws: Vec<RawNode> = rows.iter().map(parse_node).collect();
        raws.into_iter().map(|r| self.finalize(&mut c, r)).collect()
    }

    // ---- write paths ------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        link_target: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        let t = now();
        let node = self.t("node");
        let sql = format!(
            "INSERT INTO {node} (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9,$9) RETURNING id"
        );
        let mut c = self.client.lock();
        let id: i64 = c
            .query(
                &sql,
                &[
                    &parent,
                    &name,
                    &(is_dir as i64),
                    &data_ref,
                    &link_target,
                    &(mode as i64),
                    &(uid as i64),
                    &(gid as i64),
                    &t,
                ],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .map(|r| r.get(0))
            .ok_or_else(|| MetaError::Db("no RETURNING row".to_string()))?;
        // Brand-new name owns a fresh inode: ino == id.
        let _ = c.execute(
            &format!("UPDATE {node} SET ino=$1 WHERE id=$1"),
            &[&id],
        );
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.insert(parent, name, is_dir, data_ref, None, mode, uid, gid)
    }

    pub fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.insert(parent, name, false, None, Some(target), 0o0777, uid, gid)
    }

    pub fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        let node = self.t("node");
        let mut c = self.client.lock();
        let row = c
            .query(
                &format!("SELECT is_dir, data_ref, mode, uid, gid FROM {node} WHERE ino=$1 LIMIT 1"),
                &[&src_ino],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .ok_or(MetaError::NotFound)?;
        let is_dir: i64 = row.get::<_, i64>(0);
        let data_ref: Option<String> = row.get(1);
        let mode: i64 = row.get(2);
        let uid: i64 = row.get(3);
        let gid: i64 = row.get(4);
        if is_dir == 1 || data_ref.is_none() {
            return Err(MetaError::OpNotPermitted);
        }
        let t = now();
        c.execute(
            &format!(
                "INSERT INTO {node} (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                 VALUES ($1,$2,0,$3,NULL,$4,$5,$6,$7,$7,$7,$8)"
            ),
            &[
                &new_parent,
                &new_name,
                &data_ref,
                &mode,
                &uid,
                &gid,
                &t,
                &src_ino,
            ],
        )
        .map_err(|e| map_db_err(&e))?;
        Ok(src_ino)
    }

    pub fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        let node = self.t("node");
        let t = now();
        let mut c = self.client.lock();
        let mut tx = c.transaction().map_err(|e| MetaError::Db(e.to_string()))?;
        let src_id: i64 = tx
            .query(
                &format!("SELECT id FROM {node} WHERE parent_id=$1 AND name=$2"),
                &[&parent, &name],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .map(|r| r.get(0))
            .ok_or(MetaError::NotFound)?;
        if is_descendant_tx(&mut tx, &node, new_parent, src_id)? {
            return Err(MetaError::BadParentCycle);
        }
        let tgt: Option<(i64, i64, Option<String>)> = tx
            .query(
                &format!("SELECT id, is_dir, data_ref FROM {node} WHERE parent_id=$1 AND name=$2"),
                &[&new_parent, &new_name],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .map(|r| (r.get(0), r.get(1), r.get(2)));
        if let Some((tid, _, _)) = &tgt {
            if *tid == src_id {
                tx.commit().map_err(|e| MetaError::Db(e.to_string()))?;
                return Ok(None);
            }
        }
        if tgt.is_some() && noreplace {
            return Err(MetaError::Exists);
        }
        let mut gc: Option<String> = None;
        if let Some((tid, is_dir, tgt_ref)) = tgt {
            let src_is_dir: i64 = tx
                .query(
                    &format!("SELECT is_dir FROM {node} WHERE id=$1"),
                    &[&src_id],
                )
                .map_err(|e| map_db_err(&e))?
                .into_iter()
                .next()
                .map(|r| r.get(0))
                .unwrap_or(0);
            if src_is_dir != is_dir {
                return Err(MetaError::TypeMismatch);
            }
            if is_dir == 1 {
                let kids: i64 = tx
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1"),
                        &[&tid],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                    .unwrap_or(0);
                if kids > 0 {
                    return Err(MetaError::NotEmpty);
                }
            }
            tx.execute(
                &format!("DELETE FROM {node} WHERE id=$1"),
                &[&tid],
            )
            .map_err(|e| map_db_err(&e))?;
            if let Some(dr) = tgt_ref {
                let refs: i64 = tx
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE data_ref=$1"),
                        &[&dr],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                    .unwrap_or(0);
                if refs == 0 {
                    gc = Some(dr);
                }
            }
        }
        tx.execute(
            &format!("UPDATE {node} SET parent_id=$1, name=$2, ctime=$3 WHERE id=$4"),
            &[&new_parent, &new_name, &t, &src_id],
        )
        .map_err(|e| map_db_err(&e))?;
        tx.commit().map_err(|e| MetaError::Db(e.to_string()))?;
        Ok(gc)
    }

    pub fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let node = self.t("node");
        let mut c = self.client.lock();
        let mut tx = c.transaction().map_err(|e| MetaError::Db(e.to_string()))?;
        let row = tx
            .query(
                &format!("SELECT id, is_dir, data_ref FROM {node} WHERE parent_id=$1 AND name=$2"),
                &[&parent, &name],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .ok_or(MetaError::NotFound)?;
        let id: i64 = row.get(0);
        let is_dir: i64 = row.get(1);
        let data_ref: Option<String> = row.get(2);
        if is_dir == 1 {
            let kids: i64 = tx
                .query(
                    &format!("SELECT COUNT(*) FROM {node} WHERE parent_id=$1"),
                    &[&id],
                )
                .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                .unwrap_or(0);
            if kids > 0 {
                return Err(MetaError::NotEmpty);
            }
        }
        tx.execute(
            &format!("DELETE FROM {node} WHERE id=$1"),
            &[&id],
        )
        .map_err(|e| map_db_err(&e))?;
        let gc = match &data_ref {
            Some(dr) => {
                let refs: i64 = tx
                    .query(
                        &format!("SELECT COUNT(*) FROM {node} WHERE data_ref=$1"),
                        &[&dr],
                    )
                    .map(|rows| rows.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
                    .unwrap_or(0);
                if refs == 0 {
                    Some(dr.clone())
                } else {
                    None
                }
            }
            None => None,
        };
        tx.commit().map_err(|e| MetaError::Db(e.to_string()))?;
        Ok(gc)
    }

    pub fn touch_mtime(&self, ino: i64) {
        let t = now();
        let node = self.t("node");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("UPDATE {node} SET mtime=$1, ctime=$1 WHERE ino=$2"),
            &[&t, &ino],
        );
    }

    pub fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let t = now();
        let node = self.t("node");
        let mut c = self.client.lock();
        if let Some(a) = atime {
            let _ = c.execute(
                &format!("UPDATE {node} SET atime=$1, ctime=$2 WHERE ino=$3"),
                &[&a, &t, &ino],
            );
        }
        if let Some(m) = mtime {
            let _ = c.execute(
                &format!("UPDATE {node} SET mtime=$1, ctime=$2 WHERE ino=$3"),
                &[&m, &t, &ino],
            );
        }
    }

    pub fn set_mode(&self, ino: i64, mode: u32) {
        let t = now();
        let node = self.t("node");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("UPDATE {node} SET mode=$1, ctime=$2 WHERE ino=$3"),
            &[&(mode as i64), &t, &ino],
        );
    }

    pub fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let t = now();
        let node = self.t("node");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("UPDATE {node} SET uid=$1, gid=$2, ctime=$3 WHERE ino=$4"),
            &[&(uid as i64), &(gid as i64), &t, &ino],
        );
    }

    // ---- HA epoch (present for API parity; GroveFS HA runs SQLite-only) ----

    pub fn get_epoch(&self) -> i64 {
        let ctrl = self.t("ctrl");
        let mut c = self.client.lock();
        c.query(&format!("SELECT v FROM {ctrl} WHERE k='epoch'"), &[])
            .ok()
            .and_then(|rows| rows.first().map(|r| r.get::<_, i64>(0)))
            .unwrap_or(0)
    }

    pub fn set_epoch(&self, e: i64) -> std::io::Result<()> {
        let ctrl = self.t("ctrl");
        let mut c = self.client.lock();
        c.execute(
            &format!(
                "INSERT INTO {ctrl} (k,v) VALUES ('epoch',$1) ON CONFLICT(k) DO UPDATE SET v=excluded.v"
            ),
            &[&e],
        )
        .map(|_| ())
        .map_err(io_e)
    }

    /// GroveFS-native HA snapshot is not offered on PG — Postgres replicates
    /// via its own mechanisms (streaming replication / PITR), and a `VACUUM
    /// INTO`-equivalent (a whole-database dump per view) is not worth
    /// duplicating here. `meta-server --store postgres://...` therefore rejects
    /// `--role/--peer/--witness` at CLI parse time and never reaches this
    /// method; the impl is only present to satisfy the MetaStore enum's
    /// uniform interface.
    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "GroveFS-native HA snapshot is SQLite-only; use PostgreSQL's own replication for a PG-backed meta-server",
        ))
    }

    // ---- POSIX advisory locks ---------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn lock_op(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
        pid: u32,
        sid: u64,
    ) -> Result<Option<LockConflict>, MetaError> {
        let locks = self.t("locks");
        let now = now();
        let mut c = self.client.lock();
        let mut tx = c.transaction().map_err(|e| MetaError::Db(e.to_string()))?;
        tx.execute(
            &format!("DELETE FROM {locks} WHERE expires_at < $1"),
            &[&now],
        )
        .map_err(|e| map_db_err(&e))?;
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        let owner_i = owner as i64;
        match ty {
            LockType::Unlock => {
                tx.execute(
                    &format!("DELETE FROM {locks} WHERE ino=$1 AND owner=$2 AND l_start<=$3 AND l_end>=$4"),
                    &[&ino, &owner_i, &e, &s],
                )
                .map_err(|er| map_db_err(&er))?;
            }
            LockType::Read | LockType::Write => {
                let want_write = ty.to_u8() as i64 == 1;
                let conflict_sql = format!(
                    "SELECT l_start, l_end, ltype, pid FROM {locks}
                     WHERE ino=$1 AND owner<>$2 AND l_start<=$3 AND l_end>=$4
                       AND ($5::bigint = 1 OR ltype = 1) LIMIT 1"
                );
                let conflict: Option<(i64, i64, i64, i64)> = tx
                    .query(
                        &conflict_sql,
                        &[&ino, &owner_i, &e, &s, &(want_write as i64)],
                    )
                    .map_err(|er| map_db_err(&er))?
                    .into_iter()
                    .next()
                    .map(|r| (r.get(0), r.get(1), r.get::<_, i64>(2), r.get::<_, i64>(3)));
                if let Some((cs, ce, ct, cp)) = conflict {
                    return Ok(Some(LockConflict {
                        start: cs.max(0) as u64,
                        end: ce.max(0) as u64,
                        ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                        pid: cp as u32,
                    }));
                }
                tx.execute(
                    &format!("DELETE FROM {locks} WHERE ino=$1 AND owner=$2 AND l_start<=$3 AND l_end>=$4"),
                    &[&ino, &owner_i, &e, &s],
                )
                .map_err(|er| map_db_err(&er))?;
                tx.execute(
                    &format!(
                        "INSERT INTO {locks} (ino,owner,sid,l_start,l_end,pid,ltype,expires_at)
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
                    ),
                    &[
                        &ino,
                        &owner_i,
                        &(sid as i64),
                        &s,
                        &e,
                        &(pid as i64),
                        &(ty.to_u8() as i64),
                        &(now + lease_secs()),
                    ],
                )
                .map_err(|er| map_db_err(&er))?;
            }
        }
        tx.commit().map_err(|e| MetaError::Db(e.to_string()))?;
        Ok(None)
    }

    pub fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        let locks = self.t("locks");
        let now = now();
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        let mut c = self.client.lock();
        let want_write = ty.to_u8() as i64 == 1;
        c.query(
            &format!(
                "SELECT l_start, l_end, ltype, pid FROM {locks}
                 WHERE ino=$1 AND owner<>$2 AND expires_at>=$3 AND l_start<=$4 AND l_end>=$5
                   AND ($6::bigint = 1 OR ltype = 1) LIMIT 1"
            ),
            &[&ino, &(owner as i64), &now, &e, &s, &want_write],
        )
        .ok()
        .and_then(|rows| {
            rows.into_iter().next().map(|r| {
                let cs: i64 = r.get(0);
                let ce: i64 = r.get(1);
                let ct: i64 = r.get(2);
                let cp: i64 = r.get(3);
                LockConflict {
                    start: cs.max(0) as u64,
                    end: ce.max(0) as u64,
                    ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                    pid: cp as u32,
                }
            })
        })
    }

    pub fn release_locks_of_owner(&self, owner: u64) {
        let locks = self.t("locks");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("DELETE FROM {locks} WHERE owner=$1"),
            &[&(owner as i64)],
        );
    }

    pub fn renew_locks(&self, sid: u64) {
        let locks = self.t("locks");
        let now = now();
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("UPDATE {locks} SET expires_at=$1 WHERE sid=$2"),
            &[&(now + lease_secs()), &(sid as i64)],
        );
    }

    pub fn release_locks_of_session(&self, sid: u64) {
        let locks = self.t("locks");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("DELETE FROM {locks} WHERE sid=$1"),
            &[&(sid as i64)],
        );
    }

    pub fn sweep_expired_locks(&self, now_secs: i64) {
        let locks = self.t("locks");
        let mut c = self.client.lock();
        let _ = c.execute(
            &format!("DELETE FROM {locks} WHERE expires_at < $1"),
            &[&now_secs],
        );
    }
}

/// Walk ancestors of `new_parent` looking for `src_id`; if we reach it, moving
/// would create a cycle. Runs inside the rename transaction.
fn is_descendant_tx(
    tx: &mut postgres::Transaction,
    node: &str,
    new_parent: i64,
    src_id: i64,
) -> Result<bool, MetaError> {
    let mut cur = new_parent;
    for _ in 0..4096 {
        if cur == src_id {
            return Ok(true);
        }
        let parent: Option<i64> = tx
            .query(
                &format!("SELECT parent_id FROM {node} WHERE id=$1"),
                &[&cur],
            )
            .map_err(|e| map_db_err(&e))?
            .into_iter()
            .next()
            .and_then(|r| r.get(0));
        match parent {
            Some(p) => cur = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

fn io_e(e: postgres::Error) -> std::io::Error {
    // `postgres::Error` Display shows only the ErrorKind (e.g. "db error"),
    // which is useless on its own. Surface the server's SQLSTATE / message /
    // detail / cause chain so a failed open or query is diagnosable from the
    // `meta-server` log line alone.
    let mut s = format!("{}", e);
    if let Some(db) = e.as_db_error() {
        s = format!("{s}: [{}] {}", db.code().code(), db.message());
        if let Some(d) = db.detail() {
            s = format!("{s}  detail={d}");
        }
        if let Some(h) = db.hint() {
            s = format!("{s}  hint={h}");
        }
    }
    if let Some(src) = std::error::Error::source(&e) {
        s = format!("{s}  cause={src}");
    }
    std::io::Error::other(s)
}

/// Map a `postgres::Error` to a MetaError. Unique-violation (23505) -> Exists,
/// everything else -> Db.
fn map_db_err(e: &postgres::Error) -> MetaError {
    if e.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) {
        MetaError::Exists
    } else {
        let mut s = format!("{}", e);
        if let Some(db) = e.as_db_error() {
            s = format!("{s}: [{}] {}", db.code().code(), db.message());
        }
        MetaError::Db(s)
    }
}

// The MetaStore enum in meta.rs implements MetaProvider by delegating to either
// MetaEngine or PgEngine; provide the same trait surface here too so callers
// holding an `Arc<PgEngine>` directly (unit tests) can also use it as a
// MetaProvider.
impl MetaProvider for PgEngine {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        PgEngine::lookup(self, parent, name)
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        PgEngine::getattr(self, ino)
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        PgEngine::readdir(self, parent)
    }
    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        PgEngine::create_node(self, parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink(&self, parent: i64, name: &str, target: &str, uid: u32, gid: u32) -> Result<i64, MetaError> {
        PgEngine::create_symlink(self, parent, name, target, uid, gid)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        PgEngine::link_node(self, src_ino, new_parent, new_name)
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        PgEngine::unlink(self, parent, name)
    }
    fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str, noreplace: bool) -> Result<Option<String>, MetaError> {
        PgEngine::rename(self, parent, name, new_parent, new_name, noreplace)
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        PgEngine::set_mode(self, ino, mode)
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        PgEngine::set_uid_gid(self, ino, uid, gid)
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        PgEngine::set_times(self, ino, atime, mtime)
    }
    fn touch_mtime(&self, ino: i64) {
        PgEngine::touch_mtime(self, ino)
    }
    #[allow(clippy::too_many_arguments)]
    fn lock_op(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
        pid: u32,
        sid: u64,
    ) -> Result<Option<LockConflict>, MetaError> {
        PgEngine::lock_op(self, ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(&self, ino: i64, owner: u64, start: u64, end: u64, ty: LockType) -> Option<LockConflict> {
        PgEngine::get_lock(self, ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        PgEngine::release_locks_of_owner(self, owner)
    }
    fn renew_locks(&self, sid: u64) {
        PgEngine::renew_locks(self, sid)
    }
}

// Silence the ROOT_INO unused-import warning if not referenced in a given build
// (it *is* referenced in comments; this keeps the compiler happy without a
// runtime use).
#[allow(dead_code)]
const _ROOT: i64 = ROOT_INO;
