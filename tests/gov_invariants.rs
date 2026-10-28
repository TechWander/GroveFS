//! Property-based machine checks of the presentation-lens governance invariants
//! over RANDOMISED (source-tree × rule-set) compositions, driven through the
//! exact `MetaProvider` interface the FUSE layer uses — the real [`LayoutMeta`]
//! lens over a real [`MetaEngine`], with no mount and no network.
//!
//! What is mechanically checked for every generated pair:
//!   * I1 (unique entries / no duplicate stored entry) — every listing the lens
//!     exposes has pairwise-distinct names, and read-only traversal through the
//!     lens never adds a row to the underlying store;
//!   * I2 (identity-preserved sharing) — a content key (`data_ref`) surfaced at
//!     more than one display path resolves to ONE and the same real inode, never
//!     two distinct ids (i.e. an alias never fabricates a duplicate object);
//!   * I4 (write-through / no materialisation) — a create through the lens in a
//!     real directory lands in the store, while a create in a synthesised
//!     (lens-only) directory is refused and leaves the store untouched.
//!
//! I3 (invalidation integrity) lives in the notify/event layer, not in the pure
//! lens, and is exercised by the end-to-end shell suites; it is deliberately NOT
//! claimed here. A deterministic xorshift PRNG seeds each case so any failure is
//! reproducible from the printed seed.

use grovefs::layout::{LayoutMeta, LayoutRules};
use grovefs::meta::{MetaEngine, MetaProvider, NodeKind, ROOT_INO};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

/// Tiny deterministic PRNG (xorshift64*) so every case is reproducible by seed.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n) as usize
        }
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

fn tmpdb(seed: u64) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "grovefs_govprop_{}_{}.db",
        std::process::id(),
        seed
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    p
}

/// Count every stored node beneath ROOT (root excluded) — the storage footprint.
fn eng_count(eng: &dyn MetaProvider) -> usize {
    let mut seen = 0usize;
    let mut q = VecDeque::new();
    q.push_back(ROOT_INO);
    while let Some(dir) = q.pop_front() {
        for c in eng.readdir(dir) {
            seen += 1;
            if matches!(c.kind, NodeKind::Dir) {
                q.push_back(c.id);
            }
        }
    }
    seen
}

/// Snapshot every stored edge (child id -> (parent id, name)); used to prove the
/// lens never materialises or disturbs source rows on read-only traversal.
fn eng_snapshot(eng: &dyn MetaProvider) -> BTreeMap<i64, (i64, String)> {
    let mut m = BTreeMap::new();
    let mut q = VecDeque::new();
    q.push_back(ROOT_INO);
    while let Some(dir) = q.pop_front() {
        for c in eng.readdir(dir) {
            m.insert(c.id, (dir, c.name.clone()));
            if matches!(c.kind, NodeKind::Dir) {
                q.push_back(c.id);
            }
        }
    }
    m
}

/// Build a random source tree in `eng` and return every created source path
/// (directories and files), rooted at names that never collide with the synthetic
/// alias namespace `/zz/...` used by the generated rules.
fn build_tree(rng: &mut Rng, eng: &Arc<MetaEngine>) -> Vec<String> {
    let mut paths = Vec::new();
    let tops = 2 + rng.below(3); // 2..4 top-level dirs
    for i in 0..tops {
        let gname = format!("g{i}");
        let g = eng
            .create_node(ROOT_INO, &gname, true, None, 0o0755, 1000, 1000)
            .unwrap();
        paths.push(format!("/{gname}"));
        let subdirs = rng.below(3); // 0..2 subdirs
        for j in 0..subdirs {
            let pname = format!("p{i}_{j}");
            let p = eng
                .create_node(g, &pname, true, None, 0o0755, 1000, 1000)
                .unwrap();
            let ppath = format!("/{gname}/{pname}");
            paths.push(ppath.clone());
            // leaf files under the subdir
            for k in 0..(rng.below(2) + 1) {
                let fname = format!("f{i}_{j}_{k}");
                eng.create_node(
                    p,
                    &fname,
                    false,
                    Some(&format!("key-{i}-{j}-{k}")),
                    0o0644,
                    1000,
                    1000,
                )
                .unwrap();
                paths.push(format!("{ppath}/{fname}"));
            }
        }
        // leaf files directly under the top dir
        for k in 0..(rng.below(3)) {
            let fname = format!("tf{i}_{k}");
            eng.create_node(
                g,
                &fname,
                false,
                Some(&format!("tkey-{i}-{k}")),
                0o0644,
                1000,
                1000,
            )
            .unwrap();
            paths.push(format!("/{gname}/{fname}"));
        }
    }
    paths
}

