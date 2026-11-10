//! Server-side / embedded half of [`crate::meta`]: the SQLite-backed
//! `MetaEngine`, the cross-view holder ledger, and the fork/graft/export/import
//! governance built on them. Gated behind the `native` feature — a
//! `remote-only` client build compiles only the types + [`MetaProvider`] trait
//! and [`MetaError`] that stay in the parent module.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::stats;

static LEDGER_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialize a write op that may touch the shared ledger: takes
/// [`LEDGER_SERIAL`] first, then runs `f` (which internally takes its engine
/// lock). All grovefs processes in one deployment run the same code, so every
/// ledger writer acquires the two locks in one order and cannot deadlock
/// against the db-level queueing.
pub fn with_ledger_serial<T>(f: impl FnOnce() -> T) -> T {
    let _g = LEDGER_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// ATTACH the sibling ledger and make sure its table exists (idempotent).
/// Called on every non-readonly engine open; a read-only open attaches only
/// best-effort when the file already exists (a standby never writes, and a
/// count query against an unattached schema simply reads "no other view").
fn attach_refs_ledger(conn: &Connection, path: &Path) -> rusqlite::Result<()> {
    let ledger = path
        .parent()
        .map(|d| d.join(REFS_DB))
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("view db has no parent dir".into()))?;
    if let Some(dir) = ledger.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let lit = ledger.to_string_lossy().replace("'", "''");
    // ATTACH has no `IF NOT EXISTS`; each engine opens a fresh connection and
    // attaches exactly once here, so re-attaching the same alias is not a
    // concern. The table is created idempotently for a brand-new ledger file.
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{lit}' AS {REFS_SCHEMA};
         CREATE TABLE IF NOT EXISTS {REFS_SCHEMA}.holders (
             data_ref TEXT PRIMARY KEY,
             holders  INTEGER NOT NULL
         );"
    ))?;
    Ok(())
}

/// Does another view's tree already hold a name for `dr`? Read through the
/// attached ledger (an unattached schema answers `false`, i.e. only this view
/// holds the key). `tx` is any rusqlite transaction on the engine
/// connection, so the read shares the txn's snapshot with its DELETE.
pub fn other_view_holds(tx: &rusqlite::Transaction<'_>, dr: &str) -> bool {
    tx.query_row(
        &format!("SELECT 1 FROM {REFS_SCHEMA}.holders WHERE data_ref=?1 LIMIT 1"),
        params![dr],
        |_| Ok(()),
    )
    .is_ok()
}

/// Book this view as holding a name for `dr` (call when the view's name count
/// for `dr` goes 0 -> 1, inside the same tx as the node INSERT).
pub fn ledger_add(tx: &rusqlite::Transaction<'_>, dr: &str) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO {REFS_SCHEMA}.holders (data_ref, holders) VALUES (?1, 1)
             ON CONFLICT(data_ref) DO UPDATE SET holders = holders + 1"
        ),
        params![dr],
    )?;
    Ok(())
}

/// Drop this view's hold on `dr` (call when the view's LAST name for `dr` is
/// removed, inside the same tx as the node DELETE). Returns `Some(dr)` when the
/// blob is now physically freeable — no view holds it anymore — and `None` when
/// another view still does (the caller GCs only on `Some`). A missing row is
/// treated as "no other holder": nothing was ever booked for this key.
pub fn ledger_remove(tx: &rusqlite::Transaction<'_>, dr: &str) -> Option<String> {
    let n = tx
        .execute(
            &format!("UPDATE {REFS_SCHEMA}.holders SET holders = holders - 1 WHERE data_ref=?1"),
            params![dr],
        )
        .unwrap_or(0);
    if n > 0 {
        let left: i64 = tx
            .query_row(
                &format!("SELECT holders FROM {REFS_SCHEMA}.holders WHERE data_ref=?1"),
                params![dr],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if left <= 0 {
            let _ = tx.execute(
                &format!("DELETE FROM {REFS_SCHEMA}.holders WHERE data_ref=?1"),
                params![dr],
            );
            return Some(dr.to_string());
        }
        None
    } else {
        // No ledger row: this view was never booked against a shared key. The
        // same-view refcount already read 0 at this call site, so freeing is
        // the safe answer for a key no other view knows about.
        Some(dr.to_string())
    }
}

/// Outcome of [`fork_view`]: directory entries copied and DISTINCT blobs the
/// new tree now co-holds with its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkStats {
    pub nodes: i64,
    pub blobs: i64,
}

/// Seed an INDEPENDENT metadata tree `name` as a point-in-time copy of the
/// physical view `base_view`'s tree (`<base>/meta/<name>.db`). Content bytes
/// are SHARED, not copied: every distinct `data_ref` in the copy is booked once
/// against the new view in the cross-view ledger, so a delete in either tree
/// frees a blob only when its last name in the last holding view disappears.
///
/// Not copied on purpose: `ctrl` (the HA fencing epoch — a fork is a new view,
/// not a standby replica) and `locks` (transient leases of the source mount).
/// Inode numbers ARE copied, so hard links inside the base stay hard links in
/// the fork; new nodes in the fork keep allocating `ino == row id`, which can
/// never collide because every copied id already exists.
///
/// The source is read through a SEPARATE read-only connection (WAL readers never
/// block the base view's writers); the destination is built in one IMMEDIATE
/// transaction that also books the ledger, so a crash leaves either no fork or a
/// fully seeded one.
pub fn fork_view(base: &Path, base_view: &str, name: &str) -> Result<ForkStats, String> {
    fork_view_with(base, base_view, name, true)
}

/// [`fork_view`] with an explicit seeding choice. `seed == false` creates the
/// fork's OWN tree at `meta/<name>.db` (schema + root only) and copies no rows,
/// so the caller can place a curated subset later with [`ForkGraft`]. The
/// definition still says `fork <base>`, so mount semantics are identical
/// (structure writable, content EROFS) and deletes stay local to the fork.
pub fn fork_view_with(
    base: &Path,
    base_view: &str,
    name: &str,
    seed: bool,
) -> Result<ForkStats, String> {
    let meta_dir = base.join("meta");
    let src_path = meta_dir.join(format!("{base_view}.db"));
    let dst_path = meta_dir.join(format!("{name}.db"));
    if !src_path.is_file() {
        return Err(format!(
            "no physical base tree {} — a fork copies a local SQLite tree",
            src_path.display()
        ));
    }
    if dst_path.exists() {
        return Err(format!("fork target {} already exists", dst_path.display()));
    }
    if !seed {
        // Nothing to copy: opening the engine seeds the schema and the root row
        // (id/ino 1, 0755, owner of this process), exactly like `view create`.
        let _ = MetaEngine::open(&dst_path)
            .map_err(|e| format!("cannot create fork tree {}: {e}", dst_path.display()))?;
        return Ok(ForkStats { nodes: 1, blobs: 0 });
    }
    // Snapshot the source rows first, inode numbers included: names that
    // shared an inode in the source stay hard links in the fork.
    struct Row {
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
    let src = Connection::open_with_flags(&src_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open base tree {}: {e}", src_path.display()))?;
    let _ = src.execute_batch("PRAGMA busy_timeout=5000;");
    let rows: Vec<Row> = {
        let mut stmt = src
            .prepare(
                "SELECT id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino
                 FROM node ORDER BY id",
            )
            .map_err(|e| format!("cannot read base tree {}: {e}", src_path.display()))?;
        let iter = stmt
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    is_dir: r.get(3)?,
                    data_ref: r.get(4)?,
                    link_target: r.get(5)?,
                    mode: r.get(6)?,
                    uid: r.get(7)?,
                    gid: r.get(8)?,
                    mtime: r.get(9)?,
                    atime: r.get(10)?,
                    ctime: r.get(11)?,
                    ino: r.get(12)?,
                })
            })
            .map_err(|e| format!("cannot read base tree {}: {e}", src_path.display()))?;
        iter.filter_map(|x| x.ok()).collect()
    };
    drop(src);
    if rows.is_empty() {
        return Err(format!(
            "base tree {} has no root row — refusing to fork an empty store",
            src_path.display()
        ));
    }
    let blobs: HashSet<String> = rows.iter().filter_map(|r| r.data_ref.clone()).collect();
    // Create the destination tree (schema + root + ledger attached), then
    // REPLACE its root with the copied rows inside one ledger-ordered tx.
    let eng = MetaEngine::open(&dst_path)
        .map_err(|e| format!("cannot create fork tree {}: {e}", dst_path.display()))?;
    let nodes = rows.len() as i64;
    let nblobs = blobs.len() as i64;
    with_ledger_serial(|| {
        let mut conn = eng.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM node", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO node
                 (id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?;
            for r in &rows {
                ins.execute(params![
                    r.id,
                    r.parent_id,
                    r.name,
                    r.is_dir,
                    r.data_ref,
                    r.link_target,
                    r.mode,
                    r.uid,
                    r.gid,
                    r.mtime,
                    r.atime,
                    r.ctime,
                    r.ino
                ])?;
            }
        }
        // Book the fork's own hold on every distinct blob (the base view
        // already holds its row — every engine books on the 0->1 name count).
        for dr in &blobs {
            ledger_add(&tx, dr)?;
        }
        tx.commit()?;
        Ok::<(), rusqlite::Error>(())
    })
    .map_err(|e| format!("cannot seed fork tree {}: {e}", dst_path.display()))?;
    Ok(ForkStats {
        nodes,
        blobs: nblobs,
    })
}

/// One line of a graft plan: pull `src` — a path relative to the SOURCE view's
/// root — into the destination tree at `dst` (relative to the plan's `dest`
/// prefix, or to the destination root when that prefix is empty).
///
/// A directory `src` brings its whole subtree with it (original names kept); a
/// file or a symlink brings just itself. Because `dst` names the destination
/// entry, **renaming is just a different basename**: this is how UUID-named
/// objects become a human-shaped tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraftItem {
    pub src: String,
    pub dst: String,
}

/// Outcome of [`graft_view`]: rows added, how many of those are directories,
/// distinct blobs the destination now co-holds, and entries left alone by the
/// `skip_existing` merge rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraftStats {
    pub nodes: i64,
    pub dirs: i64,
    pub blobs: i64,
    pub skipped: i64,
}

/// Normalize one plan path: strip leading/trailing slashes and `.`
/// components, refuse `..` outright. A plan is DATA handed over by another
/// program, never a shell word, so it must not be able to walk out of the view.
fn normalize_plan_path(p: &str) -> Result<String, String> {
    if p.contains('\0') {
        return Err("path contains a NUL byte".to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            return Err(format!("path '{p}' must not contain '..'"));
        }
        parts.push(c);
    }
    if parts.is_empty() {
        return Err(
            "empty path — the root itself cannot be grafted, name an entry inside it".to_string(),
        );
    }
    Ok(parts.join("/"))
}

/// Parse a graft plan: one entry per line, `<src>` or `<src>TAB<dst>`, with
/// blank lines and `#` comments ignored. Only the FIRST tab splits, so names
/// may contain spaces; a relative `src` with a leading `/` is accepted. An
/// entry without `dst` keeps its path (that is the bulk form: pipe a plain list
/// of source paths and nothing gets renamed).
pub fn parse_graft_plan(text: &str) -> Result<Vec<GraftItem>, String> {
    let mut out: Vec<GraftItem> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (src, dst) = match line.find('\t') {
            Some(k) => (&line[..k], &line[k + 1..]),
            None => (line, ""),
        };
        // Trim the fields (never the inside): plans are often aligned with
        // spaces, and a CRLF tail would otherwise become part of a name.
        let src = normalize_plan_path(src.trim()).map_err(|e| format!("line {lineno}: {e}"))?;
        let dst = if dst.trim().is_empty() {
            src.clone()
        } else {
            normalize_plan_path(dst.trim()).map_err(|e| format!("line {lineno}: {e}"))?
        };
        out.push(GraftItem { src, dst });
    }
    Ok(out)
}

/// One node row of a tree being read for a graft (the destination is only ever
/// written, never bulk-read).
struct GraftRow {
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

fn read_graft_rows(path: &Path) -> Result<Vec<GraftRow>, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open source tree {}: {e}", path.display()))?;
    let _ = conn.execute_batch("PRAGMA busy_timeout=5000;");
    let mut stmt = conn
        .prepare(
            "SELECT id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino
             FROM node ORDER BY id",
        )
        .map_err(|e| format!("cannot read source tree {}: {e}", path.display()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(GraftRow {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                is_dir: r.get(3)?,
                data_ref: r.get(4)?,
                link_target: r.get(5)?,
                mode: r.get(6)?,
                uid: r.get(7)?,
                gid: r.get(8)?,
                mtime: r.get(9)?,
                atime: r.get(10)?,
                ctime: r.get(11)?,
                ino: r.get(12)?,
            })
        })
        .map_err(|e| format!("cannot read source tree {}: {e}", path.display()))?
        .filter_map(|x| x.ok())
        .collect();
    Ok(rows)
}

fn graft_children(rows: &[GraftRow]) -> HashMap<i64, Vec<usize>> {
    let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(p) = r.parent_id {
            m.entry(p).or_default().push(i);
        }
    }
    m
}

/// Resolve a plan path (`a/b/c`) against the source rows. `None` means the
/// path does not exist in that tree — the caller names the view in the error.
fn graft_resolve(
    rows: &[GraftRow],
    children: &HashMap<i64, Vec<usize>>,
    root: i64,
    path: &str,
) -> Option<usize> {
    let mut parent = root;
    let mut cur: Option<usize> = None;
    for comp in path.split('/') {
        let idx = *children
            .get(&parent)?
            .iter()
            .find(|&&k| rows[k].name == comp)?;
        parent = rows[idx].id;
        cur = Some(idx);
    }
    cur
}

/// Mutable state carried through one graft transaction: the id/ino allocator,
/// the merge rule, the running totals and the blobs that need booking.
struct GraftCtx {
    next: i64,
    skip_existing: bool,
    stats: GraftStats,
    blobs: HashSet<String>,
    /// source `ino` -> destination `ino`: names that shared an inode in the
    /// source stay hard links of each other in the destination.
    inos: HashMap<i64, i64>,
}

const GRAFT_INSERT: &str = "INSERT INTO node
     (id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)";

/// `mkdir -p` inside the destination tree: walk `path` from the root, creating
/// any missing directory. Returns the directory's row id.
fn graft_ensure_dir(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
    ctx: &mut GraftCtx,
) -> Result<i64, String> {
    let mut cur = ROOT_INO;
    if path.is_empty() {
        return Ok(cur);
    }
    for comp in path.split('/') {
        let found: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id,is_dir FROM node WHERE parent_id=?1 AND name=?2",
                params![cur, comp],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("cannot read the destination tree: {e}"))?;
        match found {
            Some((id, is_dir)) if is_dir != 0 => cur = id,
            Some(_) => {
                return Err(format!(
                    "'{comp}' in the destination path is a file, not a directory"
                ))
            }
            None => {
                let id = ctx.next;
                ctx.next += 1;
                let t = now();
                tx.execute(
                    GRAFT_INSERT,
                    params![
                        id,
                        cur,
                        comp,
                        1,
                        None::<String>,
                        None::<String>,
                        0o0755,
                        crate::platform::getuid() as i64,
                        crate::platform::getgid() as i64,
                        t,
                        t,
                        t,
                        id
                    ],
                )
                .map_err(|e| format!("cannot create directory '{comp}' in the destination: {e}"))?;
                ctx.stats.nodes += 1;
                ctx.stats.dirs += 1;
                cur = id;
            }
        }
    }
    Ok(cur)
}

