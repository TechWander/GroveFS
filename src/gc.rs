//! Offline orphan-object audit and reclaim for a LOCAL store.
//!
//! An object is an ORPHAN when nothing names its bytes anymore: the page
//! `<data-dir>/blocks/<2hex>/<key>` exists, no surviving view tree carries a
//! `node.data_ref = key`, and the cross-view ledger (`meta/refs.db`) does not
//! book it either. That residue is produced by two documented accidents:
//!
//! - a view db removed out-of-band (`rm meta/<view>.db`): its tree vanishes, so
//!   its blobs lose their last namer, but no unlink ever ran;
//! - a crash between `backend.create` and the node INSERT.
//!
//! The scan is deliberately OFFLINE (or at least snapshot-sound): it reads
//! every surviving `<meta>/*.db` and FAILS when one cannot be read, because a
//! skipped tree would make every blob it names look orphaned. `--apply` is
//! gated by the CLI on "no live local mount / no local grovefs process using
//! this store", and never deletes an object younger than the grace window.
//!
//! Striped layouts: a tree names `s<N>:<stripe>:<base>` while the bytes live on
//! data-server roots as `<base>#<i>` chunks. A chunk is KEPT when any tree names
//! a layout with that base; only chunks whose base is unknown to every tree are
//! reported as orphans. A sharded backend never materializes the logical key,
//! so the reverse confusion (deleting a live layout's bytes) cannot happen.
//!
//! The ledger is a second safety net: a key still booked in `refs.db` is
//! reported as `held` and NEVER deleted, even when no tree names it — that is
//! the documented over-count from an out-of-band tree removal. Run
//! `grovefs refs rebuild` first and the same object becomes reclaimable.

use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Object flavour, decided by the key's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A whole object named by its key (`<uuid>`, an imported key, ...).
    Plain,
    /// One `<base>#<i>` chunk of a striped layout living on a data-server root.
    Chunk,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Plain => "plain",
            ObjectKind::Chunk => "chunk",
        }
    }
}

/// One orphan/held object, with everything the report needs.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub key: String,
    /// Exact on-disk page that would be removed.
    pub path: PathBuf,
    /// The `--data-dir` this page was found under.
    pub data_root: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
    pub kind: ObjectKind,
    /// Younger than the grace window: reported, never deleted this pass.
    pub within_grace: bool,
}

impl Candidate {
    pub fn kind_str(&self) -> &'static str {
        self.kind.as_str()
    }
}

/// Result of a scan (read-only, deletes nothing).
#[derive(Debug)]
pub struct Report {
    pub meta_root: PathBuf,
    pub data_roots: Vec<PathBuf>,
    pub views: Vec<String>,
    /// Distinct `data_ref` keys across every surviving tree.
    pub refs: usize,
    /// Distinct striped layouts (`s<N>:<stripe>:<base>`) among those refs.
    pub striped_layouts: usize,
    pub objects: usize,
    pub objects_bytes: u64,
    /// Objects kept because a tree names them (or their striped base).
    pub matched: usize,
    pub orphans: Vec<Candidate>,
    /// Unreferenced but still booked in `refs.db`: never deleted here.
    pub held: Vec<Candidate>,
    /// Entries deliberately left alone (unreadable/non-object/non-UTF8).
    pub skipped: Vec<String>,
    pub grace: Duration,
}

impl Report {
    pub fn orphan_bytes(&self) -> u64 {
        self.orphans.iter().map(|c| c.size).sum()
    }

    pub fn reclaimable(&self) -> usize {
        self.orphans.iter().filter(|c| !c.within_grace).count()
    }

    pub fn reclaimable_bytes(&self) -> u64 {
        self.orphans
            .iter()
            .filter(|c| !c.within_grace)
            .map(|c| c.size)
            .sum()
    }

    pub fn within_grace(&self) -> usize {
        self.orphans.len() - self.reclaimable()
    }

    pub fn held_bytes(&self) -> u64 {
        self.held.iter().map(|c| c.size).sum()
    }
}

