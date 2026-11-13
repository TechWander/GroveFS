//! Fast server-side disk-usage aggregation (`grovefs du`).
//!
//! Avoids the FUSE per-file-stat round-trip pattern of system `du` by:
//! 1. Querying the metadata backend (PG or SQLite) for the full tree structure.
//! 2. Batch-stats content sizes via a single `OP_BATCH_LEN` RPC to the
//!    data-server (remote mode) or direct local `stat()` on blocks/ (local).
//! 3. Aggregating subtree totals per directory and printing du-compatible
//!    output.
//!
//! The data source is resolved explicitly ([`data_source`]): a size sweep
//! either has a place to read bytes from, or the command refuses. An unset
//! `--data-servers`/`--data-dir` never yields a report of all zeros.
//!
//! For 15k files this typically runs in <50 ms vs ~25 s via FUSE.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A flat node from the metadata tree (both backends normalize to this).
pub struct FlatNode {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub is_dir: bool,
    pub data_ref: Option<String>,
}

/// Options for the du operation.
pub struct DuOpts {
    pub view: String,
    pub meta: Option<String>,
    pub meta_token: Option<String>,
    pub meta_api_key: Option<String>,
    pub store: Option<String>,
    pub db_path: Option<PathBuf>,
    pub data: Option<String>,
    pub data_root: Option<PathBuf>,
    pub api_key: Option<String>,
    pub token: Option<String>,
    pub path: String,
    pub depth: usize,
    pub bytes: bool,
}
/// Per-directory result: (path, subtree_total_bytes).
pub type DuEntry = (String, u64);

/// Execute the du command.
pub fn run(opts: &DuOpts) -> io::Result<()> {
    // 1. Gather tree nodes from metadata backend.
    let nodes = gather_nodes(opts)?;

    // 2. Collect file data_refs for batch stat.
    let refs: Vec<String> = nodes
        .iter()
        .filter_map(|n| if !n.is_dir { n.data_ref.clone() } else { None })
        .collect();

    // 3. Batch-get sizes.
    let (sizes, lost, damaged, damaged_sample) = resolve_sizes(opts, &refs)?;
    if lost > 0 {
        // Numbers that quietly omit something are the failure mode this
        // command exists to avoid, so the gap is announced, not folded in.
        eprintln!(
            "warning: {lost} of {} object(s) are not in the data source and count as 0 bytes \
             (objects on other storage roots need their own --data-servers/--data-dir)",
            refs.len()
        );
    }
    if damaged > 0 {
        // A damaged striped object is bytes that ARE present but not in the
        // contiguous run the size depends on — a different problem from a plain
        // miss, and one a bare shorter total would hide. Announce it on its own
        // line so "content is corrupt" is never mistaken for "pass --data-dir".
        eprintln!(
            "warning: {damaged} of {} object(s) are damaged striped objects and are excluded \
             from the total (their true size is unknown); first: {}",
            refs.len(),
            damaged_sample.unwrap_or_else(|| "(reason unavailable)".to_string())
        );
    }

    // 4. Build id→size map and id→node map.
    let mut size_map: HashMap<i64, u64> = HashMap::with_capacity(refs.len());
    let mut id_node: HashMap<i64, &FlatNode> = HashMap::with_capacity(nodes.len());
    let mut si = 0usize;
    for n in &nodes {
        id_node.insert(n.id, n);
        if !n.is_dir && n.data_ref.is_some() && si < sizes.len() {
            size_map.insert(n.id, sizes[si]);
            si += 1;
        }
    }

    // 5. Build path for each node, compute subtree totals.
    let root_id = find_root(&nodes);
    let root_path = normalize_path(&opts.path);
    let start_id = if root_path == "/" {
        root_id
    } else {
        find_by_path(&nodes, root_path.trim_matches('/'), root_id).ok_or_else(|| {
            // Echo the normalized path actually searched (a leading `/` is added
            // by normalize_path, so `docs` is looked up as `/docs`) and name the
            // expected form, so a `path not found` reads as actionable rather
            // than a bare string (§6-6).
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "path not found: {} (resolved to '{}' from the view root; --path takes a view-relative path such as /<top-dir>/...)",
                    opts.path, root_path
                ),
            )
        })?
    };

    // Children map: parent_id → [child_ids]
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for n in &nodes {
        children.entry(n.parent_id).or_default().push(n.id);
    }

    // Compute subtree sum (bottom-up)
    let mut subtree_sum: HashMap<i64, u64> = HashMap::with_capacity(nodes.len());
    compute_subtree(start_id, &children, &size_map, &mut subtree_sum);

    // 6. Enumerate directories up to depth limit, print.
    let mut results: Vec<DuEntry> = Vec::new();
    collect_entries(
        start_id,
        &root_path,
        &children,
        &id_node,
        &subtree_sum,
        opts.depth,
        0,
        &mut results,
    );
    results.sort_by(|a, b| a.0.cmp(&b.0));

    // 7. Print.
    for (path, sz) in &results {
        if opts.bytes {
            println!("{}\t{}", sz, path);
        } else {
            println!("{}\t{}", human(*sz), path);
        }
    }
    Ok(())
}

