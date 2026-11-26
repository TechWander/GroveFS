//! Metadata engine: the per-view **view tree** that makes "different users see
//! different file lists" possible, plus custom folder hierarchy.
//!
//! The physical bytes live in the [`crate::backend::StorageBackend`]; this
//! module only stores *structure*: which nodes exist, how they nest, and what a
//! file node points to. Node kinds:
//! - directory          : is_dir=1
//! - regular file       : is_dir=0, data_ref=Some(key), link_target=None
//! - symbolic link      : is_dir=0, data_ref=None,   link_target=Some(path)
//!
//! Hard links share one `data_ref`; inside a view the object is GC'd only when
//! the last node referencing it disappears (see [`MetaEngine::unlink`]), and
//! ACROSS views a shared key (two views holding names for one `data_ref`, e.g.
//! the same source imported twice) is guarded by the cross-view holder ledger
//! in `<base>/meta/refs.db` (see the ledger section below) — a blob is only
//! physically freeable when its last name in a view goes AND no other view
//! holds it. Because a client only ever queries its own view DB, files it
//! cannot see are never materialized on the network — isolation is enforced
//! here, not by permission bits.
//!
//! Inode model: a FUSE inode number == `node.ino`. Root is always ino 1.
//! Multiple directory entries (rows) may share one `ino` — that *is* a hard
//! link: they name the same inode and therefore the same data object, the same
//! size and the same timestamps. Fresh files/dirs get `ino == row id`.

// The client-facing half of this module (types + [`MetaProvider`] + [`MetaError`])
// needs only `std::sync::Arc` for the fence-handle default, referenced inline as
// `std::sync::Arc`. The heavy `rusqlite`/`serde_json`/`std::fs` imports live with
// the native-only `engine` submodule below.

pub const ROOT_INO: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    Regular,
    Symlink,
}

/// POSIX record lock type (matches FUSE `l_type` after kernel translation for
/// both `fcntl F_SETLK` and BSD `flock(2)` — the kernel collapses them into
/// F_RDLCK/F_WRLCK/F_UNLCK before sending FUSE_LK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    /// Shared (F_RDLCK / LOCK_SH).
    Read,
    /// Exclusive (F_WRLCK / LOCK_EX).
    Write,
    /// Release the interval for this owner (F_UNLCK / LOCK_UN).
    Unlock,
}

impl LockType {
    /// Wire code used by the meta protocol and the SQLite `ltype` column.
    /// Kept stable so primary/standby snapshots interop across versions.
    pub fn to_u8(self) -> u8 {
        match self {
            LockType::Read => 0,
            LockType::Write => 1,
            LockType::Unlock => 2,
        }
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(LockType::Read),
            1 => Some(LockType::Write),
            2 => Some(LockType::Unlock),
            _ => None,
        }
    }
    /// Wire value for `struct flock.l_type` (kernel FUSE passes it through).
    pub fn from_fuse_i32(v: i32) -> Option<Self> {
        // F_RDLCK=0, F_WRLCK=1, F_UNLCK=2 on Linux.
        match v {
            0 => Some(LockType::Read),
            1 => Some(LockType::Write),
            2 => Some(LockType::Unlock),
            _ => None,
        }
    }
    pub fn to_fuse_i32(self) -> i32 {
        match self {
            LockType::Read => 0,
            LockType::Write => 1,
            LockType::Unlock => 2,
        }
    }
}

/// The lock currently blocking our requested interval, reported by F_GETLK
/// so the caller can render `struct flock` for the application.
#[derive(Debug, Clone, Copy)]
pub struct LockConflict {
    pub start: u64,
    pub end: u64,
    pub ty: LockType,
    pub pid: u32,
}

/// Default POSIX advisory-lock lease. A lock row's `expires_at` is refreshed by
/// the owning mount daemon's renewal heartbeat (`renew_locks`) every
/// `secs()/3`, so a *live* holder never hits this deadline. The lease is the
/// backstop for a holder that died WITHOUT renewing (daemon crash, network
/// partition, SIGKILL before `release`): with renewal in place the lease can be
/// short — 15s bounds the post-crash dangling window (down from the old 60s)
/// while a healthy holder is invisible to it. Operators/tests may override via
/// `GROVEFS_LOCK_LEASE_SECS`.
pub const LOCK_LEASE_SECS: i64 = 15;

/// Runtime lease knob. The default is `LOCK_LEASE_SECS`. Test suites and
/// operators may lower/raise it by exporting `GROVEFS_LOCK_LEASE_SECS=<secs>`
/// before spawning `mount` / `meta-server` — the CLI surface stays unchanged
/// (an internal knob, not a supported tuning), so e2e tests can exercise the
/// SIGKILL-then-expire path without sleeping 60s. Cached on first read.
pub struct LockLease;

impl LockLease {
    fn slot() -> &'static std::sync::atomic::AtomicI64 {
        static V: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
        &V
    }
    pub fn secs() -> i64 {
        let cur = Self::slot().load(std::sync::atomic::Ordering::Relaxed);
        if cur > 0 {
            return cur;
        }
        let v = std::env::var("GROVEFS_LOCK_LEASE_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(LOCK_LEASE_SECS);
        Self::slot().store(v, std::sync::atomic::Ordering::Relaxed);
        v
    }
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub kind: NodeKind,
    pub data_ref: Option<String>,
    pub link_target: Option<String>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub nlink: u64,
    pub size: u64,
}

impl NodeInfo {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
}

// ===================== cross-view blob reference ledger ====================
//
// A `data_ref` names ONE backend object, but several physical views can hold
// names for the same key: `grovefs import` of the same source file into two
// views resolves to the same content key, and an operator can seed two trees
// over shared objects. The per-view `node.data_ref` refcount can only see names
// inside its own db — left alone, a delete in view A would hand the caller a
// freed key and GC the bytes view B still lists. So bookkeeping lives beside
// the view trees, in `<base>/meta/refs.db`: one row per data_ref that SOME view
// holds with its last name, counting how many VIEWS hold it. A blob is only
// physically freeable when its last name in a view is removed AND no other view
// holds a name (see [`ledger_remove`]).
//
// Mechanics: every view engine ATTACHes the sibling `refs.db` as schema `refs`
// at open, so the ledger row moves inside the SAME IMMEDIATE transaction as the
// node INSERT/DELETE it books. Both files lock at BEGIN and commit together, but
// note SQLite's own caveat: with the main db in WAL mode a transaction spanning
// attached dbs is atomic per database, NOT as a set (no master journal). A crash
// inside that window can therefore land the node row without the holder row;
// `grovefs refs rebuild` is the repair path (see docs §13.3).
// Deadlock control: `BEGIN IMMEDIATE` takes the write lock of EVERY attached
// db at start, so each ledger-touching txn locks `refs.db` first and its view
// db second in one fixed global order — cycles are structurally impossible
// even with WAL's non-detecting busy handler; `busy_timeout` just queues the
// losers. Within one process several engines (several mutexes) share refs.db,
// so ledger writers additionally take [`LEDGER_SERIAL`] BEFORE their engine
// lock (a process-wide order; cross-process order is the db-lock order).
//
// Staleness boundary: a view db deleted out-of-band (`rm <view>.db`, a fresh
// `--home`) never decrements its holders — the ledger over-counts, blobs are
// kept too long, and `grovefs refs rebuild` rewrites the ledger from the trees
// that actually exist. A key with no ledger row is simply private to one view
// (no other view known -> free on last name).