/// Generate a well-formed rule script over `paths`. The `/zz` alias namespace is
/// never a real source prefix, so synthesised alias parents cannot create a
/// relocation cycle; sources and destinations are kept pairwise distinct so the
/// composition respects the paper's well-formedness (Λ) precondition.
fn gen_rules(rng: &mut Rng, paths: &[String]) -> (String, usize, usize, usize, usize) {
    let mut lines: Vec<String> = Vec::new();
    let (mut n_only, mut n_hide, mut n_map, mut n_link) = (0, 0, 0, 0);
    let mut used_src: BTreeSet<String> = BTreeSet::new();
    let mut used_dst: BTreeSet<String> = BTreeSet::new();
    let mut alias = 0usize;

    // `only` is a whole-view whitelist: used alone when chosen.
    if rng.chance(20) && !paths.is_empty() {
        let src = &paths[rng.below(paths.len() as u64)];
        lines.push(format!("only {src}"));
        n_only += 1;
        return (lines.join("\n") + "\n", n_only, n_hide, n_map, n_link);
    }

    let ops = rng.below(6); // 0..5 hide/map/link ops
    for _ in 0..ops {
        if paths.is_empty() {
            break;
        }
        let src = paths[rng.below(paths.len() as u64)].clone();
        let pick = rng.below(3);
        if pick == 0 {
            // hide any source path (idempotent, always well-formed)
            lines.push(format!("hide {src}"));
            n_hide += 1;
        } else {
            // map / link: require an unused src and a fresh, unique dst.
            if used_src.contains(&src) {
                continue;
            }
            let name = src.rsplit('/').next().unwrap_or("x").to_string();
            let dst = format!("/zz/a{alias}/{name}");
            alias += 1;
            if used_dst.contains(&dst) {
                continue;
            }
            used_src.insert(src.clone());
            used_dst.insert(dst.clone());
            if pick == 1 {
                lines.push(format!("map {src} {dst}"));
                n_map += 1;
            } else {
                lines.push(format!("link {src} {dst}"));
                n_link += 1;
            }
        }
    }
    (
        lines.join("\n") + if lines.is_empty() { "" } else { "\n" },
        n_only,
        n_hide,
        n_map,
        n_link,
    )
}