// --- Metadata gathering ---

fn gather_nodes(opts: &DuOpts) -> io::Result<Vec<FlatNode>> {
    if let Some(addr) = &opts.meta {
        return gather_via_meta(
            addr,
            &opts.view,
            opts.meta_token.as_deref().unwrap_or(""),
            opts.meta_api_key.as_deref().unwrap_or(""),
        );
    }
    let store = resolve_store(&opts.store);
    match store.as_deref() {
        #[cfg(feature = "fdb")]
        Some(uri) if uri.starts_with("fdb://") => gather_fdb(uri, &opts.view),
        #[cfg(not(feature = "fdb"))]
        Some(uri) if uri.starts_with("fdb://") => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{uri} names the FoundationDB backend, but this binary was built without \
                 `--features fdb`; run `du --meta-server` against an FDB meta-server instead, \
                 or drop --meta-store for a local SQLite view"
            ),
        )),
        Some(uri) => gather_pg(uri, &opts.view),
        None => {
            let db_path = opts.db_path.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "need --meta-server, --meta-store or --meta-dir",
                )
            })?;
            gather_sqlite(db_path, &opts.view)
        }
    }
}

/// The FDB counterpart of `gather_pg`: one read-only subspace scan, no meta-server
/// round trip. Opening read-only matters — `du` must not seed a root row for a
/// view name that does not exist (the same "a probe must not create" rule
/// `crate::mcp` applies to its PG probe), and the same existence probe decides
/// whether this is a tree at all or a misspelt name.
#[cfg(feature = "fdb")]
fn gather_fdb(uri: &str, view: &str) -> io::Result<Vec<FlatNode>> {
    if !crate::fdb_store::FdbStore::subspace_exists(uri, view).map_err(io::Error::other)? {
        return Err(no_tree(view, uri));
    }
    let store = crate::fdb_store::FdbStore::open(uri, view, true)?;
    Ok(store
        .dump_tree()
        .into_iter()
        .map(|(id, parent_id, name, is_dir, data_ref)| FlatNode {
            id,
            parent_id,
            name,
            is_dir,
            data_ref: data_ref.filter(|d| !d.is_empty()),
        })
        .collect())
}

fn resolve_store(explicit: &Option<String>) -> Option<String> {
    explicit
        .clone()
        .or_else(|| std::env::var("GROVEFS_META_STORE").ok())
}

/// One sentence for "this name has no tree here", raised by all three gatherers.
///
/// Without it a misspelt `--view` was answered three different ways: SQLite said
/// `sqlite open ...: No such file or directory`, PG said `pg query: db error`
/// (SQLSTATE 42P01 in disguise) and FDB said nothing at all — 0 B, byte-identical
/// to a genuinely empty view. A measuring tool must not be able to report "empty"
/// for a name that does not exist.
fn no_tree(view: &str, store: &str) -> io::Error {
    io::Error::other(format!(
        "no tree for view '{view}' in {store} — `grovefs view create --view {view}` makes one, \
         and `grovefs view list` shows the names that exist"
    ))
}

fn gather_pg(uri: &str, view: &str) -> io::Result<Vec<FlatNode>> {
    let schema = grovefs_pg_schema(view);
    // Ask the catalog BEFORE selecting from it: the schema is the tree, so a
    // missing one is "no such view", not a query failure to be quoted raw.
    match crate::pg_store::PgEngine::tree_exists(uri, view) {
        Ok(true) => {}
        Ok(false) => return Err(no_tree(view, uri)),
        Err(e) => return Err(io::Error::other(e)),
    }
    let conn = postgres::Client::connect(uri, postgres::tls::NoTls)
        .map_err(|e| io::Error::other(format!("pg connect: {e}")))?;
    let sql =
        format!("SELECT id, parent_id, name, is_dir, data_ref FROM {schema}.node ORDER BY id");
    let mut conn = conn;
    let rows = conn
        .query(sql.as_str(), &[])
        .map_err(|e| io::Error::other(format!("pg query: {e}")))?;
    Ok(rows
        .iter()
        .map(|r| FlatNode {
            id: r.get(0),
            parent_id: r.get::<_, Option<i64>>(1).unwrap_or(0),
            name: r.get(2),
            is_dir: r.get::<_, i64>(3) != 0,
            data_ref: r.get(4),
        })
        .collect())
}

