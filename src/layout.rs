//! Per-mount presentation overlay ("layout"): the same stored tree, rendered
//! differently for different clients.
//!
//! A view is ONE authoritative tree, but an operator may want client A to see
//! only a slice of it and client B to see the same bytes reorganized under a
//! friendlier hierarchy. [`LayoutMeta`] wraps any [`MetaProvider`] (local
//! [`crate::meta::MetaEngine`] or a remote `MetaServer` client) and translates
//! between a *display* namespace and the *source* tree at the metadata level,
//! below the FUSE layer. Nothing is copied or duplicated: directory entries
//! exist exactly once in the source view, each mount renders them through its
//! own rule lens, and every write lands on the real entry — so object GC,
//! refcounts and cross-client sharing are untouched.
//!
//! Rules (plain text, one per line, `#` starts a comment):
//! - `hide  <src-path>`            hide a subtree at its original location
//! - `only  <src-path>`            whitelist mode: show only these subtrees
//!                                 (plus the ancestor dirs needed to reach them)
//! - `map   <src-path> <dst-path>` reorganize: the entry is displayed at
//!                                 `<dst>` and disappears from its source spot
//! - `link  <src-path> <dst-path>` alias: displayed at `<dst>` AND kept at its
//!                                 source spot (same inode in both places)
//!
//! Design notes & honest limits:
//! - `hide`/`only` are evaluated against SOURCE paths; `map`/`link` display a
//!   subtree regardless of `only`/`hide` at the source spot (that is their
//!   whole purpose), while filtering still applies INSIDE the moved subtree by
//!   source path. Filters and relocation compose without contradiction.
//! - Intermediate display dirs that do not exist in the source view are
//!   synthesized with inodes from a reserved high range (`i64::MAX` downward);
//!   creating directly inside a synthesized dir is refused with EPERM (there is
//!   no source anchor for the entry). Everything under a real or relocated dir
//!   is fully writable — writes are passed to the source coordinates.
//! - `map`/`link` leaves override the node's reported name/parent so `..` and
//!   attributes stay inside the lens (for `map`; a `link` alias keeps its
//!   canonical source identity and its `..` leads to the source parent).
//! - Path→inode caches expire after a short TTL so out-of-band renames are
//!   picked up; a local mutation on this mount flushes the resolution caches.
//! - With a shared `meta-server`, push invalidation events reference SOURCE
//!   inodes; the lens translates each source entry change into the display
//!   coordinates it renders at (see [`LayoutMeta::display_targets`]), so even
//!   a leaf hanging under a synthesized dir converges on the push instead of
//!   the attribute TTL. The residual TTL-bounded case is a RULE-SET change:
//!   scaffolding dirs appear/disappear only with the rules, and a rule edit
//!   converges by server-side hot-reload, not by an entry event.

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, ROOT_INO};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Resolution-cache lifetime. Matches the FUSE attr/dentry TTL, so the lens is
/// never staler than what the kernel already caches underneath it.
const CACHE_TTL: Duration = Duration::from_secs(2);

/// Hard ceilings on the path/inode resolution caches (see [`bound_map`]). TTL
/// expiry is the normal bound, but an entry is only reclaimed when the SAME
/// path is looked up again after expiring — a mount that walks a very large,
/// churning namespace would otherwise keep every distinct path forever. These
/// caps bound that growth; eviction prefers the stalest entries so the working
/// set is retained.
const RESOLVE_CAP: usize = 65_536;
const SRC_PATH_CAP: usize = 65_536;
/// How many materialized directory listings a lens keeps. The kernel asks for
/// one directory in many READDIR buffers; without this the whole shaped
/// listing (rule engine included) was recomputed per buffer, so a big
/// directory cost O(n^2). Four dirs covers a `find` walking down one branch
/// plus a concurrent `ls` of another.
const PAGE_CACHE_DIRS: usize = 4;
/// Ceiling on the entries held across those listings. Past it the listing is
/// still served (correct, just recomputed next buffer) rather than eating RAM.
const PAGE_CACHE_ROWS: usize = 400_000;

/// Enforce a hard ceiling on a `(Instant, _)`-valued TTL map. First drop every
/// entry already past [`CACHE_TTL`] — the steady-state bound, since a still-used
/// path is refreshed in place on its next resolve — and only if the map is
/// STILL over `cap` (extreme churn inside one TTL window) keep the `cap`
/// freshest entries by their stored resolve timestamp, dropping the rest.
///
/// NOTE: this is deliberately eviction-by-RESOLVE-time, not by last-ACCESS.
/// Bumping the timestamp on a cache hit would make a continuously-read entry
/// never expire, silently breaking the mandatory freshness contract (an
/// out-of-band rename must become visible after CACHE_TTL even under sustained
/// traffic). So we keep the two roles separate: TTL governs staleness, `cap`
/// governs memory, and stalest-first eviction only kicks in under memory
/// pressure. O(len) work happens only when already over `cap`, amortizing to a
/// `len()` check in the common case.
fn bound_map<K, V>(map: &mut HashMap<K, (Instant, V)>, cap: usize)
where
    K: Eq + std::hash::Hash,
{
    if map.len() <= cap {
        return;
    }
    map.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    if map.len() <= cap {
        return;
    }
    let mut times: Vec<Instant> = map.values().map(|(at, _)| *at).collect();
    times.sort();
    let cutoff = times[times.len() - cap];
    map.retain(|_, (at, _)| *at >= cutoff);
}

/// One relocation rule: source path displayed at a destination path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloc {
    /// Source path in the underlying view (canonical identity of the entry).
    pub from: String,
    /// Destination path in the display namespace.
    pub to: String,
    /// `true` for `link` (alias: source spot keeps showing it too), `false`
    /// for `map` (the entry disappears from its source spot).
    pub keep_src: bool,
}

/// A parsed, normalized set of display rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutRules {
    pub hide: Vec<String>,
    pub only: Vec<String>,
    pub reloc: Vec<Reloc>,
}

/// What a server-side `<db-path>/meta/views/<name>.layout` declares. The
/// shapes share one file slot and are mutually exclusive per entry:
/// - [`ViewDef::Derived`]: first line `base <physical view>` plus rule verbs
///   — a centrally defined virtual shape; mounts are READ-ONLY (every write
///   belongs to the base view).
/// - [`ViewDef::Alias`]: a lone `alias <physical view>` line, no rules — a
///   PURE ALIAS: the mount surface is the alias name, serving the full tree
///   WRITABLE, and the real name stops being mountable through a
///   `meta-server` (name governance, so public names can rotate without
///   exposing storage identity).
/// - [`ViewDef::Fork`]: a lone `fork <physical view>` line, no rules — an
///   INDEPENDENT metadata tree seeded from the base view at creation time
///   (`<db-path>/meta/<name>.db`, content bytes shared through the cross-view
///   ledger). Structure (mkdir/rename/unlink) is writable; CONTENT writes are
///   refused, and a delete removes only this view's names — the base's tree
///   and the shared bytes are untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewDef {
    Derived(String, LayoutRules),
    Alias(String),
    Fork(String),
}