/// Outcome of [`reclaim`].
#[derive(Debug, Default)]
pub struct Reclaimed {
    pub deleted: usize,
    pub bytes: u64,
    pub within_grace: usize,
    pub failed: Vec<String>,
}

/// Parse `--older-than`: a bare integer is seconds; suffixes are s/m/h/d
/// (case-insensitive). `0` disables the grace window.
pub fn parse_age(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("--older-than must be a duration like 30m, 24h, 7d, or 0".to_string());
    }
    let (digits, unit) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&t[..i], Some(c.to_ascii_lowercase())),
        _ => (t, None),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "bad duration '{s}' (use seconds, or a suffix s/m/h/d, e.g. 30m, 24h, 7d)"
        ));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("duration '{s}' is out of range"))?;
    let mult = match unit {
        None | Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => 24 * 60 * 60,
        Some(c) => {
            return Err(format!(
                "unknown duration unit '{c}' in '{s}' (use s/m/h/d)"
            ))
        }
    };
    Ok(Duration::from_secs(n.saturating_mul(mult)))
}

/// How long ago `t` was, in seconds (0 for a future timestamp).
pub fn age_secs(t: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn older_than(t: SystemTime, age: Duration) -> bool {
    if age.is_zero() {
        return true;
    }
    match SystemTime::now().duration_since(t) {
        Ok(d) => d >= age,
        // A timestamp in the future is never "old": keep the object.
        Err(_) => false,
    }
}

/// `<base>#<i>` -> `Some(base)`; anything else (including a non-numeric
/// suffix) is not a chunk name.
fn chunk_base(key: &str) -> Option<&str> {
    let (base, idx) = key.rsplit_once('#')?;
    if base.is_empty() || idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(base)
}

/// `s<N>:<stripe>:<base>` -> `Some(base)`, matching
/// `backend::sharded::parse_layout` exactly (N > 0, stripe > 0).
fn layout_base(key: &str) -> Option<&str> {
    let rest = key.strip_prefix('s')?;
    let (n, tail) = rest.split_once(':')?;
    let (stripe, base) = tail.split_once(':')?;
    if n.is_empty()
        || !n.bytes().all(|b| b.is_ascii_digit())
        || stripe.is_empty()
        || !stripe.bytes().all(|b| b.is_ascii_digit())
        || base.is_empty()
    {
        return None;
    }
    if n.parse::<usize>().ok()? == 0 || stripe.parse::<u64>().ok()? == 0 {
        return None;
    }
    Some(base)
}

/// Every physical view tree kept beside the ledger (sorted, control dbs out).
pub fn physical_views(meta_root: &Path) -> Result<Vec<String>, String> {
    let meta = meta_root.join("meta");
    let rd = fs::read_dir(&meta)
        .map_err(|e| format!("no metadata directory {}: {e}", meta.display()))?;
    let mut views = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".db") || name == "refs.db" || name == "api_keys.db" {
            continue;
        }
        if !p.is_file() {
            continue;
        }
        views.push(name.trim_end_matches(".db").to_string());
    }
    views.sort();
    Ok(views)
}

struct Refs {
    views: Vec<String>,
    keys: HashSet<String>,
    striped: HashSet<String>,
}