fn gather_sqlite(db_path: &std::path::Path, view: &str) -> io::Result<Vec<FlatNode>> {
    let db_file = db_path.join("meta").join(format!("{view}.db"));
    if !db_file.exists() {
        return Err(no_tree(view, &db_file.display().to_string()));
    }
    let conn = rusqlite::Connection::open(&db_file)
        .map_err(|e| io::Error::other(format!("sqlite open {}: {e}", db_file.display())))?;
    let mut stmt = conn
        .prepare("SELECT id, parent_id, name, is_dir, data_ref FROM node ORDER BY id")
        .map_err(|e| io::Error::other(format!("sqlite prep: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(FlatNode {
                id: r.get(0)?,
                // The root's `parent_id` is NULL; read it the way `gather_pg` does
                // so the local-SQLite gather and the PG/meta-server gathers hand
                // the tree walk the same set of rows.
                parent_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                name: r.get(2)?,
                is_dir: r.get::<_, i64>(3)? != 0,
                data_ref: r.get(4)?,
            })
        })
        .map_err(|e| io::Error::other(format!("sqlite query: {e}")))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Mirror pg_store's deterministic schema naming: `grovefs_v_` + hex(view).
fn grovefs_pg_schema(view: &str) -> String {
    let hex: String = view.bytes().map(|b| format!("{b:02x}")).collect();
    format!("grovefs_v_{hex}")
}

/// Gather tree via meta-server RPC (M_OPEN + M_TREE_DUMP). No direct PG/SQLite needed.
fn gather_via_meta(
    addr: &str,
    view: &str,
    token: &str,
    api_key: &str,
) -> io::Result<Vec<FlatNode>> {
    use crate::remote::proto;
    use std::io::BufReader;
    use std::net::TcpStream;

    // Name the target so a refused/unreachable meta-server reads as
    // "cannot reach meta-server 127.0.0.1:50053: Connection refused" rather
    // than a bare errno (§5-12); the ErrorKind is preserved for callers.
    let stream = TcpStream::connect(addr)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach meta-server {addr}: {e}")))?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // M_OPEN handshake: [key(view)][key(token)][u32 uid=0][key(api_key)]
    let mut body = Vec::new();
    proto::put_key(&mut body, view);
    proto::put_key(&mut body, token);
    proto::put_u32(&mut body, 0);
    proto::put_key(&mut body, api_key);
    proto::send_request(&mut writer, 16, &body)?; // M_OPEN=16
    let (status, _, _) = proto::recv_response(&mut reader)?;
    if status != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("meta-server rejected open: status {status}"),
        ));
    }

    // M_TREE_DUMP: empty body
    proto::send_request(&mut writer, 38, &[])?; // M_TREE_DUMP=38
    let (status, _, data) = proto::recv_response(&mut reader)?;
    if status != 0 {
        return Err(io::Error::other(format!(
            "M_TREE_DUMP failed: status {status}"
        )));
    }

    // Decode: [count: u32][id(8B) pid(8B) name(key) is_dir(1B) data_ref(key)]*
    let mut i = 0usize;
    let count = proto::read_u32(&data, &mut i)
        .map_err(|_| io::Error::other("tree_dump: short header"))? as usize;
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let id = i64::from_be_bytes(
            data[i..i + 8]
                .try_into()
                .map_err(|_| io::Error::other("tree_dump: id"))?,
        );
        i += 8;
        let parent_id = i64::from_be_bytes(
            data[i..i + 8]
                .try_into()
                .map_err(|_| io::Error::other("tree_dump: pid"))?,
        );
        i += 8;
        let name =
            proto::get_key(&data, &mut i).map_err(|_| io::Error::other("tree_dump: name"))?;
        let is_dir = *data
            .get(i)
            .ok_or_else(|| io::Error::other("tree_dump: dir flag"))?
            != 0;
        i += 1;
        let dr = proto::get_key(&data, &mut i).map_err(|_| io::Error::other("tree_dump: ref"))?;
        nodes.push(FlatNode {
            id,
            parent_id,
            name,
            is_dir,
            data_ref: if dr.is_empty() { None } else { Some(dr) },
        });
    }
    Ok(nodes)
}