/// Copy one source entry (and, for a directory, its whole subtree) under
/// `parent` as `name`. Rows get fresh ids/ino numbers in the destination; the
/// source's bytes are referenced, never copied.
#[allow(clippy::too_many_arguments)]
fn graft_copy(
    tx: &rusqlite::Transaction<'_>,
    ins: &mut rusqlite::Statement<'_>,
    rows: &[GraftRow],
    children: &HashMap<i64, Vec<usize>>,
    idx: usize,
    parent: i64,
    name: &str,
    ctx: &mut GraftCtx,
) -> Result<(), String> {
    let r = &rows[idx];
    let existing: Option<(i64, i64)> = tx
        .query_row(
            "SELECT id,is_dir FROM node WHERE parent_id=?1 AND name=?2",
            params![parent, name],
            |x| Ok((x.get(0)?, x.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("cannot read the destination tree: {e}"))?;
    let dir_id = match existing {
        None => {
            let id = ctx.next;
            ctx.next += 1;
            let ino = *ctx.inos.entry(r.ino).or_insert(id);
            ins.execute(params![
                id,
                parent,
                name,
                r.is_dir,
                r.data_ref,
                r.link_target,
                r.mode,
                r.uid,
                r.gid,
                r.mtime,
                r.atime,
                r.ctime,
                ino
            ])
            .map_err(|e| format!("cannot graft '{name}': {e}"))?;
            ctx.stats.nodes += 1;
            if r.is_dir != 0 {
                ctx.stats.dirs += 1;
            }
            if let Some(dr) = &r.data_ref {
                ctx.blobs.insert(dr.clone());
            }
            if r.is_dir == 0 {
                return Ok(());
            }
            id
        }
        Some((id, is_dir)) => {
            if !ctx.skip_existing {
                return Err(format!(
                    "'{name}' already exists in the destination — rename it in the plan or pass --skip-existing"
                ));
            }
            if is_dir == 0 || r.is_dir == 0 {
                ctx.stats.skipped += 1;
                return Ok(());
            }
            // Both are directories: keep the one already there and merge the
            // missing children into it (re-running a curation script must not
            // fail on the skeleton it made last time).
            id
        }
    };
    if let Some(kids) = children.get(&r.id) {
        for k in kids {
            let child_name = rows[*k].name.clone();
            graft_copy(tx, ins, rows, children, *k, dir_id, &child_name, ctx)?;
        }
    }
    Ok(())
}

/// Graft a curated subset of `src_view`'s tree into the tree of `dst_view`: for
/// every [`GraftItem`], copy that entry (a directory brings its subtree) to
/// `dest/<dst>`, renaming it to the destination basename. Bytes are shared —
/// every distinct `data_ref` is booked in the cross-view ledger exactly like a
/// fork's, so the destination never owns a delete the source does not know
/// about and the object is freed only when no view names it.
///
/// All-or-nothing: the source is read through a separate read-only snapshot and
/// the whole plan lands in ONE `IMMEDIATE` transaction, so a bad line writes
/// nothing. Missing parent directories under `dest` are created (0755).
/// `skip_existing` turns a conflict into "leave what is there and keep going",
/// merging into an existing directory and skipping colliding files.
pub fn graft_view(
    base: &Path,
    dst_view: &str,
    src_view: &str,
    items: &[GraftItem],
    dest: &str,
    skip_existing: bool,
) -> Result<GraftStats, String> {
    if items.is_empty() {
        return Err("empty graft plan — nothing to copy".to_string());
    }
    let meta_dir = base.join("meta");
    let src_path = meta_dir.join(format!("{src_view}.db"));
    let dst_path = meta_dir.join(format!("{dst_view}.db"));
    if !src_path.is_file() {
        return Err(format!(
            "no source tree {} — a graft copies names out of a LOCAL SQLite view",
            src_path.display()
        ));
    }
    if !dst_path.is_file() {
        return Err(format!(
            "no destination tree {} — a graft lands in a BASE view or a fork (a lens/alias has no tree of its own)",
            dst_path.display()
        ));
    }
    let dest_prefix = if dest.trim().is_empty() {
        String::new()
    } else {
        normalize_plan_path(dest)?
    };
    let rows = read_graft_rows(&src_path)?;
    let root = rows
        .iter()
        .find(|r| r.parent_id.is_none())
        .map(|r| r.id)
        .ok_or_else(|| {
            format!(
                "source tree {} has no root row — refusing to graft from an empty store",
                src_path.display()
            )
        })?;
    let children = graft_children(&rows);
    // Resolve every source path BEFORE writing anything, so a typo on line 900
    // of a 100k-line plan cannot leave a half-grafted tree behind.
    let mut plan: Vec<(usize, String)> = Vec::with_capacity(items.len());
    for it in items {
        let idx = graft_resolve(&rows, &children, root, &it.src)
            .ok_or_else(|| format!("'{}' does not exist in view '{src_view}'", it.src))?;
        let full = if dest_prefix.is_empty() {
            it.dst.clone()
        } else {
            format!("{dest_prefix}/{}", it.dst)
        };
        plan.push((idx, full));
    }
    let eng = MetaEngine::open(&dst_path)
        .map_err(|e| format!("cannot open destination tree {}: {e}", dst_path.display()))?;
    let mut ctx = GraftCtx {
        next: 0,
        skip_existing,
        stats: GraftStats::default(),
        blobs: HashSet::new(),
        inos: HashMap::new(),
    };
    with_ledger_serial(|| -> Result<(), String> {
        let mut conn = eng.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("cannot start the graft transaction: {e}"))?;
        // Fresh ids/ino numbers start ABOVE both existing id space and existing
        // inode numbers, so a graft never collides with rows copied by an
        // earlier fork (whose inos came from another tree).
        let max_id: i64 = tx
            .query_row("SELECT IFNULL(MAX(id),0) FROM node", [], |r| r.get(0))
            .map_err(|e| format!("cannot size the destination tree: {e}"))?;
        let max_ino: i64 = tx
            .query_row("SELECT IFNULL(MAX(ino),0) FROM node", [], |r| r.get(0))
            .map_err(|e| format!("cannot size the destination tree: {e}"))?;
        ctx.next = max_id.max(max_ino) + 1;
        {
            let mut ins = tx
                .prepare(GRAFT_INSERT)
                .map_err(|e| format!("cannot prepare the destination tree: {e}"))?;
            for (idx, full) in &plan {
                let (parent_path, name) = match full.rfind('/') {
                    Some(k) => (&full[..k], &full[k + 1..]),
                    None => ("", full.as_str()),
                };
                let parent = graft_ensure_dir(&tx, parent_path, &mut ctx)?;
                graft_copy(
                    &tx, &mut ins, &rows, &children, *idx, parent, name, &mut ctx,
                )?;
            }
        }
        // Book the destination's hold on every blob it now names; if the SOURCE
        // view was never booked (a store predating the ledger and never
        // rebuilt), book it first — otherwise its last-name delete would free a
        // blob this graft still names.
        for dr in &ctx.blobs {
            let booked = tx
                .query_row(
                    &format!("SELECT 1 FROM {REFS_SCHEMA}.holders WHERE data_ref=?1 LIMIT 1"),
                    params![dr],
                    |_| Ok(()),
                )
                .is_ok();
            if !booked {
                ledger_add(&tx, dr)
                    .map_err(|e| format!("cannot book '{dr}' in the ledger: {e}"))?;
            }
            ledger_add(&tx, dr).map_err(|e| format!("cannot book '{dr}' in the ledger: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("cannot commit the graft into {}: {e}", dst_path.display()))?;
        Ok(())
    })?;
    ctx.stats.blobs = ctx.blobs.len() as i64;
    eng.drop_cursors();
    Ok(ctx.stats)
}

/// Portable METADATA export: the node rows of one view, one JSON object per
/// line, header first. Content bytes are deliberately NOT included — a node
/// references its blob by `data_ref` only, and the on-disk location is derived
/// from that key (FNV-1a bucket + key), so copying the store's `blocks/` tree
/// to another machine preserves every reference. No host path, device id,
/// hostname or absolute path is stored in the export.
pub const VIEW_EXPORT_FORMAT: &str = "grovefs-view-metadata";
pub const VIEW_EXPORT_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewExportStats {
    pub nodes: i64,
    pub blobs: i64,
    pub kind: String,
}

fn node_row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "parent_id": r.get::<_, Option<i64>>(1)?,
        "name": r.get::<_, String>(2)?,
        "is_dir": r.get::<_, i64>(3)?,
        "data_ref": r.get::<_, Option<String>>(4)?,
        "link_target": r.get::<_, Option<String>>(5)?,
        "mode": r.get::<_, i64>(6)?,
        "uid": r.get::<_, i64>(7)?,
        "gid": r.get::<_, i64>(8)?,
        "mtime": r.get::<_, i64>(9)?,
        "atime": r.get::<_, i64>(10)?,
        "ctime": r.get::<_, i64>(11)?,
        "ino": r.get::<_, i64>(12)?,
    }))
}

/// Export one view's metadata tree to `out` as line-delimited JSON (header +
/// one object per node, ordered by id). The source is read through a separate
/// READ-ONLY connection inside one snapshot transaction, so a concurrent
/// writer yields a consistent point-in-time copy (WAL readers never block it).
/// `kind` is `fork` when a `fork <base>` definition slot exists, `physical`
/// otherwise; virtual names have no tree of their own and are refused.
pub fn export_view(
    base: &Path,
    view: &str,
    out: &Path,
    force: bool,
) -> Result<ViewExportStats, String> {
    let src_path = base.join("meta").join(format!("{view}.db"));
    if !src_path.is_file() {
        return Err(format!(
            "no metadata tree {} — '{view}' is not a physical tree (an alias or derived view has only a definition file under meta/views/; a PostgreSQL-backed view lives in Postgres)",
            src_path.display()
        ));
    }
    if out.exists() && !force {
        return Err(format!(
            "output {} already exists — pass --force to overwrite it",
            out.display()
        ));
    }
    let def_path = base
        .join("meta")
        .join("views")
        .join(format!("{view}.layout"));
    let definition = std::fs::read_to_string(&def_path).ok();
    let kind = match definition.as_deref() {
        Some(t) if t.lines().any(|l| l.trim_start().starts_with("fork ")) => "fork",
        _ => "physical",
    };
    // Write beside the destination then rename, so an interrupted export never
    // leaves a half-written file at the requested path.
    let mut tmp = out.to_path_buf();
    let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("export");
    tmp.set_file_name(format!(".{name}.tmp-{}", std::process::id()));

    let src = Connection::open_with_flags(&src_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open tree {}: {e}", src_path.display()))?;
    let _ = src.execute_batch("PRAGMA busy_timeout=5000;");
    src.execute_batch("BEGIN")
        .map_err(|e| format!("cannot start export snapshot: {e}"))?;
    let counts: (i64, i64) = src
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT data_ref) FROM node",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("cannot count tree {}: {e}", src_path.display()))?;
    let write_result = (|| -> Result<(), String> {
        let f = File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        let mut w = BufWriter::new(f);
        let header = json!({
            "format": VIEW_EXPORT_FORMAT,
            "version": VIEW_EXPORT_VERSION,
            "view": view,
            "kind": kind,
            "definition": definition,
            "exported_at": now(),
            "nodes": counts.0,
            "blobs": counts.1,
        });
        writeln!(w, "{header}").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        let mut stmt = src
            .prepare(
                "SELECT id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino
                 FROM node ORDER BY id",
            )
            .map_err(|e| format!("cannot read tree {}: {e}", src_path.display()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| format!("cannot read tree {}: {e}", src_path.display()))?;
        while let Some(r) = rows
            .next()
            .map_err(|e| format!("cannot read tree {}: {e}", src_path.display()))?
        {
            let row = node_row_json(r)
                .map_err(|e| format!("cannot read tree {}: {e}", src_path.display()))?;
            writeln!(w, "{row}").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        }
        w.flush()
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        w.get_ref()
            .sync_all()
            .map_err(|e| format!("cannot flush {}: {e}", tmp.display()))?;
        Ok(())
    })();
    let _ = src.execute_batch("COMMIT");
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot move export into {}: {e}", out.display())
    })?;
    Ok(ViewExportStats {
        nodes: counts.0,
        blobs: counts.1,
        kind: kind.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewImportStats {
    pub nodes: i64,
    pub blobs: i64,
    pub kind: String,
    pub replaced: bool,
}

/// Import a metadata tree written by [`export_view`] as `<base>/meta/<view>.db`.
/// The target must not be governed by a definition; an existing tree is only
/// overwritten with `replace` (and every mount/meta-server using the store must
/// be stopped first). Rows are inserted in ONE transaction with their original
/// ids/inodes, so hard links stay hard links, then the cross-view ledger is
/// rebuilt from the trees that actually exist. Content bytes are not touched:
/// the caller must have copied the referenced blobs into the target store.
pub fn import_view(
    base: &Path,
    view: &str,
    input: &Path,
    replace: bool,
) -> Result<ViewImportStats, String> {
    let meta_dir = base.join("meta");
    std::fs::create_dir_all(&meta_dir)
        .map_err(|e| format!("cannot create {}: {e}", meta_dir.display()))?;
    let dst = meta_dir.join(format!("{view}.db"));
    let f = File::open(input).map_err(|e| format!("cannot open {}: {e}", input.display()))?;
    let mut rdr = BufReader::new(f);
    let mut header_line = String::new();
    if rdr
        .read_line(&mut header_line)
        .map_err(|e| format!("cannot read {}: {e}", input.display()))?
        == 0
    {
        return Err(format!(
            "{} is empty — not a grovefs view export",
            input.display()
        ));
    }
    if header_line.len() > (1 << 20) {
        return Err(format!("{}:1: header line too long", input.display()));
    }
    let header: Value = serde_json::from_str(header_line.trim_end()).map_err(|e| {
        format!(
            "{}:1: not a grovefs view export header ({e})",
            input.display()
        )
    })?;
    if header["format"] != VIEW_EXPORT_FORMAT {
        return Err(format!(
            "{}:1: unknown format '{}' (expected '{VIEW_EXPORT_FORMAT}')",
            input.display(),
            header["format"].as_str().unwrap_or("<missing>")
        ));
    }
    let version = header["version"].as_u64().unwrap_or(0);
    if version != VIEW_EXPORT_VERSION {
        return Err(format!(
            "{}:1: unsupported export version {version} (this build reads {VIEW_EXPORT_VERSION})",
            input.display()
        ));
    }
    let claimed_nodes = header["nodes"].as_i64();
    let claimed_blobs = header["blobs"].as_i64();
    let kind = header["kind"].as_str().unwrap_or("physical").to_string();
    let replaced = dst.exists();
    if replaced && !replace {
        return Err(format!(
            "target metadata tree {} already exists — pass --replace to overwrite it (stop every mount/meta-server using this store first)",
            dst.display()
        ));
    }
    // Build BESIDE the destination and rename at the very end: a corrupt export
    // (or a crash mid-import) never destroys an existing tree and never leaves a
    // half-written one behind. The temp name does not end in `.db`, so
    // `rebuild_ledger` and every view lister ignore it.
    let tmp = meta_dir.join(format!(".{view}.import-{}-{}", std::process::id(), now()));
    let side = |p: &std::path::Path, ext: &str| -> std::path::PathBuf {
        let mut s = p.as_os_str().to_os_string();
        s.push(ext);
        std::path::PathBuf::from(s)
    };
    let scratch = |ext: &str| side(&tmp, ext);
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(scratch(ext));
    }
    let built = (|| -> Result<(i64, i64), String> {
        let eng = MetaEngine::open(&tmp)
            .map_err(|e| format!("cannot build target tree {}: {e}", tmp.display()))?;
        let res = with_ledger_serial(|| -> Result<(i64, i64), String> {
            let mut conn = eng.conn.lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| format!("cannot start import transaction: {e}"))?;
            tx.execute("DELETE FROM node", [])
                .map_err(|e| format!("cannot clear the target tree: {e}"))?;
            let mut nodes = 0i64;
            let mut blobs: HashSet<String> = HashSet::new();
            {
                let mut ins = tx
                .prepare(
                    "INSERT INTO node
                     (id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                )
                .map_err(|e| format!("cannot prepare the target tree: {e}"))?;
                let mut line = String::new();
                let mut lineno = 1usize;
                loop {
                    line.clear();
                    let n = rdr
                        .read_line(&mut line)
                        .map_err(|e| format!("cannot read {}: {e}", input.display()))?;
                    if n == 0 {
                        break;
                    }
                    lineno += 1;
                    if line.len() > (4 << 20) {
                        return Err(format!("{}:{lineno}: line too long", input.display()));
                    }
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: Value = serde_json::from_str(line.trim_end())
                        .map_err(|e| format!("{}:{lineno}: invalid JSON: {e}", input.display()))?;
                    let id = v["id"].as_i64().ok_or_else(|| {
                        format!("{}:{lineno}: missing integer \"id\"", input.display())
                    })?;
                    let parent_id = match v.get("parent_id") {
                        None | Some(Value::Null) => None,
                        Some(x) => Some(x.as_i64().ok_or_else(|| {
                            format!(
                                "{}:{lineno}: \"parent_id\" must be an integer or null",
                                input.display()
                            )
                        })?),
                    };
                    let name = v["name"].as_str().ok_or_else(|| {
                        format!("{}:{lineno}: missing string \"name\"", input.display())
                    })?;
                    if name.is_empty() {
                        return Err(format!("{}:{lineno}: empty name", input.display()));
                    }
                    let is_dir = v["is_dir"].as_i64().ok_or_else(|| {
                        format!("{}:{lineno}: missing integer \"is_dir\"", input.display())
                    })?;
                    if is_dir != 0 && is_dir != 1 {
                        return Err(format!(
                            "{}:{lineno}: \"is_dir\" must be 0 or 1, got {is_dir}",
                            input.display()
                        ));
                    }
                    let data_ref = v["data_ref"].as_str().map(str::to_string);
                    let link_target = v["link_target"].as_str().map(str::to_string);
                    let mode =
                        v["mode"]
                            .as_i64()
                            .unwrap_or(if is_dir == 1 { 0o755 } else { 0o644 });
                    let uid = v["uid"].as_i64().unwrap_or(0);
                    let gid = v["gid"].as_i64().unwrap_or(0);
                    let t = now();
                    let mtime = v["mtime"].as_i64().unwrap_or(t);
                    let atime = v["atime"].as_i64().unwrap_or(mtime);
                    let ctime = v["ctime"].as_i64().unwrap_or(mtime);
                    let ino = v["ino"].as_i64().ok_or_else(|| {
                        format!("{}:{lineno}: missing integer \"ino\"", input.display())
                    })?;
                    ins.execute(params![
                        id,
                        parent_id,
                        name,
                        is_dir,
                        data_ref,
                        link_target,
                        mode,
                        uid,
                        gid,
                        mtime,
                        atime,
                        ctime,
                        ino
                    ])
                    .map_err(|e| {
                        format!("{}:{lineno}: cannot insert node {id}: {e}", input.display())
                    })?;
                    if let Some(dr) = &data_ref {
                        blobs.insert(dr.clone());
                    }
                    nodes += 1;
                }
            }
            let root_rows: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM node WHERE parent_id IS NULL",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| format!("cannot validate the imported tree: {e}"))?;
            if root_rows != 1 {
                return Err(format!(
                "import rejected: expected exactly one root row (parent_id NULL), found {root_rows}"
            ));
            }
            let (root_id, root_is_dir): (i64, i64) = tx
                .query_row(
                    "SELECT id, is_dir FROM node WHERE parent_id IS NULL",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| format!("cannot validate the imported tree: {e}"))?;
            if root_id != ROOT_INO || root_is_dir != 1 {
                return Err(format!(
                "import rejected: root row must be id {ROOT_INO} and a directory (got id {root_id}, is_dir {root_is_dir})"
            ));
            }
            let dangling: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM node c LEFT JOIN node p ON c.parent_id=p.id
                 WHERE c.parent_id IS NOT NULL AND (p.id IS NULL OR p.is_dir=0)",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| format!("cannot validate the imported tree: {e}"))?;
            if dangling != 0 {
                return Err(format!(
                "import rejected: {dangling} node(s) point at a missing or non-directory parent"
            ));
            }
            if let Some(c) = claimed_nodes {
                if c != nodes {
                    return Err(format!(
                        "import rejected: header declares {c} node(s), file carries {nodes}"
                    ));
                }
            }
            if let Some(c) = claimed_blobs {
                if c != blobs.len() as i64 {
                    return Err(format!(
                        "import rejected: header declares {c} blob reference(s), file carries {}",
                        blobs.len()
                    ));
                }
            }
            tx.commit()
                .map_err(|e| format!("cannot commit the imported tree: {e}"))?;
            Ok((nodes, blobs.len() as i64))
        });
        // Fold the WAL back into the main file before the tree is moved into
        // place, so the rename carries a self-contained database.
        let _ = eng
            .conn
            .lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        drop(eng);
        res
    })();
    let (nodes, blobs) = match built {
        Ok(v) => v,
        Err(e) => {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(scratch(ext));
            }
            return Err(e);
        }
    };
    // Install: drop the OLD sidecars first, then let `rename` atomically replace
    // the old db — at every instant either the old tree or the new one is in
    // place, never a mixture and never nothing.
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(side(&dst, ext));
    }
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(scratch(ext));
        }
        return Err(format!(
            "cannot install imported tree {}: {e}",
            dst.display()
        ));
    }
    // A TRUNCATE checkpoint normally removes the sidecars; carry any that
    // survive along so the db keeps its WAL pair.
    for ext in ["-wal", "-shm"] {
        if scratch(ext).exists() {
            let _ = std::fs::rename(scratch(ext), side(&dst, ext));
        }
    }
    rebuild_ledger(base)
        .map_err(|e| format!("imported the tree but cannot rebuild {}: {e}", REFS_DB))?;
    Ok(ViewImportStats {
        nodes,
        blobs,
        kind,
        replaced,
    })
}