/// One randomised case. `has_only` switches off the write-through create probe
/// (in `only` mode a shown ancestor is deliberately read-only).
fn check_case(seed: u64, counters: &mut Counters) {
    let path = tmpdb(seed);
    let eng = Arc::new(MetaEngine::open(&path).unwrap());
    let mut rng = Rng::new(seed);
    let paths = build_tree(&mut rng, &eng);
    let (text, n_only, n_hide, n_map, n_link) = gen_rules(&mut rng, &paths);
    let has_only = n_only > 0;

    let rules = match LayoutRules::parse(&text) {
        Ok(r) => r,
        Err(_) => {
            cleanup(&path);
            return; // generator produced a script the parser rejects: not a
                    // governance violation, just skip this seed.
        }
    };
    counters.seeds += 1;
    if has_only {
        counters.only_seen += 1;
    }
    if n_hide > 0 {
        counters.hide_seen += 1;
    }
    if n_map > 0 {
        counters.map_seen += 1;
    }
    if n_link > 0 {
        counters.link_seen += 1;
    }

    let storage_before = eng_count(&*eng);
    let snap_before = eng_snapshot(&*eng);
    let lens = LayoutMeta::new(eng.clone(), rules);

    // ---- DFS the lens, checking I1-a (distinct names), I2 (identity),
    //      I1-b (reads add no rows) ----
    let mut stack = vec![ROOT_INO];
    let mut expansions = 0usize;
    let mut content_key_id: BTreeMap<String, i64> = BTreeMap::new();
    let mut real_dirs: Vec<i64> = Vec::new();
    let mut synth_dirs: Vec<i64> = Vec::new();
    while let Some(dir) = stack.pop() {
        expansions += 1;
        if expansions > 50_000 {
            panic!("seed {seed}: lens traversal did not terminate (alias cycle?)");
        }
        let entries = lens.readdir(dir);
        // I1-a: a single listing never repeats a name.
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for e in &entries {
            assert!(
                names.insert(e.name.as_str()),
                "seed {seed} text={text:?}: duplicate directory entry {:?} in listing of {dir}",
                e.name
            );
            // I2: one content key => one inode (aliases share, never duplicate).
            if let Some(dr) = &e.data_ref {
                match content_key_id.get(dr) {
                    Some(prev) => assert_eq!(
                        *prev,
                        e.id,
                        "seed {seed} text={text:?}: content key {dr:?} surfaced at two distinct ids ({prev} vs {}) — duplicate stored entry",
                        e.id
                    ),
                    None => {
                        content_key_id.insert(dr.clone(), e.id);
                    }
                }
            }
            // A node is real iff it exists in the underlying store; synthesised
            // dirs exist only under the lens and must never masquerade as files.
            let real = eng.getattr(e.id).is_some();
            if real {
                // identity: the store's own record agrees on name
                let e0 = eng.getattr(e.id).unwrap();
                assert_eq!(
                    e0.name, e.name,
                    "seed {seed}: id {} name mismatch lens={:?} store={:?}",
                    e.id, e.name, e0.name
                );
                if matches!(e.kind, NodeKind::Dir) {
                    real_dirs.push(e.id);
                    stack.push(e.id);
                }
            } else {
                assert!(
                    matches!(e.kind, NodeKind::Dir),
                    "seed {seed}: synthesised (not-in-store) node id {} must be a directory",
                    e.id
                );
                synth_dirs.push(e.id);
                // descend into a synthesised dir; its children map onto real ids
                stack.push(e.id);
            }
        }
    }

    // I1-b: read-only traversal must not have touched the store.
    assert_eq!(
        eng_count(&*eng),
        storage_before,
        "seed {seed} text={text:?}: read-only lens traversal changed stored node count"
    );
    assert_eq!(
        eng_snapshot(&*eng),
        snap_before,
        "seed {seed} text={text:?}: read-only lens traversal disturbed a stored edge"
    );

    // ---- I4 write-through: a create in a REAL dir lands in the store ----
    if !has_only {
        let mut marker = 0usize;
        for d in real_dirs.iter().take(6) {
            let name = format!("MARK{marker}");
            marker += 1;
            match lens.create_node(*d, &name, false, Some("mk"), 0o0644, 1000, 1000) {
                Ok(_) => {
                    assert!(
                        eng.lookup(*d, &name).is_some(),
                        "seed {seed}: write-through create at real dir {d} /{name} not visible in store (I4)"
                    );
                    counters.write_through_verified += 1;
                }
                Err(err) => {
                    panic!(
                        "seed {seed}: lens refused create in a REAL dir {d} ({err:?}) — a non-synthesised directory must stay writable"
                    );
                }
            }
        }
    }

    // ---- I4 no-materialisation: create in a SYNTHESISED dir is refused and
    //      leaves the store untouched ----
    let before_refusals = eng_count(&*eng);
    let mut refused_here = 0usize;
    for d in synth_dirs.iter().take(6) {
        if let Ok(_) = lens.create_node(*d, "sneak", true, None, 0o0755, 1000, 1000) {
            panic!(
                "seed {seed}: lens materialised a source twin by creating in synthesised dir {d} (I4)"
            );
        }
        refused_here += 1;
    }
    if refused_here > 0 {
        assert_eq!(
            eng_count(&*eng),
            before_refusals,
            "seed {seed}: a refused create in a synthesised dir still changed the store"
        );
        counters.refusal_verified += refused_here;
    }

    // ---- I1 survives mutation: after writes the listing stays name-unique ----
    for d in real_dirs.iter().take(4) {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for e in lens.readdir(*d) {
            assert!(
                seen.insert(e.name.clone()),
                "seed {seed}: post-write duplicate name in dir {d}"
            );
        }
    }

    cleanup(&path);
}

struct Counters {
    seeds: usize,
    only_seen: usize,
    hide_seen: usize,
    map_seen: usize,
    link_seen: usize,
    write_through_verified: usize,
    refusal_verified: usize,
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn governance_invariants_hold_over_random_lens_compositions() {
    let mut counters = Counters {
        seeds: 0,
        only_seen: 0,
        hide_seen: 0,
        map_seen: 0,
        link_seen: 0,
        write_through_verified: 0,
        refusal_verified: 0,
    };
    for seed in 1..=400u64 {
        check_case(seed, &mut counters);
    }

    // The generator must actually have exercised every primitive and both I4
    // branches, else the property check would be vacuously true.
    assert!(counters.seeds > 300, "expected broad seed coverage");
    assert!(counters.only_seen > 0, "never exercised an `only` view");
    assert!(counters.hide_seen > 0, "never exercised a `hide` rule");
    assert!(counters.map_seen > 0, "never exercised a `map` rule");
    assert!(counters.link_seen > 0, "never exercised a `link` rule");
    assert!(
        counters.write_through_verified > 0,
        "never verified write-through into the store"
    );
    assert!(
        counters.refusal_verified > 0,
        "never exercised synthesised-dir no-materialisation refusals"
    );

    eprintln!(
        "gov_invariants: seeds={} only={} hide={} map={} link={} write_through_ok={} refusals_ok={}",
        counters.seeds,
        counters.only_seen,
        counters.hide_seen,
        counters.map_seen,
        counters.link_seen,
        counters.write_through_verified,
        counters.refusal_verified
    );
}