/// The shared ledger's stem; its home is [`control_db_path`] (`refs.db`).
pub const REFS_DB: &str = "refs.db";
/// The schema name every engine connection ATTACHes the ledger under.
pub const REFS_SCHEMA: &str = "refs";

/// The stems of the CONTROL-PLANE stores rather than view trees: the
/// api-key/audit store, the web console's account/grant store, and the holder
/// ledger itself. Since layout v2 they live in their OWN slot
/// ([`CONTROL_SUBDIR`]) instead of beside the trees, so no enumerator has to
/// know this list to tell a tree from a store — the question is answered by
/// node kind ([`is_view_tree_dir`]). The list stays because the migrator must
/// still recognize these stems when it meets the flat v1 layout, and because
/// [`is_view_db`] is the v1-side twin of that check.
pub const CONTROL_DB_STEMS: &[&str] = &["api_keys", "refs", "web_accounts"];

/// Is `name` a FLAT (layout v1) `meta/*.db` entry holding a view tree? Anything
/// that is not a `.db`, or is one of [`CONTROL_DB_STEMS`], is not. Only the
/// migrator and the layout guard need this now; live code paths ask
/// [`is_view_tree_dir`] about a directory instead.
pub fn is_view_db(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".db") else {
        return false;
    };
    !stem.is_empty() && !CONTROL_DB_STEMS.contains(&stem)
}

// ================== on-disk layout v2: one dir per view tree =================
//
// Layout v1 put every object of a store side by side in `<base>/meta/`: one
// `<view>.db` per physical tree, the three control-plane stores as three more
// `.db` files, and the definition slots under `views/`. That made "is this
// entry a view?" unanswerable from the structure, so the question lived in a
// name whitelist ([`CONTROL_DB_STEMS`]) that all five enumerators had to
// remember independently — and a store added without editing every one of them
// shows up as a MOUNTABLE VIEW and makes `refs rebuild` and `gc` fail on its
// missing `node` table (that is exactly what happened when `web_accounts`
// landed). One more store, or one more enumerator, was one more such bug.
//
// Layout v2 separates the kinds by NODE KIND instead of by name:
//
// ```text
// <base>/meta/
//   <view>/            one directory per PHYSICAL TREE, so the tree and its
//     <view>.db        WAL/SHM sidecars and its import scratch are one unit
//   _control/          the control-plane stores (refs/api_keys/web_accounts)
//   views/             view definitions, `<name>.layout` (unchanged)
//   .xfer/             server-side export/import spool (dot-prefixed)
// ```
//
// The tree's db keeps its OWN name inside its directory, so the path stays
// `f(name)`: computable from the name alone, with no definition read first.
// That is the deliberate boundary of this change. Grouping by GENEALOGY (a
// base view's folder holding its forks) would instead make the path depend on
// the definition, and the definition (a) is not replicated to an HA standby,
// which has to resolve a snapshot target from a name alone, (b) can be removed
// while the tree it names still exists, and (c) is already readable from
// `view list`. Family placement would encode a relationship a second time, in
// the one place that cannot be re-derived once it drifts.
//
// What v2 buys: whole-tree operations (archive, move, delete, `chmod`, quota,
// "which files belong to this view") become directory operations; a listing of
// `meta/` is a listing of trees; and a control store can never masquerade as a
// view again. It buys nothing for the CONTENT bytes, which are
// content-addressed and shared across views in `<data-dir>/blocks/` — a tree
// directory is a metadata unit, not a restorable data unit.
//
// ================== on-disk layout v3: family placement =====================
//
// Layout v3 keeps v2's node-kind separation and adds GENEALOGY to the
// placement: a fork's tree and a derived/alias definition live INSIDE the
// directory of the base view they belong to, so `ls meta/` reads as a forest
// of families instead of an anonymous pile. The old objection (the path would
// depend on a definition the HA standby does not hold) is answered by making
// placement FINDABLE FROM THE FILESYSTEM: every tree db is still its own
// directory's namesake (`<name>/<name>.db`), so the resolvers below scan for
// that shape instead of consulting a definition, and `M_PUSH` carries the
// resolved relative home so a standby installs a replicated fork beside its
// base rather than at the top level. Definitions moved with their family stay
// discoverable through [`find_def_slot`]; nothing re-derives a path from a
// name alone any more.
//
// ```text
// <base>/meta/
//   <view>/              base tree, unchanged
//     <view>.db
//     <fork>/            NESTED (the default): a fork's whole family dir
//       <fork>.db         sits inside its base directory
//       <fork>.layout     a fork's definition lives WITH ITS OWN TREE, so the
//     <bias>.layout       unit (tree + sidecars + def) is one directory
//     <lens>.layout       alias/lens definitions sit beside the base
//     forks/             GROUPED (opt-in): the same members under
//       <fork>/...       reserved separator slots instead of loose
//         <fork>.layout
//     views/
//       <bias>.layout    (names carry their kind: `bias_*` aliases,
//                        `lens_*` derived views — the file name alone
//                        states purpose and family)
//   views/               top-level definition slots (flat policy, and every
//                        pre-v3 definition that was never re-homed)
//   _control/  .xfer/    unchanged
// ```
//
// The policy is one line in `meta/.family` (`flat` | `nested` | `grouped`;
// absent means `nested`). Placement decides where NEW members land; every
// READ resolves by scanning, so a store with pre-v3 (flat) members keeps
// working untouched and a policy flip never orphans an existing tree.