/// Normalize an absolute rule path: `/a//b/` -> `/a/b`; root is `/`.
pub fn canon(p: &str) -> Result<String, String> {
    if !p.starts_with('/') {
        return Err(format!("path must start with '/': '{p}'"));
    }
    let mut out = String::new();
    for c in p.split('/').filter(|c| !c.is_empty()) {
        if c == "." || c == ".." {
            return Err(format!("'.'/'..' not allowed in rule path: '{p}'"));
        }
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// `a` is an ancestor-or-self of `b` on path components.
fn is_anc(a: &str, b: &str) -> bool {
    if a == b || a == "/" {
        return true;
    }
    let av: Vec<&str> = a.split('/').filter(|s| !s.is_empty()).collect();
    let bv: Vec<&str> = b.split('/').filter(|s| !s.is_empty()).collect();
    av.len() < bv.len() && av.iter().zip(bv.iter()).all(|(x, y)| x == y)
}

/// Join a display path with a child name.
fn jjoin(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// A view name used as a db/file component (`meta/<view>.db`,
/// `meta/views/<name>.layout`): must be a single safe path element.
pub fn valid_view_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_whitespace)
}

impl LayoutRules {
    /// Parse a rules file body. Fails on an unknown verb or a destination used
    /// by two rules (display paths must be unambiguous).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut r = LayoutRules::default();
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let verb = it.next().unwrap();
            let err = |m: String| Err(format!("line {}: {m}", ln + 1));
            match verb {
                "hide" | "only" => {
                    let p = canon(it.next().ok_or_else(|| format!("'{verb}' needs a path"))?)?;
                    if it.next().is_some() {
                        return err(format!("'{verb}' takes exactly one path"));
                    }
                    if verb == "hide" {
                        r.hide.push(p);
                    } else {
                        r.only.push(p);
                    }
                }
                "map" | "link" => {
                    let f = it
                        .next()
                        .ok_or_else(|| format!("'{verb}' needs <src> <dst>"))?;
                    let t = it
                        .next()
                        .ok_or_else(|| format!("'{verb}' needs <src> <dst>"))?;
                    if it.next().is_some() {
                        return err(format!("'{verb}' takes exactly two paths"));
                    }
                    let from = canon(f)?;
                    let to = canon(t)?;
                    if from == to {
                        return err(format!("'{verb}': src and dst are the same ({from})"));
                    }
                    if r.reloc.iter().any(|x| x.to == to) {
                        return err(format!("destination '{to}' used by two rules"));
                    }
                    r.reloc.push(Reloc {
                        from,
                        to,
                        keep_src: verb == "link",
                    });
                }
                other => return err(format!("unknown layout verb '{other}'")),
            }
        }
        Ok(r)
    }

    pub fn parse_file(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read layout file {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse a server-side **named view** definition: the same rule verbs as
    /// [`LayoutRules::parse`] plus exactly one `base <view>` line naming the
    /// physical view tree the lens reads. Such a file lives at
    /// `<db-path>/meta/views/<name>.layout`; the meta-server resolves it at
    /// `M_OPEN`, so a client just mounts `--view <name>` and gets its own
    /// presentation of one shared tree — no rules file on the client.
    pub fn parse_view(text: &str) -> Result<(String, Self), String> {
        let mut base: Option<String> = None;
        let mut body = String::new();
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.starts_with("base ") || line.starts_with("base\t") {
                let mut it = line.split_whitespace();
                it.next().unwrap();
                let b = it
                    .next()
                    .ok_or_else(|| format!("line {}: 'base' needs a view name", ln + 1))?;
                if it.next().is_some() {
                    return Err(format!(
                        "line {}: 'base' takes exactly one view name",
                        ln + 1
                    ));
                }
                if base.is_some() {
                    return Err(format!("line {}: duplicate 'base' directive", ln + 1));
                }
                if !valid_view_name(b) {
                    return Err(format!("line {}: invalid base view name '{b}'", ln + 1));
                }
                base = Some(b.to_string());
            } else {
                body.push_str(raw);
                body.push('\n');
            }
        }
        let base = base.ok_or_else(|| "view definition needs a `base <view>` line".to_string())?;
        let rules = Self::parse(&body)?;
        Ok((base, rules))
    }

    pub fn parse_view_file(path: &std::path::Path) -> Result<(String, Self), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read view file {}: {e}", path.display()))?;
        Self::parse_view(&text)
    }

    /// Parse a server-side named-view definition in one of its THREE
    /// governance shapes (mutually exclusive in one file):
    /// - `base <view>` + rule verbs → a DERIVED (virtual) view: centrally
    ///   shaped, mounted READ-ONLY.
    /// - `alias <view>` alone → a PURE ALIAS: the externally used name maps
    ///   1:1 onto a physical view, keeps its full shape and stays WRITABLE;
    ///   a `meta-server` then refuses mounts of the real name itself, so
    ///   public names can be renamed freely while the storage identity
    ///   stays unexposed ("the name is the alias, the mapping is the entry").
    /// - `fork <view>` alone → a FORK: an independent metadata tree seeded
    ///   from the base at creation; structure writable, content read-only,
    ///   deletes remove only this view's names.
    pub fn parse_def(text: &str) -> Result<ViewDef, String> {
        let mut alias: Option<String> = None;
        let mut fork: Option<String> = None;
        let mut has_base = false;
        let mut rules_seen = false;
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("alias ") || line.starts_with("alias\t") {
                let mut it = line.split_whitespace();
                it.next().unwrap();
                let v = it
                    .next()
                    .ok_or_else(|| format!("line {}: 'alias' needs a view name", ln + 1))?;
                if it.next().is_some() {
                    return Err(format!(
                        "line {}: 'alias' takes exactly one view name",
                        ln + 1
                    ));
                }
                if !valid_view_name(v) {
                    return Err(format!("line {}: invalid view name '{v}'", ln + 1));
                }
                if alias.replace(v.to_string()).is_some() {
                    return Err(format!(
                        "line {}: at most one `alias` line per definition",
                        ln + 1
                    ));
                }
            } else if line.starts_with("fork ") || line.starts_with("fork\t") {
                let mut it = line.split_whitespace();
                it.next().unwrap();
                let v = it
                    .next()
                    .ok_or_else(|| format!("line {}: 'fork' needs a view name", ln + 1))?;
                if it.next().is_some() {
                    return Err(format!(
                        "line {}: 'fork' takes exactly one view name",
                        ln + 1
                    ));
                }
                if !valid_view_name(v) {
                    return Err(format!("line {}: invalid view name '{v}'", ln + 1));
                }
                if fork.replace(v.to_string()).is_some() {
                    return Err(format!(
                        "line {}: at most one `fork` line per definition",
                        ln + 1
                    ));
                }
            } else if line.starts_with("base ") || line.starts_with("base\t") {
                has_base = true;
            } else {
                rules_seen = true;
            }
        }
        match (alias, fork, has_base) {
            (Some(v), None, false) => {
                if rules_seen {
                    return Err(
                        "`alias` definitions carry no rule lines (use `base` for a shaped view)"
                            .to_string(),
                    );
                }
                Ok(ViewDef::Alias(v))
            }
            (None, Some(v), false) => {
                if rules_seen {
                    return Err("`fork` definitions carry no rule lines (a fork is an independent tree, not a lens)".to_string());
                }
                Ok(ViewDef::Fork(v))
            }
            (None, None, true) => Self::parse_view(text).map(|(b, r)| ViewDef::Derived(b, r)),
            (Some(_), Some(_), _) => {
                Err("definition has both `alias` and `fork` lines".to_string())
            }
            (Some(_), _, true) => Err("definition has both `alias` and `base` lines".to_string()),
            (_, Some(_), true) => Err("definition has both `fork` and `base` lines".to_string()),
            (None, None, false) => Err(
                "view definition needs a `base <view>`, `alias <view>` or `fork <view>` line"
                    .to_string(),
            ),
        }
    }

    pub fn parse_def_file(path: &std::path::Path) -> Result<ViewDef, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read view file {}: {e}", path.display()))?;
        Self::parse_def(&text)
    }

    pub fn is_empty(&self) -> bool {
        self.hide.is_empty() && self.only.is_empty() && self.reloc.is_empty()
    }

    /// Is a SOURCE path suppressed from its original spot through the lens?
    /// Covers `hide` subtrees, `only` whitelists, and `map`-emptied sources.
    fn blocked_src(&self, p: &str) -> bool {
        if self.hide.iter().any(|h| is_anc(h, p)) {
            return true;
        }
        if self.reloc.iter().any(|r| !r.keep_src && r.from == p) {
            return true;
        }
        if !self.only.is_empty() {
            let shown = self.only.iter().any(|s| is_anc(s, p) || is_anc(p, s))
                // A relocated subtree is displayed wholesale through its dst.
                || self.reloc.iter().any(|r| is_anc(&r.from, p));
            if !shown {
                return true;
            }
        }
        false
    }

    /// May a NEW entry at source path `p` be created through the lens? Under an
    /// `only` whitelist, creating into a mere pass-through ancestor (a dir the
    /// lens shows only so the eye can reach a shown subtree) is refused — the
    /// new entry could never appear where the user is looking.
    fn writable_src(&self, p: &str) -> bool {
        if self.only.is_empty() {
            return true;
        }
        self.only.iter().any(|s| is_anc(s, p)) || self.reloc.iter().any(|r| is_anc(&r.from, p))
    }

    /// Is this source path the `from` of a `map` (non-alias) relocation? Its
    /// display node gets name/parent overridden so `..` stays inside the lens.
    fn is_mapped_from(&self, p: &str) -> bool {
        self.reloc.iter().any(|r| !r.keep_src && r.from == p)
    }
}