/// Rewrite the ledger from the trees that actually exist: `data_ref` -> the
/// number of DISTINCT view dbs holding a name for it. Backs `grovefs refs
/// rebuild` after out-of-band view-db removal (the only over-count source).
pub fn rebuild_ledger(base: &Path) -> rusqlite::Result<usize> {
    let meta_dir = base.join("meta");
    let mut views: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&meta_dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            // Control-plane dbs never carry node rows; skip them and the ledger
            // itself (a stray match would just count zero refs anyway).
            if name.ends_with(".db") && name != REFS_DB && name != "api_keys.db" {
                views.push(p);
            }
        }
    }
    // If this fails the very next `Connection::open` surfaces the real error.
    let _ = std::fs::create_dir_all(&meta_dir);
    let mut conn = Connection::open(meta_dir.join(REFS_DB))?;
    conn.execute_batch(
        "PRAGMA busy_timeout=5000;
         PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS holders (
             data_ref TEXT PRIMARY KEY,
             holders  INTEGER NOT NULL
         );",
    )?;
    // One IMMEDIATE tx over the whole rewrite: concurrent ledger writers queue
    // on the write lock, so an add landing during the rebuild is not wiped.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM holders", [])?;
    let mut counted = 0usize;
    {
        // Scoped so `stmt`'s borrow of `tx` ends before `tx.commit()` moves it.
        let mut stmt = tx.prepare(
            "INSERT INTO holders (data_ref, holders) VALUES (?1, 1)
             ON CONFLICT(data_ref) DO UPDATE SET holders = holders + 1",
        )?;
        for vp in &views {
            let ro =
                Connection::open_with_flags(vp, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(|c| {
                    c.execute_batch("PRAGMA busy_timeout=5000;")?;
                    Ok(c)
                });
            let ro = match ro {
                Ok(c) => c,
                // A half-written/locked sibling db: skip it rather than fail the
                // whole rebuild (it simply won't count as a holder this round).
                Err(_) => continue,
            };
            let mut q =
                ro.prepare("SELECT DISTINCT data_ref FROM node WHERE data_ref IS NOT NULL")?;
            let refs: Vec<String> = q
                .query_map([], |r| r.get::<_, String>(0))?
                .filter_map(|x| x.ok())
                .collect();
            for dr in refs {
                stmt.execute(params![dr])?;
                counted += 1;
            }
        }
    }
    tx.commit()?;
    Ok(counted)
}

/// Stats for a whole-base-view removal: what the tree held and what died with it.
#[derive(Debug)]
pub struct BaseViewRemoval {
    /// Node rows (names) that were in the tree.
    pub nodes: i64,
    /// DISTINCT blob references the tree held (striped `s<N>:<stripe>:<uuid>`
    /// keys included — the caller decides what a given backend can free).
    pub blob_refs: Vec<String>,
}

/// Remove a physical BASE view's metadata tree from a local store: collect its
/// blob references first (the tree is the only ledger of what it held), delete
/// `meta/<view>.db` (+ WAL/SHM), then rebuild the cross-view ledger from the
/// surviving trees so the dead tree's holds are gone. Returns the refs the tree
/// held — a blob stays alive iff the rebuilt ledger still lists it; freeing the
/// unreferenced ones is the caller's job through a `StorageBackend` (the key→
/// path layout is backend-owned and offline here we only know the store root).
/// A name governed by a definition (derived/alias/fork slot) is refused: delete
/// the definition with `view rm` first, so the shared file slot is never
/// double-interpreted.
pub fn remove_base_view(base: &Path, view: &str) -> Result<BaseViewRemoval, String> {
    if !crate::layout::valid_view_name(view) {
        return Err(format!("invalid view name '{view}'"));
    }
    let meta_dir = base.join("meta");
    let db = meta_dir.join(format!("{view}.db"));
    if !db.is_file() {
        return Err(format!(
            "no physical base tree {} — '{view}' is either a governed name (see `grovefs view rm` WITHOUT --base), a name this store never had a tree for, or a tree this store does not hold",
            db.display()
        ));
    }
    if meta_dir
        .join("views")
        .join(format!("{view}.layout"))
        .is_file()
    {
        return Err(format!(
            "'{view}' is governed by a view definition — remove the definition first (`grovefs view rm --name {view}`) before removing a base tree of the same name"
        ));
    }
    // Read the tree's blob set BEFORE deleting: once the db is gone there is no
    // record of what it referenced (same reason `view export` must run while the
    // tree exists).
    let (nodes, blob_refs) = {
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("cannot open {}: {e}", db.display()))?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")
            .map_err(|e| format!("cannot lock {}: {e}", db.display()))?;
        let nodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM node", [], |r| r.get(0))
            .unwrap_or(0);
        let mut stmt = conn
            .prepare("SELECT DISTINCT data_ref FROM node WHERE data_ref IS NOT NULL")
            .map_err(|e| format!("cannot read blob references: {e}"))?;
        let refs: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("cannot read blob references: {e}"))?
            .filter_map(|x| x.ok())
            .collect();
        (nodes, refs)
    }; // read-only handle dropped before the delete
    for ext in ["", "-wal", "-shm"] {
        let p = meta_dir.join(format!("{view}.db{ext}"));
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| format!("cannot delete {}: {e}", p.display()))?;
        }
    }
    // The dead tree's holds must die with it: rewrite the ledger from the
    // surviving trees so a blob it shared with no other view reads holders 0.
    rebuild_ledger(base)
        .map_err(|e| format!("tree removed, but the ledger rebuild failed: {e}"))?;
    Ok(BaseViewRemoval { nodes, blob_refs })
}

/// Does the freshly rebuilt ledger still hold `dr`? A ref absent from
/// `holders` has no surviving view name → its blob may be freed (same
/// exists-check idiom as [`other_view_holds`]). Public so the CLI (which owns
/// the backend) can decide per ref after [`remove_base_view`].
pub fn ledger_holds(base: &Path, dr: &str) -> rusqlite::Result<bool> {
    let conn = Connection::open_with_flags(
        base.join("meta").join(REFS_DB),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    let found = conn
        .query_row(
            "SELECT 1 FROM holders WHERE data_ref=?1 LIMIT 1",
            params![dr],
            |_| Ok(()),
        )
        .is_ok();
    Ok(found)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct MetaEngine {
    // A single connection, serialized behind a fast parking_lot mutex. The fuser
    // high-level session is already single-threaded, so this lock mainly guards
    // direct/concurrent callers and keeps each logical metadata op atomic.
    // Cross-process sharing (the *same* view mounted by two processes) is handled
    // by SQLite WAL plus the `busy_timeout` set in `open`, so a contending writer
    // waits for the lock instead of failing with SQLITE_BUSY (which would surface
    // to FUSE as EIO).
    conn: parking_lot::Mutex<Connection>,
    /// Pooled read-only connections (opened read-write to sidestep WAL `-shm`
    /// restrictions on strictly read-only handles, but only ever used for
    /// SELECT) so concurrent metadata reads fan out instead of serializing
    /// behind the single writer's `conn` mutex.
    read_pool: parking_lot::Mutex<Vec<Connection>>,
    /// DB path, kept so the pool can open extra read connections when drained.
    path: std::path::PathBuf,
    /// Present only on a *writable* engine: the shutdown handle for this view's
    /// background WAL-checkpoint thread. `None` for a read-only/standby open
    /// (which never commits and must not spawn a thread). Dropped in
    /// [`MetaEngine::drop`], which flips the flag and wakes the thread so it
    /// exits within one interval. The thread holds only an `Arc` clone of this
    /// controller — never of `MetaEngine` — so there is no reference cycle.
    ckpt: Option<Arc<CheckpointCtl>>,
    /// Keyset cursors for sequential READDIR paging: `(parent, next offset)` ->
    /// the last name the previous page returned. The next page then runs
    /// `name > last` instead of `LIMIT/OFFSET`, whose OFFSET re-walks every row
    /// it skips and turns a sequential walk of a big directory into O(n^2).
    /// Any name mutation clears the map; a miss falls back to the OFFSET query,
    /// so `seekdir` and out-of-order resumes stay correct.
    cursors: parking_lot::Mutex<HashMap<(i64, usize), String>>,
}

/// Shutdown signal for the per-view WAL-checkpoint background thread: a boolean
/// guarded by a std mutex with a condvar so `Drop` can wake it immediately
/// rather than waiting out the sleep interval. Kept separate from the engine's
/// `parking_lot` writer lock; this lock is cold-path only (thread start/stop).
struct CheckpointCtl {
    stopped: StdMutex<bool>,
    wake: Condvar,
}

impl CheckpointCtl {
    /// Tell the checkpoint thread to exit at its next wake-up and nudge it now.
    fn shutdown(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}

/// How often a writable view engine runs a background `wal_checkpoint(PASSIVE)`.
/// A PASSIVE checkpoint never blocks the writer or readers; doing it here keeps
/// the WAL short so the writer's *inline* auto-checkpoint (a synchronous WAL
/// flush that showed up as a ~125 ms outlier on `create`) essentially never
/// fires on the hot path — the stall moves off the user's metadata write.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

/// Number of pooled read connections opened per view engine for concurrent
/// metadata reads.
pub const READ_POOL_SIZE: usize = 4;
/// Ceiling on the keyset cursors kept for READDIR paging. One checkpoint per
/// [`READDIR_CURSOR_STRIDE`] rows per open directory; a walk of ten million
/// entries needs ~300k, so this only trips under pathological seekdir churn,
/// where clearing is harmless (the next page just falls back to OFFSET once).
const READDIR_CURSOR_CAP: usize = 65_536;
/// Granularity of the READDIR resume checkpoints. The kernel resumes a listing
/// at the entry after the last one its reply buffer accepted — an offset that
/// almost never coincides with one of our own page boundaries (a 4 KiB reply
/// holds a few hundred entries, our fetch ladder 32..1024). Recording a cursor
/// only at page ends therefore left one OFFSET re-scan per reply; recording it
/// every STRIDE rows lets any resume be served by a keyset seek plus at most
/// STRIDE-1 rows skipped in memory.
const READDIR_CURSOR_STRIDE: usize = 32;

/// Borrows one read-pool connection; returns it on drop (bounded). Derefs to
/// `Connection` so callers use it exactly like the writer guard.
struct ReadGuard<'a> {
    conn: Option<Connection>,
    eng: &'a MetaEngine,
}
impl std::ops::Deref for ReadGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("read conn alive until drop")
    }
}
impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let mut pool = self.eng.read_pool.lock();
            if pool.len() < READ_POOL_SIZE {
                pool.push(c);
            }
        }
    }
}

/// The columns returned by every node SELECT, in order.
const COLS: &str =
    "id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino";

/// Run `SELECT <keycol>, COUNT(*) FROM node WHERE <keycol> IN (<keys>) <tail>
/// GROUP BY <keycol>` over `keys`, folding each `key -> count` into `out`. The
/// key list is chunked so a statement never exceeds SQLite's bound-variable
/// limit (a full-directory `readdir` can carry far more ids than that). `tail`
/// is a fixed, trusted SQL fragment (e.g. `AND is_dir=1`), never user input.
/// Keys with no matching rows are absent from `out`; the caller applies the
/// per-field default (0 sub-dirs, or a floor of 1 hard link).
fn grouped_counts(
    conn: &Connection,
    keycol: &str,
    tail: &str,
    keys: &[i64],
    out: &mut HashMap<i64, i64>,
) {
    const CHUNK: usize = 500;
    for chunk in keys.chunks(CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {keycol}, COUNT(*) FROM node WHERE {keycol} IN ({placeholders}) {tail} GROUP BY {keycol}"
        );
        let args: Vec<&dyn rusqlite::types::ToSql> = chunk
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        if let Ok(mut stmt) = conn.prepare_cached(&sql) {
            if let Ok(rows) = stmt.query_map(args.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }) {
                for row in rows.flatten() {
                    out.insert(row.0, row.1);
                }
            }
        }
    }
}

/// Stop the background WAL-checkpoint thread when the engine goes away (view
/// unmounted / hot-reloaded / test teardown). Detached: we signal stop and
/// wake it rather than join, so no metadata path ever blocks on shutdown; the
/// thread exits within one [`CHECKPOINT_INTERVAL`] (or immediately on the wake).
impl Drop for MetaEngine {
    fn drop(&mut self) {
        if let Some(ctl) = self.ckpt.take() {
            ctl.shutdown();
        }
    }
}