/// Union of every surviving tree's DISTINCT `data_ref` values. A tree that
/// cannot be opened aborts the scan: skipping it would misreport its blobs as
/// orphans, and this module's whole contract is "never guess when deleting".
fn collect_refs(meta_root: &Path) -> Result<Refs, String> {
    let meta = meta_root.join("meta");
    let rd = fs::read_dir(&meta)
        .map_err(|e| format!("no metadata directory {}: {e}", meta.display()))?;
    let mut views = Vec::new();
    let mut keys = HashSet::new();
    let mut striped = HashSet::new();
    for ent in rd.flatten() {
        let p = ent.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".db") || name == "refs.db" || name == "api_keys.db" {
            continue;
        }
        if !p.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("cannot read view tree {}: {e}", p.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))
            .map_err(|e| format!("cannot configure {}: {e}", p.display()))?;
        let mut stmt = conn
            .prepare("SELECT DISTINCT data_ref FROM node WHERE data_ref IS NOT NULL")
            .map_err(|e| {
                format!(
                    "cannot read view tree {} (not a view db?): {e}",
                    p.display()
                )
            })?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("cannot read view tree {}: {e}", p.display()))?;
        for dr in rows.flatten() {
            if let Some(base) = layout_base(&dr) {
                striped.insert(base.to_string());
            }
            keys.insert(dr);
        }
        views.push(name.trim_end_matches(".db").to_string());
    }
    views.sort();
    Ok(Refs {
        views,
        keys,
        striped,
    })
}

struct Ledger {
    keys: HashSet<String>,
    striped: HashSet<String>,
}

/// Keys the cross-view ledger still books (`holders > 0`). A missing or
/// table-less ledger means "no holds", never an error.
fn collect_ledger(meta_root: &Path) -> Result<Ledger, String> {
    let p = meta_root.join("meta").join("refs.db");
    let mut keys = HashSet::new();
    let mut striped = HashSet::new();
    if !p.is_file() {
        return Ok(Ledger { keys, striped });
    }
    let conn = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot read the ledger {}: {e}", p.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(|e| format!("cannot configure {}: {e}", p.display()))?;
    let has_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='holders'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_table {
        return Ok(Ledger { keys, striped });
    }
    let mut stmt = conn
        .prepare("SELECT data_ref FROM holders WHERE holders > 0")
        .map_err(|e| format!("cannot read the ledger {}: {e}", p.display()))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("cannot read the ledger {}: {e}", p.display()))?;
    for dr in rows.flatten() {
        if let Some(base) = layout_base(&dr) {
            striped.insert(base.to_string());
        }
        keys.insert(dr);
    }
    Ok(Ledger { keys, striped })
}

enum Verdict {
    Referenced,
    Held(ObjectKind),
    Orphan(ObjectKind),
}

fn verdict(key: &str, refs: &Refs, ledger: &Ledger) -> Verdict {
    if refs.keys.contains(key) {
        return Verdict::Referenced;
    }
    if let Some(base) = chunk_base(key) {
        if refs.striped.contains(base) {
            return Verdict::Referenced;
        }
        if ledger.striped.contains(base) {
            return Verdict::Held(ObjectKind::Chunk);
        }
        return Verdict::Orphan(ObjectKind::Chunk);
    }
    if ledger.keys.contains(key) {
        return Verdict::Held(ObjectKind::Plain);
    }
    Verdict::Orphan(ObjectKind::Plain)
}