/// The metadata half of a store root. Naming it (rather than only spelling it in
/// [`meta_dir`]) is what [`control_dir_for_db`] needs to recognize a `meta/`.
pub const META_SUBDIR: &str = "meta";
/// The definition slot inside `<base>/meta/`.
pub const DEFS_SUBDIR: &str = "views";
/// The control-plane slot inside `<base>/meta/`. Leading underscore keeps it
/// lexicographically apart from tree names; the reserved-name check makes the
/// separation structural rather than cosmetic.
pub const CONTROL_SUBDIR: &str = "_control";
/// The export/import spool (dot-prefixed, so it is never a tree).
pub const XFER_SUBDIR: &str = ".xfer";
/// The grouped-family separator slot inside a base view's directory: with
/// policy `grouped`, fork family dirs live under `<base>/meta/<B>/forks/`
/// instead of loose beside the base tree. Reserved as a view name so a fork
/// literally called `forks` can never be ambiguous with the separator.
pub const FORKS_SUBDIR: &str = "forks";
/// Names that address a layout slot and can therefore never be a view name.
/// [`crate::layout::valid_view_name`] refuses these, which is what lets
/// [`is_view_tree_dir`] answer by node kind alone.
pub const RESERVED_VIEW_NAMES: &[&str] = &[DEFS_SUBDIR, CONTROL_SUBDIR, FORKS_SUBDIR];
/// The one-line family-placement marker inside `meta/` (dot-prefixed, so it
/// is never a tree and never trips [`legacy_flat_entries`]).
pub const FAMILY_POLICY_FILE: &str = ".family";
/// The files one tree owns: main db, WAL, shared-memory sidecar.
pub const TREE_DB_EXTS: &[&str] = &["", "-wal", "-shm"];

use std::path::{Path, PathBuf};

/// `<base>/meta` — the metadata half of a store root.
pub fn meta_dir(base: &Path) -> PathBuf {
    base.join(META_SUBDIR)
}

/// `<base>/meta/views` — the view-definition slots.
pub fn defs_dir(base: &Path) -> PathBuf {
    meta_dir(base).join(DEFS_SUBDIR)
}

/// `<base>/meta/_control` — where the control-plane stores live.
pub fn control_dir(base: &Path) -> PathBuf {
    meta_dir(base).join(CONTROL_SUBDIR)
}

/// One control-plane store, e.g. `<base>/meta/_control/refs.db`.
pub fn control_db_path(base: &Path, stem: &str) -> PathBuf {
    control_dir(base).join(format!("{stem}.db"))
}

/// The control plane that OWNS a given metadata db, found by walking up to the
/// store's `meta/` directory instead of by counting parents.
///
/// Counting parents looks identical and is not. The layout puts exactly one file
/// inside a tree home that is deliberately not the tree — the `import` scratch
/// db — and for that file "two levels up" is the tree directory itself, which
/// would hand the scratch its OWN ledger. Holder bookkeeping that two processes
/// believe they share has to be one file, so the store is identified by name,
/// not by depth.
///
/// A db that is under no `meta/` at all is not part of a store: `MetaEngine`
/// tests and any caller that opens a standalone file get their ledger beside it,
/// which is what such a file had before this layout existed.
pub fn control_dir_for_db(db: &Path) -> PathBuf {
    let mut up = db.parent();
    while let Some(dir) = up {
        if dir.file_name().and_then(|n| n.to_str()) == Some(META_SUBDIR) {
            return dir.join(CONTROL_SUBDIR);
        }
        up = dir.parent();
    }
    db.parent().unwrap_or(Path::new(".")).join(CONTROL_SUBDIR)
}

/// The store-wide holder ledger a given metadata db belongs to — the single
/// place that answers "which refs.db does this tree write bookkeeping into?".
pub fn refs_ledger_for_db(db: &Path) -> PathBuf {
    control_dir_for_db(db).join(REFS_DB)
}

/// `<base>/meta/.xfer` — the spool for staged exports/imports.
pub fn xfer_dir(base: &Path) -> PathBuf {
    meta_dir(base).join(XFER_SUBDIR)
}

/// Where a NEW family member (fork tree, derived/alias definition) of one base
/// view lands. `Flat` keeps the v2 top-level placement; `Nested` (the default)
/// puts the member directly inside the base's directory; `Grouped` puts it
/// under the base's reserved `forks/` / `views/` separator slots so a base with
/// many members still lists cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyLayout {
    Flat,
    Nested,
    Grouped,
}

impl FamilyLayout {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "flat" => Some(Self::Flat),
            "nested" => Some(Self::Nested),
            "grouped" => Some(Self::Grouped),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Nested => "nested",
            Self::Grouped => "grouped",
        }
    }
}

/// The store's current placement policy, read from `meta/.family` on every
/// call (it is one stat + one tiny read beside paths this module already
/// touched; caching would add a staleness class to save nanoseconds).
/// An absent or unparseable marker means `Nested` — the v3 default.
pub fn family_policy(base: &Path) -> FamilyLayout {
    match std::fs::read_to_string(meta_dir(base).join(FAMILY_POLICY_FILE)) {
        Ok(t) => FamilyLayout::parse(&t).unwrap_or(FamilyLayout::Nested),
        Err(_) => FamilyLayout::Nested,
    }
}

/// Record the placement policy for NEW family members. Never moves anything
/// that already exists — reads resolve by scanning regardless of policy.
pub fn set_family_policy(base: &Path, mode: FamilyLayout) -> std::io::Result<()> {
    std::fs::create_dir_all(meta_dir(base))?;
    std::fs::write(
        meta_dir(base).join(FAMILY_POLICY_FILE),
        format!("{}\n", mode.as_str()),
    )
}

/// The home of ONE physical view tree. Resolves first — a base tree lives at
/// `<base>/meta/<view>`, a family-nested fork at `<base>/meta/<parent>/<view>`
/// or `<base>/meta/<parent>/forks/<view>` — and falls back to the top-level
/// shape for a tree that does not exist yet (the `view create` case).
pub fn view_dir(base: &Path, view: &str) -> PathBuf {
    find_view_db(base, view)
        .unwrap_or_else(|| meta_dir(base).join(view).join(format!("{view}.db")))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| meta_dir(base).join(view))
}

/// Where a tree db that does not exist yet would be FRESHLY placed, given its
/// base view (`None` = a new top-level base tree). Creation sites —
/// [`fork_view_with`] in the engine, the import/rename arms — pass the family
/// context here so the v3 placement decision has exactly one implementation.
pub fn new_tree_db_path(base: &Path, view: &str, parent: Option<&str>) -> PathBuf {
    let m = meta_dir(base);
    // The parent resolves through the same scan: a base created before v3 (or
    // re-homed by hand) nests at wherever it ACTUALLY lives, not at a
    // recomputed top-level guess.
    match (parent.map(|p| view_dir(base, p)), family_policy(base)) {
        (Some(pd), FamilyLayout::Nested) => pd.join(view).join(format!("{view}.db")),
        (Some(pd), FamilyLayout::Grouped) => {
            pd.join(FORKS_SUBDIR).join(view).join(format!("{view}.db"))
        }
        _ => m.join(view).join(format!("{view}.db")),
    }
}