/// Everything mutable lives behind one fast lock; the FUSE session is already
/// multi-threaded, and all inner-provider calls happen OUTSIDE the guard
/// (parking_lot locks are not reentrant).
struct State {
    /// source display path -> resolved inode (positive and negative).
    resolve: HashMap<String, (Instant, Option<i64>)>,
    /// real inode -> its SOURCE path (walk of the raw tree).
    src_path: HashMap<i64, (Instant, String)>,
    /// real inode -> display path, only for inodes the lens relocated (so a
    /// nested rule under the relocated dir keys on the display prefix).
    disp_of: HashMap<i64, String>,
    /// relocated display leaves: real inode -> NodeInfo with lens name/parent.
    overrides: HashMap<i64, NodeInfo>,
    /// synthesized dirs: virtual inode -> (display path, parent inode).
    virt_by_ino: HashMap<i64, (String, i64)>,
    virt_by_path: HashMap<String, i64>,
    next_vino: i64,
}

/// One materialized lens listing, served to every READDIR buffer of that
/// directory until [`CACHE_TTL`] expires. Holds the already-shaped rows, so a
/// cache hit costs a slice, not a rule pass.
struct CachedListing {
    at: Instant,
    rows: Arc<Vec<NodeInfo>>,
}

/// Per-directory listing cache (see [`PAGE_CACHE_DIRS`]).
struct PageCache {
    dirs: HashMap<i64, CachedListing>,
    rows: usize,
}

/// The overlay itself: `MetaProvider` implementation translating between the
/// display namespace and the wrapped source provider.
pub struct LayoutMeta {
    inner: Arc<dyn MetaProvider>,
    rules: LayoutRules,
    /// parent display path -> (leaf display name -> source path).
    trie: HashMap<String, HashMap<String, String>>,
    /// parent display path -> synthesized intermediate dir names.
    vdirs: HashMap<String, Vec<String>>,
    st: Mutex<State>,
    /// Materialized directory listings, keyed by the directory inode (see
    /// [`PageCache`]): the shaped listing is computed once per [`CACHE_TTL`]
    /// and then sliced per READDIR buffer.
    pages: Mutex<PageCache>,
    uid: u32,
    gid: u32,
    born: i64,
}