fn scan_root(
    blocks: &Path,
    data_root: &Path,
    refs: &Refs,
    ledger: &Ledger,
    grace: Duration,
    rep: &mut Report,
) -> Result<(), String> {
    let rd = match fs::read_dir(blocks) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            rep.skipped.push(format!(
                "{}: no blocks/ directory under this data root (wrong --data-dir?)",
                data_root.display()
            ));
            return Ok(());
        }
        Err(e) => return Err(format!("cannot list {}: {e}", blocks.display())),
    };
    for bucket in rd.flatten() {
        let bp = bucket.path();
        let bmd = match fs::symlink_metadata(&bp) {
            Ok(m) => m,
            Err(e) => {
                rep.skipped.push(format!("{}: {e}", bp.display()));
                continue;
            }
        };
        if !bmd.is_dir() {
            // The backend only ever puts pages inside bucket directories; a
            // loose file here is foreign and is never deleted.
            rep.skipped
                .push(format!("{}: file outside the bucket layout", bp.display()));
            continue;
        }
        let entries = match fs::read_dir(&bp) {
            Ok(e) => e,
            Err(e) => {
                rep.skipped.push(format!("{}: {e}", bp.display()));
                continue;
            }
        };
        for ent in entries.flatten() {
            let p = ent.path();
            let md = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(e) => {
                    rep.skipped.push(format!("{}: {e}", p.display()));
                    continue;
                }
            };
            if !md.is_file() {
                rep.skipped.push(format!(
                    "{}: not a regular file (symlink or directory)",
                    p.display()
                ));
                continue;
            }
            let Some(key) = ent.file_name().to_str().map(str::to_string) else {
                rep.skipped
                    .push(format!("{}: object name is not valid UTF-8", p.display()));
                continue;
            };
            rep.objects += 1;
            rep.objects_bytes = rep.objects_bytes.saturating_add(md.len());
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            match verdict(&key, refs, ledger) {
                Verdict::Referenced => rep.matched += 1,
                Verdict::Held(kind) => rep.held.push(Candidate {
                    key,
                    path: p,
                    data_root: data_root.to_path_buf(),
                    size: md.len(),
                    mtime,
                    kind,
                    within_grace: !older_than(mtime, grace),
                }),
                Verdict::Orphan(kind) => rep.orphans.push(Candidate {
                    key,
                    path: p,
                    data_root: data_root.to_path_buf(),
                    size: md.len(),
                    mtime,
                    kind,
                    within_grace: !older_than(mtime, grace),
                }),
            }
        }
    }
    Ok(())
}

/// Scan `data_roots` against the trees under `meta_root` (read-only).
pub fn scan(meta_root: &Path, data_roots: &[PathBuf], grace: Duration) -> Result<Report, String> {
    let refs = collect_refs(meta_root)?;
    let ledger = collect_ledger(meta_root)?;
    let mut rep = Report {
        meta_root: meta_root.to_path_buf(),
        data_roots: Vec::new(),
        views: refs.views.clone(),
        refs: refs.keys.len(),
        striped_layouts: refs.striped.len(),
        objects: 0,
        objects_bytes: 0,
        matched: 0,
        orphans: Vec::new(),
        held: Vec::new(),
        skipped: Vec::new(),
        grace,
    };
    let mut seen = HashSet::new();
    for root in data_roots {
        let canon = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        rep.data_roots.push(root.clone());
        scan_root(&canon.join("blocks"), root, &refs, &ledger, grace, &mut rep)?;
    }
    rep.orphans
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.key.cmp(&b.key)));
    rep.held
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.key.cmp(&b.key)));
    Ok(rep)
}