impl MetaEngine {
    /// Open (and if necessary initialize) the view database for a view.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::open_ex(path, false)
    }

    /// Open the view database, optionally as strictly read-only (`readonly`),
    /// which is how a HA *standby* meta-server serves its replicated copies:
    /// SQLite refuses any write at the API level, so a mistaken write-intent
    /// on a standby is impossible rather than merely discouraged. A read-only
    /// open never runs the schema-creation/migration statements (they would
    /// fail); the replicated copy already carries the schema.
    pub fn open_ex(path: &Path, readonly: bool) -> rusqlite::Result<Self> {
        let conn = if readonly {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            Connection::open(path)
        }?;
        if readonly {
            let _ = conn.execute_batch("PRAGMA busy_timeout=5000;");
            // Best-effort ledger attach (no DDL — the file must already exist):
            // a standby never writes, and its cross-view hold checks simply read
            // "no other view" when the sibling refs.db is absent.
            if path
                .parent()
                .map(|d| d.join(REFS_DB).is_file())
                .unwrap_or(false)
            {
                let _ = attach_refs_ledger(&conn, path);
            }
            return Ok(Self {
                conn: parking_lot::Mutex::new(conn),
                read_pool: parking_lot::Mutex::new(Vec::new()),
                path: path.to_path_buf(),
                ckpt: None,
                cursors: parking_lot::Mutex::new(HashMap::new()),
            });
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA wal_autocheckpoint=1000;
             CREATE TABLE IF NOT EXISTS node (
                 id           INTEGER PRIMARY KEY,
                 parent_id    INTEGER,
                 name         TEXT NOT NULL,
                 is_dir       INTEGER NOT NULL,
                 data_ref     TEXT,
                 link_target  TEXT,
                 mode         INTEGER NOT NULL,
                 uid          INTEGER NOT NULL,
                 gid          INTEGER NOT NULL,
                 mtime        INTEGER NOT NULL,
                 atime        INTEGER NOT NULL,
                 ctime        INTEGER NOT NULL,
                 ino          INTEGER,
                 UNIQUE(parent_id, name)
             );
             CREATE INDEX IF NOT EXISTS idx_node_parent ON node(parent_id);
             -- Directory nlink (2 + subdirs) is a COUNT over the parent's rows
             -- filtered by is_dir; with only idx_node_parent SQLite walks every
             -- child row to test is_dir (9.6ms for 100k children -> once per
             -- READDIRPLUS buffer). Carrying is_dir in the key makes it an
             -- index-only scan (0.02ms).
             CREATE INDEX IF NOT EXISTS idx_node_parent_kind ON node(parent_id, is_dir);
             CREATE INDEX IF NOT EXISTS idx_node_dataref ON node(data_ref);
             -- FUSE `getattr` and every setattr-path UPDATE key on `ino`, not
             -- the rowid `id`; without this index each is a full table scan.
             -- This is the single hottest metadata read.
             CREATE INDEX IF NOT EXISTS idx_node_ino ON node(ino);
             CREATE TABLE IF NOT EXISTS ctrl (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS locks (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 ino         INTEGER NOT NULL,
                 owner       INTEGER NOT NULL,
                 sid         INTEGER NOT NULL DEFAULT 0,
                 l_start     INTEGER NOT NULL,
                 l_end       INTEGER NOT NULL,
                 pid         INTEGER NOT NULL,
                 ltype       INTEGER NOT NULL,
                 expires_at  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_locks_ino    ON locks(ino, l_start, l_end);
             CREATE INDEX IF NOT EXISTS idx_locks_owner  ON locks(owner);
             CREATE INDEX IF NOT EXISTS idx_locks_sid    ON locks(sid);
             CREATE INDEX IF NOT EXISTS idx_locks_expire ON locks(expires_at);",
        )?;
        // Attach the shared cross-view holder ledger (sibling refs.db) so every
        // IMMEDIATE tx below takes its write lock FIRST (uniform global lock
        // order, no cycles) and books adds/removes inside the same commit as
        // the node rows they describe. See the ledger section at the top.
        attach_refs_ledger(&conn, path)?;

        let has_root: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node WHERE id = ?1",
                params![ROOT_INO],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_root == 0 {
            let t = now();
            conn.execute(
                "INSERT INTO node (id,parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
                 VALUES (?1,NULL,'/',1,NULL,NULL,?2,?3,?4,?5,?5,?5,?1)",
                params![ROOT_INO, 0o0755u32, crate::platform::getuid(), crate::platform::getgid(), t],
            )?;
        }
        let mut pool = Vec::new();
        for _ in 0..READ_POOL_SIZE {
            if let Ok(rc) = Connection::open(path) {
                let _ = rc.execute_batch("PRAGMA busy_timeout=5000;");
                pool.push(rc);
            }
        }
        // Background WAL checkpoint (see CHECKPOINT_INTERVAL). Only when the
        // dedicated thread actually starts do we disable the writer's inline
        // auto-checkpoint; if spawning fails we leave the default in place so
        // the WAL still gets bounded by opportunistic inline passes.
        let ckpt = Self::spawn_checkpoint_thread(path, &conn);
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
            read_pool: parking_lot::Mutex::new(pool),
            path: path.to_path_buf(),
            ckpt,
            cursors: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Spawn this view's WAL-checkpoint thread. On success it opens its own
    /// connection, drives `wal_checkpoint(PASSIVE)` every [`CHECKPOINT_INTERVAL`]
    /// (and a `TRUNCATE` every ~30 intervals to shrink the `-wal` file), and
    /// returns the shutdown handle while disabling the writer's inline
    /// auto-checkpoint (the ~125 ms `create` tail this removes). On any failure
    /// it returns `None` and the writer keeps its default auto-checkpoint.
    fn spawn_checkpoint_thread(path: &Path, writer: &Connection) -> Option<Arc<CheckpointCtl>> {
        let ctl = Arc::new(CheckpointCtl {
            stopped: StdMutex::new(false),
            wake: Condvar::new(),
        });
        let db_path = path.to_path_buf();
        let thread_ctl = Arc::clone(&ctl);
        // Handshake: the thread reports whether it managed to open its own
        // connection. `thread::spawn` returning Ok only proves the OS thread
        // started, NOT that `Connection::open` inside it will succeed — and a
        // failed open would leave the thread exiting without ever checkpointing
        // while we had already disabled the writer's inline auto-checkpoint (an
        // unbounded-growth gap, the opposite of the comment's promise). So we
        // only disable inline checkpointing once the thread confirms readiness.
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let started = std::thread::Builder::new()
            .name("grove-wal-ckpt".to_string())
            .spawn(move || {
                let cc = match Connection::open(&db_path) {
                    Ok(c) => c,
                    // Cannot open: tell the parent to keep inline checkpointing,
                    // then bow out. The WAL stays bounded by opportunistic passes.
                    Err(_) => {
                        let _ = tx.send(false);
                        return;
                    }
                };
                // Connection is live and this thread will now own the checkpoint
                // cadence — green-light the parent to disable inline checkpointing.
                let _ = tx.send(true);
                let _ = cc.execute_batch("PRAGMA busy_timeout=5000;");
                let mut pass: u32 = 0;
                loop {
                    // Park for up to one interval, but wake immediately when the
                    // engine is dropped (stop flag set) so shutdown is prompt.
                    let guard = thread_ctl.stopped.lock().unwrap_or_else(|e| e.into_inner());
                    let (guard, _) = thread_ctl
                        .wake
                        .wait_timeout_while(guard, CHECKPOINT_INTERVAL, |stopped| !*stopped)
                        .unwrap_or_else(|e| e.into_inner());
                    if *guard {
                        break; // engine dropped -> exit
                    }
                    drop(guard);
                    pass += 1;
                    // TRUNCATE (reclaims disk) every ~30s, PASSIVE (non-blocking)
                    // otherwise. Both are safe under WAL and never block readers.
                    let sql = if pass.is_multiple_of(30) {
                        "PRAGMA wal_checkpoint(TRUNCATE);"
                    } else {
                        "PRAGMA wal_checkpoint(PASSIVE);"
                    };
                    let _ = cc.execute_batch(sql);
                }
            });
        match started {
            // Wait (bounded) for the thread's open result. On the normal path it
            // opens in microseconds, so this returns immediately; we only stall
            // toward the timeout when the open genuinely fails/hangs, in which
            // case keeping the writer's inline auto-checkpoint is the safe choice
            // (a late-starting thread would merely run idempotent PASSIVE passes
            // alongside inline ones — never a gap).
            Ok(_) => {
                if rx.recv_timeout(CHECKPOINT_INTERVAL).unwrap_or(false) {
                    // The thread is alive and will keep the WAL small: disable
                    // the writer's inline auto-checkpoint so it never stalls a
                    // commit (this is what removes the ~125 ms `create` tail).
                    let _ = writer.execute_batch("PRAGMA wal_autocheckpoint=0;");
                    Some(ctl)
                } else {
                    // Thread couldn't open its connection (or panicked before
                    // reporting): keep the writer's default auto-checkpoint so
                    // the WAL is still bounded inline. No orphan-thread gap.
                    None
                }
            }
            Err(_) => None,
        }
    }

    /// Borrow a pooled read connection for one SELECT-only op; falls back to a
    /// lazily-opened connection when the pool is drained (returned on drop).
    fn read_conn(&self) -> ReadGuard<'_> {
        let c = self.read_pool.lock().pop().unwrap_or_else(|| {
            stats::count(stats::Counter::PoolLazyOpen, 1);
            let rc = Connection::open(&self.path).expect("open read connection");
            let _ = rc.execute_batch("PRAGMA busy_timeout=5000;");
            rc
        });
        ReadGuard {
            conn: Some(c),
            eng: self,
        }
    }

    /// Resolve (parent, name) -> node. This is the FUSE `lookup` hot path.
    /// NOTE: DROP the lock before `finalize` (which re-locks) — parking_lot's
    /// Mutex is not reentrant, so holding it across the call would deadlock.
    pub fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaLookup);
        let sql = format!("SELECT {COLS} FROM node WHERE parent_id=?1 AND name=?2");
        let raw = {
            let _r = stats::start(stats::Stage::SqliteRead);
            let conn = self.read_conn();
            let mut stmt = conn.prepare_cached(&sql).ok()?;
            stmt.query_row(params![parent, name], row_to_raw).ok()
        };
        raw.map(|r| self.finalize(r))
    }

    /// Fetch a node by inode number (FUSE `getattr`). Any name sharing the
    /// inode resolves to the same attributes.
    pub fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaGetattr);
        let sql = format!("SELECT {COLS} FROM node WHERE ino=?1 LIMIT 1");
        let raw = {
            let _r = stats::start(stats::Stage::SqliteRead);
            let conn = self.read_conn();
            let mut stmt = conn.prepare_cached(&sql).ok()?;
            stmt.query_row(params![ino], row_to_raw).ok()
        };
        raw.map(|r| self.finalize(r))
    }

    /// Raw parent_id lookup without nlink/size computation (cheap, for fs_impl).
    pub fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaReaddir);
        // `name>''` only pins the plan to UNIQUE(parent_id,name) so the scan
        // comes out already ordered (see `readdir_page`).
        let sql = format!("SELECT {COLS} FROM node WHERE parent_id=?1 AND name>'' ORDER BY name");
        let raws = {
            let _r = stats::start(stats::Stage::SqliteRead);
            let conn = self.read_conn();
            let mut stmt = match conn.prepare_cached(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt.query_map(params![parent], row_to_raw) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
        };
        self.finalize_batch(&raws)
    }

    /// One slice of the deterministic listing: children in the same
    /// `ORDER BY name` sequence `readdir` uses, starting at `offset` and at most
    /// `limit` rows. Paging is pushed into SQL so replying to one kernel
    /// READDIR/READDIRPLUS buffer costs O(buffer) instead of re-scanning the
    /// whole directory once per buffer; the entries are then finalized together
    /// by [`Self::finalize_batch`] (a couple of grouped `COUNT`s, not one per
    /// entry) — the difference between a large directory feeling instant and
    /// taking minutes.
    pub fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        let _t = stats::start(stats::Stage::MetaReaddirPage);
        // Resume from the nearest checkpoint at or before `offset`. A keyset
        // page costs O(limit*log n); the OFFSET form walks every row it skips,
        // so a full listing paid sum(offset) = O(n^2). Offset 0 has no
        // predecessor by definition, and any offset whose checkpoint is gone
        // (seekdir, a fresh reader, a directory mutated since) keeps the old
        // query — never wrong, just the slow path.
        let base = offset - offset % READDIR_CURSOR_STRIDE;
        let after = if offset == 0 {
            None
        } else {
            self.cursors.lock().get(&(parent, base)).cloned()
        };
        let start = if after.is_some() { base } else { offset };
        let skip = offset - start;
        // Fetch the skipped prefix together with the page, then drop it here.
        let fetch = limit.saturating_add(skip);
        let raws = {
            let _r = stats::start(stats::Stage::SqliteRead);
            let conn = self.read_conn();
            match &after {
                Some(last) => {
                    let sql = format!(
                        "SELECT {COLS} FROM node WHERE parent_id=?1 AND name>?2 ORDER BY name LIMIT ?3"
                    );
                    let mut stmt = match conn.prepare_cached(&sql) {
                        Ok(s) => s,
                        Err(_) => return Vec::new(),
                    };
                    let rows = match stmt.query_map(params![parent, last, fetch as i64], row_to_raw)
                    {
                        Ok(r) => r,
                        Err(_) => return Vec::new(),
                    };
                    rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
                }
                None => {
                    // `name>''` is a no-op (a name is never empty) whose only job
                    // is to pin the plan to the UNIQUE(parent_id,name) index:
                    // without it the planner may pick a parent-prefix index that
                    // cannot produce `ORDER BY name`, then scan the whole
                    // directory into a temp b-tree for every OFFSET page.
                    let sql = format!(
                        "SELECT {COLS} FROM node WHERE parent_id=?1 AND name>'' ORDER BY name LIMIT ?2 OFFSET ?3"
                    );
                    let mut stmt = match conn.prepare_cached(&sql) {
                        Ok(s) => s,
                        Err(_) => return Vec::new(),
                    };
                    let rows = match stmt
                        .query_map(params![parent, fetch as i64, start as i64], row_to_raw)
                    {
                        Ok(r) => r,
                        Err(_) => return Vec::new(),
                    };
                    rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
                }
            }
        };
        let mut out = self.finalize_batch(&raws);
        if skip > 0 {
            out.drain(..skip.min(out.len()));
        }
        // Checkpoint the listing: `(parent, n)` -> the name of row n-1, so a
        // resume at any offset within STRIDE rows of n becomes a keyset seek.
        {
            let mut c = self.cursors.lock();
            for (i, row) in out.iter().enumerate() {
                let next = offset + i + 1;
                if next.is_multiple_of(READDIR_CURSOR_STRIDE) {
                    if c.len() >= READDIR_CURSOR_CAP {
                        c.clear();
                    }
                    c.insert((parent, next), row.name.clone());
                }
            }
        }
        out
    }

    /// Drop the paging cursors: any name change can invalidate the "resume
    /// after this name" position a reader is holding.
    fn drop_cursors(&self) {
        let mut c = self.cursors.lock();
        if !c.is_empty() {
            c.clear();
        }
    }

    /// Create a new file/dir node. `data_ref` is Some for files, None for dirs.
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

    /// Create a symbolic link node pointing at `target`.
    pub fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        // Symlink perms are always 0777 (ignored by the kernel on Linux).
        self.insert(parent, name, false, None, Some(target), 0o0777, uid, gid)
    }

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
        let _t = stats::start(stats::Stage::MetaCreate);
        self.drop_cursors();
        let t = now();
        // Ledger-ordered write: see [`with_ledger_serial`] (one process-wide
        // lock order for everything that can book the shared refs ledger).
        with_ledger_serial(|| {
            let mut conn = self.conn.lock();
            // Atomic, exactly like rename/unlink/lock_op: one IMMEDIATE tx wraps the
            // INSERT *and* the `ino = rowid` back-fill. Two autocommit statements
            // would let a concurrent reader on a separate WAL connection observe the
            // row between them with `ino IS NULL` (invisible to `WHERE ino=?`), and a
            // crash in that window would leave a dangling NULL-ino row until the next
            // open's back-fill. A failed INSERT (constraint) rolls the tx back and
            // still surfaces as Exists; the UPDATE's error is propagated, not swallowed.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Is this the view's FIRST name for the key (count before the INSERT)?
            // Only then does the cross-view ledger take a new hold.
            let first_name = data_ref
                .map(|dr| {
                    tx.query_row(
                        "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                        params![dr],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                        == 0
                })
                .unwrap_or(false);
            tx.execute(
            "INSERT INTO node (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,?9)",
            params![parent, name, is_dir as i64, data_ref, link_target, mode, uid, gid, t],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                MetaError::Exists
            }
            other => MetaError::Db(other.to_string()),
        })?;
            let rowid = tx.last_insert_rowid();
            // A brand-new name owns a fresh inode: ino == its own row id.
            tx.execute("UPDATE node SET ino=?1 WHERE id=?1", params![rowid])?;
            if first_name {
                if let Some(dr) = data_ref {
                    ledger_add(&tx, dr)?;
                }
            }
            tx.commit()?;
            Ok(rowid)
        })
    }

    /// Create a hard link: a new directory entry that SHARES `src_ino`'s inode
    /// (same data object, same size, same timestamps). Only regular files may be
    /// hard-linked (POSIX forbids linking dirs). Returns the shared inode number.
    pub fn link_node(
        &self,
        src_ino: i64,
        new_parent: i64,
        new_name: &str,
    ) -> Result<i64, MetaError> {
        let _t = stats::start(stats::Stage::MetaCreate);
        self.drop_cursors();
        with_ledger_serial(|| {
            let mut conn = self.conn.lock();
            // One IMMEDIATE tx over read-source + INSERT (+ ledger bookkeeping):
            // the 0->1 name-count transition must be decided against the same
            // snapshot the new row commits in, or two racing hard-link creators
            // could each read "count 0" and double-book this view's hold.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row = tx
                .query_row(
                    "SELECT is_dir,data_ref,mode,uid,gid FROM node WHERE ino=?1 LIMIT 1",
                    params![src_ino],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .map_err(|_| MetaError::NotFound)?;
            let (is_dir, data_ref, mode, uid, gid) = row;
            if is_dir == 1 || data_ref.is_none() {
                return Err(MetaError::OpNotPermitted);
            }
            let t = now();
            let first_name = tx
                .query_row(
                    "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                    params![data_ref],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                == 0;
            tx.execute(
            "INSERT INTO node (parent_id,name,is_dir,data_ref,link_target,mode,uid,gid,mtime,atime,ctime,ino)
             VALUES (?1,?2,0,?3,NULL,?4,?5,?6,?7,?7,?7,?8)",
            params![new_parent, new_name, data_ref, mode, uid, gid, t, src_ino],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                MetaError::Exists
            }
            other => MetaError::Db(other.to_string()),
        })?;
            if first_name {
                if let Some(dr) = data_ref.as_deref() {
                    ledger_add(&tx, dr)?;
                }
            }
            tx.commit()?;
            Ok(src_ino)
        })
    }

    /// Rename / move a node within the view tree (custom hierarchy lives here).
    /// Wrapped in an IMMEDIATE transaction so the read-modify-write (find src ->
    /// cycle check -> replace target -> reparent) is atomic and takes the write
    /// lock up front, so two concurrent mounts can never observe a half-applied
    /// rename.
    pub fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        let _t = stats::start(stats::Stage::MetaRename);
        self.drop_cursors();
        let t = now();
        with_ledger_serial(|| {
            let mut conn = self.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (src_id, src_ino): (i64, Option<i64>) = tx
                .query_row(
                    "SELECT id,ino FROM node WHERE parent_id=?1 AND name=?2",
                    params![parent, name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| MetaError::NotFound)?;
            // Refuse to move a directory into its own subtree (would corrupt the tree).
            if self.is_descendant_locked(&tx, new_parent, src_id) {
                return Err(MetaError::BadParentCycle);
            }
            let tgt: Option<(i64, i64, Option<String>, Option<i64>)> = tx
                .query_row(
                    "SELECT id,is_dir,data_ref,ino FROM node WHERE parent_id=?1 AND name=?2",
                    params![new_parent, new_name],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .ok();
            // rename(a, a): the target row IS the source row — a successful no-op.
            // Without this guard the DELETE below would remove the source and the
            // later UPDATE would match nothing, silently destroying the entry.
            // The same-inode case covers two *different* hard link names of one
            // file: POSIX (man 2 rename) also mandates a silent no-op there, but
            // the row-id guard alone would miss it and delete the target name,
            // shrinking nlink (a link vanishes). Compare ino, not just row id.
            if let Some((tid, _, _, tino)) = &tgt {
                if *tid == src_id || (*tino).is_some() && *tino == src_ino {
                    tx.commit()?;
                    return Ok(None);
                }
            }
            // RENAME_NOREPLACE: an existing target must fail with EEXIST instead of
            // being silently overwritten and destroyed.
            if tgt.is_some() && noreplace {
                return Err(MetaError::Exists);
            }
            // Blob freed by overwriting a target that held the last name to its data;
            // the caller GCs it in the storage backend (mirrors unlink's refcount rule).
            let mut gc: Option<String> = None;
            if let Some((tid, is_dir, tgt_ref, _)) = tgt {
                let src_is_dir: i64 = tx
                    .query_row(
                        "SELECT is_dir FROM node WHERE id=?1",
                        params![src_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if src_is_dir != is_dir {
                    return Err(MetaError::TypeMismatch);
                }
                // Renaming a directory over an EXISTING directory requires the target
                // be empty: deleting a non-empty one orphans its children (their
                // parent_id points at the removed row). POSIX: ENOTEMPTY.
                if is_dir == 1 {
                    let kids: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM node WHERE parent_id=?1",
                            params![tid],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if kids > 0 {
                        return Err(MetaError::NotEmpty);
                    }
                }
                tx.execute("DELETE FROM node WHERE id=?1", params![tid])?;
                // A dir/symlink target has no data_ref (gc stays None); a regular-file
                // target's blob is freed only once no OTHER NAME in this view AND no
                // other view (ledger) still references it.
                if let Some(dr) = tgt_ref {
                    let refs: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                            params![dr],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if refs == 0 {
                        gc = ledger_remove(&tx, &dr);
                    }
                }
            }
            tx.execute(
                "UPDATE node SET parent_id=?1, name=?2, ctime=?3 WHERE id=?4",
                params![new_parent, new_name, t, src_id],
            )?;
            tx.commit()?;
            Ok(gc)
        })
    }

    fn is_descendant_locked(&self, conn: &Connection, new_parent: i64, src_id: i64) -> bool {
        // Walk ancestors of new_parent; if we reach src_id it's a cycle.
        let mut cur = new_parent;
        for _ in 0..4096 {
            if cur == src_id {
                return true;
            }
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT parent_id FROM node WHERE id=?1",
                    params![cur],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            match parent {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Remove a node. Returns the freed data_ref if this was the last reference
    /// (so the caller can GC the backend blob).
    ///
    /// The find -> DELETE -> refcount-read runs as one IMMEDIATE transaction so
    /// two processes hardlinking the same inode can never interleave and either
    /// GC a still-referenced object or miss GC of the last name; `busy_timeout`
    /// makes the loser of a lock race wait rather than error.
    pub fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let _t = stats::start(stats::Stage::MetaUnlink);
        self.drop_cursors();
        // Ledger-ordered write (see [`with_ledger_serial`]): the holder count and
        // the node rows it describes leave in one IMMEDIATE commit.
        with_ledger_serial(|| {
            let mut conn = self.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (id, is_dir, data_ref) = tx
                .query_row(
                    "SELECT id,is_dir,data_ref FROM node WHERE parent_id=?1 AND name=?2",
                    params![parent, name],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .map_err(|_| MetaError::NotFound)?;
            if is_dir == 1 {
                let kids: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM node WHERE parent_id=?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if kids > 0 {
                    return Err(MetaError::NotEmpty);
                }
            }
            tx.execute("DELETE FROM node WHERE id=?1", params![id])?;
            // The blob is freeable only once this view holds no other name for the
            // key AND the cross-view ledger confirms no other view holds it (the
            // last remove deletes the holder row and returns the key). See
            // [`ledger_remove`].
            let gc = match &data_ref {
                Some(dr) => {
                    let refs: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM node WHERE data_ref=?1",
                            params![dr],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if refs == 0 {
                        ledger_remove(&tx, dr)
                    } else {
                        None
                    }
                }
                None => None,
            };
            tx.commit()?;
            Ok(gc)
        })
    }

    /// Update a file's mtime/ctime (after a write or truncate). Applies to every
    /// name sharing the inode, so hard links stay in sync.
    pub fn touch_mtime(&self, ino: i64) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE node SET mtime=?1, ctime=?1 WHERE ino=?2",
            params![t, ino],
        );
    }

    /// Explicitly set access/modification times (setattr with SET_ATIME/SET_MTIME).
    pub fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        let t = now();
        let conn = self.conn.lock();
        if let Some(a) = atime {
            let _ = conn.execute(
                "UPDATE node SET atime=?1, ctime=?2 WHERE ino=?3",
                params![a, t, ino],
            );
        }
        if let Some(m) = mtime {
            let _ = conn.execute(
                "UPDATE node SET mtime=?1, ctime=?2 WHERE ino=?3",
                params![m, t, ino],
            );
        }
    }

    pub fn set_mode(&self, ino: i64, mode: u32) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE node SET mode=?1, ctime=?2 WHERE ino=?3",
            params![mode, t, ino],
        );
    }

    pub fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        let t = now();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE node SET uid=?1, gid=?2, ctime=?3 WHERE ino=?4",
            params![uid, gid, t, ino],
        );
    }

    /// Current HA epoch stored in this DB's `ctrl` table (0 when absent, e.g.
    /// pre-HA files or a read-only copy whose `ctrl` row hasn't been seeded).
    pub fn get_epoch(&self) -> i64 {
        let conn = self.conn.lock();
        conn.query_row("SELECT v FROM ctrl WHERE k='epoch'", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Set the HA epoch row (creating it when the ctrl table was just added).
    pub fn set_epoch(&self, e: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ctrl (k,v) VALUES ('epoch',?1)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![e],
        )?;
        Ok(())
    }

    /// Consistent whole-database snapshot as raw bytes for HA replication.
    /// Uses SQLite's `VACUUM INTO` (single-writer online backup: the output is
    /// a complete, checkpointed image — a side-copy of `<view>.db` would race
    /// WAL checkpoints and could ship a torn file). Holds the engine's own
    /// connection lock for the duration, so replication never reads a
    /// half-written page; the step blocks concurrent metadata writes for the
    /// vacuum time of one view DB (ms for view-sized trees).
    pub fn backup_to_bytes(&self) -> rusqlite::Result<Vec<u8>> {
        static SNAP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SNAP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("grovefs_snap_{}_{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&tmp);
        let conn = self.conn.lock();
        let r = conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy().as_ref()]);
        let out = match r {
            Ok(_) => std::fs::read(&tmp).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    Some(e.to_string()),
                )
            }),
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_file(&tmp);
        out
    }

    // ---- POSIX advisory record locks ------------------------------------------------
    //
    // Kernel FUSE translates both `fcntl F_SETLK` and BSD `flock(2)` into the
    // same wire contract (`typ` = F_RDLCK/F_WRLCK/F_UNLCK + interval + owner),
    // with flock degenerating to `[0, ULLONG_MAX)` on the whole file. We store
    // the interval as signed i64 and clamp ULLONG_MAX to i64::MAX — 9.2 exabytes
    // is far past any real file, and it lets us index the table.
    //
    // Conflict rules (POSIX): Read/Read compatible; Read/Write and Write/Write
    // conflict; a lock owned by the same `owner` (file description) *converts*
    // overlapping ranges instead of conflicting (matches `fcntl` semantics).
    //
    // Lease: each row carries an `expires_at` (unix seconds, now + LOCK_LEASE_SECS
    // at insert/renew time). Expired rows are swept at the head of every write
    // transaction — this is our safety net for clients SIGKILL'd before their
    // FUSE `release` reached us.

    /// Perform a POSIX advisory lock operation. `ty == Unlock` removes the
    /// overlapping interval for `owner`; `Read`/`Write` either grants (returns
    /// `Ok(None)`) or reports the first blocking lock (returns `Ok(Some(..))`).
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
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now();
        tx.execute("DELETE FROM locks WHERE expires_at < ?1", params![now])?;
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        match ty {
            LockType::Unlock => {
                tx.execute(
                    "DELETE FROM locks WHERE ino=?1 AND owner=?2 AND l_start<=?3 AND l_end>=?4",
                    params![ino, owner as i64, e, s],
                )?;
            }
            LockType::Read | LockType::Write => {
                // Detect cross-owner conflict first; same-owner overlaps convert.
                let conflict: Option<(i64, i64, i64, i64)> = tx
                    .query_row(
                        "SELECT l_start, l_end, ltype, pid FROM locks \
                         WHERE ino=?1 AND owner<>?2 AND l_start<=?3 AND l_end>=?4 \
                         AND (?5 = 1 OR ltype = 1) \
                         LIMIT 1",
                        params![ino, owner as i64, e, s, ty.to_u8() as i64],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                if let Some((cs, ce, ct, cp)) = conflict {
                    return Ok(Some(LockConflict {
                        start: cs.max(0) as u64,
                        end: ce.max(0) as u64,
                        ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
                        pid: cp as u32,
                    }));
                }
                // Convert: drop any same-owner overlapping rows, then insert.
                tx.execute(
                    "DELETE FROM locks WHERE ino=?1 AND owner=?2 AND l_start<=?3 AND l_end>=?4",
                    params![ino, owner as i64, e, s],
                )?;
                tx.execute(
                    "INSERT INTO locks (ino, owner, sid, l_start, l_end, pid, ltype, expires_at) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        ino,
                        owner as i64,
                        sid as i64,
                        s,
                        e,
                        pid as i64,
                        ty.to_u8() as i64,
                        now + LockLease::secs()
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(None)
    }

    /// F_GETLK: report the first blocking lock for the requested interval, or
    /// `None` if the operation would succeed. No row is written.
    pub fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        let conn = self.conn.lock();
        let now = now();
        let s = start.min(i64::MAX as u64) as i64;
        let e = end.min(i64::MAX as u64) as i64;
        conn.query_row(
            "SELECT l_start, l_end, ltype, pid FROM locks \
             WHERE ino=?1 AND owner<>?2 AND expires_at>=?3 AND l_start<=?4 AND l_end>=?5 \
             AND (?6 = 1 OR ltype = 1) \
             LIMIT 1",
            params![ino, owner as i64, now, e, s, ty.to_u8() as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .ok()
        .map(|(cs, ce, ct, cp)| LockConflict {
            start: cs.max(0) as u64,
            end: ce.max(0) as u64,
            ty: LockType::from_u8(ct as u8).unwrap_or(LockType::Read),
            pid: cp as u32,
        })
    }

    /// Release every lock held by `owner` — called from FUSE `release` on the
    /// last close of a file description that had locks. Idempotent.
    pub fn release_locks_of_owner(&self, owner: u64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM locks WHERE owner=?1", params![owner as i64]);
    }

    /// Renewal heartbeat: push `expires_at` out by one full lease for every lock
    /// row owned by session `sid`. Driven by the mount daemon on a
    /// `lease/3` cadence so a *live* holder never expires; a dead one stops
    /// renewing and ages out within one lease. No-op (zero rows) when idle.
    pub fn renew_locks(&self, sid: u64) {
        let conn = self.conn.lock();
        let now = now();
        let n = conn
            .execute(
                "UPDATE locks SET expires_at = ?1 WHERE sid = ?2",
                params![now + LockLease::secs(), sid as i64],
            )
            .unwrap_or(0);
        if n > 0 {
            stats::count(stats::Counter::LockRenew, n as u64);
        }
    }

    /// Reap every lock held by session `sid`. Called on the meta-server when a
    /// client's connection drops (daemon crash / unmount): reclaims its locks
    /// *immediately* instead of waiting out the lease, shrinking the dangling
    /// window to ~0 for the common remote-crash case. The lease stays only as
    /// the final backstop for network partitions where the socket lingers.
    pub fn release_locks_of_session(&self, sid: u64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM locks WHERE sid=?1", params![sid as i64]);
    }

    /// Explicitly reap expired locks. Called opportunistically inside every
    /// write txn; exposed so tests (and future lease-renew RPCs) can drive it.
    pub fn sweep_expired_locks(&self, now_secs: i64) {
        let conn = self.conn.lock();
        let n = conn
            .execute("DELETE FROM locks WHERE expires_at < ?1", params![now_secs])
            .unwrap_or(0);
        if n > 0 {
            stats::count(stats::Counter::LockExpireReclaim, n as u64);
        }
    }

    /// Compute nlink/size and resolve the node kind. Re-locks the connection,
    /// so callers MUST have already released their own guard.
    fn finalize(&self, raw: RawNode) -> NodeInfo {
        let kind = Self::node_kind(&raw);
        let conn = self.read_conn();
        let (nlink, size) = match kind {
            NodeKind::Dir => {
                let subdirs: i64 = conn
                    .prepare_cached("SELECT COUNT(*) FROM node WHERE parent_id=?1 AND is_dir=1")
                    .and_then(|mut s| s.query_row(params![raw.id], |r| r.get(0)))
                    .unwrap_or(0);
                (2 + subdirs as u64, 4096u64)
            }
            NodeKind::Symlink => {
                let n = raw
                    .link_target
                    .as_ref()
                    .map(|s| s.len() as u64)
                    .unwrap_or(0);
                (1, n)
            }
            NodeKind::Regular => {
                let links: i64 = conn
                    .prepare_cached("SELECT COUNT(*) FROM node WHERE ino=?1")
                    .and_then(|mut s| s.query_row(params![raw.ino], |r| r.get(0)))
                    .unwrap_or(1);
                (links.max(1) as u64, 0u64)
            }
        };
        Self::build_info(&raw, kind, nlink, size)
    }

    /// Classify a raw row into a node kind. Shared by the scalar and batched
    /// finalize paths so the two can never diverge.
    fn node_kind(raw: &RawNode) -> NodeKind {
        if raw.is_dir == 1 {
            NodeKind::Dir
        } else if raw.link_target.is_some() {
            NodeKind::Symlink
        } else {
            NodeKind::Regular
        }
    }

    /// Assemble a `NodeInfo` from a raw row plus the already-computed nlink and
    /// size. Pure (no store access); the single source of truth for the field
    /// mapping, used by both finalize paths.
    fn build_info(raw: &RawNode, kind: NodeKind, nlink: u64, size: u64) -> NodeInfo {
        NodeInfo {
            id: raw.ino,
            parent_id: raw.parent_id.unwrap_or(0),
            name: raw.name.clone(),
            kind,
            data_ref: raw.data_ref.clone(),
            link_target: raw.link_target.clone(),
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

    /// Finalize a whole directory listing with at most two grouped `COUNT`
    /// queries — one for every regular file's hard-link count, one for every
    /// directory's sub-directory count — instead of one `COUNT` per entry.
    ///
    /// `readdir`/`readdir_page` map this over the raw rows they already
    /// fetched, so a listing of N entries used to pay N separate `read_conn()`
    /// pool borrows and N `COUNT` round-trips (the READDIRPLUS hot spot); now it
    /// pays a single pool borrow and a constant number of chunked queries.
    /// Symlinks need no query. Output is identical to mapping the scalar
    /// [`Self::finalize`] over the same rows: an inode's hard-link count is
    /// global (`WHERE ino IN (...)`), and a directory's nlink is `2 +` its count
    /// of directory children.
    fn finalize_batch(&self, raws: &[RawNode]) -> Vec<NodeInfo> {
        let mut dir_ids: Vec<i64> = Vec::new();
        let mut reg_inos: Vec<i64> = Vec::new();
        for r in raws {
            match Self::node_kind(r) {
                NodeKind::Dir => dir_ids.push(r.id),
                NodeKind::Regular => reg_inos.push(r.ino),
                NodeKind::Symlink => {}
            }
        }
        let mut nlink_map: HashMap<i64, i64> = HashMap::new();
        let mut subdir_map: HashMap<i64, i64> = HashMap::new();
        if !dir_ids.is_empty() || !reg_inos.is_empty() {
            let _f = stats::start(stats::Stage::SqliteFinalizeCount);
            let conn = self.read_conn();
            grouped_counts(&conn, "ino", "", &reg_inos, &mut nlink_map);
            grouped_counts(
                &conn,
                "parent_id",
                "AND is_dir=1",
                &dir_ids,
                &mut subdir_map,
            );
        }
        raws.iter()
            .map(|r| {
                let kind = Self::node_kind(r);
                let (nlink, size) = match kind {
                    NodeKind::Dir => (2 + *subdir_map.get(&r.id).unwrap_or(&0) as u64, 4096u64),
                    NodeKind::Symlink => (
                        1,
                        r.link_target.as_ref().map(|s| s.len() as u64).unwrap_or(0),
                    ),
                    NodeKind::Regular => {
                        let links = *nlink_map.get(&r.ino).unwrap_or(&1);
                        (links.max(1) as u64, 0u64)
                    }
                };
                Self::build_info(r, kind, nlink, size)
            })
            .collect()
    }
}

/// The metadata interface [`crate::fs_impl::GroveFS`] actually depends on. It is
impl MetaProvider for MetaEngine {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        MetaEngine::lookup(self, parent, name)
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        MetaEngine::getattr(self, ino)
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        MetaEngine::readdir(self, parent)
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        MetaEngine::readdir_page(self, parent, offset, limit)
    }
    fn supports_paged_readdir(&self) -> bool {
        true
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
        MetaEngine::create_node(self, parent, name, is_dir, data_ref, mode, uid, gid)
    }
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        MetaEngine::create_symlink(self, parent, name, target, uid, gid)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        MetaEngine::link_node(self, src_ino, new_parent, new_name)
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        MetaEngine::unlink(self, parent, name)
    }
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        MetaEngine::rename(self, parent, name, new_parent, new_name, noreplace)
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        MetaEngine::set_mode(self, ino, mode)
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        MetaEngine::set_uid_gid(self, ino, uid, gid)
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        MetaEngine::set_times(self, ino, atime, mtime)
    }
    fn touch_mtime(&self, ino: i64) {
        MetaEngine::touch_mtime(self, ino)
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
        MetaEngine::lock_op(self, ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        MetaEngine::get_lock(self, ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        MetaEngine::release_locks_of_owner(self, owner)
    }
    fn renew_locks(&self, sid: u64) {
        MetaEngine::renew_locks(self, sid)
    }
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        let conn = self.read_conn();
        // `parent_id` is NULL for the root row (`INSERT ... VALUES (1,NULL,'/',...)`
        // at open), and reading that column as a plain `i64` fails, which silently
        // DROPPED the root from the dump. `pg_store::dump_tree` reads it as
        // `Option<i64>` and maps NULL to 0, so the two dumps were different
        // artifacts — visible to `du` (whose `find_root` happens to fall back to id
        // 1) and fatal to a differential comparison, which is P3/P6's whole point.
        // Read it the PG way so one name means one thing in all three backends.
        let mut stmt = match conn
            .prepare("SELECT id, parent_id, name, is_dir, data_ref FROM node ORDER BY id")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}

// ===================== pluggable metadata store (SQLite | PostgreSQL) ======

/// A concrete metadata store the central `meta-server` (or a direct mount) can
/// drive: the embedded SQLite [`MetaEngine`] or the [`crate::pg_store::PgEngine`]
/// PostgreSQL backend. Both implement [`MetaProvider`], and this enum exposes
/// the *same* inherent surface (`get_epoch`/`set_epoch`/`backup_to_bytes`) that
/// the server's HA/replication code calls on the engine, so swapping backends is
/// a construction-time choice in `engine_for` — the dispatch and event code never
/// branch on the store.
///
/// GroveFS-native HA (`VACUUM INTO` snapshot shipping) is SQLite-only; under the
/// PG variant `backup_to_bytes` reports unsupported and a `--meta-store postgres://…`
/// meta-server rejects the `--role/--peer/--witness` flags at CLI time.
///
/// Every server-side holder keeps this in an `Arc<MetaStore>` and picks the
/// variant once at construction (`engine_for`), so the size gap between the two
/// variants never shows up in a per-op move — boxing the SQLite handle would add
/// an indirection to the hot path for no allocation win.
#[allow(clippy::large_enum_variant)]
pub enum MetaStore {
    Sqlite(MetaEngine),
    Pg(Box<crate::pg_store::PgEngine>),
    /// The FoundationDB backend (`--meta-store fdb://<cluster file>`). The variant
    /// itself is feature-gated, so a build without `--features fdb` does not name
    /// `FdbStore`, does not link `libfdb_c`, and cannot reach an `fdb://` dispatch
    /// arm at all — the two backends that ship today compile exactly as before.
    #[cfg(feature = "fdb")]
    Fdb(Box<crate::fdb_store::FdbStore>),
}

impl MetaStore {
    /// Open a SQLite store for `<base>/meta/<view>.db` (`readonly` for a standby).
    pub fn open_sqlite(
        base: &std::path::Path,
        view: &str,
        readonly: bool,
    ) -> rusqlite::Result<Self> {
        let path = base.join("meta").join(format!("{view}.db"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        MetaEngine::open_ex(&path, readonly).map(MetaStore::Sqlite)
    }

    /// Open a PostgreSQL store for `view` in the database at `conn_uri`.
    pub fn open_pg(conn_uri: &str, view: &str, readonly: bool) -> std::io::Result<Self> {
        crate::pg_store::PgEngine::open(conn_uri, view, readonly)
            .map(|p| MetaStore::Pg(Box::new(p)))
    }

    /// Open a FoundationDB store for `view` against the cluster named by
    /// `uri` (`fdb://<path-to-cluster-file>`). `readonly` is passed straight to
    /// [`crate::fdb_store::FdbStore::open`], where it suppresses the root seed: a
    /// read-only handle probes, it does not provision. The GroveFS-native standby
    /// path is refused for this backend at `serve()` (an FDB deployment gets its
    /// replicas and its `fdbbackup` from the cluster, not from `--role standby`),
    /// so the flag here exists for probes such as `du`.
    #[cfg(feature = "fdb")]
    pub fn open_fdb(uri: &str, view: &str, readonly: bool) -> std::io::Result<Self> {
        crate::fdb_store::FdbStore::open(uri, view, readonly).map(|f| MetaStore::Fdb(Box::new(f)))
    }

    pub fn get_epoch(&self) -> i64 {
        match self {
            MetaStore::Sqlite(m) => m.get_epoch(),
            MetaStore::Pg(p) => p.get_epoch(),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.get_epoch(),
        }
    }

    pub fn set_epoch(&self, e: i64) -> std::io::Result<()> {
        match self {
            MetaStore::Sqlite(m) => m
                .set_epoch(e)
                .map_err(|err| std::io::Error::other(err.to_string())),
            MetaStore::Pg(p) => p.set_epoch(e),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.set_epoch(e),
        }
    }

    pub fn backup_to_bytes(&self) -> std::io::Result<Vec<u8>> {
        match self {
            MetaStore::Sqlite(m) => m
                .backup_to_bytes()
                .map_err(|err| std::io::Error::other(err.to_string())),
            MetaStore::Pg(p) => p.backup_to_bytes(),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.backup_to_bytes(),
        }
    }
}

impl MetaProvider for MetaStore {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        match self {
            MetaStore::Sqlite(m) => m.lookup(parent, name),
            MetaStore::Pg(p) => p.lookup(parent, name),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.lookup(parent, name),
        }
    }
    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        match self {
            MetaStore::Sqlite(m) => m.getattr(ino),
            MetaStore::Pg(p) => p.getattr(ino),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.getattr(ino),
        }
    }
    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        match self {
            MetaStore::Sqlite(m) => m.readdir(parent),
            MetaStore::Pg(p) => p.readdir(parent),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.readdir(parent),
        }
    }
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        match self {
            MetaStore::Sqlite(m) => m.readdir_page(parent, offset, limit),
            MetaStore::Pg(p) => p.readdir_page(parent, offset, limit),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.readdir_page(parent, offset, limit),
        }
    }
    fn supports_paged_readdir(&self) -> bool {
        // All three concrete stores page inside their own store: SQLite and PG
        // through LIMIT/OFFSET on their read pool, FDB through a `start_after`
        // key-selector range plus the same stride-32 checkpoints (§3.3 of
        // `docs/fdb-keyspace.md`). So a meta-server backed by any of them can
        // serve M_READDIRP and offer READDIRPLUS to its clients. A derived view
        // served through a [`crate::layout::LayoutMeta`] lens does NOT reach
        // here — the lens keeps the trait default (false) because its
        // hidden/mapped shape cannot be expressed as a single page.
        match self {
            MetaStore::Sqlite(m) => MetaProvider::supports_paged_readdir(m),
            MetaStore::Pg(p) => MetaProvider::supports_paged_readdir(&**p),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => MetaProvider::supports_paged_readdir(&**f),
        }
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
        match self {
            MetaStore::Sqlite(m) => m.create_node(parent, name, is_dir, data_ref, mode, uid, gid),
            MetaStore::Pg(p) => p.create_node(parent, name, is_dir, data_ref, mode, uid, gid),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.create_node(parent, name, is_dir, data_ref, mode, uid, gid),
        }
    }
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        match self {
            MetaStore::Sqlite(m) => m.create_symlink(parent, name, target, uid, gid),
            MetaStore::Pg(p) => p.create_symlink(parent, name, target, uid, gid),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.create_symlink(parent, name, target, uid, gid),
        }
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        match self {
            MetaStore::Sqlite(m) => m.link_node(src_ino, new_parent, new_name),
            MetaStore::Pg(p) => p.link_node(src_ino, new_parent, new_name),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.link_node(src_ino, new_parent, new_name),
        }
    }
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        match self {
            MetaStore::Sqlite(m) => m.unlink(parent, name),
            MetaStore::Pg(p) => p.unlink(parent, name),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.unlink(parent, name),
        }
    }
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        match self {
            MetaStore::Sqlite(m) => m.rename(parent, name, new_parent, new_name, noreplace),
            MetaStore::Pg(p) => p.rename(parent, name, new_parent, new_name, noreplace),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.rename(parent, name, new_parent, new_name, noreplace),
        }
    }
    fn set_mode(&self, ino: i64, mode: u32) {
        match self {
            MetaStore::Sqlite(m) => m.set_mode(ino, mode),
            MetaStore::Pg(p) => p.set_mode(ino, mode),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.set_mode(ino, mode),
        }
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        match self {
            MetaStore::Sqlite(m) => m.set_uid_gid(ino, uid, gid),
            MetaStore::Pg(p) => p.set_uid_gid(ino, uid, gid),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.set_uid_gid(ino, uid, gid),
        }
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        match self {
            MetaStore::Sqlite(m) => m.set_times(ino, atime, mtime),
            MetaStore::Pg(p) => p.set_times(ino, atime, mtime),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.set_times(ino, atime, mtime),
        }
    }
    fn touch_mtime(&self, ino: i64) {
        match self {
            MetaStore::Sqlite(m) => m.touch_mtime(ino),
            MetaStore::Pg(p) => p.touch_mtime(ino),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.touch_mtime(ino),
        }
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
        match self {
            MetaStore::Sqlite(m) => m.lock_op(ino, owner, start, end, ty, pid, sid),
            MetaStore::Pg(p) => p.lock_op(ino, owner, start, end, ty, pid, sid),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.lock_op(ino, owner, start, end, ty, pid, sid),
        }
    }
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        match self {
            MetaStore::Sqlite(m) => m.get_lock(ino, owner, start, end, ty),
            MetaStore::Pg(p) => p.get_lock(ino, owner, start, end, ty),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.get_lock(ino, owner, start, end, ty),
        }
    }
    fn release_locks_of_owner(&self, owner: u64) {
        match self {
            MetaStore::Sqlite(m) => m.release_locks_of_owner(owner),
            MetaStore::Pg(p) => p.release_locks_of_owner(owner),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.release_locks_of_owner(owner),
        }
    }
    fn renew_locks(&self, sid: u64) {
        match self {
            MetaStore::Sqlite(m) => m.renew_locks(sid),
            MetaStore::Pg(p) => p.renew_locks(sid),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.renew_locks(sid),
        }
    }
    fn fence_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        match self {
            MetaStore::Sqlite(m) => m.fence_handle(),
            MetaStore::Pg(p) => p.fence_handle(),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => f.fence_handle(),
        }
    }
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        match self {
            MetaStore::Sqlite(m) => MetaProvider::dump_tree(m),
            MetaStore::Pg(p) => MetaProvider::dump_tree(&**p),
            #[cfg(feature = "fdb")]
            MetaStore::Fdb(f) => MetaProvider::dump_tree(&**f),
        }
    }
}

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