impl LayoutMeta {
    pub fn new(inner: Arc<dyn MetaProvider>, rules: LayoutRules) -> Self {
        let mut trie: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut vdirs: HashMap<String, Vec<String>> = HashMap::new();
        for r in &rules.reloc {
            // Walk the destination path: every strict prefix is a dir the
            // display tree needs; the final component is the relocated leaf.
            let comps: Vec<&str> = r.to.split('/').filter(|c| !c.is_empty()).collect();
            let mut cur = "/".to_string();
            for c in &comps[..comps.len().saturating_sub(1)] {
                let e = vdirs.entry(cur.clone()).or_default();
                if !e.iter().any(|d| d == c) {
                    e.push(c.to_string());
                }
                cur = jjoin(&cur, c);
            }
            if let Some(leaf) = comps.last() {
                trie.entry(cur)
                    .or_default()
                    .insert(leaf.to_string(), r.from.clone());
            }
        }
        Self {
            inner,
            rules,
            trie,
            vdirs,
            st: Mutex::new(State {
                resolve: HashMap::new(),
                src_path: HashMap::new(),
                disp_of: HashMap::new(),
                overrides: HashMap::new(),
                virt_by_ino: HashMap::new(),
                virt_by_path: HashMap::new(),
                next_vino: i64::MAX,
            }),
            pages: Mutex::new(PageCache {
                dirs: HashMap::new(),
                rows: 0,
            }),
            uid: crate::platform::getuid(),
            gid: crate::platform::getgid(),
            born: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }

    fn is_virtual(&self, ino: i64) -> bool {
        self.st.lock().virt_by_ino.contains_key(&ino)
    }

    /// Flush the path-resolution caches after a mutation made through THIS
    /// mount (renames/creates/unlinks can move source paths underneath us).
    fn invalidate(&self) {
        let mut st = self.st.lock();
        st.resolve.clear();
        st.src_path.clear();
        drop(st);
        let mut c = self.pages.lock();
        c.dirs.clear();
        c.rows = 0;
    }

    /// Materialize (or reuse) the shaped listing of `parent`. The kernel walks
    /// one directory across many READDIR buffers; running the rules once per
    /// [`CACHE_TTL`] window and slicing per buffer turns that O(n^2) walk into
    /// O(n). A listing larger than [`PAGE_CACHE_ROWS`] is still served — just
    /// not cached, so it costs a rebuild on the next buffer.
    fn listing(&self, parent: i64) -> Arc<Vec<NodeInfo>> {
        {
            let c = self.pages.lock();
            if let Some(hit) = c.dirs.get(&parent) {
                if hit.at.elapsed() < CACHE_TTL {
                    return Arc::clone(&hit.rows);
                }
            }
        }
        let rows = Arc::new(self.readdir(parent));
        if rows.len() > PAGE_CACHE_ROWS {
            return rows;
        }
        let mut c = self.pages.lock();
        if let Some(old) = c.dirs.insert(
            parent,
            CachedListing {
                at: Instant::now(),
                rows: Arc::clone(&rows),
            },
        ) {
            c.rows = c.rows.saturating_sub(old.rows.len());
        }
        c.rows += rows.len();
        // Evict the stalest directory once either bound is exceeded; the one
        // just inserted is the freshest, so it is never the victim while
        // another entry remains.
        while c.dirs.len() > PAGE_CACHE_DIRS || c.rows > PAGE_CACHE_ROWS {
            let victim = c
                .dirs
                .iter()
                .min_by_key(|(ino, l)| (l.at, *ino))
                .map(|(ino, _)| *ino);
            match victim {
                Some(k) if c.dirs.len() > 1 => {
                    if let Some(v) = c.dirs.remove(&k) {
                        c.rows = c.rows.saturating_sub(v.rows.len());
                    }
                }
                _ => break,
            }
        }
        rows
    }

    /// Resolve a SOURCE path to its inode by walking the raw tree. Results
    /// (including misses) are cached for `CACHE_TTL`.
    fn resolve_src(&self, path: &str) -> Option<i64> {
        if path == "/" {
            return Some(ROOT_INO);
        }
        if let Some((at, v)) = self.st.lock().resolve.get(path).cloned() {
            if at.elapsed() < CACHE_TTL {
                return v;
            }
        }
        let mut cur = ROOT_INO;
        let mut chain = vec![(ROOT_INO, "/".to_string())];
        let mut acc = String::new();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            match self.inner.lookup(cur, c) {
                Some(n) => {
                    acc.push('/');
                    acc.push_str(c);
                    cur = n.id;
                    chain.push((cur, acc.clone()));
                }
                None => {
                    let mut st = self.st.lock();
                    st.resolve.insert(path.to_string(), (Instant::now(), None));
                    bound_map(&mut st.resolve, RESOLVE_CAP);
                    return None;
                }
            }
        }
        {
            let mut st = self.st.lock();
            st.resolve
                .insert(path.to_string(), (Instant::now(), Some(cur)));
            for (ino, p) in chain {
                st.src_path.insert(ino, (Instant::now(), p));
            }
            bound_map(&mut st.resolve, RESOLVE_CAP);
            bound_map(&mut st.src_path, SRC_PATH_CAP);
        }
        Some(cur)
    }

    /// The SOURCE path of a real inode (ancestor walk on the raw tree).
    fn src_path_of(&self, ino: i64) -> Option<String> {
        if ino == ROOT_INO {
            return Some("/".to_string());
        }
        if let Some((at, p)) = self.st.lock().src_path.get(&ino).cloned() {
            if at.elapsed() < CACHE_TTL {
                return Some(p);
            }
        }
        let mut parts = Vec::new();
        let mut cur = ino;
        for _ in 0..4096 {
            let n = self.inner.getattr(cur)?;
            if n.id == ROOT_INO {
                break;
            }
            parts.push(n.name.clone());
            cur = n.parent_id;
            if cur == ROOT_INO {
                break;
            }
        }
        if cur != ROOT_INO {
            return None;
        }
        let p = format!("/{}", parts.into_iter().rev().collect::<Vec<_>>().join("/"));
        {
            let mut st = self.st.lock();
            st.src_path.insert(ino, (Instant::now(), p.clone()));
            bound_map(&mut st.src_path, SRC_PATH_CAP);
        }
        Some(p)
    }

    /// The DISPLAY path a rule dictionary (`trie`/`vdirs`) should be consulted
    /// under for this real inode: the relocated prefix when the lens moved it,
    /// else its source path.
    fn disp_path_of(&self, ino: i64) -> Option<String> {
        if let Some(d) = self.st.lock().disp_of.get(&ino).cloned() {
            return Some(d);
        }
        self.src_path_of(ino)
    }

    /// The inode this lens would hand the kernel for display path `dp`,
    /// walking its own `lookup` so synthesized scaffolding is allocated
    /// exactly as a real traversal would allocate it (the same virtual inode
    /// number, from the same shared map). `None` when the current rules
    /// render nothing at that path (filtered away, or not yet resolvable).
    fn disp_ino_of(&self, dp: &str) -> Option<i64> {
        let mut cur = ROOT_INO;
        for c in dp.split('/').filter(|s| !s.is_empty()) {
            cur = self.lookup(cur, c)?.id;
        }
        Some(cur)
    }