// --- Size resolution ---

/// Where the content bytes behind `data_ref` live for one invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum DataSource {
    /// One or more data-server endpoints, in the order the writer used. A
    /// single address answers every size in one batch RPC; a list means the
    /// store is striped, so sizes come back per object from the shard map.
    Remote(Vec<String>),
    /// A storage root that already holds `blocks/`, stat'ed directly.
    Local(PathBuf),
}

/// Split a `--data-servers`/`GROVEFS_DATA_SERVERS` value into addresses (same trimming the
/// mount uses, so one string form means the same thing in both commands).
fn split_addrs(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Decide where to read object sizes from. Precedence is
/// `--data-servers` > `--data-dir` > the local store root the tree itself came from.
///
/// The last step is what makes `grovefs du --home R --view V` self-contained:
/// `grovefs init` puts `meta/<view>.db` and `blocks/` under the same root R.
/// A tree that came from a meta-server or a PostgreSQL store has no local
/// counterpart to infer from, so the source stays unresolvable and this
/// returns an error instead of measuring nothing.
pub fn data_source(opts: &DuOpts) -> io::Result<DataSource> {
    let data = opts.data.clone().or_else(|| {
        std::env::var("GROVEFS_DATA_SERVERS")
            .ok()
            .filter(|s| !s.is_empty())
    });
    data_source_at(
        data.as_deref(),
        opts.data_root.as_deref(),
        opts.meta.as_deref(),
        resolve_store(&opts.store).as_deref(),
        opts.db_path.as_deref(),
    )
}

/// The decision itself, with flags and environment already folded together.
fn data_source_at(
    data: Option<&str>,
    data_root: Option<&std::path::Path>,
    meta: Option<&str>,
    store: Option<&str>,
    db_path: Option<&std::path::Path>,
) -> io::Result<DataSource> {
    if let Some(list) = data {
        let addrs = split_addrs(list);
        if !addrs.is_empty() {
            return Ok(DataSource::Remote(addrs));
        }
    }
    if let Some(root) = data_root {
        return Ok(DataSource::Local(root.to_path_buf()));
    }
    let remote_tree = meta.is_some() || store.is_some();
    if !remote_tree {
        if let Some(root) = db_path {
            return Ok(DataSource::Local(root.to_path_buf()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "no data source to measure: pass --data-servers <host:port> for a data-server \
         (or set GROVEFS_DATA_SERVERS), or --data-dir <dir holding blocks/>",
    ))
}

/// Sizes of `refs`, plus how many refs could not be located (each of those
/// counted as 0 bytes, which the caller must say out loud), how many are
/// damaged striped objects (present-but-not-contiguous, size unknown), and a
/// sample damage reason for the warning line. Only a confirmed miss is counted
/// as lost: an answer nobody gave is an error, not a number.
fn resolve_sizes(
    opts: &DuOpts,
    refs: &[String],
) -> io::Result<(Vec<u64>, usize, usize, Option<String>)> {
    if refs.is_empty() {
        return Ok((Vec::new(), 0, 0, None));
    }
    match data_source(opts)? {
        DataSource::Remote(addrs) => resolve_remote(opts, &addrs, refs),
        DataSource::Local(root) => {
            // Local: open_local expects the root dir that CONTAINS blocks/.
            // Opening a backend creates its bucket tree, and a reporting
            // command has no business writing into the namespace it measures,
            // so a root without blocks/ is refused rather than trusted.
            if !root.join("blocks").is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no blocks/ under {} — objects are not here; point --data-servers at \
                         the data-server, or --data-dir at the storage root holding \
                         blocks/",
                        root.display()
                    ),
                ));
            }
            let backend = crate::backend::open_local(&root)?;
            let mut missing = 0usize;
            let mut unconfirmed: Option<String> = None;
            let mut sizes = Vec::with_capacity(refs.len());
            for k in refs {
                match backend.len(k) {
                    Ok(n) => sizes.push(n),
                    Err(e) if confirmed_missing(&e) => {
                        missing += 1;
                        sizes.push(0);
                    }
                    // A bucket we could not open (permissions, EIO under a
                    // failing disk) says nothing about the bytes, so it cannot
                    // be summed as if it did.
                    Err(e) => {
                        unconfirmed.get_or_insert_with(|| format!("{k}: {e}"));
                        sizes.push(0);
                    }
                }
            }
            if let Some(why) = unconfirmed {
                return Err(io::Error::other(format!(
                    "object(s) under {} could not be read to confirm their size (first \
                     failure: {why}); no total is reported rather than one that omits them",
                    root.display()
                )));
            }
            // The local (single-root) path never strips across servers, so an
            // interior chunk gap cannot arise here; damaged is always 0.
            Ok((sizes, missing, 0, None))
        }
    }
}

