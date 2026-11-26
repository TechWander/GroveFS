//! `grovefs import` — mirror an EXISTING on-disk directory tree into a view,
//! zero-copy where storage allows. This is the *metadata + data ingest* path,
//! distinct from the view-export restore of `meta::import_view` / `pg_store::import_view`
//! (which replay a serialized dump). Here the input is a plain directory.
//!
//! The core (`ingest`) is written against the backend-agnostic [`MetaProvider`]
//! trait rather than a concrete store, so the SAME walk serves every metadata
//! backend: SQLite [`crate::meta::MetaEngine`], PostgreSQL
//! [`crate::pg_store::PgEngine`] and treedb [`crate::treedb_store::TreedbStore`]
//! all implement it. The data half is likewise the shared [`StorageBackend`]
//! (`import_at`), so imported bytes land at the identical `blocks/<bucket>/<key>`
//! path a mounted write uses — one storage convention per deployment, never two.
//!
//! ## Incremental semantics (add-only, idempotent)
//! * a name already present in the view is SKIPPED, never overwritten — a re-run
//!   converges rather than churns, and switching `--prefix` forces re-registration
//!   under a different home;
//! * entries the source has since DELETED are not removed from the view (no delete
//!   sync) — the safe default for an ingest that must never destroy data;
//! * `--since` (or a persisted watermark from a prior run) prunes directory
//!   recursion: a directory whose `mtime` predates the watermark AND already exists
//!   in the view cannot have gained a new child without its own `mtime` moving
//!   (a fresh entry bumps its parent dir), so the whole subtree is skipped instead
//!   of rescanned. Content edits to existing files don't move a dir `mtime` but are
//!   skipped by the name rule anyway, so the prune stays sound for detecting NEW
//!   names — which is all an add-only ingest promises to pick up.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::{Imported, StorageBackend};
use crate::meta::{MetaProvider, ROOT_INO};

/// Tallies for `grovefs import`.
#[derive(Default, Debug, Clone)]
pub struct ImportStats {
    pub dirs: usize,
    pub files: usize,
    pub linked: usize,
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
    /// The moment THIS walk began (unix secs), persisted as the next run's prune
    /// watermark. Anything the source gained afterwards necessarily sits in a
    /// dir with `mtime > watermark`, so the prune cannot miss an addition.
    /// Deliberately NOT "max mtime seen" — a dir touched during the same second
    /// the scan finished would otherwise be pruned by the next run.
    pub watermark: Option<u64>,
}

/// `ensure_dir` variant that REUSES pre-existing nodes instead of failing — what
/// makes a re-run an incremental no-op over the already-mirrored parts of the
/// tree. Generic over any metadata backend via [`MetaProvider`].
pub fn ensure_dir_soft(
    meta: &dyn MetaProvider,
    path: &str,
    dirs: &mut HashMap<String, i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cur_ino = ROOT_INO;
    let mut cur_path = String::from("/");
    for comp in path.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
        let next_path = if cur_path == "/" {
            format!("/{comp}")
        } else {
            format!("{cur_path}/{comp}")
        };
        if let Some(&ino) = dirs.get(&next_path) {
            cur_ino = ino;
            cur_path = next_path;
            continue;
        }
        let ino = match meta.lookup(cur_ino, comp) {
            Some(node) => node.id,
            None => meta.create_node(
                cur_ino,
                comp,
                true,
                None,
                0o0755,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
            )?,
        };
        dirs.insert(next_path.clone(), ino);
        cur_ino = ino;
        cur_path = next_path;
    }
    Ok(())
}

/// Pure source-side tally for `import --dry-run`: counts what WOULD be mirrored,
/// without opening the store at all — "touch nothing", literally.
pub fn count_walk(src: &Path, st: &mut ImportStats) -> Result<(), Box<dyn std::error::Error>> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ty = e.file_type()?;
        if ty.is_dir() {
            st.dirs += 1;
            count_walk(&e.path(), st)?;
        } else if ty.is_file() {
            st.files += 1;
        } else {
            st.skipped += 1;
        }
    }
    Ok(())
}