fn row_to_raw(r: &rusqlite::Row) -> rusqlite::Result<RawNode> {
    Ok(RawNode {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        name: r.get(2)?,
        is_dir: r.get(3)?,
        data_ref: r.get(4)?,
        link_target: r.get(5)?,
        mode: r.get(6)?,
        uid: r.get(7)?,
        gid: r.get(8)?,
        mtime: r.get(9)?,
        atime: r.get(10)?,
        ctime: r.get(11)?,
        ino: r.get(12)?,
    })
}

impl From<rusqlite::Error> for MetaError {
    fn from(e: rusqlite::Error) -> Self {
        MetaError::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rename(a, a) — same directory entry — is a POSIX no-op and must NOT
    /// destroy the file. Guards the bug where the overwrite target lookup
    /// returned the source row, the DELETE removed it, and the trailing UPDATE
    /// matched nothing, silently deleting the entry.
    #[test]
    fn rename_onto_itself_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let id = e
            .create_node(ROOT_INO, "f", false, Some("datak"), 0o644, 1000, 1000)
            .unwrap();
        assert!(
            e.rename(ROOT_INO, "f", ROOT_INO, "f", false).is_ok(),
            "rename(a,a) must succeed"
        );
        let n = e
            .lookup(ROOT_INO, "f")
            .expect("entry must survive rename(a,a)");
        assert_eq!(n.id, id, "no-op must keep the same inode");
    }