/// Whether an error is the data source *saying* the bytes are absent.
///
/// The distinction is not cosmetic: `not in the data source` is a statement
/// about the user's data, and a timeout, a refused connection, an `EIO` from a
/// chunk probe that kept failing, or a permission error has not earned it. The
/// striped path already keeps these apart internally (`Probe::Absent` versus
/// `Probe::Unknown`, which [`crate::backend::ShardedBackend`] deliberately
/// refuses to conflate because doing so once left permanent GC orphans); a
/// reporting command may not merge them back together.
fn confirmed_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Refuse an address list that cannot describe this tree, before asking any
/// server a single question.
///
/// `data_ref` carries the server count the writer used, so the mismatch is
/// known from the metadata alone. Which way it is wrong matters: chunks map to
/// `i % n`, so a shorter list means chunks pointed at servers that never held
/// them and a longer one means chunks spread over a different set — either way
/// the objects are NOT missing, they are simply not addressable under this
/// list. Saying "pass every data-server" to someone who passed one too many
/// sends them the wrong way, so the two cases get opposite advice here.
///
/// Keys with no `s<N>` prefix state no count and constrain nothing: a whole
/// object is looked up on member 0 whatever the list length, so a store whose
/// objects predate striping stays measurable under a striped list.
fn check_server_count(addrs: &[String], refs: &[String]) -> io::Result<()> {
    let counts: BTreeSet<usize> = refs
        .iter()
        .filter_map(|k| crate::backend::sharded::layout_server_count(k))
        .collect();
    let m = addrs.len();
    let err = |why: String| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{why}; no sizes are reported"),
        )
    };
    match counts.len() {
        // Nothing to check, or every object names the count of this list.
        0 => Ok(()),
        1 if counts.contains(&m) => Ok(()),
        1 => {
            let n = *counts.iter().next().unwrap();
            if n > m {
                Err(err(format!(
                    "this tree was written across {n} data-servers but --data-servers names {m}: the \
                     stripe map needs every address in the writer's order, so pass the full \
                     list as one comma-separated value (e.g. --data-servers host1:port,host2:port)"
                )))
            } else {
                Err(err(format!(
                    "this tree was written across {n} data-servers but --data-servers names {m}: extra \
                     addresses move logical chunks onto servers that never held them, so pass \
                     exactly the {n} addresses in the order the writer used"
                )))
            }
        }
        // More than one generation in one tree: no single list is right for it.
        _ => {
            let list = counts
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" and ");
            Err(err(format!(
                "this tree holds objects written across {list} different numbers of \
                 data-servers (a store that was expanded after some of it was written): one \
                 --data-servers list cannot measure both generations, so measure each with the list \
                 it was written with"
            )))
        }
    }
}