    /// Display coordinates of the entry whose SOURCE path is `src`: one pair
    /// per relocation rule that renders exactly this entry elsewhere. A leaf
    /// under a synthesized scaffolding dir is the coordinate a source-event
    /// invalidation provably cannot reach, which is what this recovers.
    fn display_spots(&self, src: &str) -> Vec<(i64, String)> {
        let mut out: Vec<(i64, String)> = Vec::new();
        for r in &self.rules.reloc {
            if r.from != src {
                continue;
            }
            let dn = basename(&r.to).to_string();
            if let Some(p) = self.disp_ino_of(&dirname(&r.to)) {
                if !out.iter().any(|(i, n)| *i == p && *n == dn) {
                    out.push((p, dn));
                }
            }
        }
        out
    }

    /// Node for a relocated display leaf: the raw source node rendered with
    /// the LENS name/parent (a local copy, so readdir lists it under the
    /// display name). `map` leaves additionally register a persistent getattr
    /// override so `..` and attrs stay inside the lens even on later
    /// by-inode calls; `link` aliases keep their source identity persistent
    /// (the entry is genuinely visible in both places).
    fn leaf_node(
        &self,
        src_path: &str,
        name: &str,
        parent_ino: i64,
        parent_disp: &str,
    ) -> Option<NodeInfo> {
        let ino = self.resolve_src(src_path)?;
        let mut n = self.inner.getattr(ino)?;
        n.name = name.to_string();
        n.parent_id = parent_ino;
        if self.rules.is_mapped_from(src_path) {
            let dp = jjoin(parent_disp, name);
            let mut st = self.st.lock();
            st.overrides.insert(ino, n.clone());
            st.disp_of.insert(ino, dp);
        }
        Some(n)
    }

    /// Resolve a `vdirs` intermediate at display path `cp` under the given
    /// display parent inode: reuse the real dir when the same path exists and
    /// is not filtered away, else synthesize a virtual dir (also used to
    /// *shadow* a real dir the rules hide, so the lens scaffolding wins).
    fn vdir_node(&self, cp: &str, parent_ino: i64) -> NodeInfo {
        if let Some(real) = self.resolve_src(cp) {
            if !self.rules.blocked_src(cp) {
                if let Some(n) = self.inner.getattr(real) {
                    return n;
                }
            }
        }
        let mut st = self.st.lock();
        if let Some(&v) = st.virt_by_path.get(cp) {
            let (p, pi) = st
                .virt_by_ino
                .get(&v)
                .cloned()
                .unwrap_or_else(|| (cp.to_string(), parent_ino));
            return Self::synth_dir(v, basename(&p), pi, self.uid, self.gid, self.born);
        }
        let v = st.next_vino;
        st.next_vino -= 1;
        st.virt_by_ino.insert(v, (cp.to_string(), parent_ino));
        st.virt_by_path.insert(cp.to_string(), v);
        Self::synth_dir(v, basename(cp), parent_ino, self.uid, self.gid, self.born)
    }

    fn synth_dir(ino: i64, name: &str, parent: i64, uid: u32, gid: u32, born: i64) -> NodeInfo {
        NodeInfo {
            id: ino,
            parent_id: parent,
            name: name.to_string(),
            kind: crate::meta::NodeKind::Dir,
            data_ref: None,
            link_target: None,
            mode: 0o0755,
            uid,
            gid,
            mtime: born,
            atime: born,
            ctime: born,
            nlink: 2,
            size: 4096,
        }
    }

    /// The virtual-dir node as currently recorded, or allocate. Public-facing
    /// getattr path; never touches the inner provider (a virtual ino has no
    /// source).
    fn virt_get(&self, ino: i64) -> Option<NodeInfo> {
        let st = self.st.lock();
        let (p, parent) = st.virt_by_ino.get(&ino).cloned()?;
        Some(Self::synth_dir(
            ino,
            basename(&p),
            parent,
            self.uid,
            self.gid,
            self.born,
        ))
    }

    /// True when (parent, name) is occupied by a LENS rule at the display
    /// spot: a relocated leaf or synthesized scaffolding dir. A plain create
    /// or rename-into must hit EEXIST instead of silently making a source
    /// entry that the lens would never show (the name is already taken in
    /// the display namespace).
    fn name_taken(&self, parent: i64, name: &str) -> bool {
        let dp = if self.is_virtual(parent) {
            match self.st.lock().virt_by_ino.get(&parent) {
                Some(x) => x.0.clone(),
                None => return false,
            }
        } else if parent == ROOT_INO {
            "/".to_string()
        } else {
            match self.disp_path_of(parent) {
                Some(d) => d,
                None => return false,
            }
        };
        self.trie
            .get(&dp)
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
            || self
                .vdirs
                .get(&dp)
                .map(|v| v.iter().any(|d| d == name))
                .unwrap_or(false)
    }

    /// Translate a *display* (parent_ino, name) that may address a relocated
    /// leaf into REAL unlink coordinates. Ok(None) = plain pass-through.
    /// Err(OpNotPermitted) = the target is lens scaffolding, not data.
    fn translate_entry(&self, parent: i64, name: &str) -> Result<Option<(i64, String)>, MetaError> {
        let dp = if self.is_virtual(parent) {
            let st = self.st.lock();
            st.virt_by_ino
                .get(&parent)
                .map(|x| x.0.clone())
                .ok_or(MetaError::NotFound)?
        } else if parent == ROOT_INO {
            "/".to_string()
        } else {
            self.disp_path_of(parent).ok_or(MetaError::NotFound)?
        };
        if let Some(sp) = self.trie.get(&dp).and_then(|m| m.get(name)) {
            let sdir = dirname(sp);
            let sname = basename(sp).to_string();
            let sin = self.resolve_src(&sdir).ok_or(MetaError::NotFound)?;
            return Ok(Some((sin, sname)));
        }
        if self
            .vdirs
            .get(&dp)
            .map(|v| v.iter().any(|d| d == name))
            .unwrap_or(false)
        {
            // Refusing to unlink/rmdir lens scaffolding: it has no source row.
            return Err(MetaError::OpNotPermitted);
        }
        Ok(None)
    }