    /// Renaming a directory over an EXISTING NON-EMPTY directory must fail
    /// ENOTEMPTY and leave both subtrees intact. Guards the bug where the
    /// overwrite path DELETEd the target dir row with no emptiness check,
    /// orphaning its children.
    #[test]
    fn rename_over_nonempty_directory_returns_enotempty() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let _a = e
            .create_node(ROOT_INO, "a", true, None, 0o755, 1000, 1000)
            .unwrap();
        let b = e
            .create_node(ROOT_INO, "b", true, None, 0o755, 1000, 1000)
            .unwrap();
        e.create_node(b, "inside_b", false, Some("k2"), 0o644, 1000, 1000)
            .unwrap();
        let r = e.rename(ROOT_INO, "a", ROOT_INO, "b", false);
        assert!(
            matches!(r, Err(MetaError::NotEmpty)),
            "expected NotEmpty, got {r:?}"
        );
        assert!(
            e.lookup(ROOT_INO, "a").is_some(),
            "source dir must survive the refused rename"
        );
        assert!(
            e.lookup(b, "inside_b").is_some(),
            "target's child must not be orphaned"
        );
    }

    /// Correctness contrast: a file over a dir is TypeMismatch, and a dir over
    /// an EMPTY dir is allowed (target removed, source moved into its place).
    #[test]
    fn rename_over_empty_directory_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let _a = e
            .create_node(ROOT_INO, "a", false, Some("ka"), 0o644, 1000, 1000)
            .unwrap();
        let _b = e
            .create_node(ROOT_INO, "b", true, None, 0o755, 1000, 1000)
            .unwrap();
        assert!(matches!(
            e.rename(ROOT_INO, "a", ROOT_INO, "b", false),
            Err(MetaError::TypeMismatch)
        ));
        let c = e
            .create_node(ROOT_INO, "c", true, None, 0o755, 1000, 1000)
            .unwrap();
        assert!(
            e.rename(ROOT_INO, "c", ROOT_INO, "b", false).is_ok(),
            "dir over empty dir ok"
        );
        assert!(
            e.lookup(ROOT_INO, "c").is_none(),
            "source name gone after move"
        );
        assert_eq!(
            e.lookup(ROOT_INO, "b").map(|n| n.id),
            Some(c),
            "moved dir now holds the name"
        );
    }

    /// Renaming a file over an EXISTING file must report the overwritten target's
    /// data_ref so the caller GCs the orphaned blob (returns it as `Some(key)`).
    /// Guards the space-leak bug where the target row was DELETEd but its storage
    /// object was never freed. A hard-linked target (still-referenced data) must
    /// NOT be reported for GC.
    #[test]
    fn rename_over_file_returns_freed_blob_key() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000)
            .unwrap();
        e.create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000)
            .unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "dst", false).unwrap();
        assert_eq!(
            gc.as_deref(),
            Some("kdst"),
            "overwritten file's blob must be freed"
        );
        assert!(e.lookup(ROOT_INO, "src").is_none(), "source name gone");
        assert_eq!(
            e.lookup(ROOT_INO, "dst")
                .and_then(|n| n.data_ref)
                .as_deref(),
            Some("ksrc"),
            "dst now holds src's data"
        );
    }

    /// A target that is a hard link (its data_ref still has another name after the
    /// overwrite) must NOT be GCd — renaming over it returns None.
    #[test]
    fn rename_over_hardlinked_target_does_not_gc() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let dst = e
            .create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000)
            .unwrap();
        e.link_node(dst, ROOT_INO, "dst_alias").unwrap(); // second name -> same data_ref
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000)
            .unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "dst", false).unwrap();
        assert_eq!(gc, None, "still-referenced blob must not be freed");
    }

    /// RENAME_NOREPLACE onto an existing name must fail EEXIST and change nothing.
    #[test]
    fn rename_noreplace_over_existing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000)
            .unwrap();
        e.create_node(ROOT_INO, "dst", false, Some("kdst"), 0o644, 1000, 1000)
            .unwrap();
        let r = e.rename(ROOT_INO, "src", ROOT_INO, "dst", true);
        assert!(
            matches!(r, Err(MetaError::Exists)),
            "expected Exists, got {r:?}"
        );
        assert!(
            e.lookup(ROOT_INO, "src").is_some(),
            "noreplace must not move src"
        );
        assert_eq!(
            e.lookup(ROOT_INO, "dst")
                .and_then(|n| n.data_ref)
                .as_deref(),
            Some("kdst"),
            "dst untouched"
        );
    }

    /// RENAME_NOREPLACE onto a free name is a normal move (Ok, no GC).
    #[test]
    fn rename_noreplace_to_free_name_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        e.create_node(ROOT_INO, "src", false, Some("ksrc"), 0o644, 1000, 1000)
            .unwrap();
        let gc = e.rename(ROOT_INO, "src", ROOT_INO, "fresh", true).unwrap();
        assert_eq!(gc, None, "move to a free name frees nothing");
        assert!(e.lookup(ROOT_INO, "src").is_none());
        assert!(e.lookup(ROOT_INO, "fresh").is_some());
    }

    /// `readdir_page` must stitch back into exactly the same `ORDER BY name`
    /// sequence as the full `readdir`, whatever chunk sizes are used: the FUSE
    /// directory callbacks stream pages to the kernel and resume from a
    /// caller-supplied index, so a boundary mismatch here would drop or
    /// duplicate entries in `ls`.
    #[test]
    fn readdir_page_stitches_into_the_full_sorted_listing() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        for i in 0..100 {
            e.create_node(
                ROOT_INO,
                &format!("f{i:03}"),
                false,
                Some(&format!("k{i}")),
                0o644,
                1000,
                1000,
            )
            .unwrap();
        }
        e.create_node(ROOT_INO, "sub", true, None, 0o755, 1000, 1000)
            .unwrap();
        let full: Vec<String> = e.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
        assert_eq!(full.len(), 101, "100 files + 1 subdir");
        for chunk in [1usize, 7, 32, 100, 1024] {
            let mut paged: Vec<String> = Vec::new();
            let mut off = 0usize;
            loop {
                let page = e.readdir_page(ROOT_INO, off, chunk);
                if page.is_empty() {
                    break;
                }
                paged.extend(page.into_iter().map(|n| n.name));
                off += chunk;
            }
            assert_eq!(
                paged, full,
                "chunk size {chunk} must reassemble the listing"
            );
        }
        // An offset past the end and a zero limit are empty pages, not panics.
        assert!(e.readdir_page(ROOT_INO, full.len(), 10).is_empty());
        assert!(e.readdir_page(ROOT_INO, 0, 0).is_empty());
    }

    /// The kernel resumes a listing at the entry after the last one its reply
    /// buffer accepted — an offset tied to a 4 KiB reply, not to our pages —
    /// so every offset must serve the same window. Mutations drop the
    /// checkpoints those resumes are built on, so a stale one can never skip
    /// or duplicate an entry.
    #[test]
    fn readdir_page_resumes_at_arbitrary_offsets_and_drops_stale_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let make = |name: String| {
            e.create_node(ROOT_INO, &name, false, Some("k"), 0o644, 1000, 1000)
                .unwrap();
        };
        for i in 0..200 {
            make(format!("f{i:03}"));
        }
        let names = |e: &MetaEngine| -> Vec<String> {
            e.readdir(ROOT_INO).into_iter().map(|n| n.name).collect()
        };
        let full = names(&e);
        assert_eq!(full.len(), 200);
        // Lay down the checkpoints a sequential walk would have left behind.
        assert_eq!(e.readdir_page(ROOT_INO, 0, 100).len(), 100);
        for off in [1usize, 31, 33, 47, 64, 96, 97, 150] {
            let got: Vec<String> = e
                .readdir_page(ROOT_INO, off, 7)
                .into_iter()
                .map(|n| n.name)
                .collect();
            let want = &full[off..(off + 7).min(full.len())];
            assert_eq!(got, want, "resume at offset {off} must match the listing");
        }
        // A deletion before a checkpoint shifts every later index: the
        // checkpoint must be gone, and the resume must follow the new order.
        e.unlink(ROOT_INO, "f010").unwrap();
        make("f005x".to_string());
        let shifted = names(&e);
        assert_ne!(shifted, full);
        let got: Vec<String> = e
            .readdir_page(ROOT_INO, 40, 9)
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(
            got,
            shifted[40..49].to_vec(),
            "a mutation must invalidate the checkpoints, not skip a row"
        );
    }

    /// Metadata reads are served from pooled side connections, so they must
    /// still observe this engine's own committed writes immediately — `create`
    /// followed by `ls` from another FUSE worker thread is exactly this path.
    #[test]
    fn reads_through_the_pool_observe_committed_writes() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();
        let id = e
            .create_node(ROOT_INO, "fresh", false, Some("kfresh"), 0o644, 1000, 1000)
            .unwrap();
        assert_eq!(
            e.lookup(ROOT_INO, "fresh").map(|n| n.id),
            Some(id),
            "lookup must see the insert"
        );
        assert_eq!(
            e.readdir_page(ROOT_INO, 0, 8)
                .into_iter()
                .map(|n| n.name)
                .collect::<Vec<_>>(),
            vec!["fresh".to_string()],
            "paged readdir must see the insert"
        );
        e.unlink(ROOT_INO, "fresh").unwrap();
        assert!(
            e.lookup(ROOT_INO, "fresh").is_none(),
            "lookup must see the delete"
        );
        assert!(
            e.readdir_page(ROOT_INO, 0, 8).is_empty(),
            "paged readdir must see the delete"
        );
    }

    /// The batched directory finalize must agree with the scalar one entry-for-
    /// entry, including the fields it derives by aggregation: hard-linked files
    /// (a *global* link count keyed on `ino`), directories (nlink = 2 + number
    /// of directory children), and symlinks (size = target length). `readdir`/
    /// `readdir_page` take the grouped-query path; `lookup` takes the per-node
    /// path — comparing the two over one mixed directory is the regression guard
    /// for collapsing the readdir N+1.
    #[test]
    fn finalize_batch_matches_scalar_finalize() {
        let dir = tempfile::tempdir().unwrap();
        let e = MetaEngine::open(&dir.path().join("view.db")).unwrap();

        // A directory with three directory children: nlink = 2 + 3 = 5.
        let d2 = e
            .create_node(ROOT_INO, "d2", true, None, 0o755, 1000, 1000)
            .unwrap();
        for n in ["a", "b", "c"] {
            e.create_node(d2, n, true, None, 0o755, 1000, 1000).unwrap();
        }
        // A directory with no directory children: nlink = 2.
        e.create_node(ROOT_INO, "d0", true, None, 0o755, 1000, 1000)
            .unwrap();
        // A lone regular file: nlink = 1.
        e.create_node(ROOT_INO, "solo", false, Some("ksolo"), 0o644, 1000, 1000)
            .unwrap();
        // A hard-linked pair (same inode, two names): nlink = 2 for both.
        let hl = e
            .create_node(ROOT_INO, "hl", false, Some("khl"), 0o644, 1000, 1000)
            .unwrap();
        e.link_node(hl, ROOT_INO, "hl2").unwrap();
        // A symlink: nlink = 1, size = target length.
        e.create_symlink(ROOT_INO, "sl", "/some/target", 1000, 1000)
            .unwrap();

        // Ground truth for the aggregate fields of the root listing.
        let expect: HashMap<&str, (u64, u64)> = HashMap::from([
            ("d2", (5u64, 4096u64)),
            ("d0", (2, 4096)),
            ("solo", (1, 0)),
            ("hl", (2, 0)),
            ("hl2", (2, 0)),
            ("sl", (1, "/some/target".len() as u64)),
        ]);

        let batch = e.readdir(ROOT_INO); // grouped path
        for n in &batch {
            if let Some(&(nlink, size)) = expect.get(n.name.as_str()) {
                assert_eq!(n.nlink, nlink, "batched nlink for {}", n.name);
                assert_eq!(n.size, size, "batched size for {}", n.name);
            }
        }
        // Grouped (readdir) must equal scalar (lookup) field-for-field.
        for name in expect.keys() {
            let b = batch
                .iter()
                .find(|n| n.name == *name)
                .unwrap_or_else(|| panic!("batched missing {name}"));
            let s = e
                .lookup(ROOT_INO, name)
                .unwrap_or_else(|| panic!("scalar missing {name}"));
            assert_eq!(b.id, s.id, "id {name}");
            assert_eq!(b.kind, s.kind, "kind {name}");
            assert_eq!(b.nlink, s.nlink, "nlink {name}");
            assert_eq!(b.size, s.size, "size {name}");
        }

        // A hard link in another directory is still counted *globally*: add a
        // cross-directory alias of `hl` and read the d2 page (grouped path).
        e.link_node(hl, d2, "hl_elsewhere").unwrap();
        let paged = e.readdir_page(d2, 0, 16);
        let alias = paged
            .iter()
            .find(|n| n.name == "hl_elsewhere")
            .expect("cross-directory alias present in the d2 page");
        assert_eq!(
            alias.nlink, 3,
            "global hard-link count (hl + hl2 + hl_elsewhere)"
        );
        assert_eq!(
            e.lookup(ROOT_INO, "hl").map(|n| n.nlink),
            Some(3),
            "scalar path must agree on the global count"
        );
    }

    /// `GroveFS` keys both the READDIRPLUS negotiation and its fetch chunking off
    /// `supports_paged_readdir`, so that flag is a contract, not a hint: an
    /// engine that pages *inside its own store* must advertise it, and one that
    /// cannot must keep the safe default (one full listing per kernel buffer, no
    /// READDIRPLUS) instead of silently multiplying full fetches. The SQLite
    /// engine pages; the server-side `MetaStore` wrapper forwards that so its
    /// `M_READDIRP` RPC handler answers a real page; a derived [`LayoutMeta`]
    /// lens cannot express its hidden/mapped shape as one SQL page, so it stays
    /// on the default.
    #[test]
    fn store_level_paging_is_advertised_only_by_providers_that_page_in_sql() {
        let dir = tempfile::tempdir().unwrap();
        let eng: std::sync::Arc<dyn MetaProvider> =
            std::sync::Arc::new(MetaEngine::open(&dir.path().join("view.db")).unwrap());
        assert!(
            eng.supports_paged_readdir(),
            "SQLite pages with LIMIT/OFFSET"
        );

        // The server-side wrapper now forwards paging method-by-method, so the
        // meta-server can serve M_READDIRP as a real sub-query — and, gated on
        // this flag, offer READDIRPLUS to its clients.
        let store: std::sync::Arc<dyn MetaProvider> = std::sync::Arc::new(MetaStore::Sqlite(
            MetaEngine::open(&dir.path().join("other.db")).unwrap(),
        ));
        assert!(
            store.supports_paged_readdir(),
            "wrapper forwards the SQLite engine's paging"
        );
        // Forwarding is not just the flag: the pages must stitch back into the
        // exact full listing `readdir` returns.
        for _ in 0..5 {
            store
                .create_node(ROOT_INO, "n", false, None, 0o644, 0, 0)
                .ok();
        }
        let full: Vec<String> = store
            .readdir(ROOT_INO)
            .into_iter()
            .map(|n| n.name)
            .collect();
        let mut stitched = Vec::new();
        let mut off = 0usize;
        loop {
            let page = store.readdir_page(ROOT_INO, off, 2);
            if page.is_empty() {
                break;
            }
            stitched.extend(page.into_iter().map(|n| n.name));
            off += 2;
        }
        assert_eq!(
            stitched, full,
            "forwarded readdir_page stitches into readdir"
        );

        // A derived lens pages over its materialized listing instead of SQL: it
        // must advertise that (so `GroveFS` keeps chunking and READDIRPLUS) and
        // its pages must stitch back into its `readdir` exactly the same way.
        for i in 0..4 {
            eng.create_node(ROOT_INO, &format!("e{i}"), false, None, 0o644, 0, 0)
                .ok();
        }
        let lens: std::sync::Arc<dyn MetaProvider> = std::sync::Arc::new(
            crate::layout::LayoutMeta::new(eng.clone(), crate::layout::LayoutRules::default()),
        );
        assert!(
            lens.supports_paged_readdir(),
            "a lens pages over one materialized listing"
        );
        let lens_full: Vec<String> = lens.readdir(ROOT_INO).into_iter().map(|n| n.name).collect();
        let mut lens_stitched = Vec::new();
        let mut lens_off = 0usize;
        loop {
            let page = lens.readdir_page(ROOT_INO, lens_off, 3);
            if page.is_empty() {
                break;
            }
            lens_stitched.extend(page.into_iter().map(|n| n.name));
            lens_off += 3;
        }
        assert_eq!(
            lens_stitched, lens_full,
            "lens pages stitch into its own readdir"
        );
    }

    /// The bug the ledger fixes: two physical views share a `data_ref` (an
    /// `import` of one source resolving to the same content key, or an
    /// operator seeding two trees over one object). Deleting the last name in
    /// view A must NOT free bytes view B still lists — only the delete that
    /// removes the final holder's last name returns the key for GC.
    #[test]
    fn cross_view_shared_key_is_not_gc_until_last_holder_leaves() {
        let dir = tempfile::tempdir().unwrap();
        // Two view dbs in one meta dir -> both ATTACH the same refs.db.
        let a = MetaEngine::open(&dir.path().join("a.db")).unwrap();
        let b = MetaEngine::open(&dir.path().join("b.db")).unwrap();
        a.create_node(ROOT_INO, "f", false, Some("shared"), 0o644, 1000, 1000)
            .unwrap();
        b.create_node(ROOT_INO, "g", false, Some("shared"), 0o644, 1000, 1000)
            .unwrap();
        // View A drops its only name: the ledger still counts view B as a
        // holder, so nothing is freed.
        assert_eq!(
            a.unlink(ROOT_INO, "f").unwrap().as_deref(),
            None,
            "key held by another view"
        );
        // View B now holds the last name across all views; removing it frees it.
        assert_eq!(
            b.unlink(ROOT_INO, "g").unwrap().as_deref(),
            Some("shared"),
            "last holder leaves -> GC"
        );
    }

    /// A fork is an independent tree that SHARES bytes: it copies names, keeps
    /// hard links, and books one ledger hold per distinct data_ref. Deleting a
    /// name in either tree must not free a blob the other still names; the last
    /// name in the last view frees it.
    #[test]
    fn fork_copies_tree_and_shares_blobs_until_last_holder_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("base.db")).unwrap();
            let d = e
                .create_node(ROOT_INO, "docs", true, None, 0o755, 1000, 1000)
                .unwrap();
            let f = e
                .create_node(d, "a.txt", false, Some("shared"), 0o644, 1000, 1000)
                .unwrap();
            // A hard link inside the base must survive as a hard link (same ino).
            e.link_node(f, d, "b.txt").unwrap();
            e.create_node(ROOT_INO, "own.txt", false, Some("own"), 0o644, 1000, 1000)
                .unwrap();
        }
        let st = fork_view(dir.path(), "base", "copy").unwrap();
        assert_eq!(st.nodes, 5, "root + docs + two hard-linked names + own.txt");
        assert_eq!(st.blobs, 2, "shared + own");
        let copy = MetaEngine::open(&meta.join("copy.db")).unwrap();
        let docs = copy.lookup(ROOT_INO, "docs").expect("dir copied");
        let a = copy.lookup(docs.id, "a.txt").expect("file copied");
        let b = copy.lookup(docs.id, "b.txt").expect("hard link copied");
        assert_eq!(
            (a.data_ref.as_deref(), a.nlink),
            (b.data_ref.as_deref(), b.nlink),
            "hard link stays one inode in the fork (same blob, shared link count)"
        );
        assert_eq!(a.nlink, 2, "one inode, two names after the copy");
        // The fork can organize its tree freely...
        copy.create_node(ROOT_INO, "scratch", true, None, 0o755, 1000, 1000)
            .unwrap();
        // ...but removing a shared name must not free the blob: the base holds it.
        let base = MetaEngine::open(&meta.join("base.db")).unwrap();
        assert_eq!(
            copy.unlink(docs.id, "a.txt").unwrap(),
            None,
            "base still names shared"
        );
        assert_eq!(
            base.unlink(docs.id, "a.txt").unwrap(),
            None,
            "fork still names shared"
        );
        // Last name across both trees: one hard link remains in the fork (b.txt).
        assert_eq!(
            base.unlink(docs.id, "b.txt").unwrap(),
            None,
            "fork still names shared via b.txt"
        );
        assert_eq!(
            copy.unlink(docs.id, "b.txt").unwrap().as_deref(),
            Some("shared"),
            "last holder frees"
        );
    }

    /// A plan is DATA handed in by another program: one `<src>` or
    /// `<src>TAB<dst>` per line, `#` comments and blank lines skipped, only the
    /// first tab splitting (names may contain spaces), `..` refused outright.
    #[test]
    fn graft_plan_parses_bulk_lists_and_renames() {
        let p = parse_graft_plan(
            "# curated subset\n\n A/uuid-1.fits \tbody/M31 2025.fits\nA/uuid-2.fits\n",
        )
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].src, "A/uuid-1.fits");
        assert_eq!(
            p[0].dst, "body/M31 2025.fits",
            "only the first tab splits, so spaces survive"
        );
        assert_eq!(p[1].dst, p[1].src, "no tab keeps the name (bulk list form)");
        let err = parse_graft_plan("a/b\n../etc/passwd\n").unwrap_err();
        assert!(
            err.contains("line 2") && err.contains(".."),
            "a plan must not walk out of the view: {err}"
        );
        assert!(parse_graft_plan("# nothing\n").unwrap().is_empty());
    }

    /// The point of `--empty` + graft: a fork holding only a curated, renamed
    /// subset — bytes shared (a graft never copies content), unlisted entries
    /// left behind, and the ledger still protecting both sides.
    #[test]
    fn empty_fork_then_graft_places_renamed_subset_and_shares_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("base.db")).unwrap();
            let docs = e
                .create_node(ROOT_INO, "docs", true, None, 0o755, 1000, 1000)
                .unwrap();
            e.create_node(docs, "uuid-a", false, Some("blob-a"), 0o644, 1000, 1000)
                .unwrap();
            let sub = e
                .create_node(docs, "sub", true, None, 0o755, 1000, 1000)
                .unwrap();
            e.create_node(sub, "uuid-b", false, Some("blob-b"), 0o644, 1000, 1000)
                .unwrap();
            e.create_node(
                ROOT_INO,
                "unrelated",
                false,
                Some("blob-c"),
                0o644,
                1000,
                1000,
            )
            .unwrap();
        }
        let st = fork_view_with(dir.path(), "base", "lab", false).unwrap();
        assert_eq!((st.nodes, st.blobs), (1, 0), "empty fork is root only");
        let lab = MetaEngine::open(&meta.join("lab.db")).unwrap();
        assert!(lab.readdir(ROOT_INO).is_empty(), "nothing was copied");

        let items =
            parse_graft_plan("docs/uuid-a\tby_target/M31_2025.fits\ndocs/sub\tby_target/raw\n")
                .unwrap();
        let g = graft_view(dir.path(), "lab", "base", &items, "", false).unwrap();
        assert_eq!(
            (g.nodes, g.dirs, g.blobs, g.skipped),
            (4, 2, 2, 0),
            "by_target + renamed file + raw dir + its child"
        );
        let by = lab.lookup(ROOT_INO, "by_target").expect("dest dir created");
        let fits = lab
            .lookup(by.id, "M31_2025.fits")
            .expect("renamed file placed");
        assert_eq!(
            fits.data_ref.as_deref(),
            Some("blob-a"),
            "bytes are shared, not copied"
        );
        let raw = lab.lookup(by.id, "raw").expect("renamed subtree dir");
        let b = lab
            .lookup(raw.id, "uuid-b")
            .expect("subtree child kept its name");
        assert_eq!(b.data_ref.as_deref(), Some("blob-b"));
        assert!(
            lab.lookup(ROOT_INO, "unrelated").is_none(),
            "entries the plan did not name stay out"
        );
        // Two holders now: the base's last-name delete must keep the bytes the
        // fork still names, and the fork's delete then frees them.
        let base = MetaEngine::open(&meta.join("base.db")).unwrap();
        let docs = base.lookup(ROOT_INO, "docs").unwrap();
        assert_eq!(
            base.unlink(docs.id, "uuid-a").unwrap(),
            None,
            "the fork still names blob-a"
        );
        assert_eq!(
            lab.unlink(by.id, "M31_2025.fits").unwrap().as_deref(),
            Some("blob-a"),
            "last holder frees"
        );
    }

    /// Re-running a curation script must not blow up on the skeleton it made
    /// last time: `--skip-existing` merges into an existing directory and skips
    /// a colliding file, while a plain graft refuses the WHOLE plan so nothing
    /// is half-written.
    #[test]
    fn graft_conflicts_are_all_or_nothing_unless_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("base.db")).unwrap();
            let d = e
                .create_node(ROOT_INO, "d", true, None, 0o755, 1000, 1000)
                .unwrap();
            e.create_node(d, "x", false, Some("blob-x"), 0o644, 1000, 1000)
                .unwrap();
        }
        fork_view_with(dir.path(), "base", "lab", false).unwrap();
        let first = parse_graft_plan("d\tstage\n").unwrap();
        let g = graft_view(dir.path(), "lab", "base", &first, "", false).unwrap();
        assert_eq!((g.nodes, g.dirs, g.blobs), (2, 1, 1));

        // Re-run with an extra line: the existing dir is merged into, the
        // colliding file is skipped, the new name lands.
        let again = parse_graft_plan("d\tstage\nd/x\tstage/new.txt\n").unwrap();
        let g = graft_view(dir.path(), "lab", "base", &again, "", true).unwrap();
        assert_eq!(
            (g.nodes, g.dirs, g.skipped, g.blobs),
            (1, 0, 1, 1),
            "one new name, one skipped collision"
        );
        let lab = MetaEngine::open(&meta.join("lab.db")).unwrap();
        let stage = lab.lookup(ROOT_INO, "stage").unwrap();
        assert!(lab.lookup(stage.id, "new.txt").is_some());
        assert!(lab.lookup(stage.id, "x").is_some());

        // Without the flag, a conflict anywhere rolls back everything: the
        // first line's fresh name must not survive the second line's refusal.
        let bad = parse_graft_plan("d/x\tstage/fresh.txt\nd\tstage\n").unwrap();
        let err = graft_view(dir.path(), "lab", "base", &bad, "", false).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            lab.lookup(stage.id, "fresh.txt").is_none(),
            "a refused plan writes nothing at all"
        );
    }

    /// Two source names sharing one inode stay hard links after a graft (one
    /// inode, nlink 2), so a curated tree cannot silently duplicate a file it
    /// only meant to rename twice.
    #[test]
    fn graft_keeps_hard_links_inside_the_subset() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("base.db")).unwrap();
            let f = e
                .create_node(ROOT_INO, "a", false, Some("one"), 0o644, 1000, 1000)
                .unwrap();
            e.link_node(f, ROOT_INO, "b").unwrap();
        }
        fork_view_with(dir.path(), "base", "lab", false).unwrap();
        let items = parse_graft_plan("a\tp/a\nb\tq/b\n").unwrap();
        let g = graft_view(dir.path(), "lab", "base", &items, "", false).unwrap();
        assert_eq!(
            (g.nodes, g.dirs, g.blobs),
            (4, 2, 1),
            "two dirs + two names, one blob"
        );
        let lab = MetaEngine::open(&meta.join("lab.db")).unwrap();
        let p = lab.lookup(ROOT_INO, "p").unwrap();
        let a = lab.lookup(p.id, "a").unwrap();
        let q = lab.lookup(ROOT_INO, "q").unwrap();
        let b = lab.lookup(q.id, "b").unwrap();
        assert_eq!(a.nlink, 2, "one inode, two names after the graft");
        assert_eq!(b.nlink, 2);
        assert_eq!(a.data_ref, b.data_ref);
    }

    /// A typo deep in a plan refuses the whole thing (nothing is written), and
    /// a `dest` prefix creates its missing directories on the successful path.
    #[test]
    fn graft_is_atomic_on_unknown_source_and_creates_dest_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("base.db")).unwrap();
            e.create_node(ROOT_INO, "x", false, Some("blob-x"), 0o644, 1000, 1000)
                .unwrap();
        }
        fork_view_with(dir.path(), "base", "lab", false).unwrap();
        let items = parse_graft_plan("x\tok.txt\nmissing\tbad.txt\n").unwrap();
        let err = graft_view(dir.path(), "lab", "base", &items, "", false).unwrap_err();
        assert!(
            err.contains("'missing' does not exist in view 'base'"),
            "{err}"
        );
        let lab = MetaEngine::open(&meta.join("lab.db")).unwrap();
        assert!(
            lab.readdir(ROOT_INO).is_empty(),
            "the good line before the bad one was rolled back too"
        );

        let items = parse_graft_plan("x\n").unwrap();
        let g = graft_view(dir.path(), "lab", "base", &items, "stage/2026", false).unwrap();
        assert_eq!(
            (g.nodes, g.dirs, g.blobs),
            (3, 2, 1),
            "stage + 2026 + the file"
        );
        let stage = lab.lookup(ROOT_INO, "stage").unwrap();
        let y = lab.lookup(stage.id, "2026").unwrap();
        assert!(lab.lookup(y.id, "x").is_some(), "dest prefix is mkdir -p");
    }

    /// Refusals: no source tree, and never overwrite an existing target db.
    #[test]
    fn fork_refuses_missing_source_and_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        assert!(fork_view(dir.path(), "absent", "copy").is_err());
        {
            let _e = MetaEngine::open(&meta.join("base.db")).unwrap();
        }
        std::fs::write(meta.join("copy.db"), b"not a tree").unwrap();
        assert!(fork_view(dir.path(), "base", "copy").is_err());
        assert!(fork_view(dir.path(), "base", "base").is_err());
    }

    /// Export -> import round-trip: ids and inode numbers survive (hard links
    /// stay hard links), timestamps/mode/ownership travel, and the destination
    /// tree is a normal writable base view. Content bytes are NOT in the file:
    /// the `data_ref` keys are the only link to the store.
    #[test]
    fn export_import_roundtrip_preserves_tree_and_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let src_meta = dir.path().join("src/meta");
        std::fs::create_dir_all(&src_meta).unwrap();
        {
            let e = MetaEngine::open(&src_meta.join("team.db")).unwrap();
            let d = e
                .create_node(ROOT_INO, "docs", true, None, 0o755, 1000, 1001)
                .unwrap();
            let f = e
                .create_node(d, "a.txt", false, Some("blob-a"), 0o640, 1000, 1001)
                .unwrap();
            e.link_node(f, d, "b.txt").unwrap();
            e.create_symlink(ROOT_INO, "link", "docs/a.txt", 1000, 1001)
                .unwrap();
            e.set_times(f, Some(111), Some(222));
        }
        let out = dir.path().join("team.jsonl");
        let st = export_view(&dir.path().join("src"), "team", &out, false).unwrap();
        assert_eq!(st.nodes, 5, "root + docs + two names + symlink");
        assert_eq!(st.blobs, 1);
        assert_eq!(st.kind, "physical");
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text.lines().count(), 6, "header + five node rows");
        assert!(text.contains("\"format\":\"grovefs-view-metadata\""));
        assert!(text.contains("\"blobs\":1"));

        let dst_meta = dir.path().join("dst/meta");
        std::fs::create_dir_all(&dst_meta).unwrap();
        let ist = import_view(&dir.path().join("dst"), "copy", &out, false).unwrap();
        assert_eq!((ist.nodes, ist.blobs), (5, 1));
        assert!(!ist.replaced);
        let copy = MetaEngine::open(&dst_meta.join("copy.db")).unwrap();
        let docs = copy.lookup(ROOT_INO, "docs").expect("dir imported");
        let a = copy.lookup(docs.id, "a.txt").expect("file imported");
        let b = copy.lookup(docs.id, "b.txt").expect("hard link imported");
        // `NodeInfo.id` IS the inode number: equal ids prove the two names still
        // name one inode. Both directory entries keep their exported row ids.
        assert_eq!(
            (a.id, b.id),
            (3, 3),
            "one inode, two names stays a hard link"
        );
        let rows: i64 = copy
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM node WHERE id IN (3,4)", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2, "row ids survive the trip");
        assert_eq!(a.nlink, 2);
        assert_eq!(a.data_ref.as_deref(), Some("blob-a"));
        assert_eq!((a.mode, a.uid, a.gid), (0o640, 1000, 1001));
        assert_eq!((a.atime, a.mtime), (111, 222));
        let l = copy.lookup(ROOT_INO, "link").expect("symlink imported");
        assert_eq!(l.link_target.as_deref(), Some("docs/a.txt"));
    }

    /// Export/import guards: no source tree, no silent overwrite of the export
    /// file, and an existing target tree requires the explicit `replace` (an
    /// offline migration choice, never the default).
    #[test]
    fn export_import_refuse_silent_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("team.db")).unwrap();
            e.create_node(ROOT_INO, "f", false, Some("k"), 0o644, 1000, 1000)
                .unwrap();
        }
        let out = dir.path().join("team.jsonl");
        assert!(export_view(dir.path(), "ghost", &out, false).is_err());
        export_view(dir.path(), "team", &out, false).unwrap();
        assert!(
            export_view(dir.path(), "team", &out, false).is_err(),
            "an existing export file must not be silently overwritten"
        );
        export_view(dir.path(), "team", &out, true).unwrap();
        assert!(
            import_view(dir.path(), "team", &out, false).is_err(),
            "an existing target tree needs --replace"
        );
        let st = import_view(dir.path(), "team", &out, true).unwrap();
        assert!(st.replaced);
        assert_eq!(st.nodes, 2, "root + f");
    }

    /// A hand-edited/corrupt export is rejected BEFORE the target is replaced:
    /// unknown format/version, dangling parents and a header node count that
    /// disagrees with the body all fail the transaction.
    #[test]
    fn import_rejects_bad_header_dangling_parent_and_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("meta")).unwrap();
        let bad = dir.path().join("bad.jsonl");
        std::fs::write(&bad, "{\"format\":\"other\",\"version\":1}\n").unwrap();
        assert!(import_view(dir.path(), "v", &bad, false).is_err());
        std::fs::write(
            &bad,
            "{\"format\":\"grovefs-view-metadata\",\"version\":99}\n",
        )
        .unwrap();
        assert!(import_view(dir.path(), "v", &bad, false).is_err());

        let dangling = format!(
            "{{\"format\":\"grovefs-view-metadata\",\"version\":1,\"kind\":\"physical\",\"nodes\":2}}\n{}\n{}\n",
            serde_json::json!({"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1}),
            serde_json::json!({"id":2,"ino":2,"parent_id":99,"name":"f","is_dir":0})
        );
        std::fs::write(&bad, dangling).unwrap();
        let err = import_view(dir.path(), "v", &bad, false).unwrap_err();
        assert!(err.contains("missing or non-directory parent"), "{err}");

        let mismatch = format!(
            "{{\"format\":\"grovefs-view-metadata\",\"version\":1,\"nodes\":9}}\n{}\n",
            serde_json::json!({"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1})
        );
        std::fs::write(&bad, mismatch).unwrap();
        let err = import_view(dir.path(), "v2", &bad, false).unwrap_err();
        assert!(err.contains("header declares 9"), "{err}");
    }

    /// A failed `replace` must leave the PREVIOUS tree installed: the import is
    /// built beside the destination and renamed only after every row and check
    /// passed, so there is no window in which the old tree is gone.
    #[test]
    fn failed_replace_keeps_the_previous_tree_and_leaves_no_scratch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("meta")).unwrap();
        let good = dir.path().join("good.jsonl");
        let text = format!(
            "{{\"format\":\"grovefs-view-metadata\",\"version\":1,\"nodes\":2}}\n{}\n{}\n",
            serde_json::json!({"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1}),
            serde_json::json!({"id":2,"ino":2,"parent_id":1,"name":"keep","is_dir":0,"data_ref":"k"})
        );
        std::fs::write(&good, text).unwrap();
        import_view(dir.path(), "v", &good, false).unwrap();

        let bad = dir.path().join("bad.jsonl");
        std::fs::write(
            &bad,
            format!(
                "{{\"format\":\"grovefs-view-metadata\",\"version\":1}}\n{}\n{}\n",
                serde_json::json!({"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1}),
                serde_json::json!({"id":2,"ino":2,"parent_id":99,"name":"x","is_dir":0})
            ),
        )
        .unwrap();
        let err = import_view(dir.path(), "v", &bad, true).unwrap_err();
        assert!(err.contains("missing or non-directory parent"), "{err}");

        let e = MetaEngine::open(&dir.path().join("meta/v.db")).unwrap();
        assert_eq!(e.lookup(ROOT_INO, "keep").unwrap().id, 2);
        let scratch: Vec<_> = std::fs::read_dir(dir.path().join("meta"))
            .unwrap()
            .flatten()
            .filter(|x| x.file_name().to_string_lossy().contains(".import-"))
            .collect();
        assert!(scratch.is_empty(), "scratch trees left behind: {scratch:?}");
    }

    /// Within one view, the second name for a key must not double-book the
    /// ledger (count 0->1 only): unlinking one of two hard links frees nothing,
    /// unlinking the last does.
    #[test]
    fn same_view_hardlink_books_ledger_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = MetaEngine::open(&dir.path().join("a.db")).unwrap();
        let ino = a
            .create_node(ROOT_INO, "f", false, Some("k"), 0o644, 1000, 1000)
            .unwrap();
        a.link_node(ino, ROOT_INO, "h").unwrap();
        assert_eq!(
            a.unlink(ROOT_INO, "f").unwrap().as_deref(),
            None,
            "one name still refers"
        );
        assert_eq!(
            a.unlink(ROOT_INO, "h").unwrap().as_deref(),
            Some("k"),
            "last name frees"
        );
    }

    /// A view db removed out-of-band never decremented its hold, so the ledger
    /// over-counts and would keep a blob forever. `rebuild_ledger` rewrites
    /// holders from the trees that still exist, unblocking the GC.
    #[test]
    fn rebuild_ledger_clears_stale_holders() {
        let dir = tempfile::tempdir().unwrap();
        // Match production layout: view dbs and refs.db all live under <base>/meta.
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        {
            let a = MetaEngine::open(&meta.join("a.db")).unwrap();
            a.create_node(ROOT_INO, "f", false, Some("k"), 0o644, 1000, 1000)
                .unwrap();
        } // drop a's engine so its db file can be removed
        std::fs::remove_file(meta.join("a.db")).unwrap();
        // Stale holder for "k" lingers from the deleted view; rebuild scans the
        // surviving trees (none reference k) and rewrites the ledger empty.
        let counted = rebuild_ledger(dir.path()).unwrap();
        assert_eq!(counted, 0, "no surviving view references k");
        // A fresh view that creates then unlinks the key now frees it (the
        // orphaned holder row no longer suppresses GC).
        let c = MetaEngine::open(&meta.join("c.db")).unwrap();
        c.create_node(ROOT_INO, "z", false, Some("k"), 0o644, 1000, 1000)
            .unwrap();
        assert_eq!(
            c.unlink(ROOT_INO, "z").unwrap().as_deref(),
            Some("k"),
            "ledger rebuilt -> GC works"
        );
    }
}