/// Sizes pulled from one or more data-servers.
///
/// One server answers the whole tree in a single `OP_BATCH_LEN`, but that batch
/// replies `0` for a key it does not have — indistinguishable from an empty
/// object. So every zero is confirmed by a single-key probe, where a real miss
/// answers ENOENT, and confirmed misses are counted as lost.
///
/// Several servers mean the store is striped: `data_ref` carries the
/// `s<N>:<stripe>:` layout, no member holds a whole object, and the only
/// honest measure is the same chunk probe a mounted read uses
/// ([`crate::backend::ShardedBackend::len`]), fanned out over a few threads so
/// a large tree still finishes in one pass. A list that cannot describe the
/// tree is refused up front from the layout in `data_ref` — see
/// [`check_server_count`] — because waiting for the probes would turn that
/// mistake into a row of failures this function would then have to explain.
fn resolve_remote(
    opts: &DuOpts,
    addrs: &[String],
    refs: &[String],
) -> io::Result<(Vec<u64>, usize, usize, Option<String>)> {
    check_server_count(addrs, refs)?;
    let token = opts.token.as_deref().unwrap_or("");
    let api_key = opts.api_key.as_deref().unwrap_or("");
    let concrete: Vec<Arc<crate::remote::client::RemoteBackend>> = addrs
        .iter()
        .map(|a| {
            crate::remote::client::RemoteBackend::new_with_creds(a, token, api_key).map(Arc::new)
        })
        .collect::<io::Result<_>>()?;
    let members: Vec<Arc<dyn crate::backend::StorageBackend>> = concrete
        .iter()
        .cloned()
        .map(|b| b as Arc<dyn crate::backend::StorageBackend>)
        .collect();
    let single = concrete.len() == 1;
    // The default stripe is fine here and needs no `--stripe-bytes` knob:
    // `du` only measures, so every size comes from `len_strict`, which decodes
    // each object's own stripe from its `s<N>:<stripe>:<uuid>` key. `self.stripe`
    // on this backend is used only by `new_key` (a WRITE path `du` never takes),
    // so the constructor value is inert regardless of how the tree was written.
    let sharded: Arc<crate::backend::ShardedBackend> =
        Arc::new(crate::backend::ShardedBackend::with_default_stripe(members));

    // One server can answer the whole sweep in a single RPC; a striped store
    // cannot, so those sizes come back from the per-object probe below.
    let mut sizes = if single {
        concrete[0].batch_len(refs)?
    } else {
        vec![0u64; refs.len()]
    };

    // Which objects still need a per-key answer: striped stores need one for
    // every object, a single server only for the zeros the batch guessed at.
    let todo: Vec<usize> = (0..refs.len())
        .filter(|i| !single || sizes[*i] == 0)
        .collect();
    if !todo.is_empty() {
        // One probe result per object: its index into `refs` and the answer.
        type Probes = Vec<(usize, io::Result<u64>)>;
        let out: Arc<Mutex<Probes>> = Arc::new(Mutex::new(Vec::new()));
        let width = todo.len().div_ceil(todo.len().min(8));
        std::thread::scope(|s| {
            for part in todo.chunks(width) {
                let sharded = sharded.clone();
                let out = out.clone();
                let part = part.to_vec();
                s.spawn(move || {
                    let done: Probes = part
                        .into_iter()
                        .map(|i| (i, sharded.len_strict(&refs[i])))
                        .collect();
                    out.lock().unwrap().extend(done);
                });
            }
        });
        let mut missing = 0usize;
        let mut damaged = 0usize;
        let mut unconfirmed = 0usize;
        let mut first_missing: Option<String> = None;
        let mut first_damaged: Option<String> = None;
        let mut first_unconfirmed: Option<String> = None;
        for (i, res) in out.lock().unwrap().drain(..) {
            match res {
                Ok(n) => sizes[i] = n,
                Err(e) if confirmed_missing(&e) => {
                    missing += 1;
                    first_missing.get_or_insert_with(|| e.to_string());
                }
                // A damaged striped object is NOT "absent from the data source":
                // chunks are present, just not the contiguous run the size
                // depends on, so its true size is unknown. Count it as 0 in the
                // total (bytes that cannot be confirmed are not summed) but
                // announce it apart from a plain miss, so "content is corrupt"
                // is never read as "move the root / pass --data-dir".
                Err(e) if crate::backend::ShardedBackend::is_damaged(&e) => {
                    damaged += 1;
                    first_damaged.get_or_insert_with(|| {
                        crate::backend::ShardedBackend::damage_reason(&e)
                    });
                }
                Err(e) => {
                    unconfirmed += 1;
                    first_unconfirmed.get_or_insert_with(|| e.to_string());
                }
            }
        }
        if unconfirmed > 0 {
            // A size nobody answered is not a size of zero: folding it in would
            // print a total that quietly omits objects, which is the failure
            // this command exists to avoid. The data source's own reason is
            // quoted because it is the only part that says what went wrong.
            return Err(io::Error::other(format!(
                "{unconfirmed} of {} object(s) could not be confirmed against the data \
                 source (first failure: {}); their sizes are unknown, so no total is \
                 reported",
                refs.len(),
                first_unconfirmed.unwrap_or_default()
            )));
        }
        if missing == refs.len() {
            // The list matched the layout, so these are bytes that are really
            // not where they were told to be: name the store, not the stripe.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no object of this tree is on the listed data source (first miss: {}); \
                     check that --data-servers names the store this view was written to, or pass \
                     --data-dir for a local blocks/",
                    first_missing.unwrap_or_default()
                ),
            ));
        }
        return Ok((sizes, missing, damaged, first_damaged));
    }
    Ok((sizes, 0, 0, None))
}

// --- Tree operations ---

fn find_root(nodes: &[FlatNode]) -> i64 {
    nodes
        .iter()
        .find(|n| n.name == "/" || n.parent_id == 0 && n.is_dir)
        .map(|n| n.id)
        .unwrap_or(1)
}