    /// Children a rule dictionary owes under this display path, appended to
    /// `out` (deduplicated by `seen`). Used by both readdir and lookup sides.
    fn rule_children(
        &self,
        dp: &str,
        parent_ino: i64,
        out: &mut Vec<NodeInfo>,
        seen: &mut HashSet<String>,
    ) {
        if let Some(m) = self.trie.get(dp) {
            for (name, sp) in m {
                if seen.contains(name) {
                    continue;
                }
                if let Some(n) = self.leaf_node(sp, name, parent_ino, dp) {
                    seen.insert(name.clone());
                    out.push(n);
                }
            }
        }
        if let Some(ds) = self.vdirs.get(dp) {
            for d in ds {
                if seen.contains(d) {
                    continue;
                }
                let cp = jjoin(dp, d);
                let n = self.vdir_node(&cp, parent_ino);
                seen.insert(d.clone());
                out.push(n);
            }
        }
    }
}

impl MetaProvider for LayoutMeta {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        // Virtual parent: only rule children exist there.
        if self.is_virtual(parent) {
            let pv = self.st.lock().virt_by_ino.get(&parent).cloned()?.0;
            if let Some(sp) = self.trie.get(&pv).and_then(|m| m.get(name)) {
                return self.leaf_node(sp, name, parent, &pv);
            }
            if self
                .vdirs
                .get(&pv)
                .map(|v| v.iter().any(|d| d == name))
                .unwrap_or(false)
            {
                let cp = jjoin(&pv, name);
                return Some(self.vdir_node(&cp, parent));
            }
            return None;
        }
        let dp = if parent == ROOT_INO {
            "/".to_string()
        } else {
            self.disp_path_of(parent)?
        };
        // a) relocated display leaf wins over whatever the source holds here.
        if let Some(sp) = self.trie.get(&dp).and_then(|m| m.get(name)) {
            return self.leaf_node(sp, name, parent, &dp);
        }
        // b) synthesized intermediate dir of some destination path.
        if self
            .vdirs
            .get(&dp)
            .map(|v| v.iter().any(|d| d == name))
            .unwrap_or(false)
        {
            let cp = jjoin(&dp, name);
            return Some(self.vdir_node(&cp, parent));
        }
        // c) pass-through, filtered by the child's SOURCE path.
        let sp = self.src_path_of(parent)?;
        let child_src = jjoin(&sp, name);
        if self.rules.blocked_src(&child_src) {
            return None;
        }
        let mut n = self.inner.lookup(parent, name)?;
        {
            let mut st = self.st.lock();
            st.src_path.insert(n.id, (Instant::now(), child_src));
            // Under a relocated prefix, register the display path too so
            // nested rules key correctly beneath a moved directory.
            if dp != sp {
                st.disp_of.insert(n.id, jjoin(&dp, name));
            }
        }
        // Apply a lens override if this inode was already relocated into view.
        if let Some(o) = self.st.lock().overrides.get(&n.id).cloned() {
            n.name = o.name;
            n.parent_id = o.parent_id;
        }
        Some(n)
    }

    /// See [`MetaProvider::display_targets`]. A relocated entry (`map` moves
    /// it, `link` mirrors it) renders at `basename(dst)` under `dirname(dst)`;
    /// when that display parent is a synthesized scaffolding dir, its inode
    /// appears in NO source event, so a peer's create/remove/rename of the
    /// source entry leaves the display dentry (and the parent's cached
    /// listing) stale until the attribute TTL. Reporting the display
    /// coordinate lets the invalidation push close that window.
    ///
    /// Complete by construction: an entry under a REAL display parent keeps
    /// its source inode identity, so the source `(parent, name)` coordinate
    /// already addresses its dentry, and scaffolding dirs themselves only
    /// appear/disappear with the rule set (a rule edit hot-reloads the lens
    /// server-side and flushes on the next op, not via an entry event).
    fn display_targets(&self, src_parent: i64, name: &str) -> Vec<(i64, String)> {
        if self.rules.reloc.is_empty() {
            return Vec::new();
        }
        let sp = match self.src_path_of(src_parent) {
            Some(p) => p,
            None => return Vec::new(),
        };
        self.display_spots(&jjoin(&sp, name))
    }

    /// See [`MetaProvider::flush_listing`]. The lens materializes a shaped
    /// listing per directory for [`CACHE_TTL`] (see [`LayoutMeta::listing`]), so
    /// a peer's mutation of the base tree — which arrives as a push event, never
    /// as an operation through THIS lens, and therefore never reaches
    /// [`LayoutMeta::invalidate`] — would otherwise stay invisible behind the
    /// cached rows until the TTL. The event handler flushes the coordinates it
    /// invalidates in the kernel, which is exactly the pair this closes.
    fn flush_listing(&self, parent: i64) {
        let mut c = self.pages.lock();
        if let Some(v) = c.dirs.remove(&parent) {
            c.rows = c.rows.saturating_sub(v.rows.len());
        }
    }

    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        if let Some(n) = self.virt_get(ino) {
            return Some(n);
        }
        if let Some(o) = self.st.lock().overrides.get(&ino).cloned() {
            return Some(o);
        }
        self.inner.getattr(ino)
    }

    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        let mut out: Vec<NodeInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if self.is_virtual(parent) {
            let pv = match self.st.lock().virt_by_ino.get(&parent).cloned() {
                Some(x) => x.0,
                None => return Vec::new(),
            };
            self.rule_children(&pv, parent, &mut out, &mut seen);
            out.sort_by(|a, b| a.name.cmp(&b.name));
            return out;
        }
        let dp = match self.disp_path_of(parent) {
            Some(d) => d,
            None => return Vec::new(),
        };
        let sp = match self.src_path_of(parent) {
            Some(s) => s,
            None => return Vec::new(),
        };
        for mut n in self.inner.readdir(parent) {
            let child_src = jjoin(&sp, &n.name);
            // Filtered away, or replaced by a rule of the same display name.
            if self.rules.blocked_src(&child_src)
                || self
                    .trie
                    .get(&dp)
                    .map(|m| m.contains_key(&n.name))
                    .unwrap_or(false)
                || self
                    .vdirs
                    .get(&dp)
                    .map(|v| v.iter().any(|d| *d == n.name))
                    .unwrap_or(false)
            {
                continue;
            }
            {
                let mut st = self.st.lock();
                st.src_path.insert(n.id, (Instant::now(), child_src));
                if dp != sp {
                    let disp = jjoin(&dp, &n.name);
                    st.disp_of.insert(n.id, disp.clone());
                    // A relocated subtree's children carry the lens name only
                    // through the display map; keep NodeInfo itself raw.
                }
            }
            if let Some(o) = self.st.lock().overrides.get(&n.id).cloned() {
                n.name = o.name;
                n.parent_id = o.parent_id;
            }
            seen.insert(n.name.clone());
            out.push(n);
        }
        self.rule_children(&dp, parent, &mut out, &mut seen);
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Serve the kernel's READDIR buffers from one materialized listing. The
    /// trait default (`readdir` + `skip/take` per buffer) re-ran the whole rule
    /// engine for every 4 KB the kernel asked for, which made listing a big
    /// directory quadratic; the shape is computed once per [`CACHE_TTL`] — the
    /// same window the mount's attr/dentry caches already give an out-of-band
    /// change — and every buffer after that is a slice.
    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        let rows = self.listing(parent);
        rows.iter().skip(offset).take(limit).cloned().collect()
    }

    /// The lens pages, just not in SQL: it pages over the materialized listing.
    /// Advertising it is what lets `GroveFS` use its normal fetch chunking (and
    /// READDIRPLUS) instead of asking for one whole listing per buffer.
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
        if self.is_virtual(parent) {
            // No source anchor for an entry inside lens scaffolding.
            return Err(MetaError::OpNotPermitted);
        }
        if self.name_taken(parent, name) {
            // The display slot is taken by a rule; a hidden source entry
            // under it could never be reached through the lens.
            return Err(MetaError::Exists);
        }
        let sp = self.src_path_of(parent).ok_or(MetaError::NotFound)?;
        if !self.rules.writable_src(&jjoin(&sp, name)) {
            return Err(MetaError::OpNotPermitted);
        }
        let r = self
            .inner
            .create_node(parent, name, is_dir, data_ref, mode, uid, gid);
        if r.is_ok() {
            self.invalidate();
        }
        r
    }

    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        if self.is_virtual(parent) {
            return Err(MetaError::OpNotPermitted);
        }
        if self.name_taken(parent, name) {
            return Err(MetaError::Exists);
        }
        let sp = self.src_path_of(parent).ok_or(MetaError::NotFound)?;
        if !self.rules.writable_src(&jjoin(&sp, name)) {
            return Err(MetaError::OpNotPermitted);
        }
        // NOTE: the link TARGET is stored verbatim; the lens does not rewrite
        // symlink payloads (a target may legitimately point at a source path).
        let r = self.inner.create_symlink(parent, name, target, uid, gid);
        if r.is_ok() {
            self.invalidate();
        }
        r
    }

    fn link_node(&self, src_ino: i64, new_parent: i64, new_name: &str) -> Result<i64, MetaError> {
        if self.is_virtual(new_parent) {
            return Err(MetaError::OpNotPermitted);
        }
        if self.name_taken(new_parent, new_name) {
            return Err(MetaError::Exists);
        }
        let sp = self.src_path_of(new_parent).ok_or(MetaError::NotFound)?;
        if !self.rules.writable_src(&jjoin(&sp, new_name)) {
            return Err(MetaError::OpNotPermitted);
        }
        let r = self.inner.link_node(src_ino, new_parent, new_name);
        if r.is_ok() {
            self.invalidate();
        }
        r
    }

    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        let r = match self.translate_entry(parent, name)? {
            Some((sin, sname)) => self.inner.unlink(sin, &sname)?,
            None => {
                if self.is_virtual(parent) {
                    // A name under lens scaffolding that no rule backs does
                    // not exist at all — ENOENT, not EPERM.
                    return Err(MetaError::NotFound);
                }
                self.inner.unlink(parent, name)?
            }
        };
        self.invalidate();
        Ok(r)
    }

    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        if self.is_virtual(new_parent) {
            return Err(MetaError::OpNotPermitted);
        }
        let (fp, fname) = match self.translate_entry(parent, name)? {
            Some((sin, sname)) => (sin, sname),
            None => {
                if self.is_virtual(parent) {
                    return Err(MetaError::NotFound);
                }
                (parent, name.to_string())
            }
        };
        if self.name_taken(new_parent, new_name) {
            // Moving onto a slot the lens reserves for a rule would hide the
            // moved entry behind it; report the name as taken instead.
            return Err(MetaError::Exists);
        }
        let sp = self.src_path_of(new_parent).ok_or(MetaError::NotFound)?;
        if !self.rules.writable_src(&jjoin(&sp, new_name)) {
            return Err(MetaError::OpNotPermitted);
        }
        let r = self
            .inner
            .rename(fp, &fname, new_parent, new_name, noreplace)?;
        self.invalidate();
        Ok(r)
    }

    fn set_mode(&self, ino: i64, mode: u32) {
        if !self.is_virtual(ino) {
            self.inner.set_mode(ino, mode);
        }
    }
    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        if !self.is_virtual(ino) {
            self.inner.set_uid_gid(ino, uid, gid);
        }
    }
    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        if !self.is_virtual(ino) {
            self.inner.set_times(ino, atime, mtime);
        }
    }
    fn touch_mtime(&self, ino: i64) {
        if !self.is_virtual(ino) {
            self.inner.touch_mtime(ino);
        }
    }

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
        if self.is_virtual(ino) {
            return Err(MetaError::NotFound);
        }
        self.inner.lock_op(ino, owner, start, end, ty, pid, sid)
    }
    fn get_lock(
        &self,
        ino: i64,
        owner: u64,
        start: u64,
        end: u64,
        ty: LockType,
    ) -> Option<LockConflict> {
        if self.is_virtual(ino) {
            return None;
        }
        self.inner.get_lock(ino, owner, start, end, ty)
    }
    fn release_locks_of_owner(&self, owner: u64) {
        self.inner.release_locks_of_owner(owner);
    }
    fn renew_locks(&self, sid: u64) {
        self.inner.renew_locks(sid);
    }

    fn fence_handle(&self) -> Arc<AtomicU64> {
        self.inner.fence_handle()
    }
    fn fetch_fence_epoch(&self) -> u64 {
        self.inner.fetch_fence_epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules_and_errors() {
        let r = LayoutRules::parse(
            "# c\nhide /a/b\nonly /docs\nonly /photos\nmap /x/y /team/y\nlink /lib /p/mylib\n",
        )
        .unwrap();
        assert_eq!(r.hide, vec!["/a/b"]);
        assert_eq!(r.only, vec!["/docs", "/photos"]);
        assert_eq!(r.reloc.len(), 2);
        assert!(!r.reloc[0].keep_src);
        assert!(r.reloc[1].keep_src);
        assert!(LayoutRules::parse("bogus /a").is_err());
        assert!(LayoutRules::parse("hide a/b").is_err());
        assert!(LayoutRules::parse("hide /a /b").is_err());
        assert!(LayoutRules::parse("map /a /b\nlink /c /b").is_err());
        assert!(LayoutRules::parse("hide /../x").is_err());
    }

    #[test]
    fn path_predicates() {
        assert!(is_anc("/", "/a/b"));
        assert!(is_anc("/a", "/a/b"));
        assert!(is_anc("/a/b", "/a/b"));
        assert!(!is_anc("/a/b", "/a/bc"));
        assert!(!is_anc("/a/bc", "/a/b"));
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(basename("/a/b"), "b");
        assert_eq!(jjoin("/", "x"), "/x");
        assert_eq!(jjoin("/x", "y"), "/x/y");
    }

    #[test]
    fn filter_logic() {
        let r = LayoutRules::parse("hide /a/secret\nonly /docs\nmap /docs/x /e\n").unwrap();
        // inside hide -> blocked
        assert!(r.blocked_src("/a/secret"));
        assert!(r.blocked_src("/a/secret/f"));
        // only-whitelist: outside shown subtrees -> blocked
        assert!(r.blocked_src("/other"));
        // ancestors of a shown path stay visible as pass-through
        assert!(!r.blocked_src("/docs"));
        // map'd source spot is vacated; its children render through the dst
        // and are only filtered by their own source paths (under show /docs)
        assert!(r.blocked_src("/docs/x"));
        assert!(!r.blocked_src("/docs/x/child"));
        // pass-through ancestors are not writable, shown subtrees are
        assert!(r.writable_src("/docs/new"));
        assert!(!r.writable_src("/newtop"));
        // moved-out: exact from path blocked at source spot, children not
        let m = LayoutRules::parse("map /d/e /f/g").unwrap();
        assert!(m.blocked_src("/d/e"));
        assert!(!m.blocked_src("/d/other"));
    }

    #[test]
    fn parse_view_definition() {
        let (base, r) = LayoutRules::parse_view(
            "# team docs lens\nbase alice\nonly /docs\nmap /d/raw /team/raw\n",
        )
        .unwrap();
        assert_eq!(base, "alice");
        assert_eq!(r.only, vec!["/docs"]);
        assert_eq!(r.reloc.len(), 1);
        // exactly one base line, and it must carry a safe single-path-element name
        assert!(LayoutRules::parse_view("only /a").is_err());
        assert!(LayoutRules::parse_view("base a\nbase b\nonly /a").is_err());
        assert!(LayoutRules::parse_view("base a b").is_err());
        assert!(LayoutRules::parse_view("base ../x").is_err());
        assert!(LayoutRules::parse_view("base a/b").is_err());
        assert!(LayoutRules::parse_view("base\t").is_err());
        assert!(valid_view_name("team-a"));
        assert!(!valid_view_name(""));
        assert!(!valid_view_name(".."));
        assert!(!valid_view_name("a/b"));
        assert!(!valid_view_name("a b"));
    }

    #[test]
    fn parse_alias_definition() {
        // PURE ALIAS: one line, no rules, writable full shape
        let d = LayoutRules::parse_def("# public name for team\nalias team\n").unwrap();
        assert_eq!(d, ViewDef::Alias("team".to_string()));
        // mutually exclusive / malformed forms are rejected
        assert!(LayoutRules::parse_def("alias a\nbase b\n").is_err());
        assert!(LayoutRules::parse_def("alias a\nalias b\n").is_err());
        assert!(LayoutRules::parse_def("alias a\nonly /x\n").is_err());
        assert!(LayoutRules::parse_def("alias").is_err());
        assert!(LayoutRules::parse_def("alias a b").is_err());
        assert!(LayoutRules::parse_def("alias ../x").is_err());
        assert!(LayoutRules::parse_def("only /x\n").is_err());
        assert!(LayoutRules::parse_def("").is_err());
        // base form still parses through the same entry point
        assert!(matches!(
            LayoutRules::parse_def("base alice\nonly /docs\n"),
            Ok(ViewDef::Derived(b, _)) if b == "alice"
        ));
    }

    #[test]
    fn parse_fork_definition() {
        // FORK: one line, no rules — an independent tree seeded from a base.
        let d = LayoutRules::parse_def("# private workspace\nfork team\n").unwrap();
        assert_eq!(d, ViewDef::Fork("team".to_string()));
        // A fork is not a lens: rules, a `base` line, or a second shape all clash.
        assert!(LayoutRules::parse_def("fork team\nonly /docs\n").is_err());
        assert!(LayoutRules::parse_def("fork team\nbase team\n").is_err());
        assert!(LayoutRules::parse_def("fork a\nfork b\n").is_err());
        assert!(LayoutRules::parse_def("alias a\nfork b\n").is_err());
        assert!(LayoutRules::parse_def("fork").is_err());
        assert!(LayoutRules::parse_def("fork a b").is_err());
        assert!(LayoutRules::parse_def("fork ../x").is_err());
        // The derived-view parser (used where only a lens makes sense) must not
        // silently accept the fork verb.
        assert!(LayoutRules::parse_view("fork team\n").is_err());
    }

    #[test]
    fn bound_map_drops_expired_before_stalest() {
        let base = Instant::now();
        let aged = |ms: u64| base - Duration::from_millis(ms);
        let expired = || base - CACHE_TTL - Duration::from_secs(1);

        // (1) Under cap is a pure no-op even with an expired entry present.
        let mut m: HashMap<String, (Instant, i64)> = HashMap::new();
        m.insert("keep".into(), (expired(), 1));
        bound_map(&mut m, 8);
        assert_eq!(m.len(), 1, "under cap must not evict");

        // (2) Over cap: expired entries are reclaimed first; the surviving
        //     fresh set is within TTL so the stalest-drop step never fires.
        let mut m: HashMap<String, (Instant, i64)> = HashMap::new();
        for k in 0..4 {
            m.insert(format!("e{k}"), (expired(), k));
        }
        m.insert("f1".into(), (aged(500), 10));
        m.insert("f2".into(), (aged(100), 11));
        bound_map(&mut m, 3);
        assert_eq!(m.len(), 2, "expired dropped, still under cap after");
        assert!(m.contains_key("f1") && m.contains_key("f2"));

        // (3) Over cap with everything fresh: keep only the cap freshest by
        //     resolve timestamp, dropping the stalest.
        let mut m: HashMap<String, (Instant, i64)> = HashMap::new();
        m.insert("oldest".into(), (aged(900), 1));
        m.insert("middle".into(), (aged(500), 2));
        m.insert("newer".into(), (aged(300), 3));
        m.insert("newest".into(), (base, 4));
        bound_map(&mut m, 2);
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("newest") && m.contains_key("newer"));
        assert!(!m.contains_key("oldest") && !m.contains_key("middle"));
    }
}