/// Delete the candidates that are old enough; idempotent per page (a missing
/// file is not an error). Held candidates are never passed here.
pub fn reclaim(cands: &[Candidate], age: Duration) -> Reclaimed {
    let mut out = Reclaimed::default();
    for c in cands {
        if !older_than(c.mtime, age) {
            out.within_grace += 1;
            continue;
        }
        match fs::remove_file(&c.path) {
            Ok(()) => {
                out.deleted += 1;
                out.bytes = out.bytes.saturating_add(c.size);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Someone else already removed it: treat as reclaimed.
                out.deleted += 1;
            }
            Err(e) => out.failed.push(format!("{}: {e}", c.key)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{MetaEngine, ROOT_INO};
    use std::io::Write;

    fn set_age(p: &Path, age: Duration) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn ages_parse_with_units_and_reject_junk() {
        assert_eq!(parse_age("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_age("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_age("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_age("24h").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_age(" 7D ").unwrap(), Duration::from_secs(7 * 86400));
        assert!(parse_age("").is_err());
        assert!(parse_age("h").is_err());
        assert!(parse_age("12x").is_err());
        assert!(parse_age("-1h").is_err());
    }

    #[test]
    fn key_shapes_match_the_sharded_backend() {
        assert_eq!(layout_base("s2:1024:abcd-uuid"), Some("abcd-uuid"));
        assert_eq!(layout_base("s1:1:a:b"), Some("a:b"));
        assert_eq!(layout_base("s0:1:x"), None, "N=0 is not a layout");
        assert_eq!(layout_base("s2:0:x"), None, "stripe=0 is not a layout");
        assert_eq!(layout_base("s2:x:y"), None);
        assert_eq!(layout_base("uuid-1234"), None);

        assert_eq!(chunk_base("abcd-uuid#0"), Some("abcd-uuid"));
        assert_eq!(chunk_base("abcd-uuid#123"), Some("abcd-uuid"));
        assert_eq!(chunk_base("abcd-uuid#x"), None);
        assert_eq!(chunk_base("abcd-uuid"), None);
        assert_eq!(chunk_base("#0"), None);
    }

    #[test]
    fn scan_separates_referenced_orphan_held_and_striped_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("store");
        let meta = base.join("meta");
        fs::create_dir_all(&meta).unwrap();
        {
            let e = MetaEngine::open(&meta.join("team.db")).unwrap();
            e.create_node(ROOT_INO, "live", false, Some("k-live"), 0o644, 1000, 1000)
                .unwrap();
            e.create_node(
                ROOT_INO,
                "striped",
                false,
                Some("s2:1024:base-live"),
                0o644,
                1000,
                1000,
            )
            .unwrap();
        }
        crate::meta::rebuild_ledger(&base).unwrap();
        // A stale holder no surviving tree names: reported as held, not deleted.
        let conn = Connection::open(meta.join("refs.db")).unwrap();
        conn.execute(
            "INSERT INTO holders (data_ref, holders) VALUES ('k-held', 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let blocks = base.join("blocks").join("aa");
        fs::create_dir_all(&blocks).unwrap();
        for k in [
            "k-live",
            "k-orphan",
            "base-live#0",
            "base-orphan#0",
            "k-held",
        ] {
            fs::File::create(blocks.join(k))
                .unwrap()
                .write_all(b"x")
                .unwrap();
        }

        let data_roots = [base.clone()];
        let rep = scan(&base, &data_roots, Duration::ZERO).unwrap();
        assert_eq!(rep.views, vec!["team"]);
        assert_eq!(rep.refs, 2);
        assert_eq!(rep.striped_layouts, 1);
        assert_eq!(rep.objects, 5);
        assert_eq!(rep.matched, 2, "k-live and base-live#0 are referenced");
        let orphans: Vec<&str> = rep.orphans.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(orphans.len(), 2, "{orphans:?}");
        assert!(orphans.contains(&"k-orphan"));
        assert!(orphans.contains(&"base-orphan#0"));
        assert_eq!(rep.held.len(), 1);
        assert_eq!(rep.held[0].key, "k-held");
        assert_eq!(rep.held[0].kind, ObjectKind::Plain);
    }

    #[test]
    fn an_unreadable_view_tree_aborts_instead_of_faking_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("store/meta");
        fs::create_dir_all(&meta).unwrap();
        fs::write(meta.join("broken.db"), b"not a sqlite database").unwrap();
        let err = scan(
            &dir.path().join("store"),
            &[dir.path().join("store")],
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(err.contains("broken.db"), "{err}");
    }

    #[test]
    fn reclaim_respects_the_grace_window() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let young = dir.path().join("young");
        fs::write(&old, b"old").unwrap();
        fs::write(&young, b"young").unwrap();
        set_age(&old, Duration::from_secs(2 * 3600));
        let cand = |p: &Path, size: u64| Candidate {
            key: p.file_name().unwrap().to_string_lossy().into_owned(),
            path: p.to_path_buf(),
            data_root: dir.path().to_path_buf(),
            size,
            mtime: fs::metadata(p).unwrap().modified().unwrap(),
            kind: ObjectKind::Plain,
            within_grace: false,
        };
        let out = reclaim(&[cand(&old, 3), cand(&young, 5)], Duration::from_secs(3600));
        assert_eq!(out.deleted, 1);
        assert_eq!(out.bytes, 3);
        assert_eq!(out.within_grace, 1);
        assert!(!old.exists());
        assert!(young.exists());
    }
}