fn find_by_path(nodes: &[FlatNode], path: &str, root_id: i64) -> Option<i64> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current = root_id;
    let mut children: HashMap<i64, Vec<&FlatNode>> = HashMap::new();
    for n in nodes {
        children.entry(n.parent_id).or_default().push(n);
    }
    for part in parts {
        let next = children
            .get(&current)?
            .iter()
            .find(|n| n.name == part && n.is_dir)
            .map(|n| n.id)?;
        current = next;
    }
    Some(current)
}

fn normalize_path(p: &str) -> String {
    if p.is_empty() || p == "/" {
        "/".to_string()
    } else {
        format!("/{}", p.trim_matches('/'))
    }
}

/// Recursive bottom-up subtree sum (iterative to avoid stack overflow).
fn compute_subtree(
    root: i64,
    children: &HashMap<i64, Vec<i64>>,
    file_sizes: &HashMap<i64, u64>,
    out: &mut HashMap<i64, u64>,
) {
    // Post-order traversal via explicit stack
    let mut stack: Vec<(i64, bool)> = vec![(root, false)];
    while let Some((id, visited)) = stack.pop() {
        if visited {
            let own = if let Some(sz) = file_sizes.get(&id) {
                *sz
            } else {
                0
            };
            let child_sum: u64 = children
                .get(&id)
                .map(|ch| ch.iter().filter_map(|c| out.get(c).copied()).sum())
                .unwrap_or(0);
            out.insert(id, own + child_sum);
        } else {
            stack.push((id, true));
            if let Some(ch) = children.get(&id) {
                for &c in ch.iter().rev() {
                    stack.push((c, false));
                }
            }
        }
    }
}

/// Depth-limited walk that flattens the subtree into `(path, size)` rows. The
/// read-only context (children / node / sum maps plus the depth bound) is passed
/// straight through the recursion instead of being rebuilt per call.
#[allow(clippy::too_many_arguments)] // recursive walk: 7-value read-only context + cursor
fn collect_entries(
    id: i64,
    path: &str,
    children: &HashMap<i64, Vec<i64>>,
    node_map: &HashMap<i64, &FlatNode>,
    sums: &HashMap<i64, u64>,
    max_depth: usize,
    depth: usize,
    results: &mut Vec<DuEntry>,
) {
    let sz = sums.get(&id).copied().unwrap_or(0);
    results.push((path.to_string(), sz));

    if depth >= max_depth {
        return;
    }
    if let Some(ch) = children.get(&id) {
        for &cid in ch {
            if let Some(n) = node_map.get(&cid) {
                if n.is_dir {
                    let child_path = if path == "/" {
                        format!("/{}", n.name)
                    } else {
                        format!("{}/{}", path, n.name)
                    };
                    collect_entries(
                        cid,
                        &child_path,
                        children,
                        node_map,
                        sums,
                        max_depth,
                        depth + 1,
                        results,
                    );
                }
            }
        }
    }
}