/// Recursively mirror the existing tree at `src` into view path `in_view`.
/// Dirs become nodes; regular files become objects via
/// [`StorageBackend::import_at`] (hard link first, copy fallback), keeping their
/// timestamps / mode / owner. Names that already exist are skipped; symlinks /
/// special files are skipped too (and never followed, which also rules out
/// symlink loops). Per-entry failures are counted and reported, not fatal — one
/// unreadable file must not abort a bulk import.
///
/// `since` prunes unchanged subtrees (see the module note). `dirs` is the running
/// `view path -> inode` map so a parent's inode is known when its children are
/// registered; seed it with `"/" -> ROOT_INO` and the resolved `in_view`.
#[allow(clippy::too_many_arguments)]
pub fn ingest(
    meta: &dyn MetaProvider,
    backend: &dyn StorageBackend,
    src: &Path,
    in_view: &str,
    dirs: &mut HashMap<String, i64>,
    st: &mut ImportStats,
    since: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A run started without a bound records its own start as the snapshot point.
    if st.watermark.is_none() {
        st.watermark = Some(epoch_secs(SystemTime::now()));
    }
    let mut entries: Vec<_> = std::fs::read_dir(src)?
        .filter_map(|e| match e {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!(
                    "import: cannot read an entry under {}: {err}",
                    src.display()
                );
                st.failed += 1;
                None
            }
        })
        .collect();
    entries.sort_by_key(|e| e.file_name()); // stable order across re-runs
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child = if in_view == "/" {
            format!("/{name}")
        } else {
            format!("{in_view}/{name}")
        };
        let ty = e.file_type()?;
        if ty.is_dir() {
            st.dirs += 1;
            let parent_ino = dirs.get(in_view).copied().unwrap_or(ROOT_INO);
            let mtime = std::fs::metadata(e.path()).map(|m| m.mtime() as u64).ok();
            // Watermark prune: an already-present dir whose mtime predates the
            // watermark cannot have gained a new child (a new entry bumps THIS
            // dir's mtime), so skip the whole subtree instead of rescanning it.
            let already =
                dirs.contains_key(&child) || meta.lookup(parent_ino, &name).is_some();
            if let Some(s) = since {
                if already && mtime.is_some_and(|mt| mt <= s) {
                    // Keep the mapped inode so siblings/parents still resolve.
                    if !dirs.contains_key(&child) {
                        if let Some(node) = meta.lookup(parent_ino, &name) {
                            dirs.insert(child.clone(), node.id);
                        }
                    }
                    st.skipped += 1;
                    continue;
                }
            }
            ensure_dir_soft(meta, &child, dirs)?;
            ingest(meta, backend, &e.path(), &child, dirs, st, since)?;
        } else if ty.is_file() {
            let parent_ino = *dirs
                .get(in_view)
                .ok_or_else(|| format!("import: missing dir node for {in_view}"))?;
            if meta.lookup(parent_ino, &name).is_some() {
                st.skipped += 1; // incremental re-run: keep whatever is there
                continue;
            }
            let fm = match std::fs::metadata(e.path()) {
                Ok(fm) => fm,
                Err(err) => {
                    eprintln!("import: {}: {err}", e.path().display());
                    st.failed += 1;
                    continue;
                }
            };
            let key = backend.new_key(&uuid::Uuid::new_v4().to_string());
            let imported = match backend.import_at(&key, &e.path()) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("import: {}: {err}", e.path().display());
                    st.failed += 1;
                    continue;
                }
            };
            match meta.create_node(
                parent_ino,
                &name,
                false,
                Some(&key),
                (fm.mode() & 0o7777) as u32,
                fm.uid(),
                fm.gid(),
            ) {
                Ok(ino) => {
                    // Keep the file's own clock, not "import happened now".
                    meta.set_times(ino, Some(fm.atime() as i64), Some(fm.mtime() as i64));
                    match imported {
                        Imported::Hardlinked => st.linked += 1,
                        Imported::Copied => st.copied += 1,
                    }
                }
                Err(crate::meta::MetaError::Exists) => {
                    // Lost a race against a concurrent writer: drop the object
                    // we just materialized so nothing dangles, count as skipped.
                    let _ = backend.remove(&key);
                    st.skipped += 1;
                }
                Err(err) => {
                    let _ = backend.remove(&key);
                    eprintln!("import: {child}: {err}");
                    st.failed += 1;
                }
            }
        } else {
            // symlink / fifo / socket / device: not representable as a plain
            // object today — counted, never followed.
            st.skipped += 1;
        }
    }
    Ok(())
}

/// Identity of one import stream, for the persisted watermark: the same
/// `(view, source, prefix)` triple is the thing that gets re-run incrementally.
pub fn watermark_key(view: &str, source_abs: &Path, prefix: &str) -> String {
    format!("{}\u{0}{}\u{0}{}", view, source_abs.display(), prefix)
}

/// Load a previously recorded watermark (unix secs) for `key`, if any. The store
/// is a flat JSON map under `<control_home>/meta/_control/import_watermarks.json`
/// — control-plane-local, so it works identically under SQLite, PG and treedb.
pub fn load_watermark(control_home: &Path, key: &str) -> Option<u64> {
    let path = watermark_path(control_home);
    let raw = std::fs::read(&path).ok()?;
    let map: HashMap<String, u64> = serde_json::from_slice(&raw).ok()?;
    map.get(key).copied()
}

/// Persist `secs` as the watermark for `key`, rewriting the map atomically.
pub fn save_watermark(control_home: &Path, key: &str, secs: u64) -> io::Result<()> {
    let path = watermark_path(control_home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut map: HashMap<String, u64> = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();
    map.insert(key.to_string(), secs);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&map).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn watermark_path(control_home: &Path) -> PathBuf {
    control_home.join("meta").join("_control").join("import_watermarks.json")
}

/// Seconds since the unix epoch for a wall-clock instant (0 for pre-epoch).
pub fn epoch_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