/// [`new_tree_db_path`] plus `create_dir_all` of its directory.
pub fn ensure_tree_db_path(
    base: &Path,
    view: &str,
    parent: Option<&str>,
) -> std::io::Result<PathBuf> {
    let path = new_tree_db_path(base, view, parent);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(path)
}

/// The same location spelled the way an operator reads it in a message or a
/// JSON field — resolved, so a family-nested fork reports
/// `meta/<parent>/<view>/<view>.db` and not a top-level guess. Built from the
/// same scan as [`view_db_path`]; a pretty string that quietly drifts from the
/// real path is how documentation starts lying. Falls back to the fresh
/// top-level spelling for a tree that does not exist yet.
pub fn tree_home_rel(base: &Path, view: &str) -> String {
    let real = view_db_path(base, view);
    match real.strip_prefix(base) {
        Ok(rel) => rel
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => real.display().to_string(),
    }
}

/// The store-relative DIRECTORY of a tree's home (no file name), for the
/// `M_PUSH` replication frame: `""` for a top-level tree, `<parent>/<fork>`
/// or `<parent>/forks/<fork>` for a family member. An empty string is also
/// what an old-format push omits — the receiver then falls back to `f(name)`.
pub fn tree_home_rel_dir(base: &Path, view: &str) -> String {
    let dir = view_dir(base, view);
    match dir.strip_prefix(meta_dir(base)) {
        Ok(rel) => rel
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => String::new(),
    }
}