// --- Formatting ---

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut sz = bytes as f64;
    let mut u = 0;
    while sz >= 1024.0 && u < 4 {
        sz /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{}B", bytes)
    } else {
        format!("{:.1}{}", sz, UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn an_explicit_data_server_wins() {
        assert_eq!(
            data_source_at(Some("h:1"), Some(&p("/r")), Some("m:2"), Some("pg"), Some(&p("/r")))
                .unwrap(),
            DataSource::Remote(vec!["h:1".into()])
        );
    }

    /// A comma-separated list is kept in order and intact: dropping members
    /// would silently remap which server each chunk belongs to.
    #[test]
    fn a_data_server_list_keeps_every_address_in_order() {
        assert_eq!(
            data_source_at(Some("a:1, b:2 ,c:3,"), None, None, None, None).unwrap(),
            DataSource::Remote(vec!["a:1".into(), "b:2".into(), "c:3".into()])
        );
    }

    /// An empty or comma-only list names no server, so it must not become a
    /// zero-address data source.
    #[test]
    fn a_blank_data_list_falls_through_to_the_refusal() {
        let err = data_source_at(Some(", ,"), None, Some("m:2"), None, None)
            .expect_err("a list without addresses is no source");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--data-servers"), "{err}");
    }

    #[test]
    fn an_explicit_data_root_beats_any_inference() {
        assert_eq!(
            data_source_at(None, Some(&p("/disks/2")), None, None, Some(&p("/store"))).unwrap(),
            DataSource::Local(p("/disks/2"))
        );
    }

    /// `--home`/`--meta-dir` names the root that holds both meta/ and blocks/,
    /// so measuring a local store needs no second flag.
    #[test]
    fn a_local_sqlite_tree_infers_its_own_root() {
        assert_eq!(
            data_source_at(None, None, None, None, Some(&p("/store"))).unwrap(),
            DataSource::Local(p("/store"))
        );
    }

    /// A tree pulled from a meta-server or from PG has no bytes next to the
    /// client's config file: refusing beats reporting a subtree of zeros.
    #[test]
    fn a_remote_tree_without_a_data_source_is_refused() {
        for (label, res) in [
            ("--meta-server", data_source_at(None, None, Some("m:2"), None, Some(&p("/store")))),
            ("--meta-store", data_source_at(None, None, None, Some("postgres://x"), Some(&p("/store")))),
        ] {
            let err = res.expect_err(&format!("{label} must not infer a local root"));
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{label}");
            assert!(err.to_string().contains("--data-servers"), "{label}: {err}");
        }
    }

    fn stripe(n: usize, keys: &[&str]) -> Vec<String> {
        keys.iter()
            .map(|k| format!("s{n}:16777216:{k}"))
            .collect()
    }

    fn addrs(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("h{i}:1")).collect()
    }

    /// A list shorter than the layout is a wrong list, and the fix is the other
    /// list: the check must say so from the metadata, without a probe.
    #[test]
    fn a_shorter_list_is_told_to_pass_the_whole_one() {
        let err = check_server_count(&addrs(1), &stripe(2, &["a", "b"])).unwrap_err();
        let msg = err.to_string();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(msg.contains("written across 2 data-servers but --data-servers names 1"), "{msg}");
        assert!(msg.contains("host1:port,host2:port"), "{msg}");
    }

    /// A list longer than the layout is the same mistake in the other
    /// direction, so telling the operator to pass MORE addresses here would
    /// send them away from the answer.
    #[test]
    fn a_longer_list_is_not_told_to_add_more() {
        let err = check_server_count(&addrs(3), &stripe(2, &["a", "b"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("extra addresses"), "{msg}");
        assert!(msg.contains("pass exactly the 2 addresses"), "{msg}");
        assert!(!msg.contains("host1:port,host2:port"), "wrong direction: {msg}");
    }

    /// Matching counts pass, including a legacy key with no layout prefix, which
    /// states no count and so stays measurable under any list.
    #[test]
    fn a_list_that_matches_the_layout_is_allowed() {
        assert!(check_server_count(&addrs(2), &stripe(2, &["a"])).is_ok());
        let legacy = "550e8400-e29b-41d4-a716-446655440000".to_string();
        for n in 1..=3 {
            assert!(check_server_count(&addrs(n), std::slice::from_ref(&legacy)).is_ok(), "{n} addrs");
        }
        // A striped tree plus legacy objects is measured by the striped list.
        let mut mixed = vec![legacy];
        mixed.extend(stripe(2, &["a"]));
        assert!(check_server_count(&addrs(2), &mixed).is_ok());
        assert!(check_server_count(&addrs(1), &[]).is_ok());
    }

    /// A store expanded after part of it was written holds two generations, and
    /// no list fits both. Averaging them into "missing objects" would hide the
    /// one fact the operator needs.
    #[test]
    fn two_generations_are_named_not_folded_into_missing() {
        let mut refs = stripe(1, &["old"]);
        refs.extend(stripe(2, &["new"]));
        let err = check_server_count(&addrs(2), &refs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1 and 2 different numbers"), "{msg}");
        assert!(msg.contains("expanded"), "{msg}");
        assert!(!msg.contains("not in the data source"), "{msg}");
    }

    /// Only a definitive ENOENT may be counted as absent bytes. Everything
    /// else leaves the size unknown, and an unknown is not a zero.
    #[test]
    fn an_unconfirmed_answer_is_not_a_missing_object() {
        use io::ErrorKind as K;
        let miss = io::Error::from_raw_os_error(2); // ENOENT
        assert!(confirmed_missing(&miss));
        for e in [
            io::Error::from_raw_os_error(111), // ECONNREFUSED
            io::Error::from_raw_os_error(5),   // EIO: a chunk probe that kept failing
            io::Error::from_raw_os_error(13),  // EACCES
            io::Error::new(K::TimedOut, "slow data-server"),
            io::Error::other("refusing to remap"),
        ] {
            assert!(!confirmed_missing(&e), "{:?} is not an absent object", e.kind());
        }
    }
}
