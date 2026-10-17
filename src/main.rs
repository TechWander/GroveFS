//! grovefs — a view-view FUSE filesystem.
//!
//! One physical store (`<base>/blocks`, shared), one *view* database per view
//! (`<base>/meta/<view>.db`). Mounting as a given view surfaces only that
//! view tree. Data IO is fully decoupled from NFS: it goes through the
//! pluggable `StorageBackend` trait (local disk today, gRPC to a cluster later).
//!
//! Subcommands:
//!   init   — create a view DB + seed some folders/files from a manifest
//!   mount  — mount a view at a mountpoint (foreground)
//!   umount — unmount

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use grovefs::acl::AclStore;
use grovefs::backend;
use grovefs::fs_impl::GroveFS;
use grovefs::meta;
use grovefs::meta::MetaEngine;
use grovefs::meta::MetaProvider;
use grovefs::remote;

#[derive(Parser)]
#[command(name = "grove", version, about = "User-view FUSE filesystem, no NFS")]
struct Cli {
    /// Shared base dir holding blocks/ and meta/. Defaults to $GROVEFS_HOME or ~/.grovefs
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a view (creates meta DB, optional seed manifest)
    Init {
        #[arg(long)]
        view: String,
        /// Optional manifest file: lines of "mkdir <path>" or "touch <path>"
        /// to pre-build a custom folder hierarchy.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Mount a view at a mountpoint (runs in foreground)
    Mount {
        #[arg(long)]
        view: String,
        #[arg(short, long)]
        mountpoint: PathBuf,
        /// Remote data service address (host:port). If set, object IO is shipped
        /// over the network to a `data-server` instead of the local disk (no NFS).
        /// A comma-separated list (addr1,addr2,...) enables deterministic
        /// cross-server striping; a single address behaves exactly as before.
        #[arg(long)]
        data: Option<String>,
        /// Remote metadata service address (host:port). If set, the view tree is
        /// served by a central `meta-server` instead of a local `<home>/meta` DB,
        /// so multiple clients mounting the same view share ONE authoritative view
        /// (consistent across machines) rather than diverging local copies.
        /// May be a comma-separated failover list (`primary,standby`): the mount
        /// stays on the first reachable node and falls over to a backup when the
        /// primary dies (pairs with HA `meta-server --role/--peer`).
        #[arg(long)]
        meta: Option<String>,
        /// Mount the view read-only: every mutating operation (create,
        /// write, mkdir, unlink, rename, symlink/link, chmod, truncate) returns
        /// EROFS. Enforced by the kernel `MS_RDONLY` flag plus a filesystem-side
        /// guard.
        #[arg(long)]
        ro: bool,
        /// Strict cache coherence for multi-client mounts: disable the kernel
        /// attribute/dentry cache and read file data straight through
        /// (FOPEN_DIRECT_IO), so one client's writes are visible to another
        /// immediately instead of after the cache TTL. Trade: no client-side
        /// caching, so more metadata/data round-trips. Pairs with `--meta`.
        #[arg(long)]
        strict: bool,
        /// Access token presented to a `meta-server` started with `--token`.
        /// Must match the server's token or the mount fails at connect time with
        /// a permission error. Ignored for local metadata (no `--meta`).
        #[arg(long)]
        token: Option<String>,
        /// Extra fuser options, e.g. allow_root, allow_other
        #[arg(long, num_args = 0..)]
        o: Vec<String>,
        /// FUSE worker threads dispatching requests concurrently (fuser 0.18
        /// session `n_threads`). 0 = single-threaded (legacy behavior, useful as
        /// an A/B knob for the bench). Each thread gets its own /dev/fuse fd via
        /// `FUSE_DEV_IOC_CLONE` (Linux 4.5+), so kernel replies are not serialized
        /// behind one channel lock.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// Report a specific uid on the `M_OPEN` handshake (remote meta only),
        /// instead of the process's real euid, so a shared `meta-server` with a
        /// view whitelist can admit this mount under a chosen slot. Also drives
        /// the local mount's read-gate check. Advisory: the client self-reports
        /// its uid, and the token is the real gate against a malicious peer.
        #[arg(long)]
        as_uid: Option<u32>,
    },
    /// Run the remote data service (central cluster node) backed by local disk
    DataServer {
        /// Listen address, e.g. 0.0.0.0:50052
        #[arg(short, long, default_value = "127.0.0.1:50052")]
        listen: String,
        /// Directory holding the physical objects on this node
        #[arg(short, long)]
        root: PathBuf,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server rejects any `OP_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer read or write arbitrary objects by
        /// key. Empty (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
    },
    /// Run the central metadata service: the authoritative per-view DB that
    /// many `mount --meta` clients share (no NFS; pairs with `data-server`).
    MetaServer {
        /// Listen address, e.g. 0.0.0.0:50053
        #[arg(short, long, default_value = "127.0.0.1:50053")]
        listen: String,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server refuses any `M_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer open arbitrary view trees. Empty
        /// (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// HA role: `primary` (default) serves reads+writes and ships snapshots to
        /// `--peer`; `standby` serves reads only from the replicated copy, rejects
        /// writes, and promotes itself if the primary's heartbeats stop.
        #[arg(long, default_value = "primary")]
        role: String,
        /// The other HA node's `host:port`. A primary pushes every ~500ms and
        /// heartbeats its epoch here; a standby promotes after missing heartbeats.
        /// Requires a *separate* `--home` from the peer (each node keeps its own
        /// copy of the view dbs; replication ships whole snapshots between them).
        #[arg(long)]
        peer: Option<String>,
        /// Opt-in synchronous replication for mutating meta ops (L2). Only
        /// meaningful with `--role primary` and a `--peer`. When set, every
        /// committed write op blocks until the standby has durably installed
        /// the pushed snapshot and ACKed on the control channel, dropping RPO
        /// from ~500ms to 0 for that op. Cost: ~one control-channel RTT +
        /// `VACUUM INTO` fsync per write, serialized across concurrent writers.
        /// On ACK timeout (2s) the op still succeeds but degrades to async with
        /// an `eprintln!` warn — so RPO is never silently misreported.
        #[arg(long)]
        sync_repl: bool,
        /// Fencing witness: one or more `data-server` `host:port` arbiters (a
        /// comma-separated list) both HA nodes can reach, used to fence the
        /// double-write window by MAJORITY. A primary holds a time-bounded LEASE
        /// on its epoch at each arbiter (renewed every tick); a grant is only
        /// authoritative once a strict majority (`N/2+1`) of them hold it, and an
        /// arbiter refuses overlapping leases, so a majority lease is itself proof
        /// of exclusive authority — a write is admitted from a cheap LOCAL lease
        /// check, not an arbiter round-trip on every op. Because two majorities
        /// always intersect, a same-value promotion race is won by exactly one
        /// node. Losing a MINORITY of arbiters neither freezes writes (a quorum is
        /// still reachable) nor lets a stale primary resurrect (a quorum still
        /// fences it); `N=1` behaves exactly as a single witness used to. The gate
        /// also covers the DATA plane: the client stamps its live epoch onto every
        /// write-intent content frame (create/write/truncate/remove/fsync), and
        /// the witness data-server refuses one whose epoch lags its token (EROFS),
        /// so stale bytes can no longer slip in by bypassing the meta layer. That
        /// content gate is only effective on a data-server the quorum raises the
        /// token of, i.e. one in the `--witness` list — deploy the witness
        /// co-located with the storage clients write to; a content store outside
        /// the set keeps a zero token and is guarded by the meta gate alone.
        /// Because the data-server holds the fence check and the backend write
        /// atomically against a promotion (a claim drains in-flight writes), a
        /// stale write can never land concurrently with the new primary's — content
        /// torn-writes are closed on the arbiter, not just bounded. Promotion is
        /// likewise non-overlapping: a would-be new primary may only seize a
        /// majority lease once the incumbent's has expired, so a stale primary
        /// provably stands down before another serves. A meta<->meta partition thus
        /// converges to one primary instead of an unbounded double-primary. Cost:
        /// one arbiter fan-out per lease renewal (amortized over all writes in the
        /// window), only when `--witness` is set. Used FAIL-SAFE: if a configured
        /// witness cannot form a lease quorum for 3s (one lease) a primary
        /// self-demotes and a standby refuses to promote (writes freeze rather than
        /// risk split-brain). Each data-server persists the token in
        /// `<root>/.fence_token`, so losing a minority of those stores cannot
        /// resurrect a deposed primary. Empty (default) keeps peer-only
        /// best-effort fencing, which a strict two-node split-brain can defeat —
        /// deploy a witness (an ODD number, 3 is the robust default) for safety.
        #[arg(long)]
        witness: Option<String>,
        /// Metadata backend URI. Default (unset or `"sqlite"`) is the embedded
        /// SQLite path: one db file per view under `<home>/meta/`, HA snapshot
        /// shipping available. Set to `postgres://user:pw@host:port/db`
        /// (`postgresql://…` also accepted) to place the authoritative view
        /// tree in PostgreSQL: one `grovefs_v_<hex(view)>` schema per view,
        /// transactions for rename/unlink/lock. Note that this mode is
        /// **incompatible** with `--role standby` / `--peer` / `--witness`:
        /// GroveFS-native HA snapshot shipping is `VACUUM INTO`-based and only
        /// makes sense on the embedded SQLite backend; under PG, run
        /// PostgreSQL's own streaming/logical replication for HA instead.
        /// The per-view uid whitelist (`acl.db`) stays local SQLite regardless
        /// of this choice.
        #[arg(long)]
        store: Option<String>,
    },
    /// Unmount a mountpoint
    Umount {
        #[arg(short, long)]
        mountpoint: PathBuf,
    },
    /// Manage the optional per-view uid whitelist (`<home>/meta/acl.db`).
    /// A view with no rows is unrestricted (any uid, once past the token gate,
    /// may mount); adding any row for a view switches it to whitelist mode.
    Acl {
        #[command(subcommand)]
        action: AclAction,
    },
}

#[derive(Subcommand)]
enum AclAction {
    /// Grant `--uid` access to `--view`. At least one of `--read`/`--write`
    /// is normally set; both-false reserves the row without granting.
    Allow {
        #[arg(long)]
        view: String,
        #[arg(long)]
        uid: u32,
        #[arg(long)]
        read: bool,
        #[arg(long)]
        write: bool,
    },
    /// Revoke `--uid`'s grant on `--view`. Removing the last row reopens the
    /// view to everyone.
    Revoke {
        #[arg(long)]
        view: String,
        #[arg(long)]
        uid: u32,
    },
    /// List grants on `--view` (empty output = unrestricted).
    List {
        #[arg(long)]
        view: String,
    },
}

fn base_dir(home: &Option<PathBuf>) -> PathBuf {
    if let Some(h) = home {
        return h.clone();
    }
    if let Ok(h) = std::env::var("GROVEFS_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".grovefs")
}

fn meta_path(base: &Path, view: &str) -> PathBuf {
    base.join("meta").join(format!("{view}.db"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let base = base_dir(&cli.home);
    std::fs::create_dir_all(base.join("meta"))?;

    match cli.command {
        Cmd::Init { view, manifest } => {
            let mp = meta_path(&base, &view);
            let meta = MetaEngine::open(&mp)?;
            println!("initialized view '{view}' at {}", mp.display());
            if let Some(mf) = manifest {
                let backend = backend::open_local(&base)?;
                seed_manifest(&meta, backend.as_ref(), &mf)?;
                println!("applied seed manifest {}", mf.display());
            }
        }
        Cmd::Mount {
            view,
            mountpoint,
            data,
            meta,
            ro,
            strict,
            token,
            o,
            threads,
            as_uid,
        } => {
            std::fs::create_dir_all(&mountpoint)?;
            // Effective uid reported to the server / used for local ACL check:
            // `--as-uid` overrides the process euid (advisory).
            let uid = as_uid.unwrap_or_else(|| unsafe { libc::geteuid() });
            // Metadata source: a central `meta-server` when --meta is given (a
            // single authoritative view shared across clients), else this machine's
            // local `<home>/meta/<view>.db`. Both satisfy `MetaProvider`, so the
            // FUSE layer below is identical either way.
            let meta_provider: Arc<dyn MetaProvider> = match &meta {
                Some(addr) => {
                    println!("using remote metadata service at {addr}");
                    Arc::new(remote::RemoteMeta::new_with_token_uid(
                        addr,
                        &view,
                        token.as_deref().unwrap_or(""),
                        uid,
                    )?)
                }
                None => {
                    // Local mount: enforce the view whitelist's read-gate before
                    // opening the sqlite file. Write-gating is a meta-server-only
                    // feature (writes here hit the file directly, behind OS fs
                    // perms); document this honestly.
                    let acl = AclStore::open(&base)?;
                    if !acl.check_read(&view, uid) {
                        return Err(format!(
                            "local view '{view}' is whitelisted; uid {uid} has no read grant (see `grove acl list --view {view}`)"
                        )
                        .into());
                    }
                    let mp = meta_path(&base, &view);
                    Arc::new(MetaEngine::open(&mp)?)
                }
            };
            // Fencing wiring: the meta client owns the live epoch handle (kept
            // current on every M_OPEN handshake/failover); the data client shares
            // the SAME handle to stamp writes, and uses the refresher to re-pull
            // the epoch from the authority when the data-server fences a stale one.
            // For a local meta this is a detached 0, so the data gate stays inert.
            let fence_epoch = meta_provider.fence_handle();
            let meta_for_fence = meta_provider.clone();
            let fence_refresh: Arc<dyn Fn() -> u64 + Send + Sync> =
                Arc::new(move || meta_for_fence.fetch_fence_epoch());
            // Pick the data path: remote over our TCP transport (no NFS) when
            // --data is given, otherwise the local disk backend. The FUSE and
            // metadata layers are identical either way thanks to the trait.
            let backend: Arc<dyn backend::StorageBackend> = match &data {
                Some(addr) => {
                    // A comma-separated list of data servers activates deterministic
                    // cross-server striping (P5); a single address keeps the plain
                    // remote path byte-for-byte unchanged.
                    let addrs: Vec<&str> =
                        addr.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                    if addrs.len() > 1 {
                        println!(
                            "using sharded remote data service across {} servers: {}",
                            addrs.len(),
                            addrs.join(",")
                        );
                        let mut members: Vec<Arc<dyn backend::StorageBackend>> =
                            Vec::with_capacity(addrs.len());
                        for a in &addrs {
                            members.push(Arc::new(
                                remote::RemoteBackend::new_with_token(
                                    a,
                                    token.as_deref().unwrap_or(""),
                                )?
                                .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                            ));
                        }
                        Arc::new(backend::ShardedBackend::with_default_stripe(members))
                    } else {
                        let one = addrs.first().copied().unwrap_or(addr.as_str());
                        println!("using remote data service at {one}");
                        Arc::new(
                            remote::RemoteBackend::new_with_token(
                                one,
                                token.as_deref().unwrap_or(""),
                            )?
                            .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                        )
                    }
                }
                None => Arc::from(backend::open_local(&base)?),
            };
            let fs = GroveFS::new(
                meta_provider,
                backend,
                ro || o.iter().any(|s| s == "ro"),
                strict || o.iter().any(|s| s == "strict"),
            );

            // Build the fuser 0.18 session Config. The old `--o` string surface is
            // preserved; only `allow_root`/`allow_other` changed shape — they are no
            // longer MountOption variants but the `acl` field (session-wide request
            // filtering), with All == allow_other and RootAndOwner == allow_root.
            let mut cfg = fuser::Config::default();
            cfg.mount_options.push(fuser::MountOption::FSName(format!("grovefs:{view}")));
            for raw in &o {
                match raw.as_str() {
                    "allow_root" => cfg.acl = fuser::SessionACL::RootAndOwner,
                    "allow_other" => cfg.acl = fuser::SessionACL::All,
                    "default_permissions" => cfg
                        .mount_options
                        .push(fuser::MountOption::DefaultPermissions),
                    "auto_unmount" => cfg.mount_options.push(fuser::MountOption::AutoUnmount),
                    "ro" => cfg.mount_options.push(fuser::MountOption::RO),
                    other => cfg.mount_options.push(fuser::MountOption::CUSTOM(other.to_string())),
                }
            }
            // `--ro` (or `-o ro`) mounts the view read-only at the kernel level.
            let want_ro = ro || o.iter().any(|s| s == "ro");
            if want_ro && !o.iter().any(|s| s == "ro") {
                cfg.mount_options.push(fuser::MountOption::RO);
            }
            // Concurrent dispatch: n_threads worker event loops, each with its own
            // cloned /dev/fuse fd. `threads = 0` maps to single-threaded legacy mode.
            cfg.n_threads = if threads > 1 { Some(threads) } else { None };
            cfg.clone_fd = threads > 1;
            println!(
                "mounting view '{view}' at {} (base {}{})",
                mountpoint.display(),
                base.display(),
                if want_ro { ", read-only" } else { "" }
            );
            // Graceful shutdown: the FUSE session returns whenever the mount goes
            // away; a helper thread watches for SIGINT/SIGTERM and asks
            // `fusermount -u` to unmount, ending the loop cleanly. An external
            // `fusermount -u` (no signal) likewise makes the session return.
            //
            // A remote-metadata mount additionally wants real-time cross-client
            // coherence without `--strict`: pushing kernel-cache invalidations
            // needs a `Notifier`, which we can grab from the session (`notifier()`
            // takes `&self` and returns an owned handle, so it does not keep the
            // session borrowed). We then run the session on the main thread in the
            // *same* blocking `run()` path as a local mount; a background listener
            // thread translates peer mutations into inval_entry/inval_inode.
            //
            // NOTE: do NOT use `spawn()` + `BackgroundSession::umount_and_join()`
            // here — `umount_and_join(self)` drops the mount handle first (an
            // active unmount) and then joins, so it tears the mount down
            // immediately instead of serving. `run()` is the blocking serve loop
            // (and in 0.18 it fans out to `cfg.n_threads` workers internally).
            let session = fuser::Session::new(fs, &mountpoint, &cfg)?;
            if let Some(addr) = &meta {
                let notifier = session.notifier();
                let tok = token.clone().unwrap_or_default();
                let _listener = remote::spawn_invalidation_listener(addr, &view, &tok, uid, notifier);
            }
            spawn_term_unmounter(mountpoint.clone());
            session.run()?;
            println!("unmounted view '{view}' at {}", mountpoint.display());
        }
        Cmd::DataServer { listen, root, token } => {
            std::fs::create_dir_all(&root)?;
            let backend = Arc::from(backend::open_local(&root)?);
            let srv = match token.as_deref() {
                Some(t) if !t.is_empty() => remote::DataServer::with_token(&listen, backend, t),
                _ => remote::DataServer::new(&listen, backend),
            };
            // Durable fencing token: a witness restart must not zero the epoch
            // and let a deposed primary sneak back in as a writer.
            let srv = srv.with_fence_store(root.join(".fence_token"));
            srv.serve()?;
        }
        Cmd::MetaServer { listen, token, role, peer, sync_repl, witness, store } => {
            let role = if role.eq_ignore_ascii_case("standby") {
                grovefs::remote::meta::Role::Standby
            } else {
                grovefs::remote::meta::Role::Primary
            };
            let mut srv = match token.as_deref() {
                Some(t) if !t.is_empty() =>
                    grovefs::remote::MetaServer::with_token(&listen, &base, t),
                _ => grovefs::remote::MetaServer::new(&listen, &base),
            };
            srv = srv.with_ha(role, peer);
            if sync_repl {
                srv = srv.with_sync_repl(true);
            }
            srv = srv.with_witness(witness);
            srv = srv.with_store(store);
            srv.serve()?;
        }
        Cmd::Umount { mountpoint } => {
            run_fusermount(&["-u", &mountpoint.to_string_lossy()])?;
            println!("unmounted {}", mountpoint.display());
        }
        Cmd::Acl { action } => {
            let acl = AclStore::open(&base)?;
            match action {
                AclAction::Allow { view, uid, read, write } => {
                    acl.allow(&view, uid, read, write)?;
                    println!("acl: granted view='{view}' uid={uid} read={read} write={write}");
                }
                AclAction::Revoke { view, uid } => {
                    acl.revoke(&view, uid)?;
                    println!("acl: revoked view='{view}' uid={uid}");
                }
                AclAction::List { view } => {
                    let rows = acl.list(&view);
                    if rows.is_empty() {
                        println!(
                            "view '{view}': UNRESTRICTED (no ACL rows — anyone who passes the token gate may mount)"
                        );
                    } else {
                        println!("view '{view}' is whitelisted:");
                        for r in rows {
                            println!(
                                "  uid {} read={} write={}",
                                r.uid, r.can_read, r.can_write
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Seed a custom folder hierarchy from a simple manifest. Demonstrates that the
/// view tree is entirely view-defined and decoupled from physical layout.
fn seed_manifest(
    meta: &MetaEngine,
    backend: &dyn backend::StorageBackend,
    manifest: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(manifest)?;
    let mut dirs: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    dirs.insert("/".to_string(), meta::ROOT_INO);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match it.next() {
            Some("mkdir") => {
                let path = it.next().ok_or("mkdir needs a path")?;
                ensure_dir(meta, path, &mut dirs)?;
            }
            Some("touch") => {
                let path = it.next().ok_or("touch needs a path")?;
                let (parent, name) = split_parent(path);
                let parent_ino = *dirs
                    .get(&parent)
                    .ok_or_else(|| format!("unknown parent {parent}"))?;
                let key = uuid::Uuid::new_v4().to_string();
                backend.create(&key)?;
                meta.create_node(parent_ino, &name, false, Some(&key), 0o0644,
                    unsafe { libc::getuid() }, unsafe { libc::getgid() })?;
            }
            _ => return Err(format!("bad manifest line: {line}").into()),
        }
    }
    Ok(())
}

fn ensure_dir(
    meta: &MetaEngine,
    path: &str,
    dirs: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cur_ino = meta::ROOT_INO;
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
        let ino = meta.create_node(cur_ino, comp, true, None, 0o0755,
            unsafe { libc::getuid() }, unsafe { libc::getgid() })?;
        dirs.insert(next_path.clone(), ino);
        cur_ino = ino;
        cur_path = next_path;
    }
    Ok(())
}

fn split_parent(path: &str) -> (String, String) {
    let p = path.trim_matches('/');
    match p.rfind('/') {
        Some(idx) => {
            let parent = format!("/{}", &p[..idx]);
            let name = p[idx + 1..].to_string();
            (parent, name)
        }
        None => ("/".to_string(), p.to_string()),
    }
}

fn run_fusermount(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let bin = if Path::new("/usr/bin/fusermount3").exists() {
        "fusermount3"
    } else {
        "fusermount"
    };
    let status = std::process::Command::new(bin).args(args).status()?;
    if !status.success() {
        return Err(format!("{bin} exited with {status}").into());
    }
    Ok(())
}

// ---- Graceful termination (signal handling) --------------------------------
//
// We can't safely call into fuser from a signal handler, so we use the classic
// self-pipe trick: the handler sets a flag and writes one byte to a pipe. The
// main thread blocks reading that pipe; the byte (or the EINTR the signal
// causes) wakes it, and it then drops the session to unmount cleanly. Covers
// Ctrl+C (SIGINT) and `kill` (SIGTERM). SIGKILL still relies on auto_unmount.
static TERM_RECEIVED: AtomicBool = AtomicBool::new(false);
static SELF_PIPE_R: AtomicI32 = AtomicI32::new(-1);
static SELF_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn term_handler(_sig: libc::c_int) {
    TERM_RECEIVED.store(true, Ordering::SeqCst);
    // write(2) is async-signal-safe; a full pipe just means we're already woken.
    let w = SELF_PIPE_W.load(Ordering::Relaxed);
    if w >= 0 {
        let b = 1u8;
        unsafe {
            libc::write(w, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

fn install_termination_handlers() {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return; // best-effort: without it we behave like before (kill leaves mount)
    }
    SELF_PIPE_R.store(fds[0], Ordering::Relaxed);
    SELF_PIPE_W.store(fds[1], Ordering::Relaxed);
    let h: extern "C" fn(libc::c_int) = term_handler;
    unsafe {
        libc::signal(libc::SIGINT, h as libc::sighandler_t);
        libc::signal(libc::SIGTERM, h as libc::sighandler_t);
    }
}

fn wait_for_termination() {
    let r = SELF_PIPE_R.load(Ordering::Relaxed);
    if r < 0 {
        // Signal setup failed; park forever so the default-kernel action on a
        // later SIGINT/SIGTERM (already unmasked) is what stops us.
        std::thread::sleep(std::time::Duration::from_secs(u64::MAX / 2));
        return;
    }
    let mut buf = [0u8; 16];
    loop {
        let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            break;
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) && TERM_RECEIVED.load(Ordering::SeqCst) {
                break;
            }
            if e.raw_os_error() != Some(libc::EINTR) {
                break; // pipe closed/errored: stop waiting
            }
        }
    }
    println!("received termination signal, unmounting cleanly...");
}

/// Spawn a watcher thread that unmounts `mountpoint` when the process gets
/// SIGINT/SIGTERM. Best-effort: if signal setup fails the thread simply idles.
fn spawn_term_unmounter(mountpoint: PathBuf) {
    std::thread::spawn(move || {
        install_termination_handlers();
        wait_for_termination();
        let _ = run_fusermount(&["-u", &mountpoint.to_string_lossy()]);
    });
}