/// A definition slot THIS STORE HOLDS for `name`, found by scanning: the
/// top-level `meta/views/<name>.layout`, a tree's own directory
/// (`meta/<name>/<name>.layout` flat, `meta/<base>/<name>/<name>.layout`
/// nested, `meta/<base>/forks/<name>/<name>.layout` grouped — a fork's
/// definition travels WITH its tree), a family-nested
/// `meta/<base>/<name>.layout` (alias/lens defs, and pre-convention forks),
/// or a grouped `meta/<base>/views/<name>.layout` (both separator spellings
/// are probed regardless of the current policy — the policy decides NEW
/// placement only). `None` = no definition file for this name anywhere in
/// the store.
pub fn find_def_slot(base: &Path, name: &str) -> Option<PathBuf> {
    let m = meta_dir(base);
    let top = m.join(DEFS_SUBDIR).join(format!("{name}.layout"));
    if top.is_file() {
        return Some(top);
    }
    // A flat fork's def inside its OWN tree directory (a base tree at
    // `meta/<name>/` may hold one too; the scan below proves it is a def by
    // content callers parse, never by location alone).
    let own = m.join(name).join(format!("{name}.layout"));
    if own.is_file() {
        return Some(own);
    }
    let rd = std::fs::read_dir(&m).ok()?;
    for ent in rd.flatten() {
        let dir = ent.path();
        let Some(bname) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !is_view_tree_dir(&bname) || !dir.is_dir() {
            continue;
        }
        let cand = dir.join(name).join(format!("{name}.layout"));
        if cand.is_file() {
            return Some(cand);
        }
        let cand = dir
            .join(FORKS_SUBDIR)
            .join(name)
            .join(format!("{name}.layout"));
        if cand.is_file() {
            return Some(cand);
        }
        let cand = dir.join(format!("{name}.layout"));
        if cand.is_file() {
            return Some(cand);
        }
        let cand = dir.join(DEFS_SUBDIR).join(format!("{name}.layout"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Where a NEW definition for `name` lands: beside its base tree's home
/// (`parent` = the base view the definition names), or at the top-level
/// `meta/views/` when there is no family or the policy is `flat`. ALIAS and
/// DERIVED defs only — a FORK's def belongs inside its own tree directory,
/// which is [`fork_def_slot`].
pub fn new_def_slot(base: &Path, name: &str, parent: Option<&str>) -> PathBuf {
    match (parent, family_policy(base)) {
        (Some(p), FamilyLayout::Nested) => view_dir(base, p).join(format!("{name}.layout")),
        (Some(p), FamilyLayout::Grouped) => view_dir(base, p)
            .join(DEFS_SUBDIR)
            .join(format!("{name}.layout")),
        _ => defs_dir(base).join(format!("{name}.layout")),
    }
}

/// Where a FORK's definition lands: in the SAME directory as the fork's own
/// tree db, so the tree and the name that makes it mountable are one unit
/// (`meta/<base>/<fork>/<fork>.layout` nested, `meta/<base>/forks/<fork>/…`
/// grouped, `meta/<fork>/<fork>.layout` flat). The tree directory is
/// resolved by the same scan as [`view_dir`], so a fork created before this
/// convention keeps updating its slot in place; a not-yet-created tree falls
/// back to the flat own-directory spelling like [`view_dir`] itself does.
pub fn fork_def_slot(base: &Path, name: &str) -> PathBuf {
    view_dir(base, name).join(format!("{name}.layout"))
}

/// The tree db THIS STORE ACTUALLY HOLDS for `view`, found by scanning
/// instead of by recomputing one formula. A name is a tree when exactly one
/// directory contains a namesake `<name>.db` inside it: the top level
/// (`meta/<name>/<name>.db`, a base tree), directly in a base's directory
/// (`meta/<B>/<name>/<name>.db`, nested) or under its `forks/` separator
/// (`meta/<B>/forks/<name>/<name>.db`, grouped). `None` = this store holds no
/// such tree.
pub fn find_view_db(base: &Path, view: &str) -> Option<PathBuf> {
    let m = meta_dir(base);
    let top = m.join(view).join(format!("{view}.db"));
    if top.is_file() {
        return Some(top);
    }
    let rd = std::fs::read_dir(&m).ok()?;
    for ent in rd.flatten() {
        let dir = ent.path();
        let Some(bname) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !is_view_tree_dir(&bname) || !dir.is_dir() {
            continue;
        }
        // nested member: meta/<B>/<view>/<view>.db
        let cand = dir.join(view).join(format!("{view}.db"));
        if cand.is_file() {
            return Some(cand);
        }
        // grouped member: meta/<B>/forks/<view>/<view>.db
        let cand = dir.join(FORKS_SUBDIR).join(view).join(format!("{view}.db"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// The tree's SQLite db. Resolves through [`find_view_db`]; a name this store
/// does not hold yields the FRESH top-level location (which is what a
/// `view create` of a new base view wants, and a missing fork must be
/// refused by its reader before any path is trusted — the same
/// `is_file() ? open : Err` arms the engine and the server already run).
/// The stem repeats the directory name on purpose — an enumerator that finds
/// `lab/lab.db` learns the view name from the FILE as well as the directory,
/// so a half-moved or hand-planted tree cannot silently change identity.
pub fn view_db_path(base: &Path, view: &str) -> PathBuf {
    find_view_db(base, view).unwrap_or_else(|| meta_dir(base).join(view).join(format!("{view}.db")))
}

/// The same tree db with a SQLite sidecar suffix (`""`, `"-wal"`, `"-shm"`).
pub fn view_db_with_ext(base: &Path, view: &str, ext: &str) -> PathBuf {
    view_dir(base, view).join(format!("{view}.db{ext}"))
}

/// The FLAT (v1) location of the same tree, `<base>/meta/<view>.db` — only the
/// guard and the migrator speak this.
pub fn legacy_db_path(base: &Path, view: &str) -> PathBuf {
    meta_dir(base).join(format!("{view}.db"))
}

/// Is a `<base>/meta/` entry name the home of a physical view tree? Structural:
/// a name that is a legal view name and not a reserved slot. Dot-prefixed
/// entries (the spool, scratch files) are never trees.
pub fn is_view_tree_dir(name: &str) -> bool {
    !name.starts_with('.')
        && !RESERVED_VIEW_NAMES.contains(&name)
        && crate::layout::valid_view_name(name)
}

/// THE enumerator: names of the physical trees this SQLite store holds,
/// sorted. Everything that asks "which views exist on disk" goes through here
/// (the ledger rebuild, both GC passes, `view list`, the server's epoch scan),
/// so a fourth control store, a spool directory or a stray sidecar cannot be
/// mistaken for a tree by one call site and not another.
///
/// A directory counts as a tree only when it actually holds `<name>.db`; an
/// empty or half-created directory is reported as absent rather than as a
/// corrupt view, which is what makes a failed `view create` or an interrupted
/// migration visible as "no such view" instead of as a mountable garbage name.
///
/// Since layout v3 the walk descends one family layer: a base directory's
/// nested member (`meta/<B>/<F>/<F>.db`) and its grouped members
/// (`meta/<B>/forks/<F>/<F>.db`) enumerate exactly like top-level trees —
/// names are unique across the store (creation refuses duplicates), so the
/// result stays a flat, sorted name list.
pub fn list_view_trees(base: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |name: String, out: &mut Vec<String>| {
        if !out.contains(&name) {
            out.push(name);
        }
    };
    if let Ok(rd) = std::fs::read_dir(meta_dir(base)) {
        for ent in rd.flatten() {
            let dir = ent.path();
            let name = match dir.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !is_view_tree_dir(&name) || !dir.is_dir() {
                continue;
            }
            if dir.join(format!("{name}.db")).is_file() {
                push(name.clone(), &mut out);
            }
            // Family layer: subdirectories of a base directory. A nested
            // member sits directly here (`<F>/<F>.db`); the grouped spelling
            // adds one more level behind the `forks` separator. `views` and
            // `_control` are reserved, so definition files and control stores
            // cannot enter the walk.
            if let Ok(rd2) = std::fs::read_dir(&dir) {
                for e2 in rd2.flatten() {
                    let d2 = e2.path();
                    let Some(x) = d2.file_name().and_then(|n| n.to_str()).map(str::to_string)
                    else {
                        continue;
                    };
                    if !d2.is_dir() {
                        continue;
                    }
                    // The grouped separator is not a tree name (reserved), so
                    // it is recognized BEFORE the tree-name filter.
                    if x == FORKS_SUBDIR {
                        if let Ok(rd3) = std::fs::read_dir(&d2) {
                            for e3 in rd3.flatten() {
                                let d3 = e3.path();
                                let Some(y) =
                                    d3.file_name().and_then(|n| n.to_str()).map(str::to_string)
                                else {
                                    continue;
                                };
                                if is_view_tree_dir(&y)
                                    && d3.is_dir()
                                    && d3.join(format!("{y}.db")).is_file()
                                {
                                    push(y, &mut out);
                                }
                            }
                        }
                        continue;
                    }
                    if is_view_tree_dir(&x) && d2.join(format!("{x}.db")).is_file() {
                        push(x, &mut out);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// The api-key store's wrap-key file, beside the store itself (`store.key`).
pub const STORE_KEY_FILE: &str = "store.key";

/// Which flat (v1) artifacts still sit directly in `<base>/meta/`? Non-empty
/// means the store predates layout v2: view dbs, their sidecars, or a control
/// file that has not been moved into `_control/` yet. Definitions and the
/// spool are not evidence — `views/` and `.xfer/` exist in both layouts.
pub fn legacy_flat_entries(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(meta_dir(base)) {
        for ent in rd.flatten() {
            let name = match ent.path().file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !ent.path().is_file() || name.starts_with('.') {
                continue;
            }
            if name.ends_with(".db")
                || name.ends_with(".db-wal")
                || name.ends_with(".db-shm")
                || name.ends_with(".db.push.tmp")
                || name == STORE_KEY_FILE
            {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// The guard every SQLite-store entrance runs before touching a tree: a flat
/// v1 store is refused with the one command that fixes it, rather than being
/// read as an empty store (which would offer `view create` over live data and,
/// far worse, hand `gc --apply` an EMPTY view list — every live blob would then
/// look orphaned and the ledger would be rebuilt to zero holds).
///
/// The entrances are deliberately the DESTRUCTIVE and the OPENING ones, not
/// every reader: [`crate::gc`], [`MetaEngine::rebuild_ledger`],
/// [`crate::mcp::ControlStore`]'s physical-view methods, the CLI's own
/// create/import arms and `MetaStore::open_sqlite` (which a `meta-server`
/// reaches at startup, so a replica cannot serve an unmigrated directory).
pub fn layout_guard(base: &Path) -> Result<(), String> {
    let flat = legacy_flat_entries(base);
    if flat.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} still uses the flat pre-v2 metadata layout ({} top-level {}, e.g. {}); no grovefs command reads that layout any more — stop every meta-server and mount on this store, then run `grovefs layout --meta-dir {} migrate`",
        meta_dir(base).display(),
        flat.len(),
        if flat.len() == 1 { "db file" } else { "db files" },
        meta_dir(base).join(&flat[0]).display(),
        base.display(),
    ))
}

/// The metadata root must EXIST before a reader turns its tree list into a
/// decision.
///
/// [`layout_guard`] refuses the flat v1 shape, but a MISSING `<base>/meta` is a
/// different thing — and the flat enumerator refused it too, because its
/// `read_dir` failed. Under layout v2 the enumerator answers "no trees" for a
/// missing directory, and that empty list IS a decision: `gc --apply` then
/// judges every object in the data root an orphan and reclaims the whole store,
/// silently, off a typo in `--meta-dir`.
///
/// So the read paths whose tree list feeds a destructive decision call this
/// first. The write paths (create/import/open) do not: they are what MAKES the
/// directory.
pub fn require_meta_dir(base: &Path) -> Result<PathBuf, String> {
    let meta = meta_dir(base);
    if !meta.is_dir() {
        return Err(format!("no metadata directory {}", meta.display()));
    }
    Ok(meta)
}

/// Create the tree's home and return its db path. Every "open, creating if
/// absent" site funnels through here so the directory never has to be
/// remembered separately.
pub fn ensure_view_db_path(base: &Path, view: &str) -> std::io::Result<PathBuf> {
    let path = view_db_path(base, view);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(path)
}

/// One `CREATE` request in a batch (see
/// [`MetaEngine::create_nodes_batch`](crate::meta::MetaEngine::create_nodes_batch)).
///
/// Borrowed, because the caller keeps the owned names and lends them to the
/// engine for the duration of one commit — no copies on the hot path. This is
/// an ENGINE api: a mount reaches its store through `RemoteMeta`, one request
/// per transaction, and never batches here.
#[derive(Clone, Copy)]
pub struct NewNode<'a> {
    pub parent: i64,
    pub name: &'a str,
    pub is_dir: bool,
    pub data_ref: Option<&'a str>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// One view's tree as the FUSE layer sees it: name/attribute/listing operations
/// over a per-view directory tree.
///
/// Implemented locally by [`MetaEngine`] (a direct per-view SQLite file) and
/// remotely by [`crate::remote::meta::RemoteMeta`] (TCP to a central
/// `MetaServer`). The same FUSE code therefore drives either a view that lives
/// only on one machine or a *single authoritative* view shared by many clients
/// over the LAN, without knowing which — this is what makes the same view's
/// folder tree consistent across machines.
pub trait MetaProvider: Send + Sync {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo>;
    fn getattr(&self, ino: i64) -> Option<NodeInfo>;
    fn readdir(&self, parent: i64) -> Vec<NodeInfo>;
    /// Extra KERNEL-cache coordinates a change at the SOURCE coordinate
    /// `(src_parent, name)` implies for this provider's display namespace.
    ///
    /// A push invalidation from a `meta-server` always names the real source
    /// `(parent inode, name)` of a mutated entry, and the kernel only ever
    /// remembers a dentry by `(display parent inode, display name)`. For a
    /// pass-through provider those are the same pair, so the default (empty)
    /// is correct. A presentation lens can render an entry SOMEWHERE ELSE —
    /// and the one coordinate a source event provably cannot reach is a leaf
    /// hanging under a *synthesized* scaffolding dir, whose inode exists only
    /// in the lens. Each returned `(parent, name)` is such an additional
    /// display coordinate for the given source entry, i.e. one the caller
    /// should ALSO invalidate (plus, for a synthesized parent, flush its
    /// cached listing: a virtual dir's mtime never moves, so its directory
    /// pages cannot self-invalidate and would otherwise hold the stale name
    /// until the attribute TTL).
    fn display_targets(&self, _src_parent: i64, _name: &str) -> Vec<(i64, String)> {
        Vec::new()
    }
    /// Drop whatever this provider has materialized for the listing of
    /// `parent` from its OWN cache, so the next read reflects the mutation that
    /// prompted the call.
    ///
    /// The companion of [`Self::display_targets`]: invalidating the KERNEL's
    /// view of a display coordinate is not enough when the provider also caches
    /// listings itself. A push event about the base tree never passes through a
    /// lens's mutating operations, so nothing else would flush it, and the
    /// display coordinate would keep serving the pre-event rows until the
    /// attribute TTL. Providers that read their store afresh (every engine, a
    /// plain remote client) keep the default no-op.
    fn flush_listing(&self, _parent: i64) {}
    /// Page `offset..offset+limit` of the same deterministic listing `readdir`
    /// returns. `GroveFS` streams directory replies through this so a big
    /// directory costs one small query per kernel buffer. Every engine in the
    /// tree pages inside its own store — SQLite, PostgreSQL and treedb push the
    /// limit down, a derived/layout lens pages over one materialized listing —
    /// and a `RemoteMeta` client forwards the page as an `M_READDIRP` when its
    /// server's `M_OPEN` ack advertised that capability. A provider that cannot
    /// page keeps working through the default below, just without the O(page)
    /// win.
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        self.readdir(parent)
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect()
    }
    /// Whether [`Self::readdir_page`] is a real sub-query (or, for a lens, a
    /// slice of a listing materialized once) rather than a full listing rebuilt
    /// per call. `GroveFS` uses this to decide its fetch chunking: a provider
    /// that cannot page is asked for one whole listing per kernel buffer
    /// (the cost of slicing it falls on the provider), and it is not offered
    /// READDIRPLUS, whose fatter records would multiply that full fetch.
    fn supports_paged_readdir(&self) -> bool {
        false
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
    ) -> Result<i64, MetaError>;
    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError>;
    /// `create_node` plus the attributes a `CREATE`/`MKDIR` reply needs.
    ///
    /// A local engine answers this with one extra store read, which costs no
    /// round trip. A remote client overrides it so the server ships the row it
    /// just wrote inside the same reply — otherwise every creation pays a
    /// second full `GETATTR` RTT purely to fill the FUSE reply (measured: 3
    /// metadata RTTs per create, see USAGE §4.1.1). Keeping the pair as a
    /// separate method leaves every other provider's signature untouched.
    #[allow(clippy::too_many_arguments)]
    fn create_node_with_attr(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let id = self.create_node(parent, name, is_dir, data_ref, mode, uid, gid)?;
        // A concurrent unlink between the create and this read is a real
        // outcome (the caller reports ENOENT), not a provider failure.
        self.getattr(id).ok_or(MetaError::NotFound)
    }
    /// Same idea for symlinks: the reply needs the entry attributes and the
    /// remote provider can fetch them without a second trip.
    fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let id = self.create_symlink(parent, name, target, uid, gid)?;
        self.getattr(id).ok_or(MetaError::NotFound)
    }
    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError>;
    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError>;
    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError>;
    fn set_mode(&self, ino: i64, mode: u32);
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32);
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>);
    fn touch_mtime(&self, ino: i64);

    /// POSIX advisory record lock operation. Implementations MUST be atomic
    /// with respect to concurrent callers on the same `ino`. `sid` is the
    /// caller's mount-session id, stamped on granted rows so the daemon can
    /// renew them (`renew_locks`) and the authority can reap them on disconnect.
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
    ) -> Result<Option<LockConflict>, MetaError>;
    /// F_GETLK: report the first blocking lock without modifying state.
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict>;
    /// FUSE `release`/`flush` cleanup: drop every lock held by `owner`.
    fn release_locks_of_owner(&self, owner: u64);
    /// Renewal heartbeat: extend the lease on every lock row owned by session
    /// `sid`. Called periodically by the mount daemon while it holds locks.
    fn renew_locks(&self, sid: u64);

    /// Handle to this authority's live fencing epoch, shared with the data-plane
    /// client so it can stamp writes with the current token. The default is a
    /// detached zero (no remote fence) — a local `MetaEngine` has no HA epoch and
    /// its (local) data path is never gated. `RemoteMeta` overrides this with the
    /// handle it refreshes on every `M_OPEN` handshake.
    fn fence_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))
    }

    /// Re-query the authoritative epoch now (e.g. after the data-server fenced a
    /// write stamped with a stale token) and return it. The default reports the
    /// already-known handle value; `RemoteMeta` reconnects to pick up a promoted
    /// node's higher epoch.
    fn fetch_fence_epoch(&self) -> u64 {
        self.fence_handle()
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Flat dump of the full tree for server-side aggregation (`du`). Each
    /// entry is `(id, parent_id, name, is_dir, data_ref)`. Providers that
    /// cannot dump (e.g. derived lenses, remote proxy) return empty.
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        Vec::new()
    }
}

#[derive(Debug)]
pub enum MetaError {
    Exists,
    NotFound,
    NotEmpty,
    TypeMismatch,
    OpNotPermitted,
    BadParentCycle,
    Db(String),
}

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaError::Exists => write!(f, "already exists"),
            MetaError::NotFound => write!(f, "not found"),
            MetaError::NotEmpty => write!(f, "directory not empty"),
            MetaError::TypeMismatch => write!(f, "type mismatch"),
            MetaError::OpNotPermitted => write!(f, "operation not permitted"),
            MetaError::BadParentCycle => write!(f, "invalid move into own subtree"),
            MetaError::Db(s) => write!(f, "metadata db error: {s}"),
        }
    }
}

impl std::error::Error for MetaError {}

// ---- server-side / embedded half (native only) ------------------------------
// Everything that opens an on-disk view database (SQLite via rusqlite). The
// `remote-only` build (Windows client) drops this whole subtree, and with it
// the bundled-SQLite C compilation.
#[cfg(feature = "native")]
mod engine;
#[cfg(feature = "native")]
pub use engine::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// The pretty spelling a message or a JSON field shows must be the layout's
    /// own path, not a string someone typed alongside it.
    #[test]
    fn the_operator_facing_tree_home_matches_the_real_path() {
        let base = Path::new("/srv/grovefs");
        for view in ["team", "a-b_c1"] {
            let rel = tree_home_rel(base, view);
            let real = view_db_path(base, view);
            assert_eq!(
                real,
                base.join(&rel),
                "`{rel}` in a message has to resolve to the file the code opens"
            );
        }
        assert_eq!(
            tree_home_rel(Path::new("/srv/grovefs"), "team"),
            "meta/team/team.db"
        );
    }

    /// Which ledger a given db writes bookkeeping into. The import scratch file
    /// is the case that a positional rule gets wrong, and getting it wrong means
    /// a private ledger — so this is asserted, not trusted.
    #[test]
    fn a_tree_and_its_scratch_share_one_store_ledger() {
        let base = Path::new("/srv/grovefs");
        let tree = view_db_path(base, "team");
        assert_eq!(
            refs_ledger_for_db(&tree),
            control_db_path(base, "refs"),
            "a tree's ledger is the store's, not one inside its own directory"
        );
        // The scratch db `import` builds inside the tree home: same store.
        let scratch = view_dir(base, "team").join(".team.import-7-9");
        assert_eq!(refs_ledger_for_db(&scratch), refs_ledger_for_db(&tree));
        // A standalone db belongs to no store and keeps its ledger beside it.
        let lone = Path::new("/tmp/whatever.db");
        assert_eq!(
            refs_ledger_for_db(lone),
            Path::new("/tmp/_control/refs.db"),
            "and must not reach out to a store it was never told about"
        );
    }

    /// A directory name is a tree only if the layout can name it one. Reserved
    /// slots and dot-prefixed scratch both live in `meta/` beside the trees.
    #[test]
    fn only_real_tree_directories_are_enumerated() {
        assert!(is_view_tree_dir("team"));
        assert!(is_view_tree_dir("a-b_c1"));
        for n in [
            DEFS_SUBDIR,
            CONTROL_SUBDIR,
            FORKS_SUBDIR,
            ".xfer",
            ".team.import-4-5",
            "", // .
            "..",
            "a b",
            "a/b",
        ] {
            assert!(!is_view_tree_dir(n), "'{n}' must not read as a view tree");
        }
    }

    /// v3: a family member resolves by SCANNING — no definition read, no name
    /// arithmetic — and the one enumerator already sees it. This is what keeps
    /// the HA standby (no defs) and `view rm`/`gc`/ledger tooling correct on a
    /// nested store without teaching any of them the genealogy.
    #[test]
    fn a_family_member_resolves_inside_its_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let m = meta_dir(base);
        std::fs::create_dir_all(m.join("team")).unwrap();
        std::fs::write(m.join("team/team.db"), b"x").unwrap();
        // nested fork: meta/team/lab/lab.db
        std::fs::create_dir_all(m.join("team/lab")).unwrap();
        std::fs::write(m.join("team/lab/lab.db"), b"x").unwrap();
        // grouped fork: meta/photos/forks/studio/studio.db
        std::fs::create_dir_all(m.join("photos/forks/studio")).unwrap();
        std::fs::write(m.join("photos/photos.db"), b"x").unwrap();
        std::fs::write(m.join("photos/forks/studio/studio.db"), b"x").unwrap();
        assert_eq!(find_view_db(base, "lab"), Some(m.join("team/lab/lab.db")));
        assert_eq!(
            find_view_db(base, "studio"),
            Some(m.join("photos/forks/studio/studio.db"))
        );
        assert_eq!(view_db_path(base, "lab"), m.join("team/lab/lab.db"));
        assert_eq!(
            view_db_with_ext(base, "lab", "-wal"),
            m.join("team/lab/lab.db-wal")
        );
        assert_eq!(view_dir(base, "lab"), m.join("team/lab"));
        let trees = list_view_trees(base);
        assert_eq!(trees, ["lab", "photos", "studio", "team"]);
        // a fork's def travels WITH its own tree directory (the convention
        // this store now writes): update-in-place finds it inside the tree
        std::fs::write(m.join("team/lab/lab.layout"), b"fork team\n").unwrap();
        assert_eq!(
            find_def_slot(base, "lab"),
            Some(m.join("team/lab/lab.layout"))
        );
        assert_eq!(fork_def_slot(base, "lab"), m.join("team/lab/lab.layout"));
        assert_eq!(
            fork_def_slot(base, "studio"),
            m.join("photos/forks/studio/studio.layout")
        );
        // a fork whose tree is not created yet falls back to the flat own
        // spelling, exactly like view_dir
        assert_eq!(fork_def_slot(base, "fresh"), m.join("fresh/fresh.layout"));
        // grouped def separator: meta/photos/views/lens.layout (the separator
        // dir exists on disk only once a grouped def lands, mirror how
        // `new_def_slot` creates it lazily)
        std::fs::create_dir_all(m.join("photos/views")).unwrap();
        std::fs::write(m.join("photos/views/lens.layout"), b"base photos\nhide a\n").unwrap();
        assert_eq!(
            find_def_slot(base, "lens"),
            Some(m.join("photos/views/lens.layout"))
        );
        // a name the store does not hold resolves to nothing
        assert_eq!(find_view_db(base, "ghost"), None);
        assert_eq!(find_def_slot(base, "ghost"), None);
        // the pretty rel lands on the REAL file, family path included
        assert_eq!(tree_home_rel(base, "lab"), "meta/team/lab/lab.db");
        assert_eq!(tree_home_rel_dir(base, "lab"), "team/lab");
        assert_eq!(tree_home_rel_dir(base, "team"), "team");
    }

    /// v3: the policy decides where NEW members land; it never re-reads or
    /// moves what exists, and the marker file is invisible to both the flat
    /// guard and the tree enumerator (dot-prefixed).
    #[test]
    fn the_family_policy_decides_fresh_placement_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(meta_dir(base).join("team")).unwrap();
        std::fs::write(meta_dir(base).join("team/team.db"), b"x").unwrap();
        assert_eq!(family_policy(base), FamilyLayout::Nested);
        assert_eq!(
            new_tree_db_path(base, "lab", Some("team")),
            meta_dir(base).join("team/lab/lab.db")
        );
        assert_eq!(
            new_def_slot(base, "lens", Some("team")),
            meta_dir(base).join("team/lens.layout")
        );
        assert_eq!(
            fork_def_slot(base, "lab"),
            meta_dir(base).join("lab/lab.layout")
        );
        set_family_policy(base, FamilyLayout::Grouped).unwrap();
        assert_eq!(family_policy(base), FamilyLayout::Grouped);
        assert_eq!(
            new_tree_db_path(base, "lab", Some("team")),
            meta_dir(base).join("team/forks/lab/lab.db")
        );
        assert_eq!(
            new_def_slot(base, "lens", Some("team")),
            meta_dir(base).join("team/views/lens.layout")
        );
        set_family_policy(base, FamilyLayout::Flat).unwrap();
        assert_eq!(
            new_tree_db_path(base, "lab", Some("team")),
            meta_dir(base).join("lab/lab.db")
        );
        assert_eq!(
            new_def_slot(base, "lens", Some("team")),
            defs_dir(base).join("lens.layout")
        );
        // no family (a new BASE view) always lands top-level, any policy
        assert_eq!(
            new_tree_db_path(base, "solo", None),
            meta_dir(base).join("solo/solo.db")
        );
        assert!(legacy_flat_entries(base).is_empty());
        assert_eq!(list_view_trees(base), vec!["team".to_string()]);
    }

    /// What counts as "still flat" — the one signal that decides whether the
    /// whole toolchain refuses a store.
    #[test]
    fn the_flat_test_sees_every_legacy_shape_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let meta = meta_dir(base);
        std::fs::create_dir_all(&meta).unwrap();
        assert!(layout_guard(base).is_ok(), "an empty or absent meta/ is v2");
        for name in [
            "team.db",
            "refs.db-wal",
            "api_keys.db-shm",
            "team.db.push.tmp",
            STORE_KEY_FILE,
        ] {
            std::fs::write(meta.join(name), b"x").unwrap();
            let flat = legacy_flat_entries(base);
            assert!(flat.contains(&name.to_string()), "'{name}' must look flat");
            assert!(layout_guard(base).is_err());
            std::fs::remove_file(meta.join(name)).unwrap();
        }
        // The v2 shapes a real store has: tree dirs, the control slot, the
        // definition slot, the spool, and a log file. None of them is flat.
        std::fs::create_dir_all(meta.join("team")).unwrap();
        std::fs::write(view_db_path(base, "team"), b"x").unwrap();
        std::fs::create_dir_all(control_dir(base)).unwrap();
        std::fs::write(control_db_path(base, "refs"), b"x").unwrap();
        std::fs::create_dir_all(defs_dir(base)).unwrap();
        std::fs::write(defs_dir(base).join("team.layout"), b"base team\n").unwrap();
        std::fs::create_dir_all(xfer_dir(base)).unwrap();
        std::fs::write(meta_dir(base).join("notes.txt"), b"ignored").unwrap();
        assert_eq!(legacy_flat_entries(base), Vec::<String>::new());
        assert!(layout_guard(base).is_ok());
        assert_eq!(list_view_trees(base), vec!["team".to_string()]);
    }

    /// A half-created tree directory (a `view create` that died before the db
    /// landed) is not a view: listing it would promise a tree nothing can open.
    #[test]
    fn a_tree_directory_without_its_db_is_not_a_view() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(view_dir(base, "ghost")).unwrap();
        std::fs::create_dir_all(view_dir(base, "real")).unwrap();
        std::fs::write(view_db_path(base, "real"), b"").unwrap();
        assert_eq!(list_view_trees(base), vec!["real".to_string()]);
    }

    #[test]
    fn control_stores_are_never_view_trees() {
        // The files that share `meta/` with the view trees must never be listed
        // as views. Five enumerators used to keep their own two-name exception
        // list, so a third store added there would have shown up in `view list`
        // and been opened as a tree by `gc` and `refs rebuild` alike.
        assert!(!is_view_db("refs.db"));
        assert!(!is_view_db("api_keys.db"));
        assert!(!is_view_db("web_accounts.db"));
        assert!(is_view_db("team.db"));
        assert!(is_view_db("a-b_c1.db"));
        // The ledger's own name has to stay in the exception list, whatever it is
        // spelled as over in `REFS_DB`.
        assert!(CONTROL_DB_STEMS.contains(&REFS_DB.strip_suffix(".db").unwrap()));
        // Not a db, a directory, a definition, a sidecar, an import scratch file,
        // or an empty stem: none of them is a tree.
        for n in [
            "views",
            "team.layout",
            "team.db-wal",
            "team.db-shm",
            ".team.import-4-5",
            "db",
            ".db",
            "team.DB",
        ] {
            assert!(!is_view_db(n), "'{n}' must not look like a view tree");
        }
    }
}
