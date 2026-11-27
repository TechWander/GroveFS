//! grovefs — a view-view FUSE filesystem.
//!
//! One physical object store (`<root>/blocks`, shared), one *view* database
//! per view (`<meta-dir>/meta/<view>/<view>.db`). Mounting as a given view surfaces
//! only that view tree, and a mount always talks to services for both — a
//! `meta-server` for the tree, a `data-server` for the bytes. Data IO is fully
//! decoupled from NFS: it goes through the pluggable `StorageBackend` trait
//! (a `LocalBackend` on the storage node, `RemoteBackend`/`ShardedBackend` on
//! a client).
//!
//! Subcommands:
//!   mount  — mount a view at a mountpoint (detaches by default; `--layout`
//!            lens, or a server-side derived view name resolved by the server)
//!   umount — unmount
//!   view   — manage server-side views (`create` a base tree, optionally from a
//!            manifest, plus define/alias/fork/list/rm): derived lenses (shape
//!            governance, mounted read-only), forks (independent tree, content
//!            read-only) and pure aliases (name governance: the alias is the
//!            writable public face of a physical view, whose real name stops
//!            being mountable)
//!   mcp    — serve the view-governance tools over MCP (stdio) so an AI agent
//!            can customize data listings through the same validated API
//!   import — mirror an EXISTING local directory tree into a view, zero-copy
//!            (hard link into blocks/) where the filesystem allows it
//!   gc     — offline orphan-object audit / reclaim for a LOCAL store
//!   data-server / meta-server — centralized storage/metadata services

use clap::builder::styling::{AnsiColor, Styles};
use clap::{Parser, Subcommand};
use grovefs::backend;
use grovefs::cli_style as ui;
use grovefs::fs_impl::GroveFS;
use grovefs::meta;
use grovefs::meta::MetaEngine;
use grovefs::meta::MetaProvider;
use grovefs::remote;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

/* The help pages use ONE accent — the console's pine — so a reader can tell a
command, an option and a placeholder apart from the prose around them. Clap
drops this automatically when stdout is a pipe (`--color=auto`), which is why
the OUTPUT CONTRACT below can promise plain, one-row-per-line output there. */
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold().underline())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::BrightBlack.on_default())
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default())
    .error(AnsiColor::Red.on_default().bold());

#[derive(Parser)]
#[command(
    name = "grovefs",
    version,
    styles = HELP_STYLES,
    about = "User-view FUSE filesystem",
    long_about = r#"User-view FUSE filesystem.

One physical object store (`<root>/blocks`, content-addressed by UUID), one
independent view tree per user (`<meta-dir>/meta/<view>/<view>.db`). Mounting a view
surfaces only that view; data IO is fully decoupled from the directory tree.

A view is always served by TWO services — a `meta-server` for the tree and a
`data-server` for the bytes — and a mount asks a `manager` for one signed
roster that carries both coordinates (`--manager` + `--roster-key-file`). A
mount never names a service address by hand: the deployment's topology is
declared once at the manager and every client resolves the SAME verified
roster, so no two mounts of one view can dial different (or half a) cluster.
The services may run on the same machine as the client or on the other side of
the room; the mount command does not care. `grovefs mcp` adds the agent-facing
governance surface.

COMMANDS BY TASK

  Create & shape     view, import, layout
  Serve a cluster    manager, meta-server, data-server
  Mount & inspect    mount, umount, du
  Access & audit     apikey, web, mcp
  Maintain           gc, refs
"#,
    after_long_help = r#"TERMS (used consistently across commands)

  base view      physical tree; one directory or schema, writable
  derived view   read-only lens over a base's shape
  alias / fork   public writable name / independent metadata tree
  metadata root  the --meta-dir / --home store holding trees + ledger
  roster         manager-signed meta + data address list
  control plane  governance API of meta-server (--control-listen)

OUTPUT CONTRACT

  Human UI: a bold heading, a dim rule, aligned key/value blocks and
    tables; ✓ success, • information, warning: / error: on stderr.
  Pipes: ANSI is disabled, soft wrapping is off (one row per line), and
    ASCII glyphs replace box drawing.
  Machine: `--json` where offered (`apikey list`, `apikey calls`, `gc`,
    `manager status`); `du` keeps `SIZE<TAB>PATH` for sort/awk.
  Exit: 0 success · 1 operation refused · 2 usage error.

COMMON USAGE

  A cluster on one machine (a manager bootstraps the two services; loopback):
    grovefs --home ~/grove view create --view alice
    grovefs --home ~/grove manager define \
      --meta 127.0.0.1:50053 --data 127.0.0.1:50100
    grovefs --home ~/grove manager serve --listen 127.0.0.1:50055 &
    # serve mints join.key / roster.key on first start

    grovefs meta-server --meta-dir ~/grove \
      --listen 127.0.0.1:50053 --advertise-addr 127.0.0.1:50053 \
      --register-with 127.0.0.1:50055 \
      --join-key-file ~/grove/manager/join.key &

    grovefs data-server --data-dir ~/grove \
      --listen 127.0.0.1:50100 --advertise-addr 127.0.0.1:50100 \
      --member-id 0 --register-with 127.0.0.1:50055 \
      --join-key-file ~/grove/manager/join.key &

    grovefs mount --view alice -m ~/mnt \
      --manager 127.0.0.1:50055 \
      --roster-key-file ~/grove/manager/roster.key
    grovefs umount -m ~/mnt

  Central cluster (one shared view across machines; addresses come from the manager):
    grovefs --home /srv/gm manager define --meta host:50053 --data host:50100
    grovefs --home /srv/gm manager serve --listen 0.0.0.0:50055

    grovefs data-server --data-dir /data --listen 0.0.0.0:50100 \
      --advertise-addr host:50100 --member-id 0 \
      --register-with host:50055 --join-key-file /srv/gm/manager/join.key \
      --token SECRET

    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 \
      --advertise-addr host:50053 --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key --token SECRET

    grovefs mount --view alice -m /mnt --manager host:50055 \
      --roster-key-file /srv/gm/manager/roster.key --token SECRET

  Onboard an existing tree (zero-copy when it shares the filesystem):
    grovefs import --meta-dir /meta --view alice --source /srv/legacy

  Per-caller mount credentials instead of one static cluster token (revocable,
  audited; a revoke closes a LIVE mount within 1s and it unmounts itself):
    grovefs apikey --meta-dir /meta create --label analyst --for mount

    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 \
      --advertise-addr host:50053 --require-api-key \
      --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key

    grovefs data-server --data-dir /data --listen 0.0.0.0:50100 \
      --advertise-addr host:50100 --member-id 0 --require-api-key \
      --meta-dir /meta --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key

    grovefs mount --view alice -m /mnt --manager host:50055 \
      --roster-key-file /srv/gm/manager/roster.key --api-key ID:SECRET

  Server-side governance (no local rules file needed on clients):
    grovefs view --meta-dir /meta define --name lens_docs \
      --base alice --rules lens.rules
    grovefs view --meta-dir /meta alias --name bias_public --real alice

  Audit/reclaim orphan bytes in a LOCAL store (offline; scan is the default):
    grovefs gc --meta-dir ~/grovefs          # report
    grovefs gc --meta-dir ~/grovefs --apply  # delete >24h orphans (offline)

  AI-agent surface over MCP (stdio), guarded by an API key:
    grovefs apikey --meta-dir /meta create \
      --label claude-desktop --for mcp   # prints <id>:<secret> once
    grovefs mcp --meta-dir /meta --require-api-key
    grovefs mcp --help   # list the tools an agent can call

Run `grovefs <command> --help` for a command's full options.

  Long commands shrink: a KEY=VALUE file is auto-loaded
  ($GROVEFS_CONFIG, else $HOME/.config/grovefs/grovefs.conf), and the
  repeat-heavy knobs fall back to env: GROVEFS_MANAGER,
  GROVEFS_ROSTER_KEY_FILE, GROVEFS_DEPLOYMENT (the mount bootstrap),
  GROVEFS_TOKEN, GROVEFS_STRIPE_BYTES, GROVEFS_HOME, GROVEFS_META_DIR,
  GROVEFS_STATS, GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, GROVEFS_BW_READ,
  GROVEFS_BW_WRITE. (GROVEFS_META_SERVER / GROVEFS_DATA_SERVERS now feed
  only the node-side audit verbs — `du`, `gc --control-server` — never a
  mount.) Precedence is always flag > live env > config file; keep the file
  600 if it holds secrets.
"#
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Default store root for node-side verbs (default: $GROVEFS_HOME or ~/.grovefs)",
        long_help = "Default store root for the NODE-side verbs (`view create`, `gc`, `import`, \
                     `apikey`, `du`, and the servers themselves): it holds `blocks/` (data \
                     objects) and one directory per view (`meta/<view>/<view>.db`). Defaults to \
                     $GROVEFS_HOME or ~/.grovefs; a storage node names its own store path instead \
                     (`meta-server --meta-dir`, `data-server --data-dir`).\n\nA `mount` never reads \
                     or writes it: a mount resolves BOTH a metadata service and a data service \
                     from one signed roster via `--manager` and keeps no authoritative tree or \
                     bytes on the client's disk — not even a cache of them."
    )]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount a view at a mountpoint (detaches into the background by default).
    ///
    /// The mount process keeps serving in the background (own session, output to
    /// `--log-file`) and this command returns once the mountpoint is live.
    /// Pass `--foreground` to keep it attached to the terminal (systemd
    /// `Type=exec`, debugging, or scripts that redirect stdout themselves).
    /// Tear down with `grovefs umount -m <mountpoint>`.
    #[command(after_long_help = r#"USAGE EXAMPLES

  The only shape a mount takes — the manager resolves both coordinates from a signed roster:
    grovefs mount --view alice -m /mnt --manager HOST:50055 --roster-key-file ~/roster.key --token SECRET

  A mount NEVER names a service address by hand: there is no --meta-server/--data-servers.
  Failover across meta-server replicas and cross-server data striping are topology the manager
  DECLARES (`grovefs manager define --meta/--data`), so the same mount line above transparently
  dials a replicated or striped deployment:
    grovefs --home /srv/gm manager define --meta PRIMARY:50053,STANDBY:50053 --data HOST1:50100,HOST2:50100,HOST3:50100

  Select a non-default deployment the manager serves (--deployment, else GROVEFS_DEPLOYMENT):
    grovefs mount --view alice -m /mnt --manager HOST:50055 --roster-key-file ~/roster.key --deployment prod

  Strict coherence (no client cache; a peer's writes show immediately):
    grovefs mount --view alice -m /mnt --manager HOST:50055 --roster-key-file ~/roster.key --no-cache

  Read-only mount (every mutation returns EROFS):
    grovefs mount --view alice -m /mnt --manager HOST:50055 --roster-key-file ~/roster.key --ro

  Mount a server-side derived lens or public alias by its name:
    grovefs mount --view lens -m /mnt --manager HOST:50055 --roster-key-file ~/roster.key

  This mount only: a local presentation overlay from a rules file:
    grovefs mount --view alice -m ~/mnt --manager HOST:50055 --roster-key-file ~/roster.key --layout my.rules

  Detaches by DEFAULT: returns once the mountpoint is live and serves in the
  background (output to --log-file); tear down with `grovefs umount -m ~/mnt`.
"#)]
    Mount {
        /// View to mount — a physical name, or a server-side derived/alias name.
        #[arg(long)]
        view: String,
        /// Existing directory to mount the view onto.
        #[arg(short, long)]
        mountpoint: PathBuf,
        /// Stripe chunk size for cross-server striping: a byte count or a
        /// K/M/G suffix (1024-based, e.g. `16M`, `4194304`). Default 16 MiB.
        /// Only meaningful when the resolved roster's data list carries several
        /// data-servers. The
        /// value is baked into each NEW object's `data_ref`
        /// (`s<N>:<stripe>:<uuid>`); existing objects keep the stripe they
        /// were written with, so changing this never requires re-ingesting
        /// data. Falls back to env `GROVEFS_STRIPE_BYTES`.
        #[arg(
            long = "stripe-bytes",
            value_name = "SIZE",
            value_parser = grovefs::bwlimit::parse_size_bytes
        )]
        stripe_bytes: Option<u64>,
        /// Resolve BOTH cluster coordinates from a manager — the only way a
        /// mount is bootstrapped. Ask `HOST:PORT` for the deployment's roster
        /// (`grovefs manager serve`), verify its signature under
        /// `--roster-key-file`, then connect to the meta- and data-servers
        /// DIRECTLY. A bootstrap step, not a proxy: the manager is consulted
        /// once during the mount and never again, so a manager restart or
        /// outage cannot disturb a live mount (it can only refuse a new one).
        /// A mount never takes a hand-typed `meta-server`/`data-server` address
        /// list: the deployment's topology is declared once at the manager and
        /// every client resolves the SAME signed roster, so no two mounts of one
        /// view can dial different (or half a) cluster. `--roster-key-file` is
        /// required: an unverified roster is an instruction to connect wherever
        /// someone else wants, so an unverifiable one fails the mount. Falls
        /// back to env `GROVEFS_MANAGER`.
        #[arg(long = "manager", value_name = "HOST:PORT")]
        manager: Option<String>,
        /// Which deployment `--manager` should resolve (default `default`, the
        /// name `manager define` uses when given no `--name`). Falls back to
        /// env `GROVEFS_DEPLOYMENT`.
        #[arg(long = "deployment", value_name = "NAME")]
        deployment: Option<String>,
        /// The roster verification key for `--manager`: a file holding 64 hex
        /// characters (`openssl rand -hex 32`) — the SAME key the manager signs
        /// rosters with as its own `--roster-key-file`. Read-only here, and
        /// named as a FILE on purpose: an HMAC key is symmetric, so this is a
        /// secret, and a command-line one would land in `ps` and shell history.
        /// Falls back to env `GROVEFS_ROSTER_KEY_FILE`.
        #[arg(long = "roster-key-file", value_name = "PATH")]
        roster_key_file: Option<PathBuf>,
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
        /// caching, so more metadata/data round-trips. Pairs with a
        /// manager-resolved `meta-server`.
        #[arg(long = "no-cache")]
        strict: bool,
        /// Access token presented to a `meta-server` started with `--token`.
        /// Must match the server's token or the mount fails at connect time with
        /// a permission error.
        #[arg(long)]
        token: Option<String>,
        /// API key (`<id>:<secret>`, minted by `grovefs apikey create --for
        /// mount`) presented to a `meta-server`/`data-server` started with
        /// `--require-api-key` instead of (or alongside) a static `--token`.
        /// Unlike `--token`, this is a per-caller credential: the operator can
        /// list it, audit the connects behind it (`grovefs apikey calls`) and
        /// revoke it — a revoke lands on THIS live mount at its first server
        /// round-trip after the 1s re-check window (a read answered purely from
        /// the kernel cache needs no round-trip, so it is not interrupted), after
        /// which the mount prints the server's reason and unmounts itself rather
        /// than serving a broken view. Falls back to env
        /// `GROVEFS_API_KEY` so a secret need not sit in a command line. Refused
        /// at connect time if the key is unknown/revoked, or scoped to `mcp`.
        #[arg(long)]
        api_key: Option<String>,
        /// Extra fuser options, e.g. allow_root, allow_other
        #[arg(long, num_args = 0..)]
        o: Vec<String>,
        /// FUSE worker threads dispatching requests concurrently (fuser 0.18
        /// session `n_threads`). 0 = single-threaded mode, useful as an A/B
        /// knob for the bench. Each thread gets its own /dev/fuse fd via
        /// `FUSE_DEV_IOC_CLONE` (Linux 4.5+), so kernel replies are not serialized
        /// behind one channel lock.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// Report a specific uid on the `M_OPEN` handshake (and on the
        /// invalidation subscription) instead of the process's real euid, so a
        /// shared `meta-server` records lock arbitration and audit rows under a
        /// chosen owner. Advisory: the client self-reports its uid, and the
        /// token / API key is the real gate against a malicious peer.
        #[arg(long = "run-as-uid", value_name = "UID")]
        as_uid: Option<u32>,
        /// Presentation overlay: a rules file that reshapes how THIS mount
        /// displays the view without touching the stored tree. Verbs: `hide
        /// <src>`, `only <src>`, `map <src> <dst>`, `link <src> <dst>` (see
        /// `grovefs::layout`). Client-side and per-mount: two clients mounting
        /// the same view with different files see different cuts of the same
        /// authoritative data. Writes through the lens land on the real
        /// entries; creating directly inside a synthesized (lens-only) dir is
        /// refused with EPERM.
        #[arg(long)]
        layout: Option<PathBuf>,
        /// Disable FUSE READDIRPLUS negotiation (kernel >= 4.6). With it off,
        /// directory listings fall back to plain READDIR + per-entry LOOKUP —
        /// useful as an A/B benchmark knob and for compatibility debugging.
        /// READDIRPLUS is only ever offered on mounts whose metadata provider
        /// pages inside its own store: a `meta-server` backed by SQLite or PG
        /// (which serve the paged M_READDIRP RPC). Derived/lens mounts answer a
        /// whole listing per buffer, so this flag changes nothing for them.
        #[arg(long)]
        no_readdirplus: bool,
        /// Raise the kernel attribute/dentry cache TTL to this many seconds
        /// (default 2). For read-heavy or immutable trees (e.g. streaming the
        /// same large files, or an astronomy "data never lands" read-once
        /// workload) a larger TTL cuts repeated LOOKUP/GETATTR round-trips.
        /// Cross-client freshness is still kept by the default notify-invalidation
        /// push, and on kernels >= 3.15 AUTO_INVAL_DATA drops stale page data when
        /// size/mtime change; `--no-cache` overrides this to 0 (full bypass).
        #[arg(long = "cache-ttl", value_name = "SECS", default_value_t = 2)]
        tune_cache: u64,
        /// Cap this mount's CONTENT read throughput, e.g. `50M`, `1G` or a
        /// plain byte count (1024-based). 0/unset = unlimited (the default).
        /// A shared GCRA slot schedule in the FUSE read path hands bytes back
        /// at exactly this rate (bytes are paced from the first op; a single
        /// op larger than one second of budget may start up to 1 s early).
        /// Content writes have their own knob (`--bw-write`); metadata ops
        /// are never throttled. Falls back to env `GROVEFS_BW_READ`. Honest
        /// boundary: a per-mount, CLIENT-side shaper — a raw data-plane
        /// client that bypasses FUSE, or N mounts of the same view, each get
        /// their own budget.
        #[arg(long, value_parser = grovefs::bwlimit::parse_rate)]
        bw_read: Option<u64>,
        /// Cap this mount's CONTENT write throughput (same syntax; env
        /// fallback `GROVEFS_BW_WRITE`). Inert on read-only mounts: their
        /// writes are refused with EROFS before any budget is spent.
        #[arg(long, value_parser = grovefs::bwlimit::parse_rate)]
        bw_write: Option<u64>,
        /// Opt-in per-stage latency telemetry. When set, the mount records how
        /// long each phase of a read/write/metadata op takes (key resolve, data
        /// backend, remote RTT, striping fan-out, SQLite/PG query, SSI retry …)
        /// into HDR histograms and prints an aggregate p50/p90/p99/max report on
        /// unmount. Off by default and cheap when off (one relaxed load per site,
        /// no clock/lock/allocation on the hot path). Diagnostic only — not a
        /// throughput claim. Also enabled by env `GROVEFS_STATS=1`.
        #[arg(long)]
        stats: bool,
        /// With `--stats`: also append the report to this file periodically and
        /// on exit (a stable, diff-friendly sink for A/B runs and CI capture).
        #[arg(long)]
        stats_file: Option<PathBuf>,
        /// With `--stats`: rewrite `--stats-file` (or stderr if no file) every
        /// N seconds while mounted, so a long-running mount can be sampled
        /// offline without unmounting. 0/unset disables periodic dumping.
        #[arg(long, default_value_t = 0)]
        stats_interval: u64,
        /// Stay attached to this terminal instead of detaching to the
        /// background (the traditional behavior; required for systemd
        /// `Type=exec` units). Informational output goes to stdout/stderr.
        #[arg(long)]
        foreground: bool,
        /// When detached (the default): file the background mount's
        /// stdout/stderr append into. Defaults to
        /// `$TMPDIR/grovefs-mount-<mountpoint>.log`; pass `/dev/null` for a
        /// fully silent mount. Ignored with `--foreground` unless the
        /// caller's stdio is a terminal.
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Run the remote data service (central cluster node) backed by local disk
    #[command(after_help = r#"EXAMPLES
  Share this node's disk with the deployment:
    grovefs data-server --data-dir /data --listen 0.0.0.0:50100 \
      --advertise-addr host:50100 --member-id 0 \
      --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key

  Require per-caller keys instead of a static --token:
    grovefs data-server --data-dir /data --listen 0.0.0.0:50100 \
      --advertise-addr host:50100 --member-id 0 --require-api-key \
      --meta-dir /meta --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key"#)]
    DataServer {
        /// Listen address, e.g. 0.0.0.0:50052
        #[arg(short, long, default_value = "127.0.0.1:50052")]
        listen: String,
        /// Directory holding the physical objects on this node. A
        /// comma-separated list (dir1,dir2,...) spreads objects across several
        /// storage roots (disks) automatically: each whole object is placed on
        /// one root by a hash of its key (chunk families pin to their object's
        /// root), so no client or metadata change is needed and `df` aggregates
        /// the disks. The root list must stay FIXED for a given store — adding
        /// or removing a disk re-maps every key. A single path behaves as before.
        #[arg(short, long = "data-dir", value_name = "DIR[,DIR]")]
        root: PathBuf,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server rejects any `OP_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer read or write arbitrary objects by
        /// key. Empty (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// Gate `OP_OPEN` on a revocable per-caller credential instead of (or in
        /// addition to) a static `--token`: every client must present a live key
        /// minted by `grovefs apikey create --for mount` (`mount --api-key`).
        /// Connects and revokes are logged, and the key is re-verified on live
        /// connections (once per second), so a revoke closes an established
        /// mount rather than only refusing the next one. The store is
        /// `<meta-dir>/meta/_control/api_keys.db`; `--meta-dir` defaults to this node's
        /// first `--data-dir`, so a colocated deployment needs no extra flag and a
        /// split one points at a synced/colocated copy of that file.
        #[arg(long)]
        require_api_key: bool,
        /// Metadata root holding `meta/_control/api_keys.db` for `--require-api-key` (the
        /// same layout `grovefs apikey --meta-dir` manages). Defaults to the
        /// first `--data-dir`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        keys_from: Option<PathBuf>,
        /// Key/audit backend for `--require-api-key`. Unset / `"sqlite"` reads
        /// the local `<meta-dir>/meta/_control/api_keys.db`; a `postgres://…`
        /// (`postgresql://…`) URI — or the `GROVEFS_META_STORE` env — reads the shared
        /// `grovefs_control.api_keys` table, so a data-server on a PG deployment
        /// gates mounts against the SAME registry its meta-server does with no
        /// file to colocate (and `--meta-dir` is then unused).
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// This node's stable identity within a striped cluster. When a
        /// deployment's roster carries several data-servers, chunk
        /// placement is normally decided by list position, so reordering the
        /// addresses would silently misroute reads/writes of existing objects.
        /// Each data-server started with a DISTINCT `--member-id` (0, 1, 2, …)
        /// lets those mounts place chunks by member IDENTITY instead: the
        /// declared data-server order then no longer matters, and a disk replaced
        /// behind the same id keeps its bytes addressable. Members of one
        /// stripe must use unique ids; the id must stay FIXED for a given
        /// store — changing it re-maps every chunk family. Falls back to
        /// `GROVEFS_MEMBER_ID`. Unset (or single-address use) keeps the
        /// historical positional placement.
        #[arg(long = "member-id", value_name = "N")]
        member_id: Option<u64>,
        /// Require a Data-Access Capability on every `OP_OPEN`, verified under
        /// this shared HMAC key (hex, e.g. `openssl rand -hex 32`; falls back to
        /// `GROVEFS_DATA_ACCESS_KEY`). It MUST match the SAME key the cluster's
        /// `meta-server --data-access-key` signs with: the meta-server mints a
        /// capability on each accepted `M_OPEN`, the client replays it here, and
        /// an `OP_OPEN` without a valid one is refused. This closes the
        /// "malicious meta-server arming a client against this honest
        /// data-server" hole — a rogue meta cannot forge the capability, so its
        /// mounts fail to open data even though the client holds the shared
        /// `--token`. Unset keeps the original token/api-key-only gate
        /// byte-for-byte. This is proof-of-admission by a genuine meta, NOT a
        /// per-view data ACL (the data keyspace stays a flat KV; see
        /// `grovefs::remote::dac`).
        #[arg(long = "data-access-key", value_name = "HEX")]
        data_access_key: Option<String>,
        /// Announce this node to a MANAGER (`grovefs manager serve`) so clients
        /// can `mount --manager HOST:PORT` by deployment name instead of typing
        /// this address. The node signs an announcement of its advertised
        /// address with `--join-key-file` and re-sends it as a heartbeat.
        /// Registration is NOT a prerequisite for serving: an unreachable
        /// manager is retried with backoff (one log line per state change,
        /// not per attempt) while this server keeps answering clients, because
        /// failing a storage node over a directory service would turn a
        /// cosmetic outage into a data outage. Requires `--join-key-file` (env
        /// `GROVEFS_REGISTER_WITH` fills this slot).
        #[arg(long = "register-with", value_name = "HOST:PORT")]
        register_with: Option<String>,
        /// File holding the JOIN key (64 hex characters, `0600`) this node signs
        /// its registration with — the same key the manager was started with.
        /// A path and not the key itself: argv is readable by every local
        /// process. Only used with `--register-with` (env
        /// `GROVEFS_JOIN_KEY_FILE` fills this slot).
        #[arg(long = "join-key-file", value_name = "PATH")]
        join_key_file: Option<PathBuf>,
        /// The address to advertise to the manager, which is not necessarily
        /// what this node binds — a server listening on `0.0.0.0:50100` is
        /// undialable as written, so it advertises `store1.example:50100`.
        /// Defaults to `--listen`, and a wildcard `--listen` therefore REQUIRES
        /// it. Must equal the address `manager define` declared for this
        /// deployment, or the registration is refused (a node cannot add itself
        /// to a topology; only the operator's declaration decides that).
        #[arg(long = "advertise-addr", value_name = "HOST:PORT")]
        advertise_addr: Option<String>,
        /// Which deployment this node belongs to, as named by
        /// `grovefs manager define --name` (default `default`, or env
        /// `GROVEFS_DEPLOYMENT`). Only meaningful with `--register-with`.
        #[arg(long = "deployment", value_name = "NAME")]
        deployment: Option<String>,
        /// How long this node's registration should stay live without a refresh
        /// (default 90s, clamped to 5..3600 by the manager; the node re-sends at
        /// a third of it, so one lost heartbeat is not an outage report). Only
        /// meaningful with `--register-with`.
        #[arg(long = "register-ttl", value_name = "SECS")]
        register_ttl: Option<u64>,
    },
    /// Run the central metadata service that many a client's mount shares.
    ///
    /// Hosts the authoritative per-view DB that those clients share (no NFS);
    /// pairs with `data-server`.
    #[command(after_help = r#"EXAMPLES
  A single node serving one metadata root:
    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 \
      --advertise-addr host:50053

  Join a manager-declared deployment:
    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 \
      --advertise-addr host:50053 --register-with host:50055 \
      --join-key-file /srv/gm/manager/join.key --token SECRET

  Also serve the governance / console API:
    grovefs meta-server --meta-dir /meta --listen 0.0.0.0:50053 \
      --advertise-addr host:50053 --control-listen 0.0.0.0:50054"#)]
    MetaServer {
        /// Listen address, e.g. 0.0.0.0:50053
        #[arg(short, long, default_value = "127.0.0.1:50053")]
        listen: String,
        /// Authoritative metadata root for this server: under SQLite every
        /// view DB lives in `<meta-dir>/meta/<view>/<view>.db` (created explicitly
        /// by `view create` / `view import`, never by a mount) and
        /// the API-key/audit store lives at `<meta-dir>/meta/_control/api_keys.db`;
        /// under PostgreSQL (`--meta-store postgres://…`) the view trees AND the
        /// control plane
        /// (definitions, keys, audit) live in that engine, making `<meta-dir>` a
        /// config-only directory (logs, scratch — no authoritative record). Takes
        /// precedence over the global `--home` when both are given.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Shared secret clients must present (`mount --token`). When set, the
        /// server refuses any `M_OPEN` whose token differs, so an unauthorized
        /// host on the LAN can no longer open arbitrary view trees. Empty
        /// (default) keeps the original LAN-trusted, no-auth behavior.
        #[arg(long)]
        token: Option<String>,
        /// Gate `M_OPEN` on a revocable per-caller credential instead of (or in
        /// addition to) a static `--token`: every client must present a live key
        /// minted by `grovefs apikey create --for mount` (`mount --api-key`). The
        /// store is `<meta-dir>/meta/_control/api_keys.db` — the same file `grovefs mcp
        /// --meta-dir` authenticates against and `grovefs apikey --meta-dir`
        /// manages, so one credential registry serves both entrances while key
        /// SCOPES keep them apart. Connects and revokes are audited, and live
        /// connections are re-verified once per second, so a revoke closes an
        /// established mount (which then unmounts itself with the server's
        /// reason printed) rather than only refusing the next mount.
        #[arg(long)]
        require_api_key: bool,
        /// Mint a Data-Access Capability for every accepted `M_OPEN` and ship it
        /// to the client in the ack, under this shared HMAC key (hex, e.g.
        /// `openssl rand -hex 32`; falls back to `GROVEFS_DATA_ACCESS_KEY`). It
        /// MUST match the SAME key every `data-server --data-access-key` verifies
        /// with. A client that mounted through THIS meta replays the capability
        /// to the data-server and is admitted; a client driven by a meta that
        /// lacks the key (a rogue/foreign one) cannot produce it, so the honest
        /// data-server refuses it. Unset issues no capability and leaves data
        /// admission gated by token/api-key only (unchanged behavior). See
        /// `grovefs::remote::dac`.
        #[arg(long = "data-access-key", value_name = "HEX")]
        data_access_key: Option<String>,
        /// Lifetime (seconds) of the minted Data-Access Capability; only
        /// meaningful with `--data-access-key`. `0` (default) mints a
        /// NON-EXPIRING capability so a long-lived mount survives a data-server
        /// restart/reconnect without being cut off mid-session. A nonzero TTL
        /// bounds the replay window (each `M_OPEN` stamps `now + TTL`) at the
        /// cost of a remount once it lapses — for stricter deployments.
        #[arg(long = "data-access-ttl", value_name = "SECS", default_value_t = 0)]
        data_access_ttl: u64,
        /// HA role: `primary` (default) serves reads+writes and ships snapshots to
        /// `--peer`; `standby` serves reads only from the replicated copy, rejects
        /// writes, and promotes itself if the primary's heartbeats stop.
        #[arg(long, default_value = "primary", value_parser = parse_ha_role)]
        role: String,
        /// The other HA node's `host:port`. A primary pushes every ~500ms and
        /// heartbeats its epoch here; a standby promotes after missing heartbeats.
        /// Requires a *separate* metadata root (`--meta-dir`, or the global
        /// `--home`) from the peer (each node keeps its own
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
        /// SQLite path: one db file per view under `<meta-dir>/meta/`, HA snapshot
        /// shipping available. Set to `postgres://user:pw@host:port/db`
        /// (`postgresql://…` also accepted) to place the authoritative view
        /// tree in PostgreSQL: one `grovefs_v_<hex(view)>` schema per view,
        /// transactions for rename/unlink/lock. Note that the PostgreSQL mode
        /// is **incompatible** with `--role standby` /
        /// `--peer` / `--witness`: GroveFS-native HA snapshot shipping is
        /// `VACUUM INTO`-based and only makes sense on the embedded SQLite
        /// backend; under PG, run PostgreSQL's own streaming/logical replication
        /// for HA instead.
        /// EXPERIMENTAL (`--features treedb` builds only): `treedb://host:port`
        /// places the view TREES in a treedb server while the control plane
        /// (definitions, keys, accounts) stays in local SQLite under
        /// `<meta-dir>`; it rejects `--role standby`/`--peer`/`--witness`,
        /// `view export/import` and `gc` audits, and no other subcommand drives
        /// it (govern its views through this server with `--meta-server`).
        /// Any other `scheme://` URI is refused at startup — it is never
        /// silently served by SQLite.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Retire a view's cached metadata engine — its PG connection pool
        /// (8 writers + 4 readers) or SQLite handle — after this many seconds
        /// with no mounted session and no traffic; `0` never evicts (engines
        /// stay resident for the process lifetime). Idle views re-open
        /// transparently on the next op, so resident PG connections scale
        /// with RECENTLY USED views instead of every view opened since start:
        /// the view-count ceiling stops being `max_connections / 12`.
        #[arg(long = "engine-idle-secs", value_name = "SECS", default_value_t = 300)]
        engine_idle_secs: u64,
        /// Delete call-audit rows (`mcp_calls`) older than this window (`90d`,
        /// `24h`, `365d`), swept in the background. Default `90d` when
        /// `--control-listen` is on: the console's audit page is meant to show
        /// a meaningful trail, and an append-only table that grows forever is
        /// not a usable one. The window is ALSO a runtime setting — the console
        /// (the Audit page) or the `audit.retention.set` control op change it
        /// without a restart, and the reaper picks the new window up on its
        /// next sweep. Pass `never` (or `0`) to keep every row forever. Needs
        /// `--control-listen` (that port is what opens the store);
        /// `grovefs apikey purge --before 90d` runs the same cut once, on demand.
        #[arg(long = "audit-retention", value_name = "DUR")]
        audit_retention: Option<String>,
        /// Where NEW family members of a base view land in the SQLite layout:
        /// `nested` (the default) puts a fork's tree directory and a
        /// derived/alias definition file INSIDE `meta/<base>/`, `grouped` puts
        /// them under `meta/<base>/forks/` and `meta/<base>/views/`, and `flat`
        /// keeps the pre-v3 top-level placement. The choice is recorded in
        /// `meta/.family`, decides NOTHING about existing files (every read
        /// resolves by scanning), and `grovefs layout set-family` changes it
        /// for a live store without this flag.
        #[arg(long = "family-layout", value_name = "MODE")]
        family_layout: Option<String>,
        /// WARM floor of each PG view's writer pool: connections pre-filled at
        /// engine open and NEVER reaped (they carry the session's prepared
        /// statements). Default 8, exactly the historical `GROVEFS_PG_WRITE_POOL`
        /// value, so setting nothing changes nothing. Lower it (with
        /// `--pg-write-pool-max` raised) to make the per-view connection floor
        /// track real metadata concurrency instead of the configured ceiling.
        /// `GROVEFS_PG_WRITE_POOL` fills the same slot when this flag is absent.
        #[arg(long = "pg-write-pool", value_name = "N")]
        pg_write_pool: Option<usize>,
        /// RETAIN CEILING of each PG view's writer pool: burst connections a
        /// `checkout` dialled are kept up to this many, then shed back to the
        /// warm floor after `--pg-pool-idle-secs` idle. Default (unset or `0`)
        /// is the floor, i.e. the pre-split behavior where anything above the
        /// floor was dropped on return. `GROVEFS_PG_WRITE_POOL_MAX` fills the
        /// same slot.
        #[arg(long = "pg-write-pool-max", value_name = "N")]
        pg_write_pool_max: Option<usize>,
        /// WARM floor of each PG view's reader lane (default 4; env
        /// `GROVEFS_PG_READ_POOL`).
        #[arg(long = "pg-read-pool", value_name = "N")]
        pg_read_pool: Option<usize>,
        /// RETAIN CEILING of each PG view's reader lane (default: the floor;
        /// env `GROVEFS_PG_READ_POOL_MAX`).
        #[arg(long = "pg-read-pool-max", value_name = "N")]
        pg_read_pool_max: Option<usize>,
        /// Idle window after which a PG pool connection held ABOVE its warm
        /// floor is closed (default 60, `0` never reaps). Sized to keep a
        /// bursty view from paying connect + SERIALIZABLE pragma + prepared
        /// re-parse again on every write (measured 577us vs 316us per write
        /// transaction). `GROVEFS_PG_POOL_IDLE_SECS` fills the same slot.
        #[arg(long = "pg-pool-idle-secs", value_name = "SECS")]
        pg_pool_idle_secs: Option<u64>,
        /// Expose the REMOTE CONTROL PLANE on a second listener
        /// (`--control-listen HOST:PORT`): `grovefs view` / `grovefs apikey` /
        /// `grovefs refs` calls arrive as framed requests and are executed HERE,
        /// by the process that already owns the metadata store. That is what
        /// replaces handing every operator machine a database password: a
        /// remote admin presents an ACCOUNT KEY (`apikey create --for admin
        /// --owner <account>`) and never sees a store URI — and it works for the
        /// SQLite backend too, whose trees were previously local-only.
        ///
        /// The port ALWAYS authenticates as `Scope::Admin`; there is no open
        /// mode (mint the first admin key on this host). Bound before `--listen`
        /// accepts, so a bad address fails the start. Every call is audited as
        /// `method=control` in the same `mcp_calls` table `apikey calls` reads.
        /// Unset (default) keeps this process exactly as it was.
        #[arg(long = "control-listen", value_name = "HOST:PORT")]
        control_listen: Option<String>,
        /// Seed the web console's FIRST owner at install time
        /// (`grovefs web --control-server` signs in against the accounts this
        /// process creates). Applied ONLY when the account store is empty:
        /// a governance entrance that can mint its own administrator has no
        /// gate. Re-named on a later start with the same account is ignored (a
        /// systemd unit re-reads its flags on every restart, and that must not
        /// be an outage); a DIFFERENT account into a non-empty store is refused.
        /// After the first sign-in, accounts come from the console or
        /// `grovefs web account create`, never from a start-up flag.
        #[arg(long = "admin-user", value_name = "ACCOUNT")]
        admin_user: Option<String>,
        /// File holding `--admin-user`'s password (its first line, trailing
        /// newline stripped). A PATH and not the password itself because argv is
        /// readable by every local process (`ps`, the journal, a shell history)
        /// — and because this value, unlike every other credential here, is
        /// stretched into `web_accounts` and then never needed again. `0600` by
        /// the owner root is the expectation; nothing here can enforce it.
        #[arg(long = "admin-password-file", value_name = "PATH")]
        admin_password_file: Option<PathBuf>,
        /// Announce this meta-server to a MANAGER (`grovefs manager serve`) so
        /// clients can `mount --manager HOST:PORT` by deployment name instead of
        /// typing this address. Same contract as `data-server --register-with`:
        /// a heartbeat signed under `--join-key-file`, retried with backoff, and
        /// never a prerequisite for serving (an HA primary that cannot reach its
        /// directory service must still fail over to its peers). Requires
        /// `--join-key-file`; with `--role`/`--peer` the ORDER of the
        /// failover list a mount gets comes from `manager define
        /// --meta primary,standby`, which is where the preference belongs. Env
        /// `GROVEFS_REGISTER_WITH`/`GROVEFS_JOIN_KEY_FILE` fill the two flags.
        #[arg(long = "register-with", value_name = "HOST:PORT")]
        register_with: Option<String>,
        /// File holding the JOIN key (64 hex characters, `0600`) this server
        /// signs its registration with — the manager's own `--join-key-file`.
        /// A path and not the key itself: argv is world-readable via `ps`.
        #[arg(long = "join-key-file", value_name = "PATH")]
        join_key_file: Option<PathBuf>,
        /// The address to advertise to the manager instead of `--listen` — a
        /// server bound to `0.0.0.0:50053` must name how clients reach it.
        /// Must match the address declared by `manager define`.
        #[arg(long = "advertise-addr", value_name = "HOST:PORT")]
        advertise_addr: Option<String>,
        /// Which deployment this server belongs to (`manager define --name`;
        /// default `default`, env `GROVEFS_DEPLOYMENT`).
        #[arg(long = "deployment", value_name = "NAME")]
        deployment: Option<String>,
        /// How long this registration stays live without a refresh (default
        /// 90s, clamped to 5..3600; re-sent at a third of it).
        #[arg(long = "register-ttl", value_name = "SECS")]
        register_ttl: Option<u64>,
    },
    /// Run the MOUNT BOOTSTRAP service: answer "where is this deployment?" with
    /// a signed roster, and declare the topologies it answers for.
    ///
    /// A manager takes the coordinates off a mount command line and does nothing
    /// else. A `meta-server`/`data-server` started with `--register-with`
    /// heartbeats its advertised address here; a client running
    /// `mount --manager HOST:PORT --roster-key-file K` asks for one deployment's
    /// roster, verifies its signature, then talks to the meta- and data-servers
    /// DIRECTLY for the life of the mount. It is never in the IO path, so a
    /// manager restart disturbs no live mount and a manager outage only refuses
    /// NEW mounts (fail-closed) — the same two coordinates, resolved once instead
    /// of typed every time.
    ///
    /// What a roster CONTAINS is what `manager define` DECLARED, not whoever is
    /// currently heartbeating: GroveFS pins a striped object to its exact member
    /// set, so a "healthy subset" is not a thing a client may be handed. Liveness
    /// is what `manager status` shows, and `--require-registered` can gate a
    /// roster on it, but it never trims one.
    ///
    /// Two keys, because two different questions are being asked:
    /// `--join-key-file` authorizes a REGISTRATION (held by servers, never by
    /// clients) and `--roster-key-file` signs the ANSWER (signed by the manager,
    /// verified by clients). A registration naming an address the operator did
    /// not declare is refused even under a correct join-key MAC, so a leaked join
    /// key cannot place an attacker's node in front of your clients.
    #[command(
        about = "Run the deployment bootstrap service (declares topologies, signs rosters)",
        after_long_help = r#"USAGE EXAMPLES

  Declare a deployment (once; the file IS the topology, reviewable in git):
    grovefs --home /srv/gm manager define --meta m1:50053,m2:50053 --data d1:50100,d2:50100

  Serve it (both keys are minted into the state dir on first start if absent):
    grovefs --home /srv/gm manager serve --listen 0.0.0.0:50055

  A node announces itself (advertise what CLIENTS can dial, not what you bind):
    grovefs data-server -l 0.0.0.0:50100 --member-id 0 --advertise-addr d1.example:50100 \
        --register-with gm.example:50055 --join-key-file /etc/grovefs/join.key

  A client mounts with no coordinates typed:
    grovefs mount --view alice -m /mnt --manager gm.example:50055 --roster-key-file /etc/grovefs/roster.key

  The fleet view — who is declared, and who last registered:
    grovefs manager status --server gm.example:50055 --join-key-file /etc/grovefs/join.key
"#
    )]
    Manager {
        /// Directory holding `manager/deployments.json` (the declared topology)
        /// and, unless `serve`'s key flags name elsewhere, the
        /// `manager/join.key`/`manager/roster.key` minted on first start.
        /// Defaults to the global `--home`.
        #[arg(long = "state-dir", value_name = "DIR", global = true)]
        state_dir: Option<PathBuf>,
        #[command(subcommand)]
        action: ManagerAction,
    },
    /// Unmount a mountpoint
    #[command(after_help = r#"EXAMPLES
  grovefs umount -m /mnt/photos
  grovefs umount --mountpoint /mnt/photos"#)]
    Umount {
        /// The mountpoint to unmount (terminates its background mount).
        #[arg(short, long)]
        mountpoint: PathBuf,
    },
    /// Manage server-side views: base trees, derived lenses, pure aliases and forks.
    ///
    /// A BASE view is a physical tree (`meta/<name>/<name>.db`): mountable read-write,
    /// the only kind whose deletes do real garbage collection. A DERIVED view is a
    /// named presentation lens over ONE physical tree, mounted read-only; a pure
    /// ALIAS maps a public name to a real name (writable, real name not
    /// mountable); a FORK is an independent metadata tree seeded from a base —
    /// structure writable, content read-only (EROFS), deletes touch only the
    /// fork. Definitions live in the control-plane store (SQLite
    /// `<meta-dir>/meta/views/<name>.layout`, PG `grovefs_control.view_defs`),
    /// clients simply mount
    /// `--view <name>` and need no local rules file: a `meta-server` resolves
    /// them at `M_OPEN`. Rules are
    /// display customization, NOT a security boundary — hard isolation stays
    /// view + API-key/token (mounting the base name directly shows the full
    /// tree to whoever is allowed past its gates). Derived (virtual) views
    /// mount READ-ONLY: the shape is centrally governed, so every write goes
    /// to the original (base) view — a mount of a derived name refuses all
    /// mutations with EROFS (kernel read-only flag negotiated at `M_OPEN`,
    /// plus an authoritative per-op gate on the meta-server). A fork mounts
    /// read-write for the tree and refuses only content writes (write/truncate)
    /// with EROFS — organize names freely, upload bytes to the base view.
    View {
        /// The metadata root — same option name and directory as `meta-server
        /// --meta-dir` / `grovefs apikey --meta-dir`; falls back to the global
        /// `--home` when omitted. Under SQLite it holds `meta/views/*.layout`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<meta-dir>/meta/views/*.layout` files; a `postgres://…`
        /// (`postgresql://…` too) URI governs the shared `grovefs_control.view_defs`
        /// table. A `postgres://…` URI — or the `GROVEFS_META_STORE` env — makes
        /// definitions and the
        /// meta-server's own resolver read/write ONE system of record. Must
        /// match the `meta-server --meta-store` this root is served by.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Govern the REMOTE deployment served by `meta-server --control-listen
        /// HOST:PORT` instead of a store on this machine: the server performs
        /// the work and this host needs only an admin ACCOUNT KEY (no database
        /// URI, no access to `<meta-dir>`). Works for BOTH backends, including
        /// SQLite. Cannot be combined with `--meta-dir`/`--meta-store`: a
        /// remote control plane is bound to its server, so a local path here
        /// would describe a store this deployment never sees (the pair is
        /// refused, not silently merged).
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key (`<id>:<secret>`, minted by
        /// `apikey create --for admin --owner <account>` and readable back with
        /// `apikey secret`) for --control-server.
        /// Falls back to `GROVEFS_CONTROL_KEY`, then `GROVEFS_API_KEY`.
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        #[command(subcommand)]
        action: ViewAction,
    },
    /// Serve view governance over MCP (stdio) so an AI agent can drive it.
    ///
    /// An MCP client (agent runtime) spawns this as a subprocess and the agent
    /// drives the same validated operations as `grovefs view`: list_views /
    /// define_derived_view / alias_view / remove_view / preview_rules — rules
    /// travel as inline text (hide/only/map/link), so an agent customizes a
    /// data listing's shape without touching files. Implements the initialize /
    /// ping / tools/list / tools/call core of the 2025-06-18 protocol revision
    /// with newline-delimited JSON-RPC 2.0 framing. Control plane only: it
    /// writes the store's view definitions (live meta-server mounts hot-reload
    /// them) but never mounts or touches data. `--meta-dir`
    /// sets the default metadata root (falls back to `--home`); a per-call
    /// `db_path` argument overrides it.
    #[command(
        after_long_help = r#"MCP TOOL SURFACE — what `tools/list` returns (10 tools).
Every tool takes an optional `db_path` (defaults to this server's --meta-dir/--home),
carries MCP annotations (readOnlyHint/destructiveHint/idempotentHint) for client
approval prompts, and the four JSON-returning ones also declare an outputSchema
and return structuredContent alongside the human-readable text.

  DISCOVERY (read-only, never mutates; metadata only, no data bytes):
    list_views            every slot: physical trees + derived lenses + forks + aliases (JSON summary)
    get_view              one view in full — the verbatim rules, for read-modify-write editing
    list_view_tree        the exact listing a mount shows; also previews an unpublished base+rules lens

  GOVERNANCE (writes <db>/meta/views/*.layout definitions; never mounts or touches data):
    define_derived_view   create/replace a READ-ONLY derived lens (name, base, rules: hide/only/map/link)
    fork_view             create an INDEPENDENT tree (name, base): structure writable, content read-only
    graft_view            copy a curated, renamable subset into a fork/base view (name, map plan)
    alias_view            create/replace a WRITABLE public alias (name, real; `retire` rotates gaplessly)
    remove_view           drop a definition by name (a fork also drops its own tree; data bytes stay while held)

  DRY RUN (validate a candidate before publishing; writes nothing):
    preview_rules         parse-check rules text alone (line-level error + accepted counts)
    validate_definition   full governance pre-check — returns the exact refusal a real publish would give

  BEYOND TOOLS (same store, no extra configuration):
    resources             grovefs://guide, grovefs://views,
                          grovefs://view/<name>/definition|tree{?path,depth,max}
    prompts               reshape-view, rotate-alias playbooks
    completion            fills view-name arguments from the store
    logging/setLevel      streams notifications/message (naming the tool) per call

NOTES
  Protocol: 2025-06-18 over newline-delimited JSON-RPC 2.0 on stdio; the
    2025-03-26 and 2024-11-05 revisions are accepted too (an unknown revision
    is answered with 2025-06-18 rather than echoed).
  Control plane only: it reshapes how a view LIST is presented; it never mounts
    a filesystem or reads/writes file bytes.
  Auth: start with --require-api-key, mint an MCP-scoped key via
    `grovefs apikey create --label <name> --for mcp`, present it as `apiKey` in
    the initialize params (or export GROVEFS_MCP_API_KEY).

USAGE EXAMPLES

  Trust the local OS boundary (anyone who can spawn it may govern):
    grovefs mcp --meta-dir /meta

  Require a key, then point an MCP client's server entry at this command:
    grovefs apikey --meta-dir /meta create --label claude-desktop --for mcp
    # client config: command "grovefs", args ["mcp", "--meta-dir", "/meta",
    #                "--require-api-key"]

  Bind a different control-plane store (PostgreSQL, shared with meta-server):
    GROVEFS_META_STORE=postgres://host/grovefs grovefs mcp --meta-dir /meta
"#
    )]
    Mcp {
        /// Metadata root holding `meta/views/` — same option name and
        /// directory as `grovefs view` / `meta-server --meta-dir`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Require an API key before the tool surface is usable. With it, an
        /// MCP client must present a `grovefs apikey create` key as `apiKey` in
        /// the `initialize` params (or the operator exports
        /// `GROVEFS_MCP_API_KEY` to pre-authenticate the whole server); until a
        /// key authenticates, `tools/list` and `tools/call` are refused with a
        /// JSON-RPC error. `initialize`/`ping` stay open so a client can always
        /// handshake. Off by default: a plain `grovefs mcp` trusts whoever can
        /// spawn it (the same OS boundary as read access to its `--meta-dir`).
        /// Either way, every
        /// tool call is appended to `<meta-dir>/meta/_control/api_keys.db`'s audit log.
        #[arg(long)]
        require_api_key: bool,
        /// Control-plane backend. Unset / `"sqlite"` governs the local
        /// `<meta-dir>/meta/views/*.layout` files and `api_keys.db`; a
        /// `postgres://…` (`postgresql://…`) URI — or the `GROVEFS_META_STORE` env —
        /// binds the server to the shared `grovefs_control` schema (definitions,
        /// keys and the call audit all in PG). A PG-bound server ignores a
        /// per-call `db_path` override: one URI, one system of record.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
    },
    /// Manage the MCP server's API keys and read its call-audit log.
    ///
    /// Keys authenticate callers of `grovefs mcp --require-api-key`; what the
    /// check consults is the stored SHA-256 digest, kept next to a wrapped copy
    /// of the credential that `apikey secret` reads back — so `api_keys.db` on
    /// its own still hands out nothing usable (opening that copy needs the
    /// store key). Revoking
    /// keeps the row (so past calls stay attributed) but fails authentication.
    /// The audit log records every tool call (timestamp, key, tool, success,
    /// detail).
    #[command(after_help = r#"EXAMPLES
  Mint a mount-only key (the secret is printed once):
    grovefs apikey --meta-dir /meta create \
      --label analyst --for mount --ttl 720h

  Audit who called what, then trim the trail:
    grovefs apikey --meta-dir /meta calls --limit 50
    grovefs apikey --meta-dir /meta purge --before 720h --dry-run

  Rotate without changing the id, then revoke:
    grovefs apikey --meta-dir /meta rotate --id 8f3a…e21c
    grovefs apikey --meta-dir /meta revoke --id 8f3a…e21c"#)]
    Apikey {
        /// The metadata root holding `meta/_control/api_keys.db` — same option name and
        /// directory as `grovefs mcp --meta-dir` / `meta-server --meta-dir`; falls
        /// back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Key/audit backend. Unset / `"sqlite"` uses the local
        /// `<meta-dir>/meta/_control/api_keys.db`; a `postgres://…` (`postgresql://…`)
        /// URI — or the `GROVEFS_META_STORE` env — uses the shared
        /// `grovefs_control.api_keys`/`mcp_calls` tables, so a key minted here
        /// is authoritative for every meta-/data-server and MCP pointing at that
        /// URI with no per-node file to colocate.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Manage the keys of the REMOTE deployment behind `meta-server
        /// --control-listen HOST:PORT` with an admin account key. The first
        /// admin key must be minted on the server host (a control port that
        /// would mint its own administrator would defeat the gate).
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        #[command(subcommand)]
        action: ApikeyAction,
    },
    /// Serve the MANAGEMENT CONSOLE, and administer its sign-in accounts.
    ///
    /// With no subcommand this is a small HTTP gateway in front of `meta-server
    /// --control-listen`: it serves the embedded single-page app (overview, views,
    /// keys, accounts and grants, audit), turns a browser session into an ACCOUNT,
    /// and forwards each click to the control plane — where the rule that decides
    /// what that account may touch lives beside the data. It holds ONE
    /// `admin`-scope key and no store credential, adds no governance rule of its
    /// own, and only ever REMOVES capability: the verbs it refuses to route
    /// (`gc`, `refs rebuild`, `view rm-base`, `fork graft`) stay CLI-only, where
    /// their dry-run reports and typed confirmations are part of calling them.
    ///
    /// Sign in as the account seeded at install time (`meta-server --admin-user
    /// <name> --admin-password-file <path>`, applied only while the account store
    /// is empty), then create the rest here or in the page. Loopback is the
    /// default bind and this server speaks plain HTTP: a routable `--listen` needs
    /// `--allow-insecure-http`, or TLS on a reverse proxy in front of it (with
    /// `--use-x-forwarded-for`, so sign-ins are throttled per client rather than
    /// per proxy).
    #[cfg(feature = "web")]
    #[command(
        about = "Serve the management console, or administer its sign-in accounts",
        after_help = r#"EXAMPLES
  Serve the console in front of a control plane:
    grovefs web --control-server host:50054 \
      --api-key ADMIN_ID:SECRET --listen 0.0.0.0:8080

  Seed a user and delegate one base view:
    grovefs web --control-server host:50054 \
      --api-key ADMIN_ID:SECRET \
      account create --account alice --password-file pass.txt --role user
    grovefs web --control-server host:50054 \
      --api-key ADMIN_ID:SECRET \
      grant add --account alice --base photos"#
    )]
    Web {
        /// The governance entrance to put the console in front of: the
        /// `meta-server --control-listen HOST:PORT` that owns the accounts, the
        /// views and the audit log. Required — this process stores nothing.
        #[arg(long = "control-server", value_name = "HOST:PORT", global = true)]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value). This
        /// is the deployment's governance credential: it must stay on this host,
        /// which is what `--api-key-file` (a `0600` secret file a unit reads) and
        /// the loopback default are for.
        #[arg(long = "api-key", value_name = "ID:SECRET", global = true)]
        control_key: Option<String>,
        /// File holding that key (its first line) instead of `--api-key`, for a
        /// systemd unit or a mounted secret — argv is visible to every local
        /// process and lands in the shell history. Beats `GROVEFS_CONTROL_KEY`.
        #[arg(long = "api-key-file", value_name = "PATH", global = true)]
        control_key_file: Option<PathBuf>,
        /// Address to serve the console on. `127.0.0.1` by default; a browser on
        /// another host reaches it through a reverse proxy or an SSH tunnel
        /// (`ssh -L 8787:127.0.0.1:8787 <host>`).
        #[arg(long = "listen", value_name = "HOST:PORT", default_value = grovefs::web::DEFAULT_LISTEN)]
        listen: String,
        /// Idle window of a signed-in session (`30m`, `8h`, `2d`). The console
        /// mints keys and changes roles, so an unattended browser gets a shorter
        /// leash than an unattended shell. A revoked grant or a disabled account
        /// takes effect sooner than this: on that session's NEXT call, because the
        /// server re-reads the principal per request.
        #[arg(long = "session-ttl", value_name = "AGE", default_value = "8h")]
        session_ttl: String,
        /// Worker threads, i.e. how many console requests may be in flight. One
        /// governance call blocks its thread, so this caps concurrency and waiting
        /// browsers at once; further requests queue in the listener.
        #[arg(long, default_value_t = 4)]
        workers: usize,
        /// Bind a NON-LOOPBACK address over plain HTTP anyway: every sign-in and
        /// every governance call then crosses the wire unencrypted. Only correct
        /// when the network itself is trusted end to end.
        #[arg(long)]
        allow_insecure_http: bool,
        /// Take the client address from `X-Forwarded-For` instead of the socket.
        /// Correct ONLY behind a proxy that sets and sanitizes that header: an
        /// untrusted value would turn the sign-in throttle and the audit's `ip`
        /// into something the caller chooses.
        #[arg(long)]
        use_x_forwarded_for: bool,
        #[command(subcommand)]
        action: Option<WebAction>,
    },
    /// Inspect and repair the cross-view blob reference ledger (the shared
    /// `meta/_control/refs.db` that decides when a physical object may actually be
    /// deleted). A blob is freed only when its last name in EVERY view holding
    /// it is removed; this ledger is what makes a delete in one view safe for
    /// another view sharing the same object (e.g. `import` of one source into
    /// two views). Normally maintenance-free — it is updated atomically with
    /// every create/delete — but a tree removed OUT-OF-BAND (`rm -rf meta/x/`,
    /// a fresh `--home`) never released its holds, so the ledger over-counts and
    /// keeps bytes too long. `refs rebuild` rewrites it from the trees that
    /// actually exist. Works on the SQLite store (`meta/_control/refs.db`) and,
    /// through `--meta-store postgres://…`, on the ledger that lives IN the
    /// PostgreSQL schemas. See `grovefs view create`.
    #[command(about = "Inspect and repair the cross-view blob reference ledger")]
    #[command(after_help = r#"EXAMPLES
  Rebuild the local SQLite ledger (quiesce the store first):
    grovefs refs --meta-dir /meta rebuild

  Rebuild the ledger that lives in PostgreSQL schemas:
    grovefs refs --meta-store postgres://user@host/db rebuild

  Rebuild a remote deployment through its control plane:
    grovefs refs --control-server host:50054 \
      --api-key ADMIN_ID:SECRET rebuild"#)]
    Refs {
        /// The metadata root holding `meta/_control/refs.db` and one directory per
        /// view tree — same option name and directory as `meta-server --meta-dir`;
        /// falls back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Metadata STORE URI: a `postgres://…` URI rebuilds the ledger that
        /// lives IN the PostgreSQL schemas instead of `meta/_control/refs.db`. Falls
        /// back to `$GROVEFS_META_STORE`, exactly like `gc`'s backend guards.
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Rebuild the ledger on the REMOTE deployment behind `meta-server
        /// --control-listen HOST:PORT` (either backend) with an admin account
        /// key instead of touching the store here.
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        #[command(subcommand)]
        action: RefsAction,
    },
    /// Inspect or migrate the on-disk METADATA LAYOUT of a local store.
    ///
    /// Layout v2 gave every physical view tree its own directory —
    /// `meta/<view>/<view>.db` with its WAL/SHM beside it — and moved the
    /// control-plane stores (`refs`, `api_keys`, `web_accounts`, the store key)
    /// into `meta/_control/`, so `meta/` lists trees and nothing else. A store
    /// created by an older build is still FLAT (`meta/<view>.db` among its
    /// control files), and no other command reads that shape: `view list` would
    /// show no views, `gc` would find no trees and `refs rebuild` would book no
    /// holders. Every such command therefore REFUSES a flat store and names this
    /// one verb instead of half-reading it.
    ///
    /// `migrate` moves files only — the ledger counts content keys, not paths, so
    /// nothing has to be recomputed — and it is idempotent: an interrupted run
    /// leaves the already-moved trees in place and finishes the rest. It must run
    /// QUIESCED (no mount of this store, no `meta-server`/`data-server` on it),
    /// which the command checks rather than trusts.
    #[command(after_help = r#"EXAMPLES
  Inspect a metadata root:
    grovefs layout --home ~/grove show

  Preview, then apply, the pre-v2 migration:
    grovefs layout --home ~/grove migrate --dry-run
    grovefs layout --home ~/grove migrate

  Choose where a base view's forks and definitions land:
    grovefs layout --home ~/grove set-family grouped"#)]
    Layout {
        /// The metadata root to inspect or migrate — the same directory
        /// `meta-server --meta-dir` uses. Falls back to the global `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        #[command(subcommand)]
        action: LayoutAction,
    },
    /// Audit and reclaim ORPHAN objects (offline GC).
    ///
    /// An orphan is an object that NO surviving view tree names and the
    /// cross-view ledger does not book — the residue of an out-of-band
    /// `rm meta/<view>/<view>.db` (no unlink ever ran) or a crash between object
    /// creation and the node insert. SCAN IS THE DEFAULT: it prints the counts,
    /// the biggest orphans and any ledger-held keys that must first be released
    /// with `grovefs refs rebuild`.
    ///
    /// Two ways to reach the store. `<data-dir>/blocks/` is audited in place,
    /// which is the local deployment. With `--control-server` the TREES are read
    /// by the server that owns them — SQLite or PostgreSQL — and
    /// the objects are listed from `--data-servers`, so any machine holding an
    /// admin key runs the same audit for either backend.
    ///
    /// `--apply` deletes only objects older than `--older-than` (a hard-linked
    /// import inherits the SOURCE file's mtime, so young-object filtering alone
    /// is not enough), never a key the ledger still holds, and refuses to run
    /// while a view is mounted or a grovefs process is using the store. A view
    /// tree that cannot be read aborts the whole scan rather than guessing — a
    /// skipped tree would make every blob it names look orphaned.
    #[command(after_help = r#"EXAMPLES
  Report unreferenced objects (never deletes):
    grovefs gc --meta-dir ~/grove

  Delete objects older than the grace window (store offline):
    grovefs gc --meta-dir ~/grove --apply

  Audit a remote deployment through its control plane:
    grovefs gc --control-server host:50054 \
      --api-key ADMIN_ID:SECRET --data-servers host:50100"#)]
    Gc {
        /// Metadata root holding one directory per view tree — the same option name
        /// and directory as `meta-server --meta-dir`; falls back to the global
        /// `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data roots holding `blocks/` — one path, or comma-separated for
        /// several disks, same convention as `data-server --data-dir`;
        /// defaults to `--meta-dir`.
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
        /// Minimum object age to reclaim (`30m`, `24h`, `7d`; `0` disables the
        /// grace window). Younger objects are never deleted: an in-flight
        /// create/import may not have committed its node row yet.
        #[arg(long, default_value = "24h", value_name = "DURATION")]
        older_than: String,
        /// Delete the reclaimable candidates (default is a dry-run report).
        #[arg(long)]
        apply: bool,
        /// Emit one JSON object instead of the text report.
        #[arg(long)]
        json: bool,
        /// Bypass the "this data root shares no object with any tree" guard —
        /// only for a store whose every surviving reference lives elsewhere.
        #[arg(long)]
        force: bool,
        /// Audit the deployment behind `meta-server --control-listen HOST:PORT`
        /// with an admin account key: the server reads its own trees (any
        /// backend) and the objects are listed from `--data-servers`.
        #[arg(long = "control-server", value_name = "HOST:PORT")]
        control_server: Option<String>,
        /// The `admin`-scope account key for --control-server
        /// (`GROVEFS_CONTROL_KEY` / `GROVEFS_API_KEY` fill a missing value).
        #[arg(long = "api-key", value_name = "ID:SECRET")]
        control_key: Option<String>,
        /// Data servers holding the bytes — one `HOST:PORT`, or a
        /// comma-separated list, the same addresses a mount uses
        /// (`GROVEFS_DATA_SERVERS` fills a missing value). Required with
        /// --control-server.
        #[arg(long = "data-servers", value_name = "HOST:PORT[,HOST:PORT]")]
        data_servers: Option<String>,
        /// Static token the data server was started with (`--require-token`);
        /// `GROVEFS_TOKEN` fills a missing value.
        #[arg(long = "data-token", value_name = "TOKEN")]
        data_token: Option<String>,
        /// Account key the data server was started with (`--require-api-key`);
        /// `GROVEFS_API_KEY` fills a missing value.
        #[arg(long = "data-api-key", value_name = "ID:SECRET")]
        data_api_key: Option<String>,
    },
    /// Import an existing local directory tree into a view, zero-copy where possible.
    ///
    /// Every regular file is registered as an object — hard-linked into
    /// `blocks/` when source and store share a filesystem (zero extra bytes;
    /// edits through either path stay visible in the other), streamed as a copy
    /// otherwise — and the directory structure is mirrored into the view tree
    /// under `--prefix`. Run it where the store lives (local `--home`, or the
    /// meta/data servers' root — same operational envelope as `grovefs
    /// view`). Re-running is an INCREMENTAL import: names already
    /// in the view are skipped, and unchanged directories are pruned from the
    /// walk via a saved watermark (or `--since`; `--full` forces a rescan).
    /// Nothing is ever deleted from the view. Symlinks and special files are
    /// skipped (counted, never followed). Timestamps, mode and owner are
    /// preserved from the source. The target must be a PHYSICAL view — importing
    /// into a derived (virtual) name is refused, since virtual views mount
    /// read-only. `--meta-store` selects the deployment's metadata backend —
    /// local SQLite (default), PostgreSQL (`postgres://…`, nodes go into the
    /// view schema's `node` table) or treedb (`treedb://host:port,…`, nodes go
    /// through the engine's POSIX metadata ops onto the instance the name is
    /// placed on) — while the BYTES always land in `--data-dir` under the same
    /// `blocks/<bucket>/<key>` layout a mounted write uses. Live
    /// `meta-server` mounts converge within their attr/dentry TTL (≤2s, no
    /// remount).
    #[command(after_help = r#"EXAMPLES
  Zero-copy import on the store host:
    grovefs import --home ~/grove --view alice --source /srv/legacy

  Preview an import under a prefix:
    grovefs import --meta-dir /meta --view alice \
      --source /srv/legacy --prefix inbox/2026 --dry-run

  Incremental re-run (only new files/ dirs are registered):
    grovefs import --home ~/grove --view alice --source /srv/legacy

  PostgreSQL deployment:
    grovefs import --meta-store postgres://grove@127.0.0.1/grovefs \
      --view alice --source /srv/legacy --data-dir /srv/grove/data

  treedb deployment (data objects still go to --data-dir):
    grovefs import --meta-store treedb://127.0.0.1:7777,10.0.0.2:7777 \
      --view alice --source /srv/legacy --data-dir /srv/grove/data"#)]
    Import {
        /// Metadata root holding `meta/<view>/<view>.db` — same option name and
        /// directory as `meta-server --meta-dir`; falls back to `--home`.
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data root holding `blocks/` — same option name and values as
        /// `data-server --data-dir` (a comma-separated list spreads imported
        /// objects across the node's disks by the same key-hash placement);
        /// defaults to `--meta-dir` (the colocated topology used by every
        /// deployment example).
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
        /// Target view (its meta DB is created if absent, like `view create`).
        #[arg(long)]
        view: String,
        /// Existing directory to mirror in.
        #[arg(long)]
        source: PathBuf,
        /// Metadata backend of the deployment this import writes into. With no
        /// store (or a plain path) the tree is a LOCAL SQLite view db
        /// (`meta/<view>/<view>.db` plus hardlinks/copies under `blocks/`). A
        /// `--meta-store postgres://…` records nodes in that PG schema's `node`
        /// table and a `--meta-store treedb://host:port,…` writes them through
        /// the treedb engine's POSIX metadata ops into the view tree the
        /// deployment routes the name to. In every case the BYTES land in the
        /// `--data-dir` root under the same `blocks/<bucket>/<key>` layout a
        /// mounted write uses, so run it on the storage node beside the
        /// data-server (a live meta-server converges on the new tree within ~2s).
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// In-view destination directory; defaults to the source dir's name
        /// at the view root.
        #[arg(long)]
        prefix: Option<String>,
        /// Incremental prune watermark (unix seconds): skip already-present
        /// source directories whose `mtime` predates it rather than rescanning
        /// them. New/renamed entries always bump a parent dir `mtime`, so this
        /// cannot miss an addition. Defaults to the watermark saved by the last
        /// successful import of this view+source+prefix; pass `--full` to ignore
        /// it and walk everything.
        #[arg(long = "since", value_name = "EPOCH_SECS")]
        since: Option<u64>,
        /// Ignore any saved watermark and rescan the whole source tree (the
        /// name-skip rule still keeps already-imported files).
        #[arg(long)]
        full: bool,
        /// Only report what would be imported; touch nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Report disk usage (subtree byte totals) without going through FUSE.
    /// Connects directly to the metadata backend (PG or SQLite) for the tree
    /// structure, then batch-stats content sizes via the data-server (or local
    /// blocks/). Output is `du`-compatible: `<size>\t<path>`, sorted by depth.
    #[command(about = "Report disk usage without going through FUSE")]
    #[command(after_help = r#"EXAMPLES
  Measure a whole view:
    grovefs du --home ~/grove --view alice

  One subtree, then exact bytes:
    grovefs du --home ~/grove --view alice --path photos
    grovefs du --home ~/grove --view alice --bytes

  Measure a remote deployment:
    grovefs du --view alice --meta-server host:50053 \
      --data-servers host:50100"#)]
    Du {
        /// View to measure.
        #[arg(long)]
        view: String,
        /// Meta-server endpoint (host:port) — uses M_TREE_DUMP RPC to get
        /// the tree without direct PG/SQLite access.
        #[arg(long = "meta-server", value_name = "ADDR[,ADDR]")]
        meta: Option<String>,
        /// Access token for meta-server (if --meta-server requires auth).
        #[arg(long)]
        meta_token: Option<String>,
        /// API key for meta-server (if --meta-server requires auth).
        #[arg(long)]
        meta_api_key: Option<String>,
        /// PostgreSQL store URI (same as meta-server --meta-store).
        #[arg(long = "meta-store", value_name = "URI")]
        store: Option<String>,
        /// Metadata root for SQLite mode (meta/<view>/<view>.db lives here).
        #[arg(long = "meta-dir", value_name = "DIR")]
        db_path: Option<PathBuf>,
        /// Data-server endpoints for batch stat: `host:port`, or a
        /// comma-separated list in the same order the mount used. Needed when
        /// the tree comes through `--meta-server`/`--meta-store`, which imply no local
        /// storage root to stat. A striped store (`s<N>` data_refs) can only
        /// be measured with exactly the `N` addresses the writer used: a list
        /// of any other length is refused in either direction, not zeroed.
        #[arg(long = "data-servers", value_name = "ADDR[,ADDR]")]
        data: Option<String>,
        /// Local data root holding blocks/; defaults to the store root named
        /// by `--meta-dir`/`--home` (same convention as `grovefs import
        /// --data-dir`). Without either a local root or `--data-servers`, sizes would
        /// be guessed at zero, so the command refuses.
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
        /// API key for data-server authentication.
        #[arg(long = "data-api-key")]
        api_key: Option<String>,
        /// Access token for data-server (legacy shared-token mode).
        #[arg(long = "data-token")]
        token: Option<String>,
        /// Subpath within the view to start from (default: root /).
        #[arg(long, default_value = "/")]
        path: String,
        /// Show per-directory breakdown up to N levels deep (0 = only the
        /// grand total of --path, 1 = plus immediate children, etc.).
        #[arg(long, default_value = "999999")]
        depth: usize,
        /// Print raw byte counts (no human K/M/G suffix).
        #[arg(long, short = 'b')]
        bytes: bool,
        /// Print only the grand total (equivalent to --depth 1 --path /).
        #[arg(long)]
        total: bool,
    },
}

#[derive(Subcommand)]
enum ManagerAction {
    /// Serve registrations, lookups and the fleet view (the daemon).
    ///
    /// Bind before you declare: a manager with nothing declared starts anyway
    /// and answers every lookup with "deployment 'x' is not declared (known:
    /// [])", which is the honest answer — it refuses to invent a topology.
    #[command(
        about = "Serve node registrations, lookups and signed rosters.",
        after_help = "EXAMPLES\n  grovefs --home /srv/gm manager serve --listen 0.0.0.0:50055"
    )]
    Serve {
        /// Listen address for registrations and lookups, e.g. 0.0.0.0:50055.
        /// A separate port from the meta-/data-planes on purpose: it speaks the
        /// same framing, but only ever answers three questions about addresses.
        #[arg(short, long, default_value = "127.0.0.1:50055")]
        listen: String,
        /// File holding the JOIN key (64 hex chars) that authorizes a
        /// registration. Created with a fresh random key if absent (and its
        /// path printed, because every node that should register needs the same
        /// file). Unset falls back to `<state-dir>/manager/join.key`.
        #[arg(long = "join-key-file", value_name = "PATH")]
        join_key_file: Option<PathBuf>,
        /// File holding the ROSTER key (64 hex chars) this process signs rosters
        /// WITH. Clients hold the same value to verify, and an HMAC key is
        /// symmetric, so treat it as a secret on the manager and distribute it
        /// read-only to clients. Unset falls back to
        /// `<state-dir>/manager/roster.key`, created if absent.
        #[arg(long = "roster-key-file", value_name = "PATH")]
        roster_key_file: Option<PathBuf>,
        /// How long an ISSUED roster stays valid (seconds, default 300, clamped
        /// to 5..3600). A mount consults the manager once and never again, so
        /// this bounds REPLAY of a captured roster rather than refreshing
        /// anything: longer means a topology change takes longer to invalidate
        /// already-handed-out rosters.
        #[arg(long = "roster-ttl", value_name = "SECS", default_value_t = grovefs::manager::server::DEFAULT_ROSTER_TTL)]
        roster_ttl: u64,
        /// How long a REGISTRATION lives without a refresh (seconds, default
        /// 90). A node re-sends at a third of this, so one dropped heartbeat is
        /// not an outage report. Clamped to 5..3600 on both ends.
        #[arg(long = "register-ttl", value_name = "SECS", default_value_t = grovefs::manager::server::DEFAULT_REGISTER_TTL)]
        register_ttl: u64,
        /// Refuse a LOOKUP while any declared address of that deployment has no
        /// live registration. Off by default, and it is a PRECHECK, not a
        /// filter: on `--require-registered` a roster is still the full declared
        /// set, because handing a client a subset of a stripe's members breaks
        /// its objects. Use it where a half-started cluster should fail mounts
        /// loudly instead of letting them discover the gap by timing out.
        #[arg(long)]
        require_registered: bool,
    },
    /// Declare (or re-declare) one deployment's addresses.
    ///
    /// This is the authoritative topology: what a signed roster answers with.
    /// It writes `<state-dir>/manager/deployments.json` atomically and a running
    /// `manager serve` re-reads it when the file changes, so declaring needs no
    /// daemon-side credential and no restart.
    #[command(
        about = "Declare one deployment's meta/data addresses.",
        after_help = r#"EXAMPLES
  grovefs --home /srv/gm manager define \
    --meta m1:50053,m2:50053 --data d1:50100,d2:50100"#
    )]
    Define {
        /// Deployment name, as clients will ask for it with
        /// `mount --deployment` (default `default`).
        #[arg(long, default_value = grovefs::manager::DEFAULT_DEPLOYMENT)]
        name: String,
        /// The meta-server failover list, IN PREFERENCE ORDER — this exact list
        /// and order is what a mount dials, so the primary is
        /// the first address here, not a flag on the client.
        #[arg(long, value_name = "ADDR[,ADDR]")]
        meta: Option<String>,
        /// The COMPLETE data-server member set. Not a candidate pool: striped
        /// objects are pinned to this exact set, so a mount given fewer addresses
        /// cannot read them. Every `data-server` of the deployment belongs here.
        #[arg(long, value_name = "ADDR[,ADDR]")]
        data: Option<String>,
    },
    /// Ask a running manager for its fleet view (JSON): every declared address
    /// plus who last registered, for how long, and with what member id.
    #[command(
        about = "Show registered nodes and roster staleness (JSON).",
        after_help = "EXAMPLES\n  grovefs manager status --server 127.0.0.1:50055"
    )]
    Status {
        /// The manager to ask, `host:port`.
        #[arg(long = "server", value_name = "HOST:PORT")]
        server: String,
        /// Limit the report to one deployment (default: all of them).
        #[arg(long)]
        deployment: Option<String>,
        /// The JOIN key file this operator holds (the manager's own
        /// `join.key`, or a copy). Required: the fleet view enumerates every
        /// address in the cluster, so it is not a public read like a lookup.
        #[arg(long = "join-key-file", value_name = "PATH")]
        join_key_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
#[command(after_help = r#"USAGE EXAMPLES

  Stand up a base tree and inspect the store:
    grovefs view --meta-dir /meta create --view alice
    grovefs view --meta-dir /meta list

  Publish a read-only lens over alice:
    grovefs view --meta-dir /meta define \
      --name lens_docs --base alice --rules lens.rules

  Give alice a writable public name (its real name stops being mountable):
    grovefs view --meta-dir /meta alias --name bias_public --real alice

  Curate a subset into an independent tree:
    grovefs view --meta-dir /meta fork --name curated --base alice
    grovefs view --meta-dir /meta graft --name curated --map plan.txt

  Govern a remote deployment instead of a local store:
    grovefs view --control-server host:50056 --api-key ID:SECRET list

Four view kinds - the verb that creates each:
  base     -> view create --view V
  derived  -> view define  --base B --name D --rules F   (read-only lens)
  alias    -> view alias   --real R --name A             (public name, writable)
  fork     -> view fork    --base B --name F             (fill it with: view graft)
Only a BASE view owns physical deletion; derived/alias/fork reshape or rename one."#)]
enum ViewAction {
    /// Create a NEW BASE (physical) view named `--view`, seeding its empty tree
    /// root (optionally from `--manifest`). This is the deliberate, explicit
    /// act of standing up a real storage tree — the one kind of view whose
    /// deletes do REAL garbage collection: removing a file's last name in a base
    /// view, once no derived lens or other view still references the bytes
    /// (cross-view ledger), frees the underlying object. Derived views
    /// (`define`), forks (`fork`) and aliases (`alias`) only reshape, copy or
    /// rename a base view; they never own physical deletion. This is the ONLY
    /// entrance for standing up a base view — a `mount` never creates one, and
    /// neither does writing into a name that has no tree.
    #[command(
        about = "Create a new BASE (physical) view.",
        after_help = r#"EXAMPLES
  grovefs view --meta-dir /meta create --view alice
  grovefs view --meta-dir /meta create --view alice --manifest ./seed.jsonl"#
    )]
    Create {
        /// Name of the new base (physical) view; its tree lives at
        /// `<meta-dir>/meta/<view>/<view>.db`.
        #[arg(long)]
        view: String,
        /// Optional seed manifest (`mkdir <path>` / `touch <path>` lines, `#`
        /// comments; paths are relative to the view root) to populate the
        /// freshly created base view with.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// With a multi-instance PostgreSQL store (`--meta-store file:/path/…`)
        /// the instance (registry name) to place this view's tree on; without
        /// it the view lands on the instance carrying the fewest placed views.
        /// Single-instance stores accept the flag only if it names their one
        /// instance.
        #[arg(long = "pg-instance", value_name = "NAME")]
        pg_instance: Option<String>,
    },
    /// Create a FORK `--name` of the physical base view `--base`: an
    /// INDEPENDENT metadata tree seeded with a point-in-time copy of the base's
    /// names. Content bytes are SHARED (booked in the cross-view ledger, no
    /// copies), so a fork costs metadata only.
    ///
    /// A fork is the writable middle ground between a base view and a lens:
    /// structure (mkdir/rename/unlink/symlink/link) is allowed, but CONTENT
    /// writes (write/truncate) are refused with EROFS — uploading bytes belongs
    /// to a base view. Deleting a name removes only the fork's own metadata;
    /// the base's tree is untouched and a shared object is freed only when no
    /// view holds a name for it anymore. Removing the fork (`view rm`) deletes
    /// its tree and releases its ledger holds; content bytes the base still
    /// names stay.
    ///
    /// The base must be a PHYSICAL view (a name with a tree of its own and no
    /// definition). Both metadata backends can fork: SQLite copies a `.db`
    /// file's rows, PostgreSQL copies one `grovefs_v_<hex>` schema into another
    /// — each in one transaction.
    #[command(
        about = "Fork a base into an independent metadata tree.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta fork --name alice-exp --base alice"
    )]
    Fork {
        /// Name of the new fork (an independent metadata tree).
        #[arg(long)]
        name: String,
        /// Physical base view whose entries are copied; content bytes stay
        /// shared through the cross-view ledger.
        #[arg(long)]
        base: String,
        /// Start EMPTY (root row only) instead of copying the base's entries,
        /// to be filled by `view graft`. Same guards and the same mount
        /// semantics; only the initial row set differs.
        #[arg(long)]
        empty: bool,
    },
    /// Graft entries of view `--src` into fork `--name`, at arbitrary
    /// destinations and under new names. One plan line per entry:
    /// `<src>` (keep its basename, placed in `--dest`) or `<src>TAB<dst>`
    /// (rename and/or another directory). `--map -` reads the plan from
    /// stdin, so an external program can generate it on the fly. A directory
    /// source brings its whole subtree; the placement is all-or-nothing — one
    /// transaction on either backend — and copies no
    /// bytes: rows enter the fork, objects
    /// stay shared through the cross-view ledger. A plan that does not fit one
    /// transaction fails whole and writes nothing; re-running a partially
    /// applied plan with `--skip-existing` continues
    /// where it stopped. An existing name refuses the
    /// plan unless `--skip-existing` leaves it alone. Only a view that OWNS
    /// its tree (a fork) can receive.
    #[command(
        about = "Copy curated entries from a source into a fork.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta graft --name alice-exp --map graft.plan"
    )]
    Graft {
        /// Fork view receiving the entries.
        #[arg(long)]
        name: String,
        /// Plan file; `-` reads the plan from stdin.
        #[arg(long)]
        map: PathBuf,
        /// View whose entries are taken; defaults to the fork's base. A BASE
        /// view must be named explicitly (nothing infers which tree to lift).
        #[arg(long)]
        src: Option<String>,
        /// In-fork destination directory for lines without a TAB; defaults
        /// to the fork root. Missing directories are created.
        #[arg(long)]
        dest: Option<String>,
        /// Leave names that already exist instead of refusing the whole plan.
        #[arg(long)]
        skip_existing: bool,
    },
    /// Create/replace derived view `--name` on physical `--base`, rules read
    /// from `--rules FILE` (same verbs as `mount --layout`: hide/only/map/link).
    /// Against a `meta-server`, live mounts of that name pick the new shape up
    /// automatically (the server re-checks the definition's change ticket per op
    /// — mtime under SQLite; the
    /// client converges within its attr/dentry TTL, ≤2s — no remount). A mount
    /// resolves its definition at `M_OPEN`. Mounts of
    /// the derived name are READ-ONLY (writes belong to the base view).
    #[command(
        about = "Create or replace a read-only derived view.",
        after_help = r#"EXAMPLES
  grovefs view --meta-dir /meta define --name lens_docs \
    --base alice --rules lens.rules"#
    )]
    Define {
        /// Name of the derived view (the name clients mount, read-only).
        #[arg(long)]
        name: String,
        /// Physical base view this lens reads; its tree is never modified.
        #[arg(long)]
        base: String,
        /// Layout rules file (one verb per line; a `base` line inside it is
        /// rejected here — the base is given by `--base`).
        #[arg(long)]
        rules: PathBuf,
    },
    /// List every view: bare base trees (`meta/<name>/<name>.db` with no definition),
    /// derived views (name, base, rule counts, read-only), forks (independent
    /// trees, structure writable / content read-only) and pure aliases
    /// (name → real view, writable).
    #[command(
        about = "List base trees, derived views, forks and aliases.",
        after_help = r#"EXAMPLES
  grovefs view --meta-dir /meta list
  grovefs view --meta-dir /meta list --json | jq '.physical[]'"#
    )]
    List {
        /// Print {store, physical, defined} as JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Remove a view. WITHOUT `--base`: remove the view DEFINITION `--name`
    /// (derived, fork or alias — the file slot is shared). For derived/alias
    /// the underlying data is untouched; already-mounted clients keep their
    /// resolved lens until they remount (or the server's cache is evicted by an
    /// HA/snapshot event). Removing an alias also makes the real name mountable
    /// again for NEW mounts. For a FORK this also deletes its independent
    /// metadata tree and rebuilds the cross-view ledger — the base's names and
    /// any still-held bytes stay.
    ///
    /// WITH `--base`: remove an entire physical BASE view `--name` — its
    /// metadata tree dies, the cross-view ledger is rebuilt from the surviving
    /// trees, and every content blob no other view holds is freed through the
    /// store's data root (`--data-dir`, default `--home`; point it at the
    /// root/data-server that actually stores the objects).
    ///
    /// What "the tree" is depends on the backend the name lives in. A local
    /// store loses `meta/<name>/<name>.db` (+ WAL/SHM) and the shared `refs` ledger is
    /// recomputed from the trees that survive. With
    /// `--meta-store postgres://…` the tree is a `grovefs_v_<hex>` schema, so
    /// the command unbooks what the schema held and drops it in ONE
    /// transaction (PostgreSQL's DDL is transactional), then frees the same
    /// way. The report states both halves separately (names gone / objects
    /// unbooked / objects not found under this data root), so "the tree is
    /// gone" and "the bytes were reclaimed" are never collapsed into one
    /// number.
    ///
    /// Guards: a governed name needs the plain `view rm` first, and an actively
    /// mounted `grovefs:<name>` is refused. Other machines' live mounts of the
    /// same meta-server are not visible here — stop them first.
    #[command(
        about = "Remove a view definition, or a whole base tree.",
        after_help = r#"EXAMPLES
  Remove the definition only (the base schema is untouched):
    grovefs view --meta-dir /meta rm --name lens_docs
  Remove a whole base tree and its unreferenced bytes:
    grovefs view --meta-dir /meta rm --base --name alice --data-dir /meta"#
    )]
    Rm {
        /// View name: a definition slot by default, or the physical base tree
        /// with `--base`.
        #[arg(long)]
        name: String,
        /// Also remove the PHYSICAL base tree and reclaim unreferenced bytes
        /// (see the long help; default removes only a definition slot).
        #[arg(long)]
        base: bool,
        /// Data root holding `blocks/` for `--base` byte reclaim (same option
        /// name and fallback as `grovefs import --data-dir`).
        #[arg(long = "data-dir", value_name = "DIR[,DIR]")]
        data_root: Option<PathBuf>,
    },
    /// Create/replace a PURE ALIAS `--name` for physical `--real`: name
    /// governance (vs `define`'s shape governance). No rules, so the alias
    /// shows the FULL tree and stays WRITABLE — it simply is the real view
    /// under a public name. A `meta-server` then refuses mounts of the real
    /// name itself: while the alias exists the real name is not a mount entry
    /// point, so public names can rotate freely without exposing the storage
    /// identity.
    /// Chained aliases, aliases over derived views, and aliasing an
    /// already-governed real name are rejected at this command.
    ///
    /// To ROTATE a leaked public name in one gapless step, add `--retire
    /// <old-alias>`: the new alias is written before the old one is removed,
    /// so the real name stays governed throughout (never briefly mountable by
    /// its own name) — the leaker's old name stops resolving, legitimate users
    /// just remount under the new name.
    #[command(
        about = "Create or rotate a writable public alias.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta alias --name bias_public --real alice"
    )]
    Alias {
        /// Public alias name clients mount.
        #[arg(long)]
        name: String,
        /// Physical view the alias resolves to; the real name stops being a
        /// mount entry point while the alias exists.
        #[arg(long)]
        real: String,
        /// Atomically retire a previous alias of the same real view (name
        /// rotation without a governance gap).
        #[arg(long)]
        retire: Option<String>,
    },
    /// Export ONE view's METADATA TREE to a portable line-delimited JSON file
    /// (`--out`): a header plus one node row per line (ids, names, inode
    /// numbers, timestamps, `data_ref` keys). Content bytes are NOT included —
    /// blobs are referenced by `data_ref`, whose on-disk bucket path is a pure
    /// function of the key, so copying the store's `blocks/` tree alongside
    /// the export reproduces the view on another machine. No host path, device
    /// id or hostname is stored. Virtual names (alias/derived) have no tree of
    /// their own and are refused — export the physical base they resolve to.
    /// With `--meta-store postgres://…` the same command reads that view's PG
    /// schema instead of a local tree; a name the schema set never provisioned
    /// is refused outright, so no empty dump is written for it.
    #[command(
        about = "Export one view's metadata tree as line-delimited JSON.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta export --view alice --out alice.jsonl"
    )]
    Export {
        /// Physical view whose metadata tree is written (aliases and derived
        /// names have no tree of their own and are refused).
        #[arg(long)]
        view: String,
        /// Destination `.jsonl` file.
        #[arg(long)]
        out: PathBuf,
        /// Overwrite `--out` if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Import a metadata tree written by `view export` into a NEW local base
    /// tree (`meta/<view>/<view>.db`), preserving ids/inode numbers (hard links stay
    /// hard links) and rebuilding the cross-view ledger afterwards. The target
    /// must not be governed by a definition; an existing tree needs
    /// `--replace`, and every mount/meta-server using this store must be
    /// stopped first. Content bytes are not in the file: copy the referenced
    /// `blocks/` tree to the target store. Add `--as-fork <base>` to record
    /// `fork <base>` after a successful import (the imported tree then keeps
    /// fork semantics: structure writable, content EROFS). Under
    /// `--meta-store postgres://…` the import instead OPENS that view's schema
    /// in the database — the only tree-building entry besides `view create` —
    /// and `--as-fork` records the definition there too (the tree is the schema
    /// the dump just filled; if writing `fork <base>` fails, the schema is
    /// dropped again).
    #[command(
        about = "Import exported metadata as a new base view.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta import --view alice --in alice.jsonl"
    )]
    Import {
        /// Name of the NEW base view to create from the dump.
        #[arg(long)]
        view: String,
        /// Dump file written by `view export`.
        #[arg(long = "in")]
        input: PathBuf,
        /// Replace an existing tree db (offline migration only).
        #[arg(long)]
        replace: bool,
        /// Record the imported tree as a fork of this physical base view
        /// (the base tree must already exist in the target store).
        #[arg(long = "as-fork")]
        as_fork: Option<String>,
    },
    /// Move a view's tree to another instance of a multi-instance PostgreSQL
    /// store (`--meta-store file:/path/…`): snapshot the source schema, build
    /// the copy on `--pg-instance`, verify it against the dump's own header,
    /// then flip the placement row (the atomic cutover) and retire the source
    /// (schema + its ledger holds, one transaction) unless `--keep-source`
    /// leaves it as a rollback point. Metadata only — content bytes stay in
    /// the data domain they live in (the server-side gc unions ALL instances'
    /// ledgers, so nothing is reclaimed out from under the moved view); mounts
    /// keep resolving the same data servers through the manager. Refuses a view mounted on
    /// this machine; quiesce other machines yourself.
    #[command(
        about = "Move a view's tree to another PostgreSQL instance.",
        after_help = "EXAMPLES\n  grovefs view --meta-dir /meta move --view alice --pg-instance pg2"
    )]
    Move {
        /// View whose tree is moved to the target instance.
        #[arg(long)]
        view: String,
        /// Target instance (registry name). A `--keep-source` move can be
        /// rolled back by re-running the command with the original home.
        #[arg(long = "pg-instance", value_name = "NAME")]
        pg_instance: String,
        /// Leave the source schema on its instance as a rollback point
        /// instead of dropping it (and unbooking its ledger holds) after the
        /// cutover succeeds.
        #[arg(long)]
        keep_source: bool,
    },
    /// Show or set a view's PostgreSQL connection-pool bounds — the WARM floor
    /// (connections held for the whole engine lifetime) and the RETAIN CEILING
    /// (burst connections kept before the idle reaper sheds them), per lane.
    /// With no flags it shows what the view declares and what the process
    /// defaults to; `--write FLOOR[:MAX]` / `--read FLOOR[:MAX]` / `--idle-secs
    /// N` record overrides in the view's OWN metadata tree (so a `view move`
    /// carries them along), and `--inherit` removes them again.
    ///
    /// This is how a fork-heavy fleet stops paying 12 connections per tree:
    /// a branch that is read and structure-written but never content-written
    /// (content writes are EROFS on a fork) needs a writer floor of 1–2, not 8,
    /// and still gets 8 during an import burst. Bounds take effect on the
    /// view's NEXT engine open — mounted sessions pin the engine and its
    /// connections hold session-scoped prepared statements, so nothing is
    /// yanked out from under a live mount.
    #[command(
        about = "Show or set per-view PostgreSQL pool bounds.",
        after_help = r#"EXAMPLES
  grovefs view --meta-dir /meta pool --view alice
  grovefs view --meta-dir /meta pool --view alice --write 2:8 --read 2:8"#
    )]
    Pool {
        /// View whose pool bounds are read or set.
        #[arg(long)]
        view: String,
        /// Writer lane as `FLOOR[:MAX]` (e.g. `2:8`); bare `N` keeps the
        /// historical floor==ceiling shape.
        #[arg(long, value_name = "FLOOR[:MAX]")]
        write: Option<String>,
        /// Reader lane as `FLOOR[:MAX]`.
        #[arg(long, value_name = "FLOOR[:MAX]")]
        read: Option<String>,
        /// Per-view idle window (seconds) before a burst connection above the
        /// floor is reaped; `0` never reaps.
        #[arg(long = "idle-secs", value_name = "SECS")]
        idle_secs: Option<u64>,
        /// Drop this view's overrides and follow the meta-server's process-wide
        /// bounds again.
        #[arg(long)]
        inherit: bool,
    },
}

#[derive(Subcommand)]
enum ApikeyAction {
    /// Mint a new key labeled `--label` and print the full plaintext
    /// `<id>:<secret>`. The store keeps two things about it: the SHA-256 digest
    /// authentication consults, and a wrapped copy for recovery — so
    /// `apikey secret --id <id>` reads the credential back later (rotate is what
    /// you reach for when the row predates that copy and has nothing to read).
    /// Give this to the agent/client; it presents it as
    /// `apiKey` in the MCP `initialize` params, or as `mount --api-key` for a
    /// `meta-server`/`data-server` started with `--require-api-key`.
    #[command(
        about = "Mint a key and print <id>:<secret> once.",
        after_help = r#"EXAMPLES
  A mount-only key:
    grovefs apikey --meta-dir /meta create --label analyst --for mount
  An MCP key owned by a person, with a note and a deadline:
    grovefs apikey --meta-dir /meta create --label bot --for mcp \
      --owner alice --note "CI runner" --ttl 720h"#
    )]
    Create {
        /// Human name shown and searched in `apikey list` (not unique; the id
        /// is the identity).
        #[arg(long)]
        label: String,
        /// Which entrance the key may open: `mount` (a `--require-api-key`
        /// meta/data-server), `mcp` (`grovefs mcp --require-api-key`), `admin`
        /// (the governance port, `meta-server --control-listen` — this is the
        /// DATABASE-REPLACEMENT credential, and it also admits mount+mcp) or
        /// `any` (mount+mcp, the default, and what every key minted before
        /// scopes existed carries — `any` deliberately does NOT imply `admin`).
        /// Narrow it so a mount credential cannot be replayed against the
        /// governance API, and an agent key cannot mount data.
        #[arg(long, default_value = "any", value_parser = parse_scope_arg)]
        r#for: String,
        /// Bind the key to ONE physical view: it can only open `--view NAME`
        /// (any other view's M_OPEN is refused). Omit for a system-level key
        /// that may connect to any view. This is the credential-level mechanism
        /// for the common "this client only touches that tree" case.
        #[arg(long)]
        view: Option<String>,
        /// Make the key read-only: the server refuses every write-intent op
        /// (create/write/truncate/remove) from it, so an accidental `mount`
        /// without `-o ro` still cannot change data. Deleting the last name in
        /// a read-only view never frees the underlying blob either (only a
        /// read-write key on the BASE view does real GC — see `view create`).
        #[arg(long)]
        ro: bool,
        /// The ACCOUNT this key belongs to (a person or a service). One account
        /// holds SEVERAL keys, which is what makes rotation and per-device
        /// revocation real: `revoke --id` kills one key while the account keeps
        /// working — the opposite of a shared database password, where every
        /// holder must be re-taught a new secret. Also what the control-plane
        /// audit attributes a call to. Required for `--for admin`.
        #[arg(long)]
        owner: Option<String>,
        /// Attach a REMARK — free text like "kitchen laptop, staging box, hand
        /// over at the end of March" (`apikey update --note` edits it later,
        /// `list --note` searches it). The field that turns a registry of
        /// hundreds from a guessing game into a list.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Make the key expire after this long: `45m`, `24h`, `7d`, `365d`
        /// (default: never). The deadline is stored and enforced SERVER-side —
        /// a key past it fails its next handshake, and a mount already running
        /// on it is refused on its next op (≤1s) — so a lost or parked laptop
        /// stops being a credential without anyone remembering to revoke it.
        /// Extend or remove it later with `apikey update --ttl`.
        #[arg(long, value_name = "DUR")]
        ttl: Option<String>,
    },
    /// List every key (id, masked key, owner, label, scope, view binding,
    /// ro/rw, created/last-used/expires/revoked, remark) — never the secret
    /// itself, only the 8-hex-digit fingerprint that identifies one row (see
    /// `apikey secret` to read a stored credential back, or `apikey rotate` to
    /// mint a new secret for a row that has none). Add
    /// `--view NAME` to show only the keys that can open that view (a pure alias
    /// also matches keys bound to its real view). `--label`/`--owner`/`--note`
    /// match case-insensitively as substrings, `--scope` narrows to one entrance
    /// (`any`/`mount`/`mcp`/`admin`) — for a registry too big to scan by eye.
    #[command(
        about = "List keys, filtered; never prints secrets.",
        after_help = r#"EXAMPLES
  grovefs apikey --meta-dir /meta list
  grovefs apikey --meta-dir /meta list --view photos --scope mount
  grovefs apikey --meta-dir /meta list --json | jq '.[].id'"#
    )]
    List {
        /// Only keys that can open this view: an exact `--view` binding, or
        /// (when NAME is a pure alias) keys bound to its real view. Unbound
        /// system keys admit every view and are summarized separately.
        #[arg(long)]
        view: Option<String>,
        /// Only keys whose label contains this (case-insensitive).
        #[arg(long)]
        label: Option<String>,
        /// Only keys owned by this account (case-insensitive substring).
        #[arg(long)]
        owner: Option<String>,
        /// Only keys whose REMARK contains this (case-insensitive substring) —
        /// the free-text field, so `--note staging` finds the staging keys.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Only keys with this exact entrance scope: `any`, `mount`, `mcp` or
        /// `admin`.
        #[arg(long, value_name = "SCOPE", value_parser = parse_scope_arg)]
        scope: Option<String>,
        /// Print the selected rows as a JSON array (no table, no ANSI) for
        /// piping into jq or another program.
        #[arg(long)]
        json: bool,
    },
    /// Revoke the key with id `--id` (short id prefix accepted). Revoked keys
    /// fail authentication but stay listed so past audit rows keep attribution.
    #[command(
        about = "Revoke a key; past audit rows keep attribution.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta revoke --id 8f3a…e21c"
    )]
    Revoke {
        /// Key id as printed by `apikey list`; a unique prefix is enough.
        #[arg(long)]
        id: String,
    },
    /// Replace the secret of an ACTIVE key in place (`--id`, short id prefix
    /// accepted, like `revoke`) and print the NEW `<id>:<secret>` — the repair
    /// when the store kept no recoverable copy of the old one (`apikey secret`
    /// reads back what it did keep). Rotate stays the verb that KILLS a lost
    /// credential's secret while sparing the revoke-repair dance: the id, label,
    /// scope, view binding, ro flag, owner and audit history all survive
    /// untouched. The old secret fails its next handshake; a session already
    /// established on it lives until it reconnects (rotation replaces a
    /// credential, only revocation kills one now). A revoked id is refused —
    /// rotate never half-resurrects a key.
    #[command(
        about = "Replace an active key's secret in place.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta rotate --id 8f3a…e21c"
    )]
    Rotate {
        /// Key id as printed by `apikey list`; a unique prefix is enough.
        #[arg(long)]
        id: String,
    },
    /// Read a credential's stored copy back: the full `<id>:<secret>` an operator
    /// can hand to a client that lost it (`--id`, short id prefix accepted, like
    /// `revoke`). The credential's OWNER or a superuser answers for it, and a
    /// read taken through a meta-server is written to ITS audit trail with the id
    /// it read — which is the sentence that makes this verb safe to offer at all.
    /// A read of the local store instead records nothing, because there is no
    /// actor left to attribute to: whoever can open that file (or holds the PG
    /// credential) already has what it holds.
    ///
    /// A REVOKED row is still readable, deliberately: revocation stops a
    /// credential authenticating, it does not erase the record, and the person
    /// who needs the string back is often the one cleaning up after a revoke.
    /// Three answers are not this one: a row minted before the copy existed,
    /// a store whose wrapping material is on another node, and a row whose
    /// wrapped copy no longer matches its digest (then refused, not printed).
    /// Each says itself; `apikey rotate` fixes the first.
    #[command(
        about = "Read back a stored credential copy.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta secret --id 8f3a…e21c"
    )]
    Secret {
        /// Key id as printed by `apikey list`; a unique prefix is enough.
        #[arg(long)]
        id: String,
    },
    /// Flip the read-only flag of an ACTIVE key IN PLACE (short id prefix
    /// accepted, like `revoke`). Default sets it read-only; `--rw` restores
    /// read-write. This moves only the server-side write verdict — it never
    /// touches liveness or authentication — and a live mount picks the new
    /// verdict up on its next re-check tick (≤1s), no reconnect, no remount
    /// (the same key's reads keep flowing while it is `ro`). A revoked id is
    /// refused: a revoke is permanent and set-ro never half-undoes it.
    #[command(
        about = "Flip a key between read-only and read-write.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta set-ro --id 8f3a…e21c --ro\n  grovefs apikey --meta-dir /meta set-ro --id 8f3a…e21c --rw"
    )]
    SetRo {
        /// Key id as printed by `apikey list`; a unique prefix is enough.
        #[arg(long)]
        id: String,
        /// Flip to read-only (the default when neither `--ro` nor `--rw` is given).
        #[arg(long)]
        ro: bool,
        /// Flip to read-write (mutually exclusive with `--ro`).
        #[arg(long, conflicts_with = "ro")]
        rw: bool,
    },
    /// Show the most recent `--limit` (default 20) MCP call-audit rows.
    #[command(
        about = "Show recent call-audit rows.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta calls\n  grovefs apikey --meta-dir /meta calls --limit 100\n  grovefs apikey --meta-dir /meta calls --json | jq '.[] | select(.ok == false)'"
    )]
    Calls {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print rows as a JSON array (no table, no ANSI).
        #[arg(long)]
        json: bool,
    },
    /// Delete call-audit rows older than `--before DUR` (`45m`, `24h`, `90d`,
    /// `365d`) and report how many went. The trail is append-forever by default
    /// — one row per governance call, per MCP tool call, per mount handshake —
    /// so this is the on-demand half of retention; `meta-server
    /// --audit-retention 90d` runs the same cut in the background. `--dry-run`
    /// counts what would go without touching it, which is the question to ask
    /// before destroying history. A SUPERUSER verb, and deliberately NOT a
    /// console button: the account whose actions are recorded here is not the
    /// account allowed to trim them.
    #[command(
        about = "Delete audit rows older than a window.",
        after_help = r#"EXAMPLES
  Preview, then delete rows older than 30 days:
    grovefs apikey --meta-dir /meta purge --before 720h --dry-run
    grovefs apikey --meta-dir /meta purge --before 720h"#
    )]
    Purge {
        /// Drop rows older than this window, measured from now.
        #[arg(long, value_name = "DUR")]
        before: String,
        /// Report the count without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Edit the BOOKKEEPING of an active key in place (short id prefix accepted,
    /// like `revoke`): its remark (`--note TEXT`, `--note ""` clears it) and its
    /// deadline (`--ttl 7d` from now, `--ttl never` to remove it). The usual
    /// reason is an expired key: it authenticates nowhere until it is given a
    /// new `--ttl`. Nothing else about a key is editable, deliberately — `--for`
    /// and `--view` decide what a credential can REACH, and moving them under a
    /// running mount would make the binding a suggestion; mint a new key (or
    /// `set-ro`, which only moves the write verdict). A revoked id is refused:
    /// revoke is final.
    #[command(
        about = "Edit a key's note or expiry.",
        after_help = "EXAMPLES\n  grovefs apikey --meta-dir /meta update --id 8f3a…e21c \\\n    --note \"rotated 2026-12\" --ttl 720h"
    )]
    Update {
        /// Key id as printed by `apikey list`; a unique prefix is enough.
        #[arg(long)]
        id: String,
        /// Replace the remark (an empty value clears it).
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// New lifetime measured from NOW (`45m`, `24h`, `7d`, `365d`) or
        /// `never`.
        #[arg(long, value_name = "DUR")]
        ttl: Option<String>,
    },
}

/// The headless half of the web console: the same account and delegation verbs
/// the page offers, for the cases a browser is wrong for (an install script, an
/// operator over SSH, `--json` for a tool). They run as the OPERATOR — the admin
/// key with no account attached — which is the plane's oldest and widest
/// credential, so these commands are how a deployment's first accounts get made
/// before anyone can sign in.
///
/// There is deliberately no `--meta-dir`/`--meta-store` form of them: the
/// meta-server owns its account store, and rules like "the last enabled admin
/// cannot be demoted" and the password policy live in front of that store. A CLI
/// that opened the table directly would be a second implementation of them, and
/// the one nobody audits.
#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebAction {
    /// Manage sign-in accounts (`--control-server` + an admin key required).
    #[command(
        subcommand,
        about = "Manage console sign-in accounts.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account list"
    )]
    Account(WebAccountAction),
    /// Manage which BASE views an account governs — its delegations. An account
    /// with no grant manages nothing: `admin` is the role that ignores grants, so
    /// `create` alone never widens anyone's reach.
    #[command(
        subcommand,
        about = "Manage per-account base-view delegations.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET grant list --account alice"
    )]
    Grant(WebGrantAction),
}

#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebAccountAction {
    /// Create an account named `--account` with the password in
    /// `--password-file` (or on stdin with `--password-stdin`) and role
    /// `--role user|admin`. A password never goes on a command line: argv is
    /// visible to every local process and lands in the shell history. The
    /// minimum length and the "not the account name" rule are the server's
    /// (`webauth::check_password`) and are reported verbatim.
    #[command(
        about = "Create a console account.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account create --account alice \\\n    --password-file pass.txt --role user"
    )]
    Create {
        /// Sign-in name to create (the server enforces its grammar).
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        /// File holding the initial password; keep it mode 600. A secret on
        /// argv is visible to every local process and lands in shell history.
        #[arg(long = "password-file", value_name = "PATH")]
        password_file: Option<PathBuf>,
        /// Read the password from stdin — `pass generate … | grovefs web account
        /// create --account alice --password-stdin`, so the secret appears in no
        /// argument list and no file.
        #[arg(long, conflicts_with = "password_file")]
        password_stdin: bool,
        /// `user` (default): governs only the base views it is granted. `admin`:
        /// governs the deployment, grants included.
        #[arg(long, default_value = "user", value_parser = parse_account_role)]
        role: String,
    },
    /// List accounts (name, role, created/last sign-in, disabled, granted base
    /// views) — never a password digest.
    #[command(
        about = "List accounts and their delegations.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account list"
    )]
    List,
    /// Set a new password for `--account` from `--password-file` /
    /// `--password-stdin`. An active browser session is NOT ended by this (it is
    /// the gateway's cookie table); `disable` is the verb that stops someone now.
    #[command(
        about = "Set a new sign-in password.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account passwd --account alice \\\n    --password-stdin"
    )]
    Passwd {
        /// Account whose password is replaced.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        /// File holding the new password; keep it mode 600.
        #[arg(long = "password-file", value_name = "PATH")]
        password_file: Option<PathBuf>,
        /// Read the new password from stdin instead of a file.
        #[arg(long, conflicts_with = "password_file")]
        password_stdin: bool,
    },
    /// Change an account's role. Demoting the LAST enabled admin is refused —
    /// that is the lock-out this guard exists for; create and verify a second
    /// admin first.
    #[command(
        about = "Change an account's role.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account role --account alice --role admin"
    )]
    Role {
        /// Account whose role changes.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        /// `user` (delegated base views only) or `admin` (the whole deployment).
        #[arg(long, value_name = "user|admin", value_parser = parse_account_role)]
        role: String,
    },
    /// Let `--account` sign in again (undo `disable`). Grants are untouched by
    /// either verb, so re-enabling returns exactly the reach it had.
    #[command(
        about = "Allow an account to sign in again.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account enable --account alice"
    )]
    Enable {
        /// Account to allow signing in again.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
    /// Stop `--account` signing in. Takes effect on that user's NEXT call, not at
    /// the end of a page's session, because the server re-reads the principal per
    /// request. Its api keys are NOT revoked (a disable is reversible; killing a
    /// running mount is not) — `grovefs apikey revoke` is that verb.
    #[command(
        about = "Stop an account signing in.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account disable --account alice"
    )]
    Disable {
        /// Account to stop from signing in.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
    /// Delete `--account` and its delegations. Its api keys stay alive and
    /// unattributed on purpose, so a delete cannot silently stop a production
    /// mount; the reply counts them so the next command is obvious.
    #[command(
        about = "Delete an account and its delegations.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET account rm --account alice"
    )]
    Rm {
        /// Account to delete (its api keys are kept and unattributed).
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
    },
}

#[cfg(feature = "web")]
#[derive(Subcommand)]
enum WebGrantAction {
    /// Give `--account` management of base view `--base` — and of everything
    /// derived from it (lenses, aliases and forks resolve outward to their base),
    /// which is why this names a BASE and not a leaf. A `--base` that is a lens or
    /// a fork is stored against the base it resolves to and says so.
    #[command(
        about = "Delegate one base view to an account.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET grant add --account alice --base photos"
    )]
    Add {
        /// Account receiving the delegation.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        /// Base view to delegate; it carries every lens, fork and alias
        /// derived from it.
        #[arg(long, value_name = "VIEW")]
        base: String,
    },
    /// Withdraw one delegation. Effective on that account's next call.
    #[command(
        about = "Withdraw one delegation.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET grant rm --account alice --base photos"
    )]
    Rm {
        /// Account losing the delegation.
        #[arg(long, value_name = "ACCOUNT")]
        account: String,
        /// Delegated base view to withdraw.
        #[arg(long, value_name = "VIEW")]
        base: String,
    },
    /// List delegations (`--account` to narrow to one person).
    #[command(
        about = "List delegations.",
        after_help = "EXAMPLES\n  grovefs web --control-server host:50054 \\\n    --api-key ADMIN_ID:SECRET grant list --account alice"
    )]
    List {
        /// Narrow the list to one account's delegations.
        #[arg(long, value_name = "ACCOUNT")]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum RefsAction {
    /// Recompute `meta/_control/refs.db` from the surviving view trees: for every
    /// `data_ref`, count the DISTINCT views still holding a name for it and
    /// rewrite the holder rows. Clears holds stranded by an out-of-band view-db
    /// removal (the only over-count source) so their blobs become freeable again.
    /// A missing/empty ledger is (re)built; healthy stores need no rebuild.
    #[command(
        about = "Rebuild the ledger from surviving view trees.",
        after_help = "EXAMPLES\n  grovefs refs --meta-dir /meta rebuild"
    )]
    Rebuild,
}

#[derive(Subcommand)]
enum LayoutAction {
    /// Report the layout one metadata root uses, the trees it holds and where
    /// each file lives. Read-only, safe against a live store.
    #[command(
        about = "Show the store layout and tree locations.",
        after_help = "EXAMPLES\n  grovefs layout --home ~/grove show"
    )]
    Show,
    /// Record where NEW family members of a base view (fork trees,
    /// derived/alias definition files) land: `nested` inside the base's
    /// directory (the default), `grouped` under its `forks/`/`views/`
    /// separators, or `flat` at the top level (the pre-v3 shape). Existing
    /// files never move — every read resolves by scanning — so flipping the
    /// policy is safe against a live store.
    #[command(
        about = "Set the family-placement policy for new forks and definitions.",
        after_help = "EXAMPLES\n  grovefs layout --home ~/grove set-family nested\n  grovefs layout --home ~/grove set-family flat"
    )]
    SetFamily {
        /// `flat`, `nested` or `grouped`.
        mode: String,
    },
    /// Move a FLAT pre-v2 store into the per-tree layout: checkpoint each db so
    /// the file that moves is self-contained, then `rename` it into its new
    /// home. Refuses while the store has a live mount or a local grovefs process
    /// using it (pass `--dry-run` to see the plan without touching anything).
    #[command(
        about = "Migrate a flat pre-v2 store in place.",
        after_help = r#"EXAMPLES
  grovefs layout --home ~/grove migrate --dry-run
  grovefs layout --home ~/grove migrate"#
    )]
    Migrate {
        /// Print what would move and stop: no checkpoint, no rename.
        #[arg(long)]
        dry_run: bool,
    },
}

/// One file the migrator has to place: `dest` is where it goes, `object` groups
/// the sidecars that belong to one db so they travel together, and `sidecar`
/// marks the wrap material a checkpoint is allowed to swallow.
struct LayoutMove {
    object: String,
    src: PathBuf,
    dest: PathBuf,
    sidecar: bool,
}

/// Classify the flat files a pre-v2 `meta/` holds into moves. Everything here
/// is name arithmetic — the same rule the layout itself is built from, so the
/// migrator cannot invent a location the runtime would not look in.
fn layout_plan(base: &Path) -> Vec<LayoutMove> {
    let mut out = Vec::new();
    for name in grovefs::meta::legacy_flat_entries(base) {
        let src = grovefs::meta::meta_dir(base).join(&name);
        // The wrap material is a plain file, not a db: it goes to the control
        // slot under its own name.
        if name == grovefs::meta::STORE_KEY_FILE {
            out.push(LayoutMove {
                object: "store.key".to_string(),
                src,
                dest: grovefs::meta::control_dir(base).join(grovefs::meta::STORE_KEY_FILE),
                sidecar: false,
            });
            continue;
        }
        // Longest suffix first: `api_keys.db-wal` must not classify as
        // `api_keys.db` + a stray `-wal`.
        let (stem, suffix) = [
            (".db.push.tmp", ".push.tmp"),
            (".db-wal", "-wal"),
            (".db-shm", "-shm"),
            (".db", ""),
        ]
        .into_iter()
        .find_map(|(sfx, keep)| name.strip_suffix(sfx).map(|st| (st.to_string(), keep)))
        .unwrap_or((name.clone(), ""));
        let file = format!("{stem}.db{suffix}");
        let dest = if grovefs::meta::CONTROL_DB_STEMS.contains(&stem.as_str()) {
            grovefs::meta::control_dir(base).join(&file)
        } else {
            grovefs::meta::view_dir(base, &stem).join(&file)
        };
        out.push(LayoutMove {
            object: stem,
            src,
            dest,
            sidecar: !suffix.is_empty(),
        });
    }
    out.sort_by(|a, b| a.dest.cmp(&b.dest));
    out
}

/// `grovefs layout show`: what a store IS, in the words an operator needs
/// before deciding to migrate it (and the answer to "where do this view's files
/// live?", which is the question that motivated the per-tree directory).
fn layout_show(base: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let flat = grovefs::meta::legacy_flat_entries(base);
    let meta = grovefs::meta::meta_dir(base);
    if !meta.is_dir() {
        return Err(format!("no metadata directory {}", meta.display()).into());
    }
    // A fixed label column keeps every path in one vertical line. `ui::pad`
    // measures in terminal cells, so a styled label never eats its padding.
    let row = |label: &str, value: String| {
        let lead = ui::pad(&ui::muted(label), 16, ui::Align::Left);
        let w = ui::width();
        let avail = if w == usize::MAX {
            usize::MAX
        } else {
            w.saturating_sub(17).max(8)
        };
        for (i, line) in ui::wrap(&value, avail).iter().enumerate() {
            if i == 0 {
                println!("{lead}{line}");
            } else {
                println!("{}{line}", " ".repeat(17));
            }
        }
    };
    row("metadata root:", base.display().to_string());
    if flat.is_empty() {
        let policy = grovefs::meta::family_policy(base);
        row(
            "layout:",
            format!(
                "{} {}",
                ui::strong("v3"),
                ui::muted("— one directory per view tree, control plane in meta/_control")
            ),
        );
        row(
            "family mode:",
            format!(
                "{} {}",
                ui::strong(policy.as_str()),
                ui::muted(if policy == grovefs::meta::FamilyLayout::Flat {
                    "— new family members land at the top level (meta/.family)"
                } else {
                    "— new family members land inside their base's directory (meta/.family)"
                })
            ),
        );
        let trees = grovefs::meta::list_view_trees(base);
        row("view trees:", ui::strong(&trees.len().to_string()));
        for t in &trees {
            let name = ui::pad(&ui::strong(t), 24, ui::Align::Left);
            let path = grovefs::meta::view_db_path(base, t).display().to_string();
            let w = ui::width();
            if w != usize::MAX && 2 + ui::display_width(&name) + ui::display_width(&path) > w {
                println!("  {name}");
                println!("    {}", ui::muted(&path));
            } else {
                println!("  {name}{}", ui::muted(&path));
            }
        }
    } else {
        row(
            "layout:",
            format!(
                "{} {}",
                ui::strong("v1 FLAT (pre-v2)"),
                ui::muted("— no command reads this shape; migrate it")
            ),
        );
        let plan = layout_plan(base);
        row("flat files:", ui::strong(&plan.len().to_string()));
        for m in &plan {
            let src = m.src.display().to_string();
            let dest = m.dest.display().to_string();
            let line = format!("{src}  ->  {dest}");
            let w = ui::width();
            if w != usize::MAX && 2 + ui::display_width(&line) > w {
                println!("  {src}");
                println!("    -> {dest}");
            } else {
                println!("  {line}");
            }
        }
    }
    for (label, p) in [
        ("definitions", grovefs::meta::defs_dir(base)),
        ("control plane", grovefs::meta::control_dir(base)),
        ("transfer spool", grovefs::meta::xfer_dir(base)),
    ] {
        let n = std::fs::read_dir(&p)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        row(
            &format!("{label}:"),
            format!(
                "{} {}",
                ui::muted(&p.display().to_string()),
                ui::muted(&format!("({})", ui::plural(n, "entry", "entries")))
            ),
        );
    }
    Ok(())
}

/// `grovefs layout migrate`: the one-shot offline move from the flat layout to
/// the per-tree layout. Idempotent, and deliberately NOT a rewrite of anything
/// but file locations — the holder ledger counts content keys, and a view's
/// inode numbers and `data_ref` keys live inside its own db, so no bookkeeping
/// depends on where the db sat.
fn layout_migrate(base: &Path, dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let flat = grovefs::meta::legacy_flat_entries(base);
    if flat.is_empty() {
        ui::success(&format!(
            "{} already uses the v2 layout — nothing to do",
            grovefs::meta::meta_dir(base).display()
        ));
        return Ok(());
    }
    let plan = layout_plan(base);
    // Legacy tree names, for the mount gate: `gc_live_mounts` matches a
    // `grovefs:` entry by VIEW NAME, and under this layout the names come from
    // the flat files, not from the (still empty) tree directories.
    let mut names: Vec<String> = Vec::new();
    for m in &plan {
        if !grovefs::meta::CONTROL_DB_STEMS.contains(&m.object.as_str())
            && m.object != "store.key"
            && !names.contains(&m.object)
        {
            names.push(m.object.clone());
        }
    }
    if dry_run {
        ui::info(&format!(
            "dry run: {} in {} would move into the v2 layout ({}, control plane into meta/_control)",
            ui::plural(plan.len(), "flat file", "flat files"),
            grovefs::meta::meta_dir(base).display(),
            ui::plural(names.len(), "view tree", "view trees"),
        ));
        for m in &plan {
            let src = m.src.display().to_string();
            let dest = m.dest.display().to_string();
            let line = format!("{src}  ->  {dest}");
            let w = ui::width();
            if w != usize::MAX && 2 + ui::display_width(&line) > w {
                println!("  {src}");
                println!("    -> {dest}");
            } else {
                println!("  {line}");
            }
        }
        ui::info("nothing was moved; run without --dry-run to migrate");
        return Ok(());
    }
    // The quiesce doors are gc's, for the same reason: a live writer would keep
    // its handle on the OLD inode, so the moved file would be authoritative for
    // nobody and the running server would keep answering from the copy that is
    // no longer where the layout says the tree lives.
    let live = gc_live_mounts(base, &[base.to_path_buf()], &names);
    if !live.is_empty() {
        return Err(format!(
            "layout migrate refused: this store has live mounts — {}; unmount them first (the layout is a file-location contract, so it cannot change under a writer)",
            live.join("; ")
        )
        .into());
    }
    let users = local_store_users(base, &[base.to_path_buf()]);
    if !users.is_empty() {
        return Err(format!(
            "layout migrate refused: this store is in use by local grovefs process(es) — {}; stop the meta-server/data-server and any mount first",
            users.join("; ")
        )
        .into());
    }
    // Fold each db's WAL into its main file BEFORE the move, so what travels is
    // a self-contained database. A checkpoint that fails (a garbage or corrupt
    // file) is not a reason to abandon the operator's data: the sidecars are
    // then carried along untouched instead of being dropped, which is the
    // conservative direction.
    let mut moved = 0usize;
    let mut absorbed = 0usize;
    for m in &plan {
        if let Some(dir) = m.dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let foldable = m.dest.to_string_lossy().ends_with(".db") && m.src.is_file();
        let folded = foldable
            && rusqlite::Connection::open(&m.src)
                .and_then(|c| {
                    c.execute_batch("PRAGMA busy_timeout=5000; PRAGMA wal_checkpoint(TRUNCATE);")
                })
                .is_ok();
        if m.src == m.dest {
            continue;
        }
        // Folding a WAL into its db deletes the `-wal`/`-shm` that were just
        // absorbed, so a sidecar can legitimately be gone by the time its own
        // turn comes — its content already travels inside the moved `.db`.
        // A missing MAIN db is a different matter: nothing else accounts for
        // those bytes, so that stays a hard error.
        if m.sidecar && !m.src.exists() {
            absorbed += 1;
            continue;
        }
        move_into(&m.src, &m.dest)?;
        moved += 1;
        if folded {
            if let Some(n) = m.src.file_name().and_then(|n| n.to_str()) {
                for sfx in ["-wal", "-shm", ".push.tmp"] {
                    let leftover = grovefs::meta::meta_dir(base).join(format!("{n}{sfx}"));
                    if leftover.is_file() {
                        let _ = std::fs::remove_file(&leftover);
                    }
                }
            }
        }
    }
    let left = grovefs::meta::legacy_flat_entries(base);
    if !left.is_empty() {
        return Err(format!(
            "migration incomplete: {} still in {} ({} moved) — run `grovefs layout --meta-dir {} migrate` again; it finishes where it stopped",
            ui::plural(left.len(), "flat file", "flat files"),
            grovefs::meta::meta_dir(base).display(),
            moved,
            base.display(),
        )
        .into());
    }
    ui::success(&format!(
        "migrated {} into the v2 layout: {} now live in meta/<view>/<view>.db, the control plane in meta/_control/{}",
        ui::plural(moved, "flat file", "flat files"),
        ui::plural(names.len(), "view tree", "view trees"),
        if absorbed == 0 {
            String::new()
        } else {
            format!(
                " ({} needed no move: the checkpoint had already folded them into their db)",
                ui::plural(absorbed, "sidecar", "sidecars")
            )
        }
    ));
    ui::note(
        "the ledger was NOT rewritten (it books content keys, not paths) — but every OTHER node holding a copy of this meta dir must be migrated the same way: an HA standby and any host that receives `meta/views/` by hand"
    );
    ui::hint(&format!(
        "verify with `grovefs layout --meta-dir {} show`, then start the meta-server",
        base.display()
    ));
    Ok(())
}

/// Rename, with a copy fallback for the one case a rename cannot express: the
/// source and destination are on different filesystems (a bind-mounted `meta/`
/// subtree, an operator staging the new layout on another disk). The copy is
/// fsynced before the source is unlinked, so a crash mid-move leaves either the
/// old file complete or the new one complete — never a half file at the new
/// location that the layout would read as a tree.
fn move_into(src: &Path, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            std::fs::copy(src, dest).map_err(|e2| -> Box<dyn std::error::Error> {
                format!(
                    "cannot move {} to {}: {e} (copy fallback: {e2})",
                    src.display(),
                    dest.display()
                )
                .into()
            })?;
            let f = std::fs::OpenOptions::new().write(true).open(dest)?;
            f.sync_all()?;
            drop(f);
            std::fs::remove_file(src)?;
            Ok(())
        }
    }
}

/// Auto-load a `KEY=VALUE` config file into the environment, WITHOUT clobbering
/// variables that are already set — the precedence chain for every knob below
/// is always flag > live env > config file > built-in default. Search order:
/// `$GROVEFS_CONFIG` (explicit path) > `$HOME/.config/grovefs/grovefs.conf`;
/// the first existing file wins. Only
/// `GROVEFS_`-prefixed keys are honored (the rest are ignored with a warning);
/// recognized: GROVEFS_MANAGER / GROVEFS_ROSTER_KEY_FILE / GROVEFS_DEPLOYMENT (the
/// mount bootstrap), GROVEFS_TOKEN (the credential a mount presents),
/// GROVEFS_STRIPE_BYTES (mount stripe size), GROVEFS_HOME (the node-side store
/// root), GROVEFS_META_DIR, GROVEFS_META_SERVER / GROVEFS_DATA_SERVERS (read by
/// `du` and `gc --control-server` only — never by a mount), GROVEFS_STATS,
/// GROVEFS_API_KEY, GROVEFS_MCP_API_KEY, …
/// The file is read once before clap parses, so the existing per-knob env
/// fallbacks consume the values without any extra plumbing; the mount
/// re-exec keeps them (the child inherits the environment). Secrets do belong
/// in here only if the file itself is protected (`chmod 600`) — exactly the
/// contract `GROVEFS_API_KEY` already carries as a plain environment variable.
fn load_config_file() {
    let path = std::env::var("GROVEFS_CONFIG")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME").ok().map(|h| {
                PathBuf::from(&h)
                    .join(".config")
                    .join("grovefs")
                    .join("grovefs.conf")
            })
        });
    let Some(path) = path else { return };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // An explicit $GROVEFS_CONFIG that cannot be read is a misconfiguration
        // worth a warning; a default location that does not exist is normal.
        if std::env::var("GROVEFS_CONFIG").is_ok() {
            ui::warn(&format!(
                "GROVEFS_CONFIG={} not readable; continuing with env/defaults",
                path.display()
            ));
        }
        return;
    };
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Optional `export ` prefix so a grovefs.conf doubles as a shell file.
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            ui::warn(&format!(
                "{}:{}: not a KEY=VALUE line; ignored",
                path.display(),
                lineno + 1
            ));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if !key.starts_with("GROVEFS_") {
            ui::warn(&format!(
                "{}:{}: {} is not a GROVEFS_* knob; ignored",
                path.display(),
                lineno + 1,
                key
            ));
            continue;
        }
        // Quote-aware so values may contain spaces (paths, rule text).
        let value = match value {
            v if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) =>
            {
                v[1..v.len() - 1].to_string()
            }
            v => v.to_string(),
        };
        // Live environment always wins; the config file only fills defaults.
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

/// `--bw-read`/`--bw-write` env fallback: an explicit flag wins; an
/// unparsable env value is a hard error (silently ignoring a cap the
/// operator believes is on would be worse than refusing to mount).
fn resolve_bw(flag: Option<u64>, env_key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(v) = flag {
        return Ok(v);
    }
    match std::env::var(env_key) {
        Ok(s) if !s.trim().is_empty() => {
            grovefs::bwlimit::parse_rate(&s).map_err(|e| format!("{env_key}: {e}").into())
        }
        _ => Ok(0),
    }
}

/// `--stripe-bytes` env fallback, same shape as [`resolve_bw`]: an explicit
/// flag wins; an unparsable env value is a hard error (silently ignoring a
/// stripe the operator believes is set would bake the wrong layout into every
/// new `data_ref`). When neither is given, the built-in default
/// (`backend::STRIPE_BYTES`, 16 MiB) is returned. A `0` value is refused by
/// the parser (a zero stripe is meaningless).
fn resolve_stripe(flag: Option<u64>, env_key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(v) = flag {
        return Ok(v);
    }
    match std::env::var(env_key) {
        Ok(s) if !s.trim().is_empty() => {
            grovefs::bwlimit::parse_size_bytes(&s).map_err(|e| format!("{env_key}: {e}").into())
        }
        _ => Ok(grovefs::backend::STRIPE_BYTES),
    }
}

fn base_dir(home: &Option<PathBuf>) -> PathBuf {
    if let Some(h) = home {
        return h.clone();
    }
    if let Ok(h) = std::env::var("GROVEFS_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grovefs")
}

fn meta_path(base: &Path, view: &str) -> PathBuf {
    // One funnel for the whole product: `crate::meta` owns the answer, so a CLI
    // arm cannot drift from what the server, the engine and `gc` compute.
    grovefs::meta::view_db_path(base, view)
}

/// Start one server's manager heartbeat when `--register-with` was given, and
/// do nothing when it was not. Shared by BOTH server roles on purpose: the
/// announcement is the same shape and the failure policy must not drift between
/// them.
///
/// A misconfigured registration is a HARD error at startup — an operator who
/// typed `--register-with` believes their node is findable, and being quietly
/// wrong about that is worse than not starting. A manager that is merely DOWN
/// is not: [`grovefs::manager::spawn_registrar`] retries with backoff while the
/// server serves, because a directory service outage must not become a storage
/// outage.
#[allow(clippy::too_many_arguments)]
fn register_with_manager(
    role: grovefs::manager::Role,
    listen: &str,
    register_with: Option<String>,
    join_key_file: Option<PathBuf>,
    advertise_addr: Option<String>,
    deployment: Option<String>,
    register_ttl: Option<u64>,
    member_id: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let what = format!("{}-server", role.name());
    // Env fallbacks, so a cluster-wide config file names its manager once
    // instead of every unit repeating it (same precedence as the mount side).
    let register_with = register_with.or_else(|| std::env::var("GROVEFS_REGISTER_WITH").ok());
    let join_key_file = join_key_file.or_else(|| {
        std::env::var("GROVEFS_JOIN_KEY_FILE")
            .ok()
            .map(PathBuf::from)
    });
    let Some(server) = register_with else {
        if join_key_file.is_some()
            || advertise_addr.is_some()
            || deployment.is_some()
            || register_ttl.is_some()
        {
            ui::note(&format!(
                "{what} --join-key-file/--advertise-addr/--deployment/--register-ttl describe \
                 a registration; without --register-with HOST:PORT there is no manager to tell, so \
                 they do nothing here"
            ));
        }
        return Ok(());
    };
    // Unsigned registrations would mean any host that can reach the port can
    // name itself a cluster member, so the key is not optional.
    let key_file = join_key_file.ok_or_else(|| {
        format!(
            "{what} --register-with {server} needs --join-key-file PATH: a registration is signed \
             with the manager's join key (the file `manager serve` printed), because an unsigned \
             one lets any host on the network claim to be your metadata service"
        )
    })?;
    let join_key = grovefs::manager::read_hex_key_file(&key_file)
        .map_err(|e| format!("{what} --join-key-file: {e}"))?;
    // Advertise what a CLIENT can dial. With no `--advertise-addr`, that is
    // `--listen` — unless `--listen` is a bind wildcard, and guessing a host
    // name here would put a plausible-but-wrong address in every roster.
    let advertised = match advertise_addr.as_deref() {
        Some(a) => grovefs::manager::check_dialable(a)
            .map_err(|e| format!("{what} --advertise-addr: {e}"))?,
        None => grovefs::manager::check_dialable(listen).map_err(|_| {
            format!(
                "{what} --register-with: --listen {listen} is not an address a client can dial, so \
                 it cannot be advertised — pass --advertise-addr HOST:PORT, exactly as \
                 `grovefs manager define` declares this node (a registration for any other \
                 address is refused)"
            )
        })?,
    };
    let deployment = deployment
        .or_else(|| std::env::var("GROVEFS_DEPLOYMENT").ok())
        .unwrap_or_else(|| grovefs::manager::DEFAULT_DEPLOYMENT.to_string());
    let ann = grovefs::manager::Announcement {
        role,
        deployment,
        addr: advertised,
        member_id,
        ttl: register_ttl.unwrap_or(grovefs::manager::server::DEFAULT_REGISTER_TTL),
    };
    grovefs::manager::spawn_registrar(server, ann, join_key);
    Ok(())
}

/// Resolve the metadata backend URI for a CONTROL-plane command. The explicit
/// `--meta-store` flag wins; otherwise the `GROVEFS_META_STORE` env fills the default
/// (same flag > env chain as `--token`/`--home`). `None` / a value that is not a
/// `postgres[ql]://` URI means the local SQLite/file control plane; a
/// `postgres[ql]://` URI routes api_keys and view definitions to the shared
/// `grovefs_control` schema. This is what makes the control plane FOLLOW the
/// metadata backend — one system of record, never a local file a PG deployment
/// would silently ignore.
///
/// Both backends govern from one place: SQLite from `<meta-dir>` files, PG from
/// `grovefs_control` — so every meta-server/MCP/CLI on a deployment shares one
/// authoritative record. `MetaServer::serve()` still refuses to pair
/// `--meta-store postgres://…` with `--peer`/`--role standby`: GroveFS-native HA
/// snapshot shipping is SQLite-only, and PostgreSQL has its own replication.
fn resolve_store(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("GROVEFS_META_STORE").ok())
}

/// Refuse a `treedb://` --meta-store on every command EXCEPT `meta-server`.
///
/// Only a treedb-capable `meta-server` drives that engine; every other
/// command's backend is a local SQLite file or a PG schema, so letting one of
/// them accept the URI would answer questions about bytes that are not the
/// trees the operator named — the same refuse-not-downgrade rule
/// `MetaServer::with_store` applies to an unknown scheme. Views under a treedb
/// store are governed through the server: `--meta-server <host:port>`.
///
/// The scheme string is spelled out here rather than imported because the lib
/// module behind it is feature-gated while this refusal must exist in EVERY
/// build; keep it equal to `grovefs::treedb_store::SCHEME`.
fn refuse_treedb_store(store: Option<&str>, cmd: &str) -> Result<(), Box<dyn std::error::Error>> {
    if store.is_some_and(|u| u.starts_with("treedb://")) {
        return Err(format!(
            "{cmd} does not drive --meta-store treedb://…: only `grovefs meta-server --meta-store \
             treedb://host:port` (an EXPERIMENTAL `--features treedb` build) serves that engine, \
             so manage its views through it (`--meta-server <host:port>`)"
        )
        .into());
    }
    Ok(())
}

/// Resolve the admin credential for a `--control-server` call: the flag wins,
/// then `GROVEFS_CONTROL_KEY`, then `GROVEFS_API_KEY` (the same variable a mount
/// already uses, so one admin key can be exported once for a whole session).
fn resolve_control_key(explicit: Option<String>) -> Option<String> {
    explicit
        .or_else(|| std::env::var("GROVEFS_CONTROL_KEY").ok())
        .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
        .filter(|k| !k.trim().is_empty())
}

/// Attach to a REMOTE governance entrance, or say exactly what is missing.
fn remote_control(
    addr: &str,
    key: Option<String>,
) -> Result<grovefs::control_remote::RemoteControlStore, Box<dyn std::error::Error>> {
    let key = key.ok_or_else(|| {
        format!(
            "--control-server {addr} needs an admin account key: pass --api-key '<id>:<secret>' \
             (or export GROVEFS_CONTROL_KEY). The first one is minted ON THE SERVER HOST with \
             `grovefs apikey --meta-dir <server-meta-dir> create --for admin --owner <account>`."
        )
    })?;
    Ok(grovefs::control_remote::RemoteControlStore::connect(
        addr, &key,
    )?)
}

/// The control plane a governance command should talk to: the remote one when
/// `--control-server` is given, else the local/`--meta-store` implementation.
/// A remote store is bound to the store ITS server owns — a local
/// `--meta-dir`/`--meta-store` beside `--control-server` describes a different
/// one and is refused by [`refuse_local_with_remote`], not merged.
fn control_store_for_target(
    vbase: &Path,
    store: Option<&str>,
    control_server: Option<&str>,
    control_key: Option<String>,
) -> Result<Arc<dyn grovefs::mcp::ControlStore>, Box<dyn std::error::Error>> {
    match control_server {
        Some(addr) => Ok(Arc::new(remote_control(addr, control_key)?)),
        None => Ok(grovefs::mcp::control_store_for(vbase, store)?),
    }
}

/// A governance command reaches ONE store. Asking for the remote control plane
/// and a local store at the same time would leave it ambiguous which one is
/// authoritative, so the combination is refused by name instead of silently
/// preferring one (the admin key alone decides what a remote call governs).
fn refuse_local_with_remote(
    addr: &str,
    db_path: &Option<PathBuf>,
    store: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if db_path.is_some() || store.is_some() {
        return Err(format!(
            "--control-server {addr} cannot be combined with --meta-dir/--meta-store: a remote \
             control plane governs the store ITS server owns, so a local path here would describe \
             a different store. Drop the local flags — the account key alone decides what is governed."
        )
        .into());
    }
    Ok(())
}

/// The first line of a secret file, stripped of its newline. Every credential
/// this CLI can take from a FILE instead of from argv goes through here: a key on
/// a command line is visible to every local process (`ps`, the journal, a shell
/// history), which is a strictly worse exposure than the `0600` file a unit
/// already reads.
#[cfg(feature = "web")]
fn read_secret_file(path: &PathBuf, what: &str) -> Result<String, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {what} {}: {e}", path.display()))?;
    secret_line(&text, what)
}

/// One secret out of the first line of what was read. Empty is refused HERE on
/// purpose: an empty password would come back as the server's policy refusal
/// ("at least 10 characters"), which hides the actual mistake — that nothing was
/// piped in.
#[cfg(feature = "web")]
fn secret_line(text: &str, what: &str) -> Result<String, Box<dyn std::error::Error>> {
    let line = text.lines().next().unwrap_or("").to_string();
    if line.is_empty() {
        return Err(format!("{what} has no secret in its first line").into());
    }
    Ok(line)
}

/// A password for `web account create|passwd`: from a file or from stdin, never
/// from argv and never invented here — a placeholder generated by the CLI is the
/// same leak with extra steps.
#[cfg(feature = "web")]
fn read_password(
    file: Option<&PathBuf>,
    stdin: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let text = match (file, stdin) {
        (Some(p), false) => std::fs::read_to_string(p)
            .map_err(|e| format!("cannot read --password-file {}: {e}", p.display()))?,
        (None, true) => {
            use std::io::Read;
            let mut s = String::new();
            let n = std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("cannot read the password from stdin: {e}"))?;
            if n == 0 {
                return Err("--password-stdin received nothing: pipe the secret in (a password \n                        manager, a file) rather than running this from a terminal.".into());
            }
            s
        }
        (Some(_), true) => {
            return Err("--password-file and --password-stdin name two sources for one password.".into())
        }
        (None, false) => {
            return Err(
                "a password comes from --password-file PATH or --password-stdin: on a command line \n             it is visible to every local process and lands in the shell history. The length and \n             composition rules are the server's and are reported verbatim."
                    .into(),
            )
        }
    };
    secret_line(&text, "the password")
}

/// One string/number/bool field of a control reply as a table cell (`-` when the
/// server omitted it or sent null, so an optional column cannot panic the CLI).
#[cfg(feature = "web")]
fn jfield(v: &serde_json::Value, f: &str) -> String {
    match v.get(f) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "-".to_string(),
    }
}

/// A string-array field of a control reply, joined for one cell.
#[cfg(feature = "web")]
fn jlist(v: &serde_json::Value, f: &str) -> String {
    v.get(f)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Clap value parser for an API-key entrance scope. Validating here (rather
/// than silently ignoring an unknown `--scope` filter) turns a typo into an
/// immediate usage error that lists the accepted words.
fn parse_scope_arg(s: &str) -> Result<String, String> {
    let v = s.to_ascii_lowercase();
    match v.as_str() {
        "any" | "mount" | "mcp" | "admin" => Ok(v),
        _ => Err("expected one of: any, mount, mcp, admin".to_string()),
    }
}

/// Clap value parser for a console account role.
fn parse_account_role(s: &str) -> Result<String, String> {
    let v = s.to_ascii_lowercase();
    match v.as_str() {
        "user" | "admin" => Ok(v),
        _ => Err("expected one of: user, admin".to_string()),
    }
}

/// Clap value parser for the meta-server HA role.
fn parse_ha_role(s: &str) -> Result<String, String> {
    let v = s.to_ascii_lowercase();
    match v.as_str() {
        "primary" | "standby" => Ok(v),
        _ => Err("expected one of: primary, standby".to_string()),
    }
}

/// A timestamp as the CLI displays it: date, time and seconds. The stored
/// RFC 3339 value carries sub-second precision that no operator scans in a
/// table; keeping it would squeeze the note off every 120-column terminal.
fn short_time(s: &str) -> String {
    if s.chars().count() <= 19 {
        s.to_string()
    } else {
        s.chars().take(19).collect()
    }
}

/// The optional tail of an API-key row: expiry/revocation plus the note. Shared
/// by the wide table and the narrow stacked layout so the facts never diverge.
fn apikey_tail(r: &grovefs::apikeys::KeyInfo) -> String {
    let mut tail = match (r.revoked_at.as_deref(), r.expired) {
        (Some(t), _) => format!("revoked {t}"),
        (None, true) => format!("EXPIRED {}", r.expires_at.as_deref().unwrap_or("")),
        (None, false) => match r.expires_at.as_deref() {
            Some(e) => format!("expires {e}"),
            None => "never".to_string(),
        },
    };
    if let Some(n) = r.note.as_deref().filter(|t| !t.is_empty()) {
        tail.push_str(&format!(" | note: {n}"));
    }
    tail
}

/// Render the shared `OutLine` stream from the governance API: informational
/// lines get the CLI's bullet/wrap treatment, notes keep their exact wording on
/// stderr. The MCP server never sees this transformation — its own renderer
/// still emits plain text for the wire.
fn print_outlines(lines: Vec<grovefs::mcp::OutLine>) {
    for line in lines {
        match line {
            grovefs::mcp::OutLine::Info(s) => ui::outline(&s),
            grovefs::mcp::OutLine::Note(s) => eprintln!("{}", ui::muted(&s)),
        }
    }
}

/// A reply that is a list of rows.
#[cfg(feature = "web")]
fn jrows(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// Run one `grovefs web account|grant` verb over the control plane and print the
/// reply. Each is ONE call, and every rule behind it stays the server's — the
/// password policy, the last-enabled-admin guard, the base a grant on a lens
/// resolves to. Formatting is the only thing this function owns, which is the
/// same argument the web gateway makes about the ACL: a second implementation of
/// a governance rule is the one nobody audits.
#[cfg(feature = "web")]
fn web_govern(
    c: &grovefs::control_remote::ControlClient,
    action: WebAction,
) -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    let call =
        |op: &str, a: serde_json::Value| -> Result<serde_json::Value, Box<dyn std::error::Error>> {
            c.call(op, a)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })
        };
    // The address echoed into every "do this next" line, so a copy-paste of the
    // hint is a working command rather than one the operator has to complete.
    let at = c.addr();
    match action {
        WebAction::Account(WebAccountAction::Create {
            account,
            password_file,
            password_stdin,
            role,
        }) => {
            let password = read_password(password_file.as_ref(), password_stdin)?;
            let r = call(
                "account.create",
                json!({"account": account, "password": password, "role": role}),
            )?;
            let made = jfield(&r, "account");
            ui::success(&format!("web account created: {made}"));
            let bases = jlist(&r, "bases");
            let mut facts = vec![
                ("role", jfield(&r, "role")),
                (
                    "bases",
                    if bases.is_empty() {
                        "none — it can sign in and govern nothing yet".to_string()
                    } else {
                        bases
                    },
                ),
            ];
            if let Some(note) = r.get("note").and_then(|v| v.as_str()) {
                facts.push(("note", note.to_string()));
            }
            ui::kv_block(2, &facts);
            ui::blank();
            ui::section("Next");
            println!("  grovefs web --control-server {at} grant add --account {made} --base VIEW");
            ui::note(
                "a grant delegates one base view — the account then governs that tree and everything derived from it",
            );
        }
        WebAction::Account(WebAccountAction::List) => {
            let rows = jrows(&call("account.list", json!({}))?);
            if rows.is_empty() {
                ui::info(&format!(
                    "no web accounts in the control plane at {at}. The first one is seeded at install time while the store is empty (`meta-server --admin-user alice --admin-password-file PATH`); after that they come from the console's Accounts page or from `grovefs web account create`."
                ));
                return Ok(());
            }
            ui::heading("Web accounts");
            let mut t = ui::Table::new(vec![
                ui::Column::left("account").min(8),
                ui::Column::left("role").min(4),
                ui::Column::left("created").min(10).priority(2),
                ui::Column::left("last sign-in").min(10).priority(1),
                ui::Column::left("disabled").min(8).priority(2),
                ui::Column::left("base views governed").min(10),
            ]);
            for r in &rows {
                let mut bases = jlist(r, "bases");
                if bases.is_empty() {
                    bases = if jfield(r, "role") == "admin" {
                        "<all: an admin is not bounded by grants>".to_string()
                    } else {
                        "-".to_string()
                    };
                }
                t.row(vec![
                    ui::Cell::new(jfield(r, "account")),
                    ui::Cell::muted(jfield(r, "role")),
                    ui::Cell::muted(jfield(r, "created_at")),
                    ui::Cell::muted(jfield(r, "last_login_at")),
                    ui::Cell::muted(jfield(r, "disabled_at")),
                    ui::Cell::new(bases),
                ]);
            }
            t.print();
        }
        WebAction::Account(WebAccountAction::Passwd {
            account,
            password_file,
            password_stdin,
        }) => {
            let password = read_password(password_file.as_ref(), password_stdin)?;
            let r = call(
                "account.passwd",
                json!({"account": account, "password": password}),
            )?;
            ui::success(&format!("password changed for {}.", jfield(&r, "account")));
            ui::note(
                "a browser session already signed in keeps its session until it goes idle (the gateway's own table, not a credential); `account disable` is the verb that stops a person on their next call",
            );
        }
        WebAction::Account(WebAccountAction::Role { account, role }) => {
            let r = call("account.role", json!({"account": account, "role": role}))?;
            ui::success(&format!(
                "{} is now role {}",
                jfield(&r, "account"),
                jfield(&r, "role")
            ));
            if jfield(&r, "role") == "admin" {
                ui::note(
                    "an admin governs the whole deployment: every view, every key, every account. Its own grants are not read, so delegate a `user` (`grant add`) rather than raising a role to solve a missing grant",
                );
            }
        }
        WebAction::Account(WebAccountAction::Enable { account }) => {
            let r = call(
                "account.disable",
                json!({"account": account, "disabled": false}),
            )?;
            ui::success(&format!("{} can sign in again.", jfield(&r, "account")));
        }
        WebAction::Account(WebAccountAction::Disable { account }) => {
            let r = call(
                "account.disable",
                json!({"account": account, "disabled": true}),
            )?;
            ui::success(&format!("{} cannot sign in.", jfield(&r, "account")));
            ui::note(&format!(
                "effective on that user's NEXT call (the server re-reads the principal per request); its delegations are untouched, so `account enable` returns exactly this reach. Its API keys are NOT revoked — a disable is reversible and stopping a live mount is not, so revoking one is a separate, deliberate verb: `grovefs apikey --control-server {at} revoke --id <ID>`"
            ));
        }
        WebAction::Account(WebAccountAction::Rm { account }) => {
            let r = call("account.delete", json!({"account": account}))?;
            ui::success(&format!("web account {} deleted.", jfield(&r, "account")));
            ui::note(&jfield(&r, "note"));
        }
        WebAction::Grant(WebGrantAction::Add { account, base }) => {
            let r = call("grant.add", json!({"account": account, "base": base}))?;
            let got = jfield(&r, "base");
            if r.get("asNamed").and_then(|v| v.as_bool()) == Some(false) {
                ui::note(&format!(
                    "'{base}' resolves to base view '{got}' — the delegation is stored on that (a grant on a lens would be a row the ACL can never match)"
                ));
            }
            ui::success(&format!(
                "{} now governs '{got}' and everything derived from it, as granted by {}.",
                jfield(&r, "account"),
                jfield(&r, "grantedBy")
            ));
            ui::note(
                "that covers its forks, aliases and lenses, and the keys bound to any of those names",
            );
        }
        WebAction::Grant(WebGrantAction::Rm { account, base }) => {
            let r = call("grant.remove", json!({"account": account, "base": base}))?;
            if r.get("removed").and_then(|v| v.as_bool()) == Some(true) {
                ui::success(&format!(
                    "{} no longer governs '{}'",
                    jfield(&r, "account"),
                    jfield(&r, "base")
                ));
            } else {
                ui::info(&format!(
                    "there was no such delegation: {} did not govern '{}'",
                    jfield(&r, "account"),
                    jfield(&r, "base")
                ));
            }
        }
        WebAction::Grant(WebGrantAction::List { account }) => {
            let arg = match &account {
                Some(a) => json!({"account": a}),
                None => json!({}),
            };
            let rows = jrows(&call("grant.list", arg)?);
            if rows.is_empty() {
                ui::info(&format!(
                    "no delegations{}",
                    match &account {
                        Some(a) => format!(" for '{a}'"),
                        None => String::new(),
                    }
                ));
                ui::hint(
                    "an account with role `user` governs nothing until it is granted a base view",
                );
                println!(
                    "  grovefs web --control-server {at} grant add --account alice --base mybase"
                );
                return Ok(());
            }
            ui::heading("Delegations");
            let mut t = ui::Table::new(vec![
                ui::Column::left("account").min(8),
                ui::Column::left("base view").min(8),
                ui::Column::left("perm").min(4),
                ui::Column::left("granted").min(10).priority(1),
                ui::Column::left("by").min(6),
            ]);
            for r in &rows {
                t.row(vec![
                    ui::Cell::new(jfield(r, "account")),
                    ui::Cell::accent(jfield(r, "base_view")),
                    ui::Cell::muted(jfield(r, "perm")),
                    ui::Cell::muted(jfield(r, "granted_at")),
                    ui::Cell::muted(jfield(r, "granted_by")),
                ]);
            }
            t.print();
        }
    }
    Ok(())
}

/// Undo a just-imported tree whose `fork <base>` definition could not be
/// recorded. The tree exists on whichever backend the store names, so the undo
/// has to be backend-aware: a `<view>.db` file or a PG schema.
/// Bytes are never touched — they are still named by the source tree, and the
/// ledger follows the trees that exist.
fn rollback_imported_tree(vbase: &Path, store: Option<&str>, view: &str) -> String {
    let uri = store.unwrap_or("");
    if grovefs::pg_store::is_pg_store(uri) {
        return match grovefs::pg_store::PgEngine::remove_view(uri, view) {
            Ok(_) => "the imported tree was removed again".to_string(),
            Err(e) => {
                format!("the imported tree could NOT be removed ({e}) — drop its schema by hand")
            }
        };
    }
    for ext in grovefs::meta::TREE_DB_EXTS {
        let _ = std::fs::remove_file(grovefs::meta::view_db_with_ext(vbase, view, ext));
    }
    let _ = std::fs::remove_dir(grovefs::meta::view_dir(vbase, view));
    let _ = meta::rebuild_ledger(vbase);
    "the imported tree was removed again".to_string()
}

/// `apikey` verbs take an id or a UNIQUE prefix of one, in one place: `revoke`,
/// `rotate`, `secret`, `set-ro` and `update` each spelled the rule again, which
/// is how "this prefix matches two keys" ends up as five different behaviors.
/// The caller decides what a 0-, 1- or n-row match means; that part genuinely
/// differs per verb (a revoke of an already-revoked key is news, a rotate of one
/// is a refusal).
fn key_by_id_prefix<'a>(
    rows: &'a [grovefs::apikeys::KeyInfo],
    id: &str,
) -> Vec<&'a grovefs::apikeys::KeyInfo> {
    if let Some(exact) = rows.iter().find(|k| k.id == id) {
        vec![exact]
    } else {
        rows.iter().filter(|k| k.id.starts_with(id)).collect()
    }
}

/// Whichever key registry an `apikey` command acts on: the store THIS machine
/// opens (`--meta-dir`/`--meta-store`), or the registry inside a remote
/// meta-server reached through its governance port. An account's keys are
/// guarded by the surface that installs them, so the server that owns the
/// store also owns the mint/revoke — the operator's machine holds only the
/// admin credential, never the store.
///
/// Every verb is one of these, so the ID resolution and the printing above
/// stay single-sourced instead of being duplicated per transport.
enum KeyAdmin {
    Local(grovefs::apikeys::ApiKeyStore),
    Remote(grovefs::control_remote::ControlClient),
}

impl KeyAdmin {
    fn list(&self) -> Result<Vec<grovefs::apikeys::KeyInfo>, String> {
        match self {
            Self::Local(k) => k.list(),
            Self::Remote(c) => {
                let rows = c.call("apikey.list", serde_json::json!({}))?;
                let arr = rows.as_array().ok_or_else(|| {
                    "malformed control reply: apikey.list is not an array".to_string()
                })?;
                let s = |v: &serde_json::Value, f: &str| -> Result<String, String> {
                    v.get(f)
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| format!("malformed control reply: key row without '{f}'"))
                };
                let opt = |v: &serde_json::Value, f: &str| -> Option<String> {
                    v.get(f).and_then(|x| x.as_str()).map(str::to_string)
                };
                arr.iter()
                    .map(|r| {
                        Ok(grovefs::apikeys::KeyInfo {
                            id: s(r, "id")?,
                            owner: opt(r, "owner"),
                            label: s(r, "label")?,
                            scope: s(r, "scope")?,
                            view: opt(r, "view"),
                            ro: r.get("ro").and_then(|x| x.as_bool()).unwrap_or(false),
                            created_at: s(r, "created_at")?,
                            last_used_at: opt(r, "last_used_at"),
                            revoked_at: opt(r, "revoked_at"),
                            note: opt(r, "note"),
                            expires_at: opt(r, "expires_at"),
                            // The SERVER's verdict — this CLI is a client, and a
                            // client comparing timestamp strings across timezones
                            // is how a key looks live when it is not.
                            expired: r.get("expired").and_then(|x| x.as_bool()).unwrap_or(false),
                            created_by: opt(r, "created_by"),
                            secret_hint: opt(r, "secret_hint"),
                            secret_available: r
                                .get("secret_available")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            }
        }
    }

    /// Mint a key. `ttl` is a duration spelling (`7d`) resolved to a deadline by
    /// whichever side OWNS the clock that will enforce it: the server on the
    /// remote path (and its answer comes back in the reply), this process on the
    /// local path. Returns `(id, plaintext, deadline)`, the deadline `None` for a
    /// key that never expires. `created_by` is deliberately NOT a parameter: the
    /// remote arm lets the server stamp it from the AUTHENTICATED account (a
    /// client naming its own minter is a claim, not a record), and the local arm
    /// has no identity to record — it is the store itself.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        label: &str,
        scope: grovefs::apikeys::Scope,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
        note: Option<&str>,
        ttl: Option<&str>,
    ) -> Result<(String, String, Option<String>), String> {
        match self {
            Self::Local(k) => {
                let expires = grovefs::apikeys::expires_from_ttl(ttl.unwrap_or(""))?;
                let (id, full) = k.create_extended(
                    label,
                    scope,
                    view,
                    ro,
                    owner,
                    note,
                    expires.as_deref(),
                    None,
                )?;
                Ok((id, full, expires))
            }
            // Closes the credential loop: a key for the ADMINISTRATOR of one
            // deployment is itself minted over that deployment's control plane,
            // so a second operator never needs the server's store either.
            Self::Remote(c) => {
                let r = c.call(
                    "apikey.create",
                    serde_json::json!({
                        "label": label,
                        "scope": scope.as_str(),
                        "view": view,
                        "ro": ro,
                        "owner": owner,
                        "note": note,
                        "ttl": ttl,
                    }),
                )?;
                let id = r.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                    "malformed control reply: apikey.create without an id".to_string()
                })?;
                let key = r.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                    "malformed control reply: apikey.create without a key".to_string()
                })?;
                let expires = r
                    .get("expires_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Ok((id.to_string(), key.to_string(), expires))
            }
        }
    }

    /// Edit one ACTIVE key's remark and/or deadline (`apikey update`). `ttl`
    /// follows [`Self::create`]'s clock rule; `None` leaves the deadline alone
    /// and `"never"` removes it. Errors name the row that refused, which on the
    /// remote path is the server's own sentence about ACL and revocation.
    fn update(&self, id: &str, note: Option<&str>, ttl: Option<&str>) -> Result<(), String> {
        match self {
            Self::Local(k) => {
                let expires = match ttl {
                    Some(t) => Some(grovefs::apikeys::expires_from_ttl(t)?),
                    None => None,
                };
                if k.update_fields(id, note, expires.as_ref().map(Option::as_deref))? {
                    Ok(())
                } else {
                    Err(format!(
                        "no ACTIVE api key with id '{id}' — the id is unknown, or the key is revoked \
                         (revoke is final; mint a new one)"
                    ))
                }
            }
            Self::Remote(c) => {
                c.call(
                    "apikey.update",
                    serde_json::json!({ "id": id, "note": note, "ttl": ttl }),
                )?;
                Ok(())
            }
        }
    }

    fn revoke(&self, id: &str) -> Result<bool, String> {
        match self {
            Self::Local(k) => k.revoke(id),
            Self::Remote(c) => Ok(c
                .call("apikey.revoke", serde_json::json!({ "id": id }))?
                .as_bool()
                .unwrap_or(false)),
        }
    }

    /// Replace an active key's secret; like `create`, yields the new plaintext,
    /// which the store also keeps as the wrapped copy [`Self::reveal`] opens.
    fn rotate(&self, id: &str) -> Result<Option<String>, String> {
        match self {
            Self::Local(k) => k.rotate(id),
            Self::Remote(c) => {
                let r = c.call("apikey.rotate", serde_json::json!({ "id": id }))?;
                Ok(r.get("key").and_then(|v| v.as_str()).map(str::to_string))
            }
        }
    }

    /// Read one credential's stored copy. The local arm opens the row and checks
    /// the digest itself; the remote arm asks the server that owns both, so an
    /// operator machine needs an admin credential and nothing else — never the
    /// database password, never the wrapping material.
    fn reveal(&self, id: &str) -> Result<grovefs::apikeys::StoredSecret, String> {
        match self {
            Self::Local(k) => k.reveal(id),
            Self::Remote(c) => {
                let r = c.call("apikey.secret", serde_json::json!({ "id": id }))?;
                if let Some(key) = r.get("key").and_then(|v| v.as_str()) {
                    return Ok(grovefs::apikeys::StoredSecret::Recoverable(key.to_string()));
                }
                // The three no-answers travel as the one sentence the server
                // wrote for them (`StoredSecret::refusal`), so this arm cannot
                // invent a fourth; which of the three it was is in the text.
                Ok(grovefs::apikeys::StoredSecret::Unopenable(
                    r.get("why")
                        .and_then(|v| v.as_str())
                        .unwrap_or("the server returned neither a credential nor a reason")
                        .to_string(),
                ))
            }
        }
    }

    /// Does this registry keep a recoverable copy of what it mints? Read off the
    /// row the mint just wrote rather than from a configuration guess, so the
    /// line printed after `apikey create` is a fact about the credential the
    /// operator now holds — and so a store with no wrapping material says so at
    /// the moment the secret would otherwise be silently lost.
    fn keeps_copies(&self, id: &str) -> bool {
        self.list()
            .map(|rows| rows.iter().any(|k| k.id == id && k.secret_available))
            .unwrap_or(false)
    }

    /// Are the calls this command makes recorded by somebody? Only the REMOTE
    /// path goes through a meta-server, and the meta-server is what writes the
    /// control-plane audit rows. A local run opens the store itself, where the
    /// gate is the file's permissions (or the PG credential) and there is no
    /// attribution left to add — whoever can read that store can read whatever
    /// it holds. A message that promises an audit row asks this first, so the
    /// promise is only made where something actually keeps it.
    fn reads_are_audited(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        match self {
            Self::Local(k) => k.set_ro(id, ro),
            Self::Remote(c) => Ok(c
                .call("apikey.set_ro", serde_json::json!({ "id": id, "ro": ro }))?
                .as_bool()
                .unwrap_or(false)),
        }
    }

    /// Trim the call audit. Returns `(cutoff, older, deleted)`. The cutoff is
    /// resolved by whichever side OWNS the clock that stamped the rows — the
    /// server on the remote arm (its answer comes back in the reply, like
    /// `update --ttl`), this process on the local arm, which IS the store.
    fn purge(&self, before: &str, dry_run: bool) -> Result<(String, usize, usize), String> {
        match self {
            Self::Local(k) => {
                let cutoff = grovefs::apikeys::cutoff_from_age(before)?;
                let older = k.count_calls_older_than(&cutoff)?;
                let deleted = if dry_run {
                    0
                } else {
                    k.purge_calls_older_than(&cutoff)?
                };
                Ok((cutoff, older, deleted))
            }
            Self::Remote(c) => {
                let r = c.call(
                    "apikey.purge",
                    serde_json::json!({ "before": before, "dryRun": dry_run }),
                )?;
                let n = |f: &str| {
                    r.get(f)
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| format!("malformed control reply: purge has no '{f}'"))
                };
                let cutoff = r
                    .get("cutoff")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "malformed control reply: purge has no 'cutoff'".to_string())?
                    .to_string();
                Ok((cutoff, n("older")? as usize, n("deleted")? as usize))
            }
        }
    }

    fn recent_calls(&self, limit: usize) -> Result<Vec<grovefs::apikeys::CallLog>, String> {
        match self {
            Self::Local(k) => k.recent_calls(limit),
            Self::Remote(c) => {
                let rows = c.call("apikey.calls", serde_json::json!({ "limit": limit }))?;
                let arr = rows.as_array().ok_or_else(|| {
                    "malformed control reply: apikey.calls is not an array".to_string()
                })?;
                let opt = |v: &serde_json::Value, f: &str| -> Option<String> {
                    v.get(f).and_then(|x| x.as_str()).map(str::to_string)
                };
                Ok(arr
                    .iter()
                    .map(|r| grovefs::apikeys::CallLog {
                        ts: opt(r, "ts").unwrap_or_default(),
                        key_id: opt(r, "key_id"),
                        key_label: opt(r, "key_label"),
                        method: opt(r, "method").unwrap_or_default(),
                        tool: opt(r, "tool").unwrap_or_default(),
                        ok: r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
                        detail: opt(r, "detail").unwrap_or_default(),
                    })
                    .collect())
            }
        }
    }
}

fn main() {
    // A CLI that reports into `head`/`less` must exit when the reader goes
    // away instead of panicking with "failed printing to stdout: Broken pipe".
    // Rust ignores SIGPIPE by default; restore the conventional disposition.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // Rust's default `Termination` for `fn main() -> Result<(), Box<dyn Error>>`
    // prints the Err via Debug, so a failed operation surfaced as
    // `Error: Os { code: 111, kind: ConnectionRefused, message: .. }` and a
    // `String`/`to_string` error came out wrapped in quotes
    // (`Error: "..."`). Neither is readable or greppable (§5-12). Route every
    // fatal error through Display with a stable `error: ` prefix, matching
    // clap's own stderr wording, and preserve the non-zero exit status.
    let rc = match real_main() {
        Ok(()) => 0,
        Err(e) => {
            ui::error(&e.to_string());
            1
        }
    };
    // Say everything before the network closes: the text above is what an
    // operator and the test board read, and a refused command is judged by it.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(rc);
}

/// The one refusal every mount without a manager gets. Spelled out rather than
/// reduced to "missing --manager": a mount is not a command that can be
/// repaired by guessing, so the operator is told what has to be running and why
/// a hand-typed address list is no longer an option.
const NO_COORDINATES: &str = concat!(
    "a mount resolves its cluster coordinates from the deployment's manager: there is no ",
    "longer a way to name a metadata or data service by hand. ",
    "Pass --manager HOST:PORT --roster-key-file PATH (or the GROVEFS_MANAGER / ",
    "GROVEFS_ROSTER_KEY_FILE env knobs) to fetch the deployment's SIGNED roster and dial ",
    "the meta- and data-servers it declares. ",
    "Directly naming the addresses was removed on purpose: one --home holding a view's tree ",
    "AND its bytes would make every client an authoritative store, and hand-typed lists let ",
    "two mounts of one view dial different (or half a) cluster; a manager declaring the ",
    "topology once and every client verifying the same signature is what keeps them ",
    "consistent. Start the services with --register-with the manager, then ",
    "`grovefs manager define`/`serve` it. --home stays the store root of the node-side ",
    "verbs (view create, gc, import, apikey, du) and of the servers themselves, never of ",
    "a mount."
);

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    load_config_file();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let base = base_dir(&cli.home);
    // NOTE: deliberately no unconditional `create_dir_all(base/meta)` here.
    // `--home` is a store root for the NODE-side verbs only (`view`, `gc`, `import`,
    // `apikey`, `du`), each of which creates what it needs in its own branch below.
    // A mount touches nothing under it: every mount resolves both a meta-server
    // and a data-server and keeps no authoritative tree
    // or bytes on this disk — no footprint at all. The servers self-provision too:
    // `meta-server` creates `<base>/meta` in `serve()`, and `MetaStore`/the
    // on-push view path all `create_dir_all` before opening their files.

    match cli.command {
        Cmd::Mount {
            view,
            mountpoint,
            stripe_bytes,
            manager,
            deployment,
            roster_key_file,
            ro,
            strict,
            token,
            api_key,
            o,
            threads,
            as_uid,
            layout,
            no_readdirplus,
            tune_cache,
            bw_read,
            bw_write,
            stats: stats_on,
            stats_file,
            stats_interval,
            foreground,
            log_file,
        } => {
            // Every mount is bootstrapped from the deployment's manager: the
            // repeat-heavy knobs fall back to their env when unset — the live
            // env wins over the config file, an explicit flag wins over both
            // (see `load_config_file`). The detached re-exec keeps these: the
            // child inherits the resolved env below.
            let manager = manager.or_else(|| std::env::var("GROVEFS_MANAGER").ok());
            let roster_key_file = roster_key_file.or_else(|| {
                std::env::var("GROVEFS_ROSTER_KEY_FILE")
                    .ok()
                    .map(PathBuf::from)
            });
            let deployment = deployment
                .or_else(|| std::env::var("GROVEFS_DEPLOYMENT").ok())
                .unwrap_or_else(|| grovefs::manager::DEFAULT_DEPLOYMENT.to_string());
            // Fail-closed, and checked BEFORE the daemonize below so an operator
            // reads this refusal on their own terminal instead of in a log nobody
            // is tailing. A mount no longer accepts hand-typed coordinates: the
            // only way to learn what to dial is the manager and its signed roster.
            let Some(mgr) = manager.as_deref() else {
                return Err(NO_COORDINATES.into());
            };
            if roster_key_file.is_none() {
                return Err(format!(
                    "mount --manager {mgr} needs --roster-key-file PATH (or \
                     GROVEFS_ROSTER_KEY_FILE): a roster nobody verified is an instruction to \
                     connect wherever whoever answered wanted you to, which is exactly what \
                     the signature exists to rule out."
                )
                .into());
            }
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            // Content rate caps: explicit flag > live env/config-file knob;
            // an unparsable env value refuses the mount (see `resolve_bw`).
            let bw_read = resolve_bw(bw_read, "GROVEFS_BW_READ")?;
            let bw_write = resolve_bw(bw_write, "GROVEFS_BW_WRITE")?;
            // Stripe size for cross-server striping: same precedence, the
            // built-in default when neither knob is set (see `resolve_stripe`).
            let stripe = resolve_stripe(stripe_bytes, "GROVEFS_STRIPE_BYTES")?;
            // Default `mount` detaches: re-exec this exact command with
            // `--foreground` in a new session (stdio → log file when attached
            // to a terminal, inherited otherwise so redirecting scripts keep
            // their sink), wait for the mountpoint to appear in /proc/mounts,
            // print a one-line summary, return. Everything below runs in the
            // child (or in the parent itself with `--foreground`).
            if !foreground {
                daemonize_and_await(&view, &mountpoint, log_file.as_deref())?;
                return Ok(());
            }
            std::fs::create_dir_all(&mountpoint)?;
            // Resolve the coordinates from the manager, ONCE, before anything is
            // dialed. Fail-closed by construction: a roster that does not verify
            // under the key we were handed is an error, never a warning followed
            // by a mount to wherever it happened to point. From here down
            // `meta_addr`/`data_addr` are exactly the coordinates an operator
            // would otherwise have had to type by hand.
            let path = roster_key_file
                .as_ref()
                .expect("--roster-key-file presence checked before the daemonize");
            let key = grovefs::manager::read_hex_key_file(path)
                .map_err(|e| format!("mount --manager: {e}"))?;
            let roster = grovefs::manager::lookup(mgr, &deployment, &key)
                .map_err(|e| format!("mount --manager {mgr}: {e}"))?;
            ui::success(&format!(
                "manager {mgr}: deployment '{}' epoch {} resolved meta [{}] data [{}] \
                 (signature verified, valid for {}s more)",
                roster.deployment,
                roster.epoch,
                roster.meta.join(","),
                roster.data.join(","),
                roster
                    .expires_at
                    .saturating_sub(grovefs::crypto::now_secs()),
            ));
            // A half deployment is not mountable either: there is no local
            // arm left to absorb the missing side, so refuse the roster.
            if roster.meta.is_empty() || roster.data.is_empty() {
                return Err(format!(
                    "mount --manager {mgr}: deployment '{}' resolves to meta [{}] data [{}] \
                     — a mount needs both a metadata service and a data service, and this \
                     roster has no registered {}; start the missing role with \
                     --register-with this manager",
                    roster.deployment,
                    roster.meta.join(","),
                    roster.data.join(","),
                    if roster.meta.is_empty() {
                        "meta-server"
                    } else {
                        "data-server"
                    }
                )
                .into());
            }
            let meta_addr = roster.meta_list();
            let data_addr = roster
                .data_list()
                .expect("roster.data non-empty (checked just above)");
            // ---- opt-in per-stage latency telemetry ----
            // Enabled by `--stats` OR env `GROVEFS_STATS=1|true`. Off by default,
            // where every instrument site is a single relaxed load that skips all
            // work (no clock, lock, or allocation on the hot path). The returned
            // `ExitDump` guard prints + flushes the aggregate report when the mount
            // arm returns — including when `session.run()?` bails — so even a
            // failed mount leaves a triage artifact.
            let stats_env = std::env::var("GROVEFS_STATS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let _stats_dump = if stats_on || stats_env {
                let interval = if stats_interval > 0 {
                    Some(std::time::Duration::from_secs(stats_interval))
                } else {
                    None
                };
                grovefs::stats::enable(stats_file.clone(), interval);
                if let Some(p) = &stats_file {
                    ui::info(&format!(
                        "telemetry: --stats on, periodic report -> {}",
                        p.display()
                    ));
                } else {
                    ui::info("telemetry: --stats on, report printed at unmount");
                }
                Some(grovefs::stats::ExitDump)
            } else {
                None
            };
            // Effective uid reported to the server (lock arbitration, audit):
            // `--run-as-uid` overrides the process euid (advisory).
            let uid = as_uid.unwrap_or_else(|| unsafe { libc::geteuid() });
            // Per-caller credential for a `--require-api-key` server. The env
            // fallback keeps a secret out of argv (visible in `ps` and shell
            // history); an explicit flag always wins.
            let api_key = api_key
                .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
                .unwrap_or_default();
            // A DERIVED (virtual) view — a shape the meta-server defines —
            // mounts read-only by contract: the shape is centrally governed,
            // writes belong to the original (base) view. Set from the M_OPEN
            // ack; folded into the FUSE readonly flag AND the kernel RO mount.
            let view_ro: bool;
            // A FORK mounts with STRUCTURE writable but CONTENT read-only: the
            // FUSE layer refuses write/truncate with EROFS while
            // mkdir/rename/unlink/symlink pass through. Set from the M_OPEN
            // ack byte [2] (the server's `fork <base>` definition).
            let content_ro: bool;
            // Data-Access Capability minted by the meta-server for this mount's
            // admission (empty without `--data-access-key`); forwarded verbatim to
            // every data-server `OP_OPEN`. A data-server in verify mode refuses a
            // mount that never passed a genuine meta — closing the rogue-meta-arms-
            // honest-data hole (see `grovefs::remote::dac`).
            let data_capability: String;
            // Metadata + data source: the two coordinates the roster just
            // resolved. There is no second arm: a mount cannot bind a local
            // `<home>/meta/<view>/<view>.db`, and `meta_addr`/`data_addr` above
            // are non-Option, so the code below cannot even express "no address".
            let meta_provider: Arc<dyn MetaProvider> = {
                let addr = meta_addr.as_str();
                ui::item(&format!("metadata service {addr}"));
                let rm = remote::RemoteMeta::new_with_creds(
                    addr,
                    &view,
                    token.as_deref().unwrap_or(""),
                    uid,
                    &api_key,
                )?;
                // The M_OPEN ack flags derived names (byte [0]); a
                // physical view stays writable.
                view_ro = rm.is_readonly_view();
                // Ack byte [2] flags a FORK: structure writable, content RO.
                content_ro = rm.is_content_ro_view();
                // The definition tail: announce the contract the ack carries, so
                // an operator never learns about EROFS from the first failed
                // write. The enforcement above is the flags; this is the notice.
                match rm.def_kind() {
                    "fork" => ui::info(&format!(
                        "fork view '{view}' of base '{}': independent tree — structure writable, content read-only (EROFS)",
                        rm.def_base_view()
                    )),
                    "derived" => {
                        ui::info(&format!(
                            "derived view '{view}' over base '{}': read-only lens",
                            rm.def_base_view()
                        ));
                        ui::note(&format!(
                            "derived (virtual) views mount read-only — write to base view '{}'",
                            rm.def_base_view()
                        ));
                    }
                    "alias" => ui::info(&format!(
                        "alias view '{view}' resolves to real view '{}' (full tree, writable)",
                        rm.resolved_view()
                    )),
                    _ => {}
                }
                // Ack tail: the Data-Access Capability for the data plane.
                data_capability = rm.data_access_capability();
                Arc::new(rm)
            };
            // Presentation overlay: wrap the provider in a per-mount lens so
            // different clients render the SAME stored tree with different
            // cuts/reorganization. Purely client-side: the server and every
            // other mount are unaffected; data IO is untouched (entries keep
            // their real inodes, so read/write/lock/GC pass straight through).
            let meta_provider: Arc<dyn MetaProvider> = match &layout {
                Some(f) => {
                    let rules = grovefs::layout::LayoutRules::parse_file(f)
                        .map_err(|e| format!("bad layout file: {e}"))?;
                    ui::item(&format!(
                        "layout overlay {}: {} hide, {} only, {}",
                        f.display(),
                        rules.hide.len(),
                        rules.only.len(),
                        ui::plural(rules.reloc.len(), "reloc rule", "reloc rules")
                    ));
                    Arc::new(grovefs::layout::LayoutMeta::new(meta_provider, rules))
                }
                None => meta_provider,
            };
            // Fencing wiring: the meta client owns the live epoch handle (kept
            // current on every M_OPEN handshake/failover); the data client shares
            // the SAME handle to stamp writes, and uses the refresher to re-pull
            // the epoch from the authority when the data-server fences a stale one.
            let fence_epoch = meta_provider.fence_handle();
            let meta_for_fence = meta_provider.clone();
            // Keep a display-capable handle for the invalidation listener:
            // for `--layout` over a roster-resolved meta-server, the lens translates
            // source-coordinate
            // events into display coordinates at the kernel notifier.
            let provider_for_events: Option<Arc<dyn MetaProvider>> = Some(meta_provider.clone());
            let fence_refresh: Arc<dyn Fn() -> u64 + Send + Sync> =
                Arc::new(move || meta_for_fence.fetch_fence_epoch());
            // Data path: remote over our TCP transport (no NFS). The FUSE and metadata
            // layers are identical whichever backend they face, which is exactly what
            // made the local disk backend removable without touching anything above it.
            let backend: Arc<dyn backend::StorageBackend> = {
                let addr = data_addr.as_str();
                // A comma-separated list of data servers activates deterministic
                // cross-server striping (P5); a single address keeps the plain
                // remote path byte-for-byte unchanged.
                let addrs: Vec<&str> = addr
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if addrs.len() > 1 {
                    ui::item(&format!(
                        "sharded data service across {} servers (stripe {} bytes): {}",
                        addrs.len(),
                        stripe,
                        addrs.join(",")
                    ));
                    let mut members: Vec<Arc<dyn backend::StorageBackend>> =
                        Vec::with_capacity(addrs.len());
                    for a in &addrs {
                        members.push(Arc::new(
                            remote::RemoteBackend::new_with_dac(
                                a,
                                token.as_deref().unwrap_or(""),
                                &api_key,
                                &data_capability,
                            )?
                            .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                        ));
                    }
                    Arc::new(backend::ShardedBackend::new(members, stripe))
                } else {
                    // A single data-server never stripes, so the knob is
                    // inert here — say so rather than let a config-file
                    // default silently vanish on a deployment that shrinks
                    // the server list. Not a hard error: the same conf is
                    // meant to survive across multi- and single-server tops.
                    if stripe_bytes.is_some() || std::env::var_os("GROVEFS_STRIPE_BYTES").is_some()
                    {
                        ui::note(
                            "--stripe-bytes / GROVEFS_STRIPE_BYTES has no effect with a single data-server in the roster (striping needs a deployment that declares several)",
                        );
                    }
                    let one = addrs.first().copied().unwrap_or(addr);
                    ui::item(&format!("data service {one}"));
                    // UnstripedGuard: a `data_ref` can still name a stripe
                    // this single address cannot serve, and "no such object"
                    // must not be answered as an empty file (see
                    // backend::sharded::UnstripedGuard).
                    Arc::new(backend::UnstripedGuard::new(Arc::new(
                        remote::RemoteBackend::new_with_dac(
                            one,
                            token.as_deref().unwrap_or(""),
                            &api_key,
                            &data_capability,
                        )?
                        .with_fence(fence_epoch.clone(), fence_refresh.clone()),
                    )))
                }
            };
            let fs = GroveFS::new(
                meta_provider,
                backend,
                ro || view_ro || o.iter().any(|s| s == "ro"),
                content_ro,
                strict || o.iter().any(|s| s == "strict"),
                !no_readdirplus,
                tune_cache,
                bw_read,
                bw_write,
            );

            // Build the fuser 0.18 session Config. The old `--o` string surface is
            // preserved; only `allow_root`/`allow_other` changed shape — they are no
            // longer MountOption variants but the `acl` field (session-wide request
            // filtering), with All == allow_other and RootAndOwner == allow_root.
            let mut cfg = fuser::Config::default();
            cfg.mount_options
                .push(fuser::MountOption::FSName(format!("grovefs:{view}")));
            for raw in &o {
                match raw.as_str() {
                    "allow_root" => cfg.acl = fuser::SessionACL::RootAndOwner,
                    "allow_other" => cfg.acl = fuser::SessionACL::All,
                    "default_permissions" => cfg
                        .mount_options
                        .push(fuser::MountOption::DefaultPermissions),
                    "auto_unmount" => cfg.mount_options.push(fuser::MountOption::AutoUnmount),
                    "ro" => cfg.mount_options.push(fuser::MountOption::RO),
                    other => cfg
                        .mount_options
                        .push(fuser::MountOption::CUSTOM(other.to_string())),
                }
            }
            // `--ro` (or `-o ro`) mounts the view read-only at the kernel level;
            // a derived (virtual) view is forced RO regardless of the flag.
            let want_ro = ro || view_ro || o.iter().any(|s| s == "ro");
            if want_ro && !o.iter().any(|s| s == "ro") {
                cfg.mount_options.push(fuser::MountOption::RO);
            }
            // Concurrent dispatch: n_threads worker event loops, each with its own
            // cloned /dev/fuse fd. `threads = 0` maps to single-threaded mode.
            // The clone ioctl (FUSE_DEV_IOC_CLONE) is Linux 4.5+, and fuser
            // propagates its failure out of `Session::run()` instead of falling
            // back — so on an older kernel asking for cloning would make the mount
            // die, not degrade. Probe the running kernel release first and drop to
            // a single worker (with a visible note) when the ioctl is unavailable.
            // An unreadable / unparseable osrelease (non-Linux, exotic string) is
            // treated as supported so a normal mount never loses parallelism.
            let mut clone_ok = true;
            if threads > 1 {
                if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
                    let mut it = s.trim().split('.');
                    let major = it.next().and_then(|x| x.parse::<u32>().ok());
                    let minor = it.next().and_then(|x| x.parse::<u32>().ok());
                    if let (Some(maj), Some(mnr)) = (major, minor) {
                        clone_ok = maj > 4 || (maj == 4 && mnr >= 5);
                    }
                }
            }
            let effective_threads = if threads > 1 && !clone_ok {
                ui::note(&format!(
                    "running kernel < 4.5 lacks FUSE_DEV_IOC_CLONE; using a single FUSE worker thread (was --threads {threads})"
                ));
                1
            } else {
                threads
            };
            cfg.n_threads = if effective_threads > 1 {
                Some(effective_threads)
            } else {
                None
            };
            cfg.clone_fd = effective_threads > 1;
            // Every mount is fully remote, so naming a `--home` base here would
            // mislead: nothing below reads or writes under it.
            let locality = "fully remote: no local store";
            ui::info(&format!(
                "mounting view '{view}' at {} ({}{})",
                mountpoint.display(),
                locality,
                if want_ro { ", read-only" } else { "" }
            ));
            // Graceful shutdown: the FUSE session returns whenever the mount goes
            // away; a helper thread watches for SIGINT/SIGTERM and asks
            // `fusermount -u` to unmount, ending the loop cleanly. An external
            // `fusermount -u` (no signal) likewise makes the session return.
            //
            // A remote-metadata mount additionally wants real-time cross-client
            // coherence without `--no-cache`: pushing kernel-cache invalidations
            // needs a `Notifier`, which we can grab from the session (`notifier()`
            // takes `&self` and returns an owned handle, so it does not keep the
            // session borrowed). The session then runs on the main thread in the plain
            // blocking `run()` path; a background listener thread translates peer
            // mutations into inval_entry/inval_inode.
            //
            // NOTE: do NOT use `spawn()` + `BackgroundSession::umount_and_join()`
            // here — `umount_and_join(self)` drops the mount handle first (an
            // active unmount) and then joins, so it tears the mount down
            // immediately instead of serving. `run()` is the blocking serve loop
            // (and in 0.18 it fans out to `cfg.n_threads` workers internally).
            let session = fuser::Session::new(fs, &mountpoint, &cfg)?;
            let notifier = session.notifier();
            let tok = token.clone().unwrap_or_default();
            let _listener = remote::spawn_invalidation_listener(
                &meta_addr,
                &view,
                &tok,
                &api_key,
                uid,
                notifier,
                provider_for_events,
            );
            spawn_term_unmounter(mountpoint.clone());
            // From here the mount is live, so a credential REFUSED mid-session
            // (an `apikey revoke` landing on this mount) is an auth loss and may
            // tear the mountpoint down instead of serving a broken view.
            remote::arm_mount_serving();
            session.run()?;
            ui::success(&format!(
                "unmounted view '{view}' at {}",
                mountpoint.display()
            ));
        }
        Cmd::DataServer {
            listen,
            root,
            token,
            require_api_key,
            keys_from,
            store,
            member_id,
            data_access_key,
            register_with,
            join_key_file,
            advertise_addr,
            deployment,
            register_ttl,
        } => {
            // Comma-separated roots = several disks behind one data-server;
            // placement across them is automatic (see PlacementBackend).
            let roots: Vec<PathBuf> = root
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("data-server --data-dir: no usable path".into());
            }
            for r in &roots {
                std::fs::create_dir_all(r)?;
            }
            // Same flag > env > file chain as the mount side.
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            let backend = Arc::from(backend::open_local_roots(&roots)?);
            let srv = match token.as_deref() {
                Some(t) if !t.is_empty() => remote::DataServer::with_token(&listen, backend, t),
                _ => remote::DataServer::new(&listen, backend),
            };
            // Per-caller credential gate. The store FOLLOWS the metadata
            // backend: a `--meta-store postgres://…` (or `GROVEFS_META_STORE`) data-server
            // gates mounts against the shared `grovefs_control` tables — the SAME
            // registry its meta-server uses — with no local file to colocate; a
            // SQLite node reads `<meta-dir>/meta/_control/api_keys.db` (defaulting to
            // this node's first `--data-dir`, or a synced copy for a split one).
            let srv = if require_api_key {
                let store = resolve_store(store);
                let kbase = keys_from.unwrap_or_else(|| roots[0].clone());
                srv.with_api_keys(grovefs::apikeys::ApiKeyStore::open_for(
                    &kbase,
                    store.as_deref(),
                )?)
            } else {
                srv
            };
            // Durable fencing token: a witness restart must not zero the epoch
            // and let a deposed primary sneak back in as a writer.
            let srv = srv.with_fence_store(roots[0].join(".fence_token"));
            // Startup banner echoes the root list + host (display only): root
            // paths repeat across nodes, so the listen line alone is ambiguous.
            let srv = srv.with_roots(&roots);
            // Stable member id for identity-based striping: flag > env >
            // unconfigured. A malformed `GROVEFS_MEMBER_ID` is a HARD error, not
            // a silent drop to positional placement — quietly losing the id an
            // operator believes is set would reintroduce the reorder hazard this
            // feature exists to remove.
            let member_id = match member_id {
                Some(v) => Some(v),
                None => match std::env::var("GROVEFS_MEMBER_ID") {
                    Ok(s) if !s.trim().is_empty() => {
                        Some(s.trim().parse::<u64>().map_err(|_| {
                            format!("GROVEFS_MEMBER_ID: invalid integer '{}'", s.trim())
                        })?)
                    }
                    _ => None,
                },
            };
            let srv = match member_id {
                Some(id) => {
                    ui::info(&format!(
                        "data-server: member-id {id} — clients that stripe across a deployment's \
                         data-server list place chunks by this identity, so their order \
                         is irrelevant (must be unique across the cluster's members)"
                    ));
                    srv.with_member_id(id)
                }
                None => srv,
            };
            // Data-Access Capability verify mode: flag > env > off. A malformed
            // hex key is a HARD error (a silently disabled gate would advertise
            // protection it does not provide).
            let dac_hex = data_access_key.or_else(|| std::env::var("GROVEFS_DATA_ACCESS_KEY").ok());
            let srv = match dac_hex.as_deref().map(str::trim) {
                Some(s) if !s.is_empty() => srv.with_data_access_key(
                    grovefs::remote::dac::decode_key(s)
                        .map_err(|e| format!("data-server --data-access-key {e}"))?,
                ),
                _ => srv,
            };
            // Announce ourselves before blocking in `serve` (the thread it
            // spawns keeps the registration warm for the process's life; a
            // manager that is down never reaches this server's clients).
            register_with_manager(
                grovefs::manager::Role::Data,
                &listen,
                register_with,
                join_key_file,
                advertise_addr,
                deployment,
                register_ttl,
                member_id,
            )?;
            srv.serve()?;
        }
        Cmd::MetaServer {
            listen,
            db_path,
            token,
            require_api_key,
            role,
            peer,
            sync_repl,
            witness,
            store,
            engine_idle_secs,
            audit_retention,
            family_layout,
            pg_write_pool,
            pg_write_pool_max,
            pg_read_pool,
            pg_read_pool_max,
            pg_pool_idle_secs,
            control_listen,
            admin_user,
            admin_password_file,
            data_access_key,
            data_access_ttl,
            register_with,
            join_key_file,
            advertise_addr,
            deployment,
            register_ttl,
        } => {
            let role = if role.eq_ignore_ascii_case("standby") {
                grovefs::remote::meta::Role::Standby
            } else {
                grovefs::remote::meta::Role::Primary
            };
            // Backend for BOTH the view trees and the control plane (view
            // definitions + api_keys): `--meta-store` > `GROVEFS_META_STORE` > SQLite.
            let store = resolve_store(store);
            // `--meta-dir` is the explicit, self-describing form of the metadata
            // root; the global `--home` is the default and loses if both are
            // given. A `GROVEFS_META_DIR` env default slots between them,
            // and `GROVEFS_TOKEN` fills a missing `--token` (same flag > env >
            // config-file chain as the mount side).
            let token = token.or_else(|| std::env::var("GROVEFS_TOKEN").ok());
            let db_path =
                db_path.or_else(|| std::env::var("GROVEFS_META_DIR").ok().map(PathBuf::from));
            let meta_base = db_path.unwrap_or_else(|| base.clone());
            let mut srv = match token.as_deref() {
                Some(t) if !t.is_empty() => {
                    grovefs::remote::MetaServer::with_token(&listen, &meta_base, t)
                }
                _ => grovefs::remote::MetaServer::new(&listen, &meta_base),
            };
            // Same registry `grovefs mcp --meta-dir` and `grovefs apikey --meta-dir`
            // use; key scopes decide which entrance each one may open. Under a PG
            // store the keys live in `grovefs_control.api_keys`, not a local file,
            // so a minted/revoked key is authoritative across every node at the URI.
            if require_api_key {
                srv = srv.with_api_keys(grovefs::apikeys::ApiKeyStore::open_for(
                    &meta_base,
                    store.as_deref(),
                )?);
            }
            // Data-Access Capability issuance: flag > env > off, mirroring the
            // data-server. Only meaningful with a matching `--data-access-key` on
            // every data-server this cluster's clients write to.
            let dac_hex = data_access_key.or_else(|| std::env::var("GROVEFS_DATA_ACCESS_KEY").ok());
            if let Some(s) = dac_hex.as_deref().map(str::trim) {
                if !s.is_empty() {
                    srv = srv.with_data_access_key(
                        grovefs::remote::dac::decode_key(s)
                            .map_err(|e| format!("meta-server --data-access-key {e}"))?,
                        data_access_ttl,
                    );
                }
            }
            srv = srv.with_ha(role, peer);
            if sync_repl {
                srv = srv.with_sync_repl(true);
            }
            srv = srv.with_witness(witness);
            // PG connection-pool bounds: flags over env over defaults, applied
            // BEFORE the first engine opens (engines open lazily per view on
            // M_OPEN, so this is the last safe point). Calling it under a
            // SQLite store is inert — no PG engine is ever constructed — so no
            // branch guards the set; only the banner is store-aware.
            grovefs::pg_store::set_pool_profile(
                pg_write_pool,
                pg_write_pool_max,
                pg_read_pool,
                pg_read_pool_max,
                pg_pool_idle_secs,
            );
            let store_is_pg = store.as_deref().is_some_and(grovefs::pg_store::is_pg_store);
            srv = srv.with_store(store);
            if store_is_pg {
                let (w, r, idle) = grovefs::pg_store::pool_profile_now();
                ui::info(&format!(
                    "meta-server: PG pool bounds per view = write {} warm/{} max, read {} warm/{} max, burst above the floor reaped after {}s idle (a view may override with `grovefs view pool`); plan max_connections as active trees x (floor + retained burst)",
                    w.warm, w.max, r.warm, r.max, idle / 1000
                ));
            }
            srv = srv.with_engine_idle_secs(engine_idle_secs);
            // Layout v3 family placement: the flag is a convenience for
            // writing `meta/.family` at startup (the runtime source of truth
            // is that file, `grovefs layout set-family` edits it live, and
            // placement only ever decides where NEW members land).
            if let Some(mode) = family_layout
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let policy = grovefs::meta::FamilyLayout::parse(mode).ok_or_else(|| {
                    format!("--family-layout: expected flat|nested|grouped, got '{mode}'")
                })?;
                grovefs::meta::set_family_policy(&meta_base, policy)
                    .map_err(|e| format!("cannot record family policy: {e}"))?;
            }
            // Retention policy: the console deployment keeps ninety days by
            // default, with no flag to remember. A duration typo still stops
            // the server at start; `never`/`0` is the explicit opt-out (the
            // flag is the only way to say "keep everything" now that silence
            // means ninety days). The sweep needs the control port because that
            // port is what builds the key/audit store at all. A runtime
            // `audit.retention.set` overrides this from then on (until restart,
            // when the flag/default is authoritative again).
            let audit_retention = match audit_retention {
                Some(w) => {
                    if control_listen.is_none() {
                        return Err(
                            "--audit-retention needs --control-listen HOST:PORT: the audit table belongs 
                             to the control plane, and without that port this process opens no key store 
                             to sweep"
                                .into(),
                        );
                    }
                    match grovefs::apikeys::duration_secs(&w) {
                        Ok(Some(_)) => Some(w),
                        Ok(None) => None,
                        Err(e) => return Err(format!("--audit-retention: {e}").into()),
                    }
                }
                None if control_listen.is_some() => Some("90d".to_string()),
                None => None,
            };
            srv = srv.with_audit_retention(audit_retention);
            // The install-time web owner, if the flags ask for one. Read here and
            // handed to `serve()` as (account, password) — the digest is written
            // by the process that owns the account store, and this value is then
            // dropped rather than kept in the runtime.
            let admin = match (&admin_user, &admin_password_file) {
                (None, None) => None,
                (Some(_), None) => {
                    return Err("--admin-user needs --admin-password-file PATH: a password has no \n                        place on a command line (argv is visible to every local process and \n                        lands in the shell history)".into())
                }
                (None, Some(_)) => {
                    return Err("--admin-password-file needs --admin-user: an owner without a name \n                        cannot be created".into())
                }
                (Some(account), Some(path)) => {
                    if control_listen.is_none() {
                        return Err(
                            "--admin-user needs --control-listen HOST:PORT: the web accounts live \n                             beside the control plane, and without that port nothing can verify a sign-in"
                                .into(),
                        )
                    }
                    let text = std::fs::read_to_string(path)
                        .map_err(|e| format!("cannot read --admin-password-file {}: {e}", path.display()))?;
                    let password = text.lines().next().unwrap_or("").to_string();
                    if password.is_empty() {
                        return Err(format!(
                            "--admin-password-file {} is empty (its first line is the password)",
                            path.display()
                        )
                        .into())
                    }
                    // Never print the secret, and never let it survive past this
                    // statement's scope in a variable the banner could echo.
                    drop(text);
                    Some((account.clone(), password))
                }
            };
            srv = srv.with_control_listen(control_listen);
            srv = srv.with_admin_account(
                admin.as_ref().map(|(a, _)| a.clone()),
                admin.map(|(_, p)| p),
            );
            // Registration carries no member id: a meta-server is identified by
            // its address in the failover list, and stripe placement is the data
            // plane's `--member-id` alone.
            register_with_manager(
                grovefs::manager::Role::Meta,
                &listen,
                register_with,
                join_key_file,
                advertise_addr,
                deployment,
                register_ttl,
                None,
            )?;
            srv.serve()?;
        }
        Cmd::Manager { state_dir, action } => {
            // The declaration file and (by default) both keys live here; the
            // global `--home` is the fallback, so a single-manager host needs
            // no flag at all.
            let sdir = state_dir.unwrap_or_else(|| base.clone());
            match action {
                ManagerAction::Serve {
                    listen,
                    join_key_file,
                    roster_key_file,
                    roster_ttl,
                    register_ttl,
                    require_registered,
                } => {
                    let mdir = grovefs::manager::server::manager_dir(&sdir);
                    let jpath = join_key_file.unwrap_or_else(|| mdir.join("join.key"));
                    let rpath = roster_key_file.unwrap_or_else(|| mdir.join("roster.key"));
                    // Mint-on-first-start so a test deployment needs no key
                    // ceremony, and say WHERE each key is before binding: the
                    // join key has to reach every node, the roster key every
                    // client, and neither is recoverable from anywhere else.
                    let join_key = grovefs::manager::ensure_hex_key_file(&jpath)?;
                    let roster_key = grovefs::manager::ensure_hex_key_file(&rpath)?;
                    ui::heading("manager keys");
                    ui::kv_block(
                        2,
                        &[
                            ("join key", jpath.display().to_string()),
                            ("roster key", rpath.display().to_string()),
                        ],
                    );
                    ui::note(
                        "the join key goes to every node started with --register-with (it authorizes a registration; a client never holds it)",
                    );
                    ui::note(
                        "the roster key goes to every client as `mount --roster-key-file` (it signs the answer; symmetric, so keep the manager's copy secret)",
                    );
                    grovefs::manager::Manager::new(
                        listen,
                        sdir,
                        join_key,
                        roster_key,
                        roster_ttl,
                        register_ttl,
                        require_registered,
                    )?
                    .serve()?;
                }
                ManagerAction::Define { name, meta, data } => {
                    let list = |v: Option<String>| -> Vec<String> {
                        v.map(|s| {
                            s.split(',')
                                .map(str::trim)
                                .filter(|x| !x.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                    };
                    let (path, file) = grovefs::manager::server::define_deployment(
                        &sdir,
                        &name,
                        &list(meta),
                        &list(data),
                    )?;
                    let d = &file.deployments[&name];
                    ui::success(&format!(
                        "deployment '{name}' declared in {}",
                        path.display()
                    ));
                    let meta_shown = d.meta.join(", ");
                    let data_shown = d.data.join(", ");
                    ui::kv_block(
                        2,
                        &[
                            (
                                "meta",
                                format!(
                                    "{} (failover order)",
                                    if d.meta.is_empty() {
                                        "none".to_string()
                                    } else {
                                        meta_shown
                                    }
                                ),
                            ),
                            (
                                "data",
                                format!(
                                    "{} (complete member set)",
                                    if d.data.is_empty() {
                                        "none — clients keep object bytes local".to_string()
                                    } else {
                                        data_shown
                                    }
                                ),
                            ),
                            ("epoch", file.epoch.to_string()),
                        ],
                    );
                    // The part an operator gets wrong: a roster is the DECLARED
                    // set, so declaring a node that does not run is not a
                    // harmless over-declaration — a mount would rather fail than
                    // hand out a subset of a stripe.
                    ui::note(
                        "every address above must be a RUNNING server started with --register-with — a roster ships the full declared set (a stripe's members are not a candidate pool), so one declared-but-down data-server fails mounts instead of being skipped; `manager status` shows who has actually registered",
                    );
                }
                ManagerAction::Status {
                    server,
                    deployment,
                    join_key_file,
                } => {
                    let jpath = join_key_file.unwrap_or_else(|| {
                        grovefs::manager::server::manager_dir(&sdir).join("join.key")
                    });
                    let key = grovefs::manager::read_hex_key_file(&jpath)?;
                    let text = grovefs::manager::status(&server, deployment.as_deref(), &key)?;
                    // Pretty when it parses as JSON, verbatim when it does not:
                    // this output is meant to be read by a human AND piped to jq.
                    let pretty = serde_json::from_str::<serde_json::Value>(&text)
                        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.clone()))
                        .unwrap_or_else(|_| text.clone());
                    println!("{pretty}");
                }
            }
        }
        Cmd::Umount { mountpoint } => {
            run_fusermount(&["-u", &mountpoint.to_string_lossy()])?;
            ui::success(&format!("unmounted {}", mountpoint.display()));
        }
        Cmd::View {
            db_path,
            store,
            control_server,
            control_key,
            action,
        } => {
            // View definitions are CONTROL-PLACE state and FOLLOW the metadata
            // backend: under SQLite they are `meta/views/*.layout` files beside
            // `api_keys.db`; under a `--meta-store postgres://…` they are the shared
            // `grovefs_control.view_defs` rows the meta-server reads at M_OPEN.
            // The governance ops below run against ONE `ControlStore` so the CLI
            // and the MCP server can never drift per backend. Base-view
            // creation and export are PHYSICAL-tree operations (a local db file)
            // and stay file-based.
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &store)?;
            }
            let vbase = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            // A treedb tree store is driven by its meta-server, never from here:
            // the branches below would create a local `<view>.db` that no
            // treedb meta-server ever reads.
            refuse_treedb_store(store.as_deref(), "grovefs view")?;
            let store_is_pg = store.as_deref().is_some_and(grovefs::pg_store::is_pg_store);
            // The same `ControlStore` the MCP server drives — except that with
            // `--control-server` it is a thin client of the meta-server's
            // governance port, so the definitions/aliases of a PG *or SQLite*
            // deployment are governed from any machine with an admin key.
            let ctrl = control_store_for_target(
                &vbase,
                store.as_deref(),
                control_server.as_deref(),
                resolve_control_key(control_key.clone()),
            )?;
            let remote_ctl = control_server.as_deref().map(|a| {
                let key = resolve_control_key(control_key).unwrap_or_default();
                grovefs::control_remote::ControlClient::new(a, &key)
            });
            // A PG deployment has no local per-view db file: a base tree is a
            // `grovefs_v_<hex(view)>` schema that `view create` provisions
            // explicitly, and its bytes move through a mount — so the file-level
            // physical ops below are refused rather than silently writing a
            // local artifact the PG meta-server never reads.
            let require_sqlite = |op: &str| -> Result<(), Box<dyn std::error::Error>> {
                if store_is_pg {
                    Err(format!(
                        "{op} operates on a local SQLite view db; under `--meta-store postgres://…` a base tree is a PG schema (create one with `view create`) and its bytes move through a mounted path — drop --meta-store for a local store"
                    )
                    .into())
                } else {
                    Ok(())
                }
            };
            match action {
                ViewAction::Create {
                    view,
                    manifest,
                    pg_instance,
                } => {
                    // Base (physical) view creation, framed as an explicit
                    // first-class step so the "only base views GC for real" rule
                    // has an obvious entry point. This is the ONLY way a tree
                    // comes into existence: a mount's M_OPEN refuses an unknown
                    // name instead of materializing an empty one.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(grovefs::layout::view_name_error(&view).into());
                    }
                    if ctrl.has_def(&view)? {
                        return Err(format!(
                            "view '{view}' already has a derived/alias/fork definition — choose a different base name (`grovefs view list` shows it)"
                        )
                        .into());
                    }
                    if ctrl.has_physical(&view)? {
                        return Err(format!(
                            "view '{view}' already exists — remove it first (`grovefs view rm --name {view} --base`) or choose a different name"
                        )
                        .into());
                    }
                    if let Some(rc) = &remote_ctl {
                        // The manifest is a FILE here but only TEXT to the
                        // server: the tree it seeds lives on the server's store,
                        // so the lines travel and the seeding happens there.
                        let mut args = serde_json::json!({ "view": view });
                        if let Some(pgi) = pg_instance.as_deref() {
                            args["pg_instance"] = serde_json::json!(pgi);
                        }
                        if let Some(mf) = manifest.as_ref() {
                            let text = std::fs::read_to_string(mf).map_err(|e| {
                                format!("cannot read manifest {}: {e}", mf.display())
                            })?;
                            args["manifest"] = serde_json::json!(text);
                        }
                        let r = rc
                            .call("view.create", args)
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        ui::success(&format!(
                            "{} (via {})",
                            r.as_str().unwrap_or("created"),
                            rc.addr()
                        ));
                    } else if store_is_pg {
                        // A PG base view is one `grovefs_v_<hex(view)>` schema.
                        // `PgEngine::open` creates the schema, its three tables
                        // and the root row; dropping the handle leaves the tree
                        // in place for mounts to open.
                        if manifest.is_some() {
                            return Err("`view create --manifest` seeds a SQLite db file; under `--meta-store postgres://…` mount the view and write the files instead".into());
                        }
                        let spec = store.as_deref().expect("store_is_pg implies a store spec");
                        // Place BEFORE the first open: the home row decides which
                        // instance every later resolve binds this view to. An
                        // explicit `--pg-instance` pins; otherwise least-loaded.
                        let (home, _) =
                            grovefs::pg_store::place_new_view(spec, &view, pg_instance.as_deref())?;
                        drop(grovefs::pg_store::PgEngine::open(spec, &view, false)?);
                        ui::success(&format!(
                            "created BASE view '{view}' (schema grovefs_v_<hex>) on instance '{home}'"
                        ));
                    } else {
                        // A flat pre-v2 store is refused here rather than being
                        // half-upgraded: creating `meta/<view>/` beside the old
                        // flat dbs would leave the two shapes disagreeing about
                        // where this store's trees live.
                        grovefs::meta::layout_guard(&vbase)?;
                        let mp = meta_path(&vbase, &view);
                        if let Some(dir) = mp.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        let meta = MetaEngine::open(&mp)?;
                        ui::success(&format!("created BASE view '{view}' at {}", mp.display()));
                        if let Some(mf) = manifest {
                            // ONE manifest implementation for every entrance
                            // (this CLI arm, the `view.create` control op, MCP):
                            // `mkdir`/`touch` lines, `#` comments, an existing
                            // name is left alone so re-running seeds nothing twice.
                            // The object a `touch` names is created in the
                            // `blocks/` BESIDE this store root - the same
                            // colocated-roots assumption the `meta-server` makes
                            // for this op, so on a deployment whose data-server
                            // holds a different `--data-dir`, create the view
                            // here and write the files through a mount instead.
                            let text = std::fs::read_to_string(&mf).map_err(|e| {
                                format!("cannot read manifest {}: {e}", mf.display())
                            })?;
                            let backend = backend::open_local(&vbase)?;
                            grovefs::mcp::seed_tree_from_text(
                                &meta,
                                Some(backend.as_ref()),
                                &text,
                                unsafe { libc::getuid() },
                                unsafe { libc::getgid() },
                            )
                            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                            ui::info(&format!("applied seed manifest {}", mf.display()));
                        }
                    }
                    ui::note(
                        "deletes in a base view free the underlying object once no other view references it",
                    );
                    ui::hint(
                        "reshape it with `view define` or publish it under a name with `view alias`",
                    );
                }
                ViewAction::Define {
                    name,
                    base: src_base,
                    rules,
                } => {
                    // The view-governance ops live in `grovefs::mcp` — the CLI
                    // and the MCP server are two entrances onto ONE validated
                    // implementation. `--rules` stays a file here; MCP agents
                    // hand the same text inline.
                    let text = std::fs::read_to_string(&rules)
                        .map_err(|e| format!("cannot read rules file {}: {e}", rules.display()))?;
                    print_outlines(grovefs::mcp::define_view(&*ctrl, &name, &src_base, &text)?);
                }
                ViewAction::Fork { name, base, empty } => {
                    // Same shared implementation as the MCP `fork_view` tool
                    // (validation, tree seeding, ledger booking, definition
                    // slot); `--empty` only changes how many rows it starts with.
                    print_outlines(grovefs::mcp::fork_view_seeded(
                        &*ctrl, &name, &base, !empty,
                    )?);
                }
                ViewAction::Graft {
                    name,
                    map,
                    src,
                    dest,
                    skip_existing,
                } => {
                    // Same shared implementation as the MCP `graft_view` tool.
                    // The plan is text either way: a file serves an editor, `-`
                    // serves a pipe — one parser, no second code path.
                    let text = if map.to_str() == Some("-") {
                        std::io::read_to_string(std::io::stdin())
                            .map_err(|e| format!("cannot read plan from stdin: {e}"))?
                    } else {
                        std::fs::read_to_string(&map)
                            .map_err(|e| format!("cannot read plan file {}: {e}", map.display()))?
                    };
                    print_outlines(grovefs::mcp::graft_view(
                        &*ctrl,
                        &name,
                        src.as_deref(),
                        &text,
                        dest.as_deref().unwrap_or(""),
                        skip_existing,
                    )?);
                }
                ViewAction::List { json } => {
                    if json {
                        let s = grovefs::mcp::list_views(&*ctrl)?;
                        let defined: Vec<_> = s
                            .defined
                            .iter()
                            .map(|e| {
                                serde_json::json!({
                                    "name": e.name,
                                    "kind": e.kind,
                                    "real_view": e.real_view,
                                    "hide": e.hide,
                                    "only": e.only,
                                    "reloc": e.reloc,
                                })
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "store": ctrl.describe(),
                                "physical": s.physical,
                                "defined": defined,
                            }))?
                        );
                        return Ok(());
                    }
                    ui::heading("Views");
                    print_outlines(grovefs::mcp::list_views_text(&*ctrl)?);
                    ui::blank();
                    // Inventory hint: keys are per-view and unlimited, so the
                    // "who can open what" question is one filtered listing away.
                    // Named against the SAME entrance this listing came from: a
                    // remote store's keys are behind the control port, not in a
                    // local `--meta-dir` directory.
                    match &remote_ctl {
                        Some(rc) => ui::hint(&format!(
                            "view keys: `grovefs apikey --control-server {} --api-key <admin-key> list [--view NAME]` (add --view to see only that view's keys)",
                            rc.addr()
                        )),
                        None => ui::hint(&format!(
                            "view keys: `grovefs apikey --meta-dir {} list [--view NAME]` (add --view to see only that view's keys)",
                            vbase.display()
                        )),
                    }
                }
                ViewAction::Alias { name, real, retire } => {
                    // Same shared implementation as the MCP `alias_view` tool
                    // (validation, gapless --retire rotation, notes).
                    print_outlines(grovefs::mcp::alias_view(
                        &*ctrl,
                        &name,
                        &real,
                        retire.as_deref(),
                    )?);
                }
                ViewAction::Rm {
                    name,
                    base: remove_base,
                    data_root,
                } => {
                    if !remove_base {
                        // Definition slot removal (derived/alias/fork) — the
                        // shared implementation behind the MCP `remove_view`
                        // tool.
                        print_outlines(grovefs::mcp::remove_view(&*ctrl, &name)?);
                        return Ok(());
                    }
                    // Whole BASE view removal: guards first, then tree + ledger,
                    // then byte reclaim through the store's data root. The guards
                    // below read the LOCAL control plane and this machine's mount
                    // table, so they are backend-independent and run before the
                    // backend split: a PG removal is refused for exactly the
                    // same reasons a SQLite one is.
                    if ctrl.has_def(&name)? {
                        return Err(format!(
                            "'{name}' is governed by a view definition — remove it first with `grovefs view rm --name {name}` (without --base)"
                        )
                        .into());
                    }
                    // A base others' definitions still resolve through must not
                    // vanish from under them: an alias/lens/fork referencing it
                    // would dangle (mounting it afterwards is refused, not
                    // auto-created).
                    for (dname, text) in ctrl.list_defs()? {
                        let dep = match grovefs::layout::LayoutRules::parse_def(&text) {
                            Ok(grovefs::layout::ViewDef::Derived(b, _)) => b == name,
                            Ok(grovefs::layout::ViewDef::Fork(b)) => b == name,
                            Ok(grovefs::layout::ViewDef::Alias(r)) => r == name,
                            Err(_) => false,
                        };
                        if dep {
                            return Err(format!(
                                "view '{dname}' is governed by a definition over base '{name}' — remove it first (`grovefs view rm --name {dname}`) before `view rm --name {name} --base`"
                            )
                            .into());
                        }
                    }
                    if let Some(mp) = view_mounted_at(&name) {
                        return Err(format!(
                            "view '{name}' is mounted here at {mp} — unmount it first (`grovefs umount -m {mp}`); mounts on OTHER machines are not visible in this mount table, stop them too"
                        )
                        .into());
                    }
                    if !grovefs::layout::valid_view_name(&name) {
                        return Err(grovefs::layout::view_name_error(&name).into());
                    }
                    if let Some(rc) = &remote_ctl {
                        // The SERVER runs the same guards (definition dependent,
                        // its own mount table) and owns the store whose tree and
                        // ledger this teardown rewrites — so the whole operation
                        // is one call, not a local half that would leave the
                        // remote ledger over-counting.
                        let mut args = serde_json::json!({ "view": name });
                        if let Some(dr) = &data_root {
                            args["data_root"] = serde_json::json!(dr.display().to_string());
                        }
                        let r = rc
                            .call("view.rm_base", args)
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        ui::success(r.as_str().unwrap_or("removed"));
                        return Ok(());
                    }
                    if store_is_pg {
                        // A PG base view IS a `grovefs_v_<hex>` schema and the
                        // cross-view ledger is `grovefs_refs.holders`, both in the
                        // database — so teardown is "unbook, then drop" inside ONE
                        // transaction (PostgreSQL's DDL is transactional). There is
                        // no local file to unlink and no rebuild pass to run
                        // afterwards: the schema drop and its holds cannot disagree,
                        // even across a crash.
                        let uri = store.as_deref().expect("store_is_pg implies a URI");
                        let rep = grovefs::pg_store::PgEngine::remove_view(uri, &name)?;
                        // Bytes: the ledger just took those refs to zero, so no view
                        // names them any more. Freeing them needs a data root, which
                        // the metadata URI cannot carry — same default and the same
                        // honest accounting as the two arms above.
                        let droot = data_root.clone().unwrap_or_else(|| vbase.clone());
                        let (mut freed, mut missing, mut failed) = (0usize, 0usize, 0usize);
                        let mut root_error = None;
                        match backend::open_local(&droot) {
                            Ok(be) => {
                                for dr in &rep.unbooked_refs {
                                    // Striped keys live as chunks on the data-servers;
                                    // a plain local root cannot free them — report,
                                    // never half-delete.
                                    if dr.starts_with('s') && dr.matches(':').count() >= 2 {
                                        failed += 1;
                                        ui::note(&format!("striped object {dr} not freed from a plain local root — reclaim it through a mount or the data-servers"));
                                        continue;
                                    }
                                    if be.len(dr).is_err() {
                                        missing += 1;
                                        continue;
                                    }
                                    match be.remove(dr) {
                                        Ok(()) => freed += 1,
                                        Err(e) => {
                                            failed += 1;
                                            ui::warn(&format!("cannot free blob {dr}: {e}"));
                                        }
                                    }
                                }
                            }
                            Err(e) => root_error = Some(e),
                        }
                        ui::success(&format!(
                            "removed base view '{name}' from {uri}: schema dropped with {}, {} no longer held by any view{}",
                            ui::plural(rep.entries as usize, "name", "names"),
                            ui::plural(rep.unbooked as usize, "object", "objects"),
                            if rep.still_held > 0 {
                                format!(", {} still held by other views", rep.still_held)
                            } else {
                                String::new()
                            }
                        ));
                        match root_error {
                            Some(e) => ui::note(&format!(
                                "cannot open data root {} to free {}: {e} — the schema and its bookings are gone and those refs are unreferenced now; free them through the data-server or a mount",
                                droot.display(),
                                ui::plural(rep.unbooked as usize, "object", "objects")
                            )),
                            None => ui::info(&format!(
                                "{} freed from {}, {} not found under that data root (wrong --data-dir? they live on another store), {} could not be freed",
                                ui::plural(freed as usize, "object", "objects"),
                                droot.join("blocks").display(),
                                missing,
                                failed
                            )),
                        }
                        ui::note(&format!(
                            "new mounts of '{name}' are now refused — `view create` starts over; this meta-dir's definitions were not touched (the schema is gone, `view list` still shows them)"
                        ));
                        return Ok(());
                    }
                    // SQLite arm. (The PG arm above owns a schema and returned
                    // before it; by here the tree really is
                    // the local file `meta/<name>/<name>.db`.)
                    require_sqlite("`view rm --base`")?;
                    let rm = meta::remove_base_view(&vbase, &name)?;
                    let droot = data_root.unwrap_or_else(|| vbase.clone());
                    // The metadata tree is already gone at this point, so a bare
                    // io error from opening the data root would leave the user
                    // with no clue that the objects are now orphans (§5-4/§6-4).
                    // Name the root and point at `gc`, which reclaims exactly this
                    // "tree deleted, bytes unreleased" state once the root is usable.
                    let backend = backend::open_local(&droot).map_err(|e| format!(
                        "cannot open data root {} to free view '{}' objects: {}. The metadata tree meta/'{}'.db was already removed, so its now-unreferenced bytes are orphans; once the root is writable, reclaim them offline with `grovefs gc --meta-dir {} --data-dir {}` (add --apply to delete) — re-running the rm will not re-touch them",
                        droot.display(),
                        name,
                        e,
                        name,
                        vbase.display(),
                        droot.display()
                    ))?;
                    let (mut freed, mut stayed, mut missing, mut failed) =
                        (0usize, 0usize, 0usize, 0usize);
                    for dr in &rm.blob_refs {
                        if meta::ledger_holds(&vbase, dr).unwrap_or(true) {
                            stayed += 1; // another view still names it
                            continue;
                        }
                        // Striped keys (s<N>:<stripe>:<uuid>) live as chunks on
                        // the data-servers a ShardedBackend knows — a plain local
                        // root cannot free them; report honestly, never half-delete.
                        if dr.starts_with("s") && dr.matches(':').count() >= 2 {
                            failed += 1;
                            ui::note(&format!(
                                "striped object {dr} not freed from a plain local root — reclaim it through a mount or the data-servers"
                            ));
                            continue;
                        }
                        // `remove` is idempotent (Ok for a never-existing key),
                        // so probe existence via `len` FIRST: a wrong
                        // --data-dir must report "not found here", never a
                        // phantom free.
                        if backend.len(dr).is_err() {
                            missing += 1;
                            continue;
                        }
                        match backend.remove(dr) {
                            Ok(()) => freed += 1,
                            Err(e) => {
                                failed += 1;
                                ui::warn(&format!("cannot free blob {dr}: {e}"));
                            }
                        }
                    }
                    ui::success(&format!(
                        "removed base view '{name}': {} gone, {} dropped, {} freed from {}, {} still held by other views{}{}",
                        ui::plural(rm.nodes as usize, "node", "nodes"),
                        ui::plural(rm.blob_refs.len(), "blob reference", "blob references"),
                        ui::plural(freed as usize, "object", "objects"),
                        droot.join("blocks").display(),
                        stayed,
                        if missing > 0 {
                            format!(", {missing} not found under that data root (wrong --data-dir? they live on another store)")
                        } else {
                            String::new()
                        },
                        if failed > 0 {
                            format!(", {failed} could not be freed (see warnings above)")
                        } else {
                            String::new()
                        }
                    ));
                    if missing > 0 {
                        ui::note(&format!(
                            "{} {} NOT deleted — re-run with --data-dir pointing at the store that holds them (default: this metadata root)",
                            ui::plural(missing as usize, "unreferenced blob", "unreferenced blobs"),
                            if missing == 1 { "was" } else { "were" }
                        ));
                    }
                    ui::note(&format!(
                        "new mounts of '{name}' are now refused — run `grovefs view create --view {name}` to start over; live mounts of the removed name on other machines keep their resolved handles until they remount"
                    ));
                }
                ViewAction::Export { view, out, force } => {
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(grovefs::layout::view_name_error(&view).into());
                    }
                    // Where does this view's tree live? Under `--control-server`
                    // the SERVER owns it and the dump is pulled over the wire in
                    // chunks (only `--out` is local). Otherwise either backend
                    // dumps here, through its own reader: a SQLite file or a PG
                    // schema — one portable format, one tool.
                    let st = if let Some(rc) = &remote_ctl {
                        rc.export_view(&view, &out, force)?
                    } else {
                        // The definition lives in the control store, not
                        // necessarily in `meta/views/*.layout`: asking it is what
                        // makes the alias/derived refusal right on both
                        // backends instead of only on the file one.
                        let definition = ctrl.get_def_raw(&view)?;
                        if let Some(text) = definition.as_deref() {
                            match grovefs::layout::LayoutRules::parse_def(text)
                                .map_err(|e| format!("bad view definition '{view}': {e}"))?
                            {
                                grovefs::layout::ViewDef::Alias(real) => {
                                    return Err(format!(
                                        "'{view}' is a pure alias of real view '{real}' — export '{real}' instead (an alias has no metadata tree of its own)"
                                    )
                                    .into())
                                }
                                grovefs::layout::ViewDef::Derived(base, _) => {
                                    return Err(format!(
                                        "'{view}' is a derived (virtual) view over '{base}' — export '{base}' instead (a derived view has no metadata tree of its own)"
                                    )
                                    .into())
                                }
                                grovefs::layout::ViewDef::Fork(_) => {}
                            }
                        }
                        if store_is_pg {
                            let uri = store.as_deref().expect("postgres:// is a store uri");
                            grovefs::pg_store::export_view(uri, &view, &out, force, definition)?
                        } else {
                            grovefs::meta::export_view(&vbase, &view, &out, force)?
                        }
                    };
                    ui::success(&format!(
                        "exported view '{view}' metadata: {}, {} -> {}",
                        ui::plural(st.nodes as usize, "node", "nodes"),
                        ui::plural(st.blobs as usize, "blob reference", "blob references"),
                        out.display()
                    ));
                    if let Some(rc) = &remote_ctl {
                        ui::note(&format!(
                            "the dump was rendered by the control plane at {} and pulled in chunks; only this file was written here",
                            rc.addr()
                        ));
                    }
                    if st.kind == "fork" {
                        ui::note("this is a fork tree; `view import` creates a normal writable base tree unless you pass --as-fork <base>");
                    }
                    ui::note("metadata only — copy the store's `blocks/` tree (the whole data root) as well; blobs are located by their data_ref key, not by any host path");
                }
                ViewAction::Import {
                    view,
                    input,
                    replace,
                    as_fork,
                } => {
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(grovefs::layout::view_name_error(&view).into());
                    }
                    // The control store is the system of record for definitions
                    // on BOTH backends (files and `grovefs_control`), so the
                    // guards below are asked of it rather than
                    // of a `meta/views/*.layout` file a PG deployment may not
                    // even keep.
                    if ctrl.get_def_raw(&view)?.is_some() {
                        return Err(format!(
                            "view '{view}' already has a definition — remove it first (`grovefs view rm --name {view}`) before importing"
                        )
                        .into());
                    }
                    if let Some(base) = &as_fork {
                        if !grovefs::layout::valid_view_name(base) {
                            return Err(format!("invalid base view name '{base}'").into());
                        }
                        if base == &view {
                            return Err("fork name must differ from its base".into());
                        }
                        if ctrl.get_def_raw(base)?.is_some() {
                            return Err(format!(
                                "base '{base}' is governed by a definition (alias/derived/fork) — fork the physical base tree instead"
                            )
                            .into());
                        }
                        if !ctrl.has_physical(base)? {
                            return Err(format!(
                                "no physical base tree '{base}' in this store — import the base first, or omit --as-fork"
                            )
                            .into());
                        }
                    }
                    // The server records `--as-fork` itself (same verdict, same
                    // rollback); locally this end writes the definition below.
                    let def_written = remote_ctl.is_some();
                    let st = if let Some(rc) = &remote_ctl {
                        rc.import_view(&view, &input, replace, as_fork.as_deref())?
                    } else if store_is_pg {
                        let uri = store.as_deref().expect("postgres:// is a store uri");
                        grovefs::pg_store::import_view(uri, &view, &input, replace)?
                    } else {
                        grovefs::meta::import_view(&vbase, &view, &input, replace)?
                    };
                    let kind_word = if as_fork.is_some() {
                        "view"
                    } else {
                        "BASE view"
                    };
                    ui::success(&format!(
                        "imported {kind_word} '{view}' from {}: {}, {}{}; cross-view ledger adjusted",
                        input.display(),
                        ui::plural(st.nodes as usize, "node", "nodes"),
                        ui::plural(st.blobs as usize, "blob reference", "blob references"),
                        if st.replaced {
                            ", replacing the previous tree"
                        } else {
                            ""
                        }
                    ));
                    if let Some(rc) = &remote_ctl {
                        ui::note(&format!(
                            "the dump was pushed in chunks to the control plane at {}; the tree now lives on that store",
                            rc.addr()
                        ));
                    }
                    if let Some(base) = &as_fork {
                        if !def_written {
                            if let Err(e) = ctrl.put_def(&view, &format!("fork {base}\n")) {
                                let rollback =
                                    rollback_imported_tree(&vbase, store.as_deref(), &view);
                                return Err(format!(
                                    "cannot record fork definition for '{view}': {e}; {rollback}"
                                )
                                .into());
                            }
                        }
                        ui::note(&format!(
                            "recorded `fork {base}` — mounts of '{view}' are structure-writable, content read-only (EROFS)"
                        ));
                    } else if st.kind == "fork" {
                        ui::note("the source was a fork tree; imported as a normal writable base view (use --as-fork <base> when you need fork semantics)");
                    }
                    ui::note("content bytes are not in the export — make sure `blocks/` from the source store is present under the target data root before mounting; a cross-machine import copies METADATA only and the CLI never silently copies bytes");
                }
                ViewAction::Move {
                    view,
                    pg_instance,
                    keep_source,
                } => {
                    // Move a tree between instances of a multi-instance store.
                    // This talks to the PostgreSQL instances DIRECTLY (snapshot,
                    // build, cutover, retire), so it runs on the storage host
                    // with --meta-store — not through --control-server — and
                    // only under a PG backend at all.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(grovefs::layout::view_name_error(&view).into());
                    }
                    if remote_ctl.is_some() {
                        return Err("view move talks to the PostgreSQL instances directly — run it on the storage host with --meta-store (drop --control-server)".into());
                    }
                    if !store_is_pg {
                        return Err("view move moves a tree between instances of a multi-instance PostgreSQL store (--meta-store file:/path/…); this store is SQLite and has exactly one home per view — there is nothing to move".into());
                    }
                    if let Some(mp) = view_mounted_at(&view) {
                        return Err(format!(
                            "view '{view}' is mounted here at {mp} — unmount it first (`grovefs umount -m {mp}`); mounts on OTHER machines are not visible in this mount table, quiesce them too (a stale copy is the failure mode)"
                        )
                        .into());
                    }
                    // An alias/derived name has no tree of its own (it renders
                    // another view's) — nothing to move. A fork owns a tree and
                    // moves like any other; its definition stays in the control
                    // plane, but its BASE does not follow: a later graft into
                    // this fork from its base would be a cross-instance pair
                    // and refused (move the base too, or graft from a view on
                    // the fork's new instance).
                    if let Some(text) = ctrl.get_def_raw(&view)? {
                        match grovefs::layout::LayoutRules::parse_def(&text)
                            .map_err(|e| format!("bad view definition '{view}': {e}"))?
                        {
                            grovefs::layout::ViewDef::Alias(real) => {
                                return Err(format!(
                                    "'{view}' is a pure alias of real view '{real}' — an alias has no metadata tree of its own, nothing to move"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Derived(base, _) => {
                                return Err(format!(
                                    "'{view}' is a derived (virtual) view over '{base}' — a derived view has no metadata tree of its own, nothing to move"
                                )
                                .into())
                            }
                            grovefs::layout::ViewDef::Fork(_) => {
                                ui::note(&format!(
                                    "'{view}' is a fork — its tree moves with it, but its base stays where it is; a later graft from the base would be a cross-instance pair and refused"
                                ));
                            }
                        }
                    }
                    let spec = store.as_deref().expect("store_is_pg implies a store spec");
                    let rep = grovefs::pg_store::move_view(spec, &view, &pg_instance, keep_source)?;
                    ui::success(&format!(
                        "moved view '{view}': {}, {}, instance '{}' -> '{}'",
                        ui::plural(rep.nodes as usize, "node", "nodes"),
                        ui::plural(rep.blobs as usize, "blob reference", "blob references"),
                        rep.from,
                        rep.to
                    ));
                    if rep.source_dropped {
                        ui::note(&format!(
                            "source schema dropped on '{}', its ledger holds unbooked (one transaction); cutover is complete and idempotent to re-run",
                            rep.from
                        ));
                    } else {
                        ui::note(
                            "source copy left behind — see the note above for the rollback/cleanup path",
                        );
                    }
                    ui::note(
                        "metadata only: content bytes stay in the data domain — keep mounts resolving the same data servers (server-side gc unions all instances' ledgers, so nothing is reclaimed out from under this view)",
                    );
                }
                ViewAction::Pool {
                    view,
                    write,
                    read,
                    idle_secs,
                    inherit,
                } => {
                    // A view's pool bounds live in its OWN metadata tree (the
                    // `ctrl` rows an engine adopts at open), so — exactly like
                    // `view move` — this talks to the store directly and runs on
                    // the storage host with --meta-store.
                    if !grovefs::layout::valid_view_name(&view) {
                        return Err(grovefs::layout::view_name_error(&view).into());
                    }
                    if remote_ctl.is_some() {
                        return Err("view pool writes into the view's own metadata tree — run it on the storage host with --meta-store (drop --control-server)".into());
                    }
                    if !store_is_pg {
                        return Err("view pool sets PostgreSQL connection-pool bounds; this store is SQLite, whose engine holds one write handle plus a --threads-sized read pool — there is no per-view PG pool to bound".into());
                    }
                    let spec = store.as_deref().expect("store_is_pg implies a store spec");
                    let mut want = grovefs::pg_store::PgPoolSettings::default();
                    if let Some(text) = &write {
                        let (warm, max) = grovefs::pg_store::parse_lane_spec(text)?;
                        want.write_warm = Some(warm);
                        want.write_max = max;
                    }
                    if let Some(text) = &read {
                        let (warm, max) = grovefs::pg_store::parse_lane_spec(text)?;
                        want.read_warm = Some(warm);
                        want.read_max = max;
                    }
                    want.idle_secs = idle_secs;
                    let bare_show = !inherit
                        && want.write_warm.is_none()
                        && want.read_warm.is_none()
                        && want.idle_secs.is_none();
                    let declared = if bare_show {
                        grovefs::pg_store::view_pool_get(spec, &view)?
                    } else {
                        grovefs::pg_store::view_pool_set(spec, &view, want, inherit)?
                    };
                    let lane = |warm: Option<usize>, max: Option<usize>| match (warm, max) {
                        (Some(w), Some(m)) if m != w => format!("{w}:{m}"),
                        // A floor with no ceiling: the burst stays whatever the
                        // serving process allows, only the floor is declared here.
                        (Some(w), None) => format!("{w}:(process ceiling)"),
                        (Some(w), Some(_)) => w.to_string(),
                        // A ceiling with no floor: the floor still comes from
                        // the serving process, only the burst is declared here.
                        (None, Some(m)) => format!("(process floor):{m}"),
                        (None, None) => "(process default)".to_string(),
                    };
                    ui::heading(&format!("view '{view}' pool bounds"));
                    ui::kv_block(
                        2,
                        &[
                            ("write", lane(declared.write_warm, declared.write_max)),
                            ("read", lane(declared.read_warm, declared.read_max)),
                            (
                                "reap after",
                                match declared.idle_secs {
                                    Some(s) => format!("{s}s"),
                                    None => "(process default)".to_string(),
                                },
                            ),
                        ],
                    );
                    ui::blank();
                    if declared == grovefs::pg_store::PgPoolSettings::default() {
                        ui::note(
                            "nothing declared — the view follows the serving meta-server's process bounds; that process prints them at startup (`--pg-write-pool` and friends), and this CLI cannot see another machine's settings",
                        );
                    } else {
                        ui::note(
                            "takes effect on the view's NEXT engine open: unmount it, or wait out --engine-idle-secs — a live mount keeps the pool it already holds (its connections carry session-scoped prepared statements)",
                        );
                    }
                }
            }
        }
        Cmd::Mcp {
            db_path,
            require_api_key,
            store,
        } => {
            // The agent entrance onto the same governance API: an MCP client
            // spawns this over stdio; EOF on stdin shuts it down. Control
            // plane only — no mount, no data IO. `--require-api-key` gates
            // tools/list + tools/call behind a `grovefs apikey` credential. The
            // control plane FOLLOWS the metadata backend: `--meta-store postgres://…`
            // (or `GROVEFS_META_STORE`) binds definitions, keys and the call audit to
            // the shared `grovefs_control` schema; unset governs the local files.
            let store = resolve_store(store);
            refuse_treedb_store(store.as_deref(), "grovefs mcp")?;
            grovefs::mcp::serve(
                db_path.unwrap_or_else(|| base.clone()),
                store.as_deref(),
                require_api_key,
            )?;
        }
        Cmd::Apikey {
            db_path,
            store,
            control_server,
            control_key,
            action,
        } => {
            // Keys + audit log are CONTROL-PLACE state and FOLLOW the metadata
            // backend: a local `api_keys.db` under SQLite, the shared
            // `grovefs_control.api_keys`/`mcp_calls` tables under a
            // `--meta-store postgres://…` — so one mint is authoritative for every
            // server pointing at the URI, with no per-node file to colocate.
            //
            // With `--control-server` the registry is the one INSTALLED IN the
            // remote meta-server's store: an operator rotates keys of a PG *or
            // SQLite* deployment from any machine, holding only an admin key
            // and never the store credential.
            let remote = match control_server.as_deref() {
                Some(addr) => {
                    refuse_local_with_remote(addr, &db_path, &store)?;
                    Some(remote_control(addr, resolve_control_key(control_key))?)
                }
                None => None,
            };
            let ak_base = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            // Where this command's keys/audit actually live, for the messages
            // that point an operator at the next command.
            let where_keys = match &remote {
                Some(rc) => format!("the control plane at {}", rc.client().addr()),
                None => ak_base.display().to_string(),
            };
            let keys = match &remote {
                Some(rc) => KeyAdmin::Remote(rc.client().clone()),
                None => KeyAdmin::Local(grovefs::apikeys::ApiKeyStore::open_for(
                    &ak_base,
                    store.as_deref(),
                )?),
            };
            match action {
                ApikeyAction::Create {
                    label,
                    r#for,
                    view,
                    ro,
                    owner,
                    note,
                    ttl,
                } => {
                    let label = label.trim().to_string();
                    if label.is_empty() {
                        return Err(
                            "`--label` must not be empty: it is the human name shown by \
                             `apikey list` and in the audit trail"
                                .into(),
                        );
                    }
                    if label.chars().any(char::is_control) {
                        return Err(
                            "`--label` must not contain control characters (newlines, tabs); \
                             use `--note` for free text"
                                .into(),
                        );
                    }
                    let scope = grovefs::apikeys::Scope::parse(&r#for)
                        .map_err(Box::<dyn std::error::Error>::from)?;
                    // An admin key with no account is an unattributable
                    // administrator — the one thing the account model exists to
                    // prevent, so it is refused instead of silently defaulted.
                    if scope == grovefs::apikeys::Scope::Admin && owner.is_none() {
                        return Err(format!(
                            "`--for admin` needs `--owner <account>`: an administrator key is \
                             attributed by account in the control-plane audit, and one account \
                             (with several keys) is what makes rotation possible. Retry with e.g.\n  \
                             grovefs apikey create --label {label} --for admin --owner alice"
                        )
                        .into());
                    }
                    let (id, full, expires) = keys
                        .create(
                            &label,
                            scope,
                            view.as_deref(),
                            ro,
                            owner.as_deref(),
                            note.as_deref(),
                            ttl.as_deref(),
                        )
                        .map_err(Box::<dyn std::error::Error>::from)?;
                    // The line has to say which kind of credential the operator
                    // is holding: with wrapping material in force the copy is
                    // recoverable and `secret` is the verb that reads it; without
                    // it the key is show-once again, and saying so HERE is the
                    // difference between a saved string and a surprise next week.
                    if keys.keeps_copies(&id) {
                        ui::success(&format!(
                            "apikey created — a recoverable copy is stored; `grovefs apikey secret --id {id}` reads it back"
                        ));
                    } else {
                        ui::warn(
                            "apikey created — shown ONCE (this store keeps no recoverable copy: no wrapping material is in force, so save it now)",
                        );
                    }
                    ui::blank();
                    let mut facts: Vec<(&str, String)> = vec![
                        ("id", id.clone()),
                        ("owner", owner.as_deref().unwrap_or("<none>").to_string()),
                        ("label", label.clone()),
                        ("scope", scope.as_str().to_string()),
                        ("view", view.as_deref().unwrap_or("<any>").to_string()),
                        (
                            "rw",
                            if ro {
                                "ro (read-only)".to_string()
                            } else {
                                "rw (read-write)".to_string()
                            },
                        ),
                    ];
                    if let Some(n) = note.as_deref().filter(|t| !t.trim().is_empty()) {
                        facts.push(("note", n.to_string()));
                    }
                    // Echoed from the side that stored it, so a deadline printed
                    // here is the deadline the handshake will compare against.
                    facts.push((
                        "expires",
                        match &expires {
                            Some(e) => e.clone(),
                            None => {
                                "never (set one with --ttl, or `apikey update --ttl`)".to_string()
                            }
                        },
                    ));
                    facts.push(("key", full.clone()));
                    ui::kv_block(2, &facts);
                    ui::blank();
                    match scope {
                        // `admin` admits mount/mcp too (`admits` is a table), but
                        // the block below is the one that describes how to USE it;
                        // printing the mount recipe first would bury that.
                        grovefs::apikeys::Scope::Mount | grovefs::apikeys::Scope::Any => {
                            ui::section("Use it");
                            println!("  grovefs mount --manager ... --roster-key-file ... --api-key '{full}' -m /mnt/x");
                            ui::note(&format!(
                                "or export GROVEFS_API_KEY='{full}' so it stays out of the command line"
                            ));
                        }
                        grovefs::apikeys::Scope::Mcp => {
                            ui::section("Use it");
                            ui::info("present it as the `apiKey` param in the MCP `initialize` handshake, or:");
                            println!("  export GROVEFS_MCP_API_KEY='{full}'");
                            println!("  grovefs mcp --require-api-key");
                        }
                        grovefs::apikeys::Scope::Admin => {}
                    }
                    if scope == grovefs::apikeys::Scope::Admin {
                        ui::section("Admin scope");
                        println!(
                            "  grovefs view --control-server HOST:PORT --api-key '{full}' list"
                        );
                        println!("  grovefs apikey --control-server HOST:PORT --api-key '{full}' create --label k2 --for mount");
                        ui::note(&format!(
                            "or export GROVEFS_CONTROL_KEY='{full}' — the operator never needs the store URI; the server performs the work and audits every call under account '{}'",
                            owner.as_deref().unwrap_or("-")
                        ));
                    }
                    if scope == grovefs::apikeys::Scope::Any {
                        ui::note(
                            "scope it to one entrance with `--for mount` / `--for mcp` to keep a mount credential out of the governance API (and vice versa); `--for admin` is the governance credential and is never implied by `any`",
                        );
                    }
                    ui::hint("revoke later with:");
                    println!(
                        "  grovefs apikey {}",
                        match &remote {
                            Some(rc) =>
                                format!("--control-server {} revoke --id {id}", rc.client().addr()),
                            None => format!("--meta-dir {} revoke --id {id}", ak_base.display()),
                        }
                    );
                }
                ApikeyAction::List {
                    view,
                    label,
                    owner,
                    note,
                    scope,
                    json,
                } => {
                    let all = keys.list()?;
                    // Resolve an alias name to its real view so keys bound to
                    // the real name also show under `--view <alias>` — read from
                    // the SAME control plane as the keys (PG rows or local files),
                    // never a path the chosen backend would ignore.
                    let real = match view.as_deref() {
                        None => None,
                        Some(v) => {
                            // The SAME control plane the keys come from: a remote
                            // registry's aliases live behind the remote server, not
                            // in this machine's `--home`.
                            let cs: Arc<dyn grovefs::mcp::ControlStore> = match &remote {
                                Some(rc) => Arc::new(rc.clone()),
                                None => {
                                    grovefs::mcp::control_store_for(&ak_base, store.as_deref())?
                                }
                            };
                            cs.load_alias_map()
                                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                                .get(v)
                                .cloned()
                        }
                    };
                    let rows: Vec<_> = all
                        .iter()
                        .filter(|r| match (&view, &real) {
                            (None, _) => true,
                            (Some(v), real) => {
                                r.view.as_deref() == Some(v.as_str())
                                    || real
                                        .as_deref()
                                        .is_some_and(|x| r.view.as_deref() == Some(x))
                            }
                        })
                        // Substring searches on the human-facing columns and an
                        // exact one on scope — a registry of hundreds is only
                        // manageable if `list` can slice it without `jq`.
                        .filter(|r| {
                            label
                                .as_deref()
                                .is_none_or(|q| r.label.to_lowercase().contains(&q.to_lowercase()))
                        })
                        .filter(|r| {
                            owner.as_deref().is_none_or(|q| {
                                r.owner
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&q.to_lowercase())
                            })
                        })
                        .filter(|r| {
                            scope
                                .as_deref()
                                .is_none_or(|q| r.scope.eq_ignore_ascii_case(q))
                        })
                        .filter(|r| {
                            note.as_deref().is_none_or(|q| {
                                r.note
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&q.to_lowercase())
                            })
                        })
                        .collect();
                    if json {
                        let arr: Vec<_> = rows
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "id": r.id,
                                    "owner": r.owner,
                                    "label": r.label,
                                    "scope": r.scope,
                                    "view": r.view,
                                    "read_only": r.ro,
                                    "created_at": r.created_at,
                                    "last_used_at": r.last_used_at,
                                    "revoked_at": r.revoked_at,
                                    "note": r.note,
                                    "expires_at": r.expires_at,
                                    "expired": r.expired,
                                    "created_by": r.created_by,
                                    "secret_hint": r.secret_hint,
                                    "secret_available": r.secret_available,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&arr)?);
                        return Ok(());
                    }
                    if all.is_empty() && view.is_none() {
                        ui::info(&format!(
                            "no api keys yet (in {where_keys}); mint one with `grovefs apikey create --label L`"
                        ));
                        return Ok(());
                    }
                    let title = match &view {
                        Some(v) => format!("API keys · view '{v}'"),
                        None => "API keys".to_string(),
                    };
                    ui::heading(&title);
                    // A registry of hundreds is scanned, not read line by line.
                    // Wide terminals get one aligned row per key; narrow ones get
                    // a stacked block per key, because squeezing eleven columns
                    // into 70 cells turns every fact into an ellipsis.
                    let w = ui::width();
                    let compact = w != usize::MAX && w < 110;
                    if compact {
                        for (i, r) in rows.iter().enumerate() {
                            ui::section(&format!("{}. {}", i + 1, r.label));
                            ui::kv_block(
                                2,
                                &[
                                    ("id", r.id.clone()),
                                    ("owner", r.owner.as_deref().unwrap_or("-").to_string()),
                                    ("scope", r.scope.to_string()),
                                    ("view", r.view.as_deref().unwrap_or("<any>").to_string()),
                                    ("rw", if r.ro { "ro" } else { "rw" }.to_string()),
                                    ("created", short_time(&r.created_at)),
                                    (
                                        "last used",
                                        r.last_used_at
                                            .as_deref()
                                            .map(short_time)
                                            .unwrap_or_else(|| "-".to_string()),
                                    ),
                                    ("expires", apikey_tail(r)),
                                ],
                            );
                            if i + 1 != rows.len() {
                                ui::blank();
                            }
                        }
                    } else {
                        let mut t = ui::Table::new(vec![
                            ui::Column::right("#").min(2).priority(2),
                            ui::Column::left("id").min(10),
                            ui::Column::left("owner").min(6).priority(2),
                            ui::Column::left("label").min(8),
                            ui::Column::left("scope").min(5).priority(1),
                            ui::Column::left("view").min(6).priority(1),
                            ui::Column::left("rw").min(2).priority(3),
                            ui::Column::left("created").min(10).priority(2).max(19),
                            ui::Column::left("last used").min(10).priority(1).max(19),
                            ui::Column::left("expiry / note").min(13),
                        ]);
                        for (i, r) in rows.iter().enumerate() {
                            // The three tail fields share one trailing column: each
                            // is optional, and a fixed 3 more padded columns would
                            // push the note — the one field worth reading in full —
                            // off the edge.
                            t.row(vec![
                                ui::Cell::muted((i + 1).to_string()),
                                ui::Cell::accent(&r.id),
                                ui::Cell::muted(r.owner.as_deref().unwrap_or("-")),
                                ui::Cell::new(&r.label),
                                ui::Cell::muted(r.scope.to_string()),
                                ui::Cell::new(r.view.as_deref().unwrap_or("<any>")),
                                ui::Cell::muted(if r.ro { "ro" } else { "rw" }),
                                ui::Cell::muted(short_time(&r.created_at)),
                                ui::Cell::muted(
                                    r.last_used_at
                                        .as_deref()
                                        .map(short_time)
                                        .unwrap_or_else(|| "-".to_string()),
                                ),
                                ui::Cell::new(apikey_tail(r)).tone(
                                    if r.revoked_at.is_some() || r.expired {
                                        ui::Tone::Warn
                                    } else {
                                        ui::Tone::Normal
                                    },
                                ),
                            ]);
                        }
                        t.print();
                    }
                    if let Some(v) = &view {
                        ui::blank();
                        ui::info(&format!(
                            "{} for view '{}' of {} total",
                            ui::plural(rows.len(), "bound key", "bound keys"),
                            v,
                            all.len()
                        ));
                        if let Some(real) = &real {
                            ui::note(&format!(
                                "'{}' is a pure alias — keys bound to its real view '{}' are included",
                                v, real
                            ));
                        }
                        let unbound = all.iter().filter(|r| r.view.is_none()).count();
                        if unbound > 0 {
                            ui::note(&format!(
                                "{} admit every view too — plain `apikey list` shows them",
                                ui::plural(unbound, "unbound key", "unbound keys")
                            ));
                        }
                    }
                }
                ApikeyAction::Revoke { id } => {
                    // Accept an exact id, else a UNIQUE prefix (ids are long hex).
                    let all = keys.list()?;
                    let resolved = key_by_id_prefix(&all, &id);
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                ui::info(&format!(
                                    "apikey: id '{}' (label '{}') is already revoked",
                                    k.id, k.label
                                ));
                            } else if keys.revoke(&k.id)? {
                                ui::success(&format!(
                                    "apikey: revoked id '{}' (label '{}'); it can no longer authenticate, past calls keep attribution",
                                    k.id, k.label
                                ));
                                ui::note(
                                    "a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself; a `grovefs mcp` session in flight keeps its identity until it reconnects",
                                );
                            } else {
                                ui::warn(&format!(
                                    "apikey: id '{}' became revoked concurrently",
                                    k.id
                                ));
                            }
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::Rotate { id } => {
                    // Same id resolution as `revoke`: exact, else a UNIQUE prefix.
                    let all = keys.list()?;
                    let resolved = key_by_id_prefix(&all, &id);
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                return Err(format!(
                                    "apikey id='{}' is revoked — rotate revives nothing (mint a new key)",
                                    k.id
                                )
                                .into());
                            }
                            let Some(full) = keys.rotate(&k.id)? else {
                                return Err(format!(
                                    "apikey id='{}' became revoked concurrently",
                                    k.id
                                )
                                .into());
                            };
                            if keys.keeps_copies(&k.id) {
                                ui::success(&format!(
                                    "apikey rotated — the new credential's copy is stored; `grovefs apikey secret --id {}` reads it back",
                                    k.id
                                ));
                            } else {
                                ui::warn(
                                    "apikey rotated — NEW key shown ONCE (this store keeps no recoverable copy: no wrapping material is in force; save it now)",
                                );
                            }
                            ui::blank();
                            ui::kv_block(
                                2,
                                &[
                                    ("id", k.id.clone()),
                                    ("owner", k.owner.as_deref().unwrap_or("<none>").to_string()),
                                    ("label", k.label.clone()),
                                    ("scope", k.scope.to_string()),
                                    ("view", k.view.as_deref().unwrap_or("<any>").to_string()),
                                    ("key", full.clone()),
                                ],
                            );
                            ui::blank();
                            ui::note(
                                "the id and every binding are unchanged — only the secret was replaced; the OLD secret fails its next handshake, and a live mount established on it stays open until it reconnects — `apikey revoke` is the kill-now verb",
                            );
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::Secret { id } => {
                    let all = keys.list()?;
                    let resolved = key_by_id_prefix(&all, &id);
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            let verdict = keys
                                .reveal(&k.id)
                                .map_err(Box::<dyn std::error::Error>::from)?;
                            let Some(full) = verdict.recoverable() else {
                                // Not a failure of the store: three different
                                // states, each with its own repair, and the
                                // sentence says which. Exit non-zero anyway — an
                                // operator scripting on this needs the credential
                                // or needs to stop.
                                return Err(format!(
                                    "apikey secret: '{}' ({}) — {}",
                                    k.id,
                                    k.label,
                                    verdict.refusal().unwrap_or("no reason reported")
                                )
                                .into());
                            };
                            ui::heading("apikey secret — the stored copy of this credential");
                            ui::kv_block(
                                2,
                                &[
                                    ("id", k.id.clone()),
                                    ("owner", k.owner.as_deref().unwrap_or("<none>").to_string()),
                                    ("label", k.label.clone()),
                                    ("scope", k.scope.to_string()),
                                    ("view", k.view.as_deref().unwrap_or("<any>").to_string()),
                                    ("key", full.to_string()),
                                ],
                            );
                            ui::blank();
                            if k.revoked_at.is_some() {
                                ui::warn(
                                    "this key is REVOKED: reading it back does not restore it; `apikey rotate` is the verb that mints a working secret",
                                );
                            } else {
                                ui::note(
                                    "authentication still runs on the stored digest; this copy is for handover",
                                );
                                // The promise and its boundary, in the same
                                // breath: a server write-down exists only for
                                // the path that goes through a server.
                                if keys.reads_are_audited() {
                                    ui::note("this read is in the server's audit trail (`apikey calls`)");
                                } else {
                                    ui::warn(
                                        "a direct read of the local store is NOT audited — whoever can open that file already owns it; a read through a meta-server (`--control-server`) is, and shows in `apikey calls`",
                                    );
                                }
                            }
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::SetRo { id, rw, .. } => {
                    // Default (neither flag) and `--ro` set read-only; `--rw`
                    // sets read-write. `ro` conflicts with `rw` at parse time.
                    let ro_now = !rw;
                    // Same id resolution as `revoke`: exact, else a UNIQUE prefix.
                    let all = keys.list()?;
                    let resolved = key_by_id_prefix(&all, &id);
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                return Err(format!(
                                    "apikey id='{}' is revoked — set-ro never resurrects a revoked key (mint a new one)",
                                    k.id
                                )
                                .into());
                            }
                            if keys.set_ro(&k.id, ro_now)? {
                                if ro_now {
                                    ui::success(&format!(
                                        "apikey: id '{}' (label '{}') is now ro (read-only)",
                                        k.id, k.label
                                    ));
                                    ui::note(
                                        "a LIVE mount presenting this key starts refusing writes on its next re-check window (≤1s) — no reconnect, no remount; its reads keep flowing",
                                    );
                                } else {
                                    ui::success(&format!(
                                        "apikey: id '{}' (label '{}') is now rw (read-write)",
                                        k.id, k.label
                                    ));
                                    ui::note(
                                        "a LIVE mount presenting this key resumes admitting writes on its next re-check window (≤1s) — no reconnect, no remount",
                                    );
                                }
                            } else {
                                return Err(
                                    format!("apikey id='{}' became revoked concurrently", k.id).into()
                                );
                            }
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::Update { id, note, ttl } => {
                    if note.is_none() && ttl.is_none() {
                        return Err(
                            "nothing to update: pass --note TEXT and/or --ttl DUR (`--ttl never` removes a deadline)"
                                .into(),
                        );
                    }
                    // Same id resolution as `revoke`: exact, else a UNIQUE prefix.
                    let all = keys.list()?;
                    let resolved = key_by_id_prefix(&all, &id);
                    match resolved.len() {
                        0 => {
                            return Err(format!(
                                "no api key with id '{id}' (see `grovefs apikey list`)"
                            )
                            .into())
                        }
                        1 => {
                            let k = resolved[0];
                            if k.revoked_at.is_some() {
                                return Err(format!(
                                    "apikey id='{}' is revoked — update edits nothing on a dead key (mint a new one)",
                                    k.id
                                )
                                .into());
                            }
                            keys.update(&k.id, note.as_deref(), ttl.as_deref())?;
                            // Re-read the row instead of echoing the flags: on the
                            // remote path the SERVER resolved `--ttl` against its own
                            // clock, and that answer is the one worth printing.
                            let after = keys.list()?.into_iter().find(|x| x.id == k.id);
                            ui::success(&format!(
                                "apikey updated: id '{}' (label '{}')",
                                k.id, k.label
                            ));
                            if let Some(x) = after {
                                ui::kv_block(
                                    2,
                                    &[
                                        ("note", x.note.as_deref().unwrap_or("<none>").to_string()),
                                        (
                                            "expires",
                                            match x.expires_at.as_deref() {
                                                Some(e) if x.expired => format!(
                                                    "{e}  (EXPIRED — it authenticates nowhere until --ttl gives it a new deadline)"
                                                ),
                                                Some(e) => e.to_string(),
                                                None => "never".to_string(),
                                            },
                                        ),
                                    ],
                                );
                            }
                        }
                        n => {
                            return Err(format!(
                                "ambiguous id prefix '{id}' matches {n} keys — use a longer prefix or the full id"
                            )
                            .into())
                        }
                    }
                }
                ApikeyAction::Purge { before, dry_run } => {
                    let (cutoff, older, deleted) = keys.purge(&before, dry_run)?;
                    if dry_run {
                        ui::info(&format!(
                            "apikey purge --dry-run: {} in {where_keys} are older than {cutoff} — nothing was deleted",
                            ui::plural(older as usize, "audit row", "audit rows")
                        ));
                        if older > 0 {
                            ui::hint(&format!(
                                "drop them: rerun without --dry-run, or start the meta-server with --audit-retention {before} to sweep this automatically"
                            ));
                        }
                    } else {
                        ui::success(&format!(
                            "apikey purged: {} older than {cutoff} deleted from {where_keys} (the count was {older} when the cut began)",
                            ui::plural(deleted as usize, "audit row", "audit rows")
                        ));
                        ui::note(
                            "this is the trail `grovefs apikey calls` reads: what is gone cannot be listed again",
                        );
                    }
                }
                ApikeyAction::Calls { limit, json } => {
                    let rows = keys.recent_calls(limit)?;
                    if json {
                        let arr: Vec<_> = rows
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "ts": r.ts,
                                    "key_id": r.key_id,
                                    "key_label": r.key_label,
                                    "method": r.method,
                                    "tool": r.tool,
                                    "ok": r.ok,
                                    "detail": r.detail,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&arr)?);
                        return Ok(());
                    }
                    if rows.is_empty() {
                        ui::info(&format!("no logged calls yet (in {where_keys}; a `grovefs mcp` run appends one row per tools/call, a `--require-api-key` meta/data-server one per mount connect, and a `--control-listen` server one per governance call)"));
                    } else {
                        ui::heading(&format!("API calls · last {}, newest first", rows.len()));
                        let mut t = ui::Table::new(vec![
                            ui::Column::left("timestamp").min(16),
                            ui::Column::left("key id").min(10).priority(1),
                            ui::Column::left("method").min(8),
                            ui::Column::left("tool").min(8).priority(1),
                            ui::Column::left("ok").min(2),
                            ui::Column::left("detail").min(12),
                        ]);
                        for r in &rows {
                            t.row(vec![
                                ui::Cell::muted(short_time(&r.ts)),
                                ui::Cell::accent(r.key_id.as_deref().unwrap_or("-")),
                                ui::Cell::new(&r.method),
                                ui::Cell::new(if r.tool.is_empty() {
                                    "-"
                                } else {
                                    r.tool.as_str()
                                }),
                                ui::Cell::new(if r.ok { "yes" } else { "no" }).tone(if r.ok {
                                    ui::Tone::Success
                                } else {
                                    ui::Tone::Error
                                }),
                                ui::Cell::muted(&r.detail),
                            ]);
                        }
                        t.print();
                    }
                }
            }
        }
        #[cfg(feature = "web")]
        Cmd::Web {
            control_server,
            control_key,
            control_key_file,
            listen,
            session_ttl,
            workers,
            allow_insecure_http,
            use_x_forwarded_for,
            action,
        } => {
            // ONE credential, three sources, one precedence: `--api-key` >
            // `--api-key-file` > `GROVEFS_CONTROL_KEY` > `GROVEFS_API_KEY`.
            let explicit = match (control_key, control_key_file) {
                (Some(k), _) => Some(k),
                (None, Some(p)) => Some(read_secret_file(&p, "--api-key-file")?),
                (None, None) => None,
            };
            let addr = control_server.ok_or_else(|| -> Box<dyn std::error::Error> {
                "grovefs web needs --control-server HOST:PORT. The console is a gateway in \n  front of `meta-server --control-listen HOST:PORT`: it holds no accounts, views, keys \n  or audit of its own, so there is nothing for it to serve without that address.\n  Try: grovefs web --control-server 127.0.0.1:9502 --api-key-file /etc/grovefs/control.key"
                    .into()
            })?;
            let key = resolve_control_key(explicit).ok_or_else(
                || -> Box<dyn std::error::Error> {
                    format!(
                        "--control-server {addr} needs the console's admin credential: --api-key \n  '<id>:<secret>', --api-key-file PATH (first line), or GROVEFS_CONTROL_KEY. It must be an\n  `admin`-scope key — mint one on the server host with `grovefs apikey --meta-dir \n  <server-meta-dir> create --for admin --owner <account>` (the first one cannot come \n  from the governance API, or that API would install its own administrators)."
                    )
                    .into()
                },
            )?;
            // A POOLED client, and the worker count is the ceiling: the gateway
            // is the one caller whose requests keep arriving for days, so it is
            // the one that must not pay a connect, a server thread and two store
            // queries per click. Everything else here stays on the one-request
            // connection — for a CLI the socket it opens is the only one it will
            // ever need.
            //
            // `GROVEFS_GATEWAY_CTL_PIN=0` puts the gateway back on that
            // one-request path, at runtime, without swapping binaries. It exists
            // because the pooled win is a MEASURED one and only half of it is
            // big: serial op latency and per-op server CPU fell clearly, while
            // the concurrent throughput ceiling moved by less than the
            // run-to-run spread of the measurement itself. Anyone repeating that
            // comparison needs both arms inside one deployment, not two
            // upgrades — and an operator who finds a connection reuse misbehaving
            // needs a way to turn it off that is not a recompile.
            let pin = std::env::var("GROVEFS_GATEWAY_CTL_PIN")
                .map(|v| v != "0")
                .unwrap_or(true);
            let client = if pin {
                grovefs::control_remote::ControlClient::pooled(&addr, &key, workers.max(1))
            } else {
                grovefs::control_remote::ControlClient::new(&addr, &key)
            };
            match action {
                // The gateway: foreground, one accept loop, no daemonizing — the
                // same contract as every other server subcommand here, so a unit
                // file's `ExecStart` works for all of them.
                None => {
                    let ttl = grovefs::gc::parse_age(&session_ttl).map_err(
                        |e| -> Box<dyn std::error::Error> {
                            format!("--session-ttl '{session_ttl}': {e}").into()
                        },
                    )?;
                    if ttl.is_zero() {
                        return Err("--session-ttl cannot be 0: every request would sign itself out \n                        before it answered.".into());
                    }
                    grovefs::web::serve(grovefs::web::Config {
                        listen,
                        control: client,
                        session_ttl: ttl,
                        xff: use_x_forwarded_for,
                        workers: workers.max(1),
                        allow_insecure_http,
                    })?;
                }
                Some(a) => web_govern(&client, a)?,
            }
        }
        Cmd::Refs {
            db_path,
            store,
            control_server,
            control_key,
            action,
        } => {
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &store)?;
                let rc = remote_control(addr, resolve_control_key(control_key))?;
                match action {
                    RefsAction::Rebuild => {
                        // The recount reads the TREES, so it must run where they
                        // live; the server applies it to whatever backend it
                        // serves and answers in its own wording.
                        let r = rc
                            .client()
                            .call("refs.rebuild", serde_json::json!({}))
                            .map_err(Box::<dyn std::error::Error>::from)?;
                        ui::success(r.as_str().unwrap_or("refs ledger rebuilt"));
                        ui::note(
                            "run it QUIESCED: a booking committed between the server's read pass and write pass would be overwritten with the pre-booking count",
                        );
                        return Ok(());
                    }
                }
            }
            let rbase = db_path.unwrap_or_else(|| base.clone());
            let store = resolve_store(store);
            refuse_treedb_store(store.as_deref(), "grovefs refs")?;
            match action {
                RefsAction::Rebuild => {
                    if store.as_deref().is_some_and(grovefs::pg_store::is_pg_store) {
                        // The ledger is `grovefs_refs.holders` IN the database, so
                        // the recount has to happen there too: the same "read the
                        // TREES, not the ledger" rule as the two arms above, and
                        // the same repair of both directions (a lost booking AND
                        // an over-count). Nothing here frees bytes — that is the
                        // data servers' business, and `gc` refuses a store URI.
                        let uri = store.as_deref().expect("just matched a pg uri");
                        let rep = grovefs::pg_store::rebuild_ledger(uri)?;
                        ui::success(&format!(
                            "refs ledger rebuilt across every instance of {uri}: {}, {}, {}; {} corrected, {} deleted",
                            ui::plural(rep.trees as usize, "tree", "trees"),
                            ui::plural(rep.refs as usize, "content key", "content keys"),
                            ui::plural(rep.holders as usize, "holder unit", "holder units"),
                            ui::plural(rep.rows_fixed as usize, "row", "rows"),
                            ui::plural(rep.rows_deleted as usize, "row", "rows")
                        ));
                        ui::note(
                            "the recount reads the TREES (every grovefs_v_* schema), not the ledger — so a lost booking is restored (the unsafe direction) as well as an over-count dropped; nothing here frees object bytes (that is the data servers' business)",
                        );
                        ui::note(
                            "run it QUIESCED: a booking committed between the read pass and the write pass would be overwritten with the pre-booking count",
                        );
                        return Ok(());
                    }
                    let counted = grovefs::meta::rebuild_ledger(&rbase)?;
                    ui::success(&format!(
                        "refs ledger rebuilt at {}: {} holder entries across the surviving views",
                        grovefs::meta::control_db_path(&rbase, "refs").display(),
                        counted
                    ));
                }
            }
        }
        Cmd::Layout { db_path, action } => {
            let lbase = db_path.unwrap_or_else(|| base.clone());
            match action {
                LayoutAction::Show => layout_show(&lbase)?,
                LayoutAction::SetFamily { mode } => {
                    let policy = grovefs::meta::FamilyLayout::parse(&mode).ok_or_else(|| {
                        format!("unknown family mode '{mode}' — expected flat, nested or grouped")
                    })?;
                    grovefs::meta::set_family_policy(&lbase, policy)?;
                    ui::success(&format!(
                        "family policy for {} set to {} — NEW forks and definition slots land {}",
                        grovefs::meta::meta_dir(&lbase).display(),
                        ui::strong(policy.as_str()),
                        match policy {
                            grovefs::meta::FamilyLayout::Nested =>
                                "inside each base view's directory",
                            grovefs::meta::FamilyLayout::Grouped =>
                                "under each base's forks/ and views/ separators",
                            grovefs::meta::FamilyLayout::Flat => "at the top level, pre-v3 style",
                        }
                    ));
                    ui::hint("existing files keep their homes; every read resolves by scanning");
                }
                LayoutAction::Migrate { dry_run } => layout_migrate(&lbase, dry_run)?,
            }
        }
        Cmd::Gc {
            db_path,
            data_root,
            older_than,
            apply,
            json,
            force,
            control_server,
            control_key,
            data_servers,
            data_token,
            data_api_key,
        } => {
            // The REMOTE audit: the server reads the trees where they live and
            // we only name the bytes. This is the one form that works under PG,
            // where no `<data-dir>` on this machine holds them.
            if let Some(addr) = control_server.as_deref() {
                refuse_local_with_remote(addr, &db_path, &None)?;
                if data_root.is_some() {
                    return Err(format!(
                        "--control-server {addr} audits bytes on the DATA SERVERS, so --data-dir cannot be combined with it — name the roots with --data-servers HOST:PORT instead"
                    )
                    .into());
                }
                let rc = remote_control(addr, resolve_control_key(control_key))?;
                let servers: Vec<String> = data_servers
                    .or_else(|| {
                        std::env::var("GROVEFS_DATA_SERVERS")
                            .ok()
                            .filter(|s| !s.is_empty())
                    })
                    .map(|s| {
                        s.split(',')
                            .map(str::trim)
                            .filter(|a| !a.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if servers.is_empty() {
                    return Err(format!(
                        "gc --control-server {addr} needs --data-servers HOST:PORT[,HOST:PORT] \
                         (or GROVEFS_DATA_SERVERS): the server owns the trees, the data servers own \
                         the bytes"
                    )
                    .into());
                }
                let token = data_token
                    .or_else(|| std::env::var("GROVEFS_TOKEN").ok())
                    .unwrap_or_default();
                let dkey = data_api_key
                    .or_else(|| std::env::var("GROVEFS_API_KEY").ok())
                    .unwrap_or_default();
                let op = if apply { "gc.apply" } else { "gc.scan" };
                let rep = rc
                    .client()
                    .call(
                        op,
                        serde_json::json!({
                            "data_servers": servers,
                            "data_token": token,
                            "data_api_key": dkey,
                            "older_than": older_than,
                            "force": force,
                        }),
                    )
                    .map_err(Box::<dyn std::error::Error>::from)?;
                if json {
                    println!("{rep}");
                } else {
                    print_gc_remote(&rep);
                }
                return Ok(());
            }
            // The trees this command audits are local `<view>.db` files. Under
            // PG they are schemas and the bytes belong to the data servers —
            // there is no offline page tree to walk here, so this refuses rather
            // than reporting "nothing to reclaim".
            let offline_store = resolve_store(None);
            refuse_treedb_store(offline_store.as_deref(), "grovefs gc")?;
            if offline_store
                .as_deref()
                .is_some_and(grovefs::pg_store::is_pg_store)
            {
                return Err("`gc` audits LOCAL page files under <data-dir>/blocks; a PostgreSQL deployment's trees are schemas and its bytes are owned by the data servers — reclaim there with the DBA/data-server tooling, not this command".into());
            }
            let mbase = db_path.unwrap_or_else(|| base.clone());
            let root_arg = data_root.unwrap_or_else(|| mbase.clone());
            let roots: Vec<PathBuf> = root_arg
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("gc: no data root to scan (pass --data-dir)".into());
            }
            let age = grovefs::gc::parse_age(&older_than)?;
            let views = grovefs::gc::physical_views(&mbase)?;
            // Offline gate: a live writable mount of any tree here (or an alias
            // that writes through to one) can commit a node row between our
            // scan and delete. A derived lens is read-only by contract and is
            // not a blocker.
            if apply {
                let live = gc_live_mounts(&mbase, &roots, &views);
                if !live.is_empty() {
                    return Err(format!(
                        "gc --apply refused: this store has live mounts — {}; unmount them first (reclaim must run offline)",
                        live.join("; ")
                    )
                    .into());
                }
            }
            let users = local_store_users(&mbase, &roots);
            if !users.is_empty() {
                let list = users.join("; ");
                if apply {
                    return Err(format!(
                        "gc --apply refused: this store is in use by local grovefs process(es) — {list}; stop them first (a hard-linked import can expose an OLD mtime before its node row commits, so the grace window alone cannot protect it)"
                    )
                    .into());
                }
                ui::note(&format!(
                    "this store is in use by local grovefs process(es) — {list}"
                ));
            }
            let rep = grovefs::gc::scan(&mbase, &roots, age)?;
            // Wrong-root guard: a data root that shares ZERO objects with the
            // surviving trees is almost always a --meta-dir/--data-dir mixup,
            // and deleting there would kill another store's bytes.
            if rep.objects > 0 && rep.refs > 0 && rep.matched == 0 && !force {
                return Err(format!(
                    "gc: no data root shares an object with any view tree in {} ({}, {}) — wrong --meta-dir/--data-dir? Nothing was deleted; pass --force only if this is expected",
                    mbase.display(),
                    ui::plural(rep.objects as usize, "object", "objects"),
                    ui::plural(rep.refs as usize, "referenced key", "referenced keys")
                )
                .into());
            }
            let reclaimed = if apply {
                Some(grovefs::gc::reclaim(&rep.orphans, age))
            } else {
                None
            };
            if json {
                println!("{}", gc_json(&rep, reclaimed.as_ref()));
            } else {
                print_gc_report(&rep, reclaimed.as_ref());
            }
        }
        Cmd::Import {
            db_path,
            store,
            data_root,
            view,
            source,
            prefix,
            since,
            full,
            dry_run,
        } => {
            let md = std::fs::metadata(&source)
                .map_err(|e| format!("import source {}: {e}", source.display()))?;
            if !md.is_dir() {
                return Err("import --source must be a directory".into());
            }
            let mbase = db_path.unwrap_or_else(|| base.clone());
            let imp_store = resolve_store(store);
            // Import writes the authoritative PHYSICAL tree directly, so a
            // governed name (derived / alias / fork) must not be an import
            // target — it would make a shadow tree nothing ever reads. Defs
            // live in the local control home for SQLite and treedb; for PG this
            // is best-effort (its defs are in the database), which is why the
            // engine still refuses to mount a virtual name as writable.
            let slot = grovefs::meta::find_def_slot(&mbase, &view); // v3: family dirs included
            if let Some(slot) = slot {
                let target = grovefs::layout::LayoutRules::parse_def_file(&slot)
                    .map(|d| match d {
                        grovefs::layout::ViewDef::Alias(real) => format!(
                            "'{view}' is a pure alias of real view '{real}' — import into '{real}' directly (an alias is not a storage identity)"
                        ),
                        grovefs::layout::ViewDef::Derived(..) => format!(
                            "import target '{view}' is a derived (virtual) view — import into its physical base view instead (derived views mount read-only)"
                        ),
                        grovefs::layout::ViewDef::Fork(base) => format!(
                            "import target '{view}' is a fork of base '{base}' — a fork refuses content writes (EROFS); import into the base view '{base}' instead"
                        ),
                    })
                    .unwrap_or_else(|_| {
                        format!(
                            "import target '{view}' has a view definition — import into its physical base view instead"
                        )
                    });
                return Err(target.into());
            }
            let droot = data_root.unwrap_or_else(|| mbase.clone());
            let prefix = prefix.unwrap_or_else(|| {
                source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "imported".to_string())
            });
            let prefix = format!("/{}", prefix.trim_matches('/'));
            if dry_run {
                // Literally touch nothing: not even a store is opened. The
                // source-side tally is backend-agnostic.
                let mut st = grovefs::import::ImportStats::default();
                grovefs::import::count_walk(&source, &mut st)?;
                ui::info(&format!(
                    "dry run: view '{view}' under {} would mirror {} and {} \
                     (symlinks/special files and names already in the view are skipped \
                     on the real run)",
                    prefix,
                    ui::plural(st.dirs, "dir", "dirs"),
                    ui::plural(st.files, "file", "files")
                ));
                return Ok(());
            }
            // Open the target view's metadata store for THIS deployment's
            // backend (SQLite / PostgreSQL / treedb), creating a new BASE view
            // when the name owns no tree yet — the same "the store owns its
            // tree" contract as `view create`. Under treedb the placement comes
            // from the shared find_tree_home / plan_placement helpers, so the
            // name lands on the SAME engine instance the meta-server routes it
            // to (the engine is shared state, so the running server sees the
            // nodes on its next read).
            let (meta, created_base) = open_import_store(imp_store.as_deref(), &mbase, &view)?;
            // Same multi-root contract as `data-server --data-dir`: a
            // comma-separated --data-dir registers imported objects onto the
            // disk their key hashes to, colocated with the running server, using
            // the identical `blocks/<bucket>/<key>` path a mounted write uses.
            let roots: Vec<PathBuf> = droot
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if roots.is_empty() {
                return Err("import --data-dir: no usable path".into());
            }
            let backend = backend::open_local_roots(&roots)?;
            // Incremental prune: an explicit --since wins; otherwise reuse the
            // watermark saved by the last successful import of this stream
            // (unless --full forces a rescan). Name-skip still guarantees
            // idempotence even on a full walk.
            let abs_src = std::fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
            let wm_key = grovefs::import::watermark_key(&view, &abs_src, &prefix);
            let since = if full {
                None
            } else {
                since.or_else(|| grovefs::import::load_watermark(&mbase, &wm_key))
            };
            let mut dirs: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
            dirs.insert("/".to_string(), meta::ROOT_INO);
            grovefs::import::ensure_dir_soft(&meta, &prefix, &mut dirs)?;
            let mut st = grovefs::import::ImportStats::default();
            grovefs::import::ingest(
                &meta,
                backend.as_ref(),
                &source,
                &prefix,
                &mut dirs,
                &mut st,
                since,
            )?;
            if created_base == Some(true) {
                ui::info(&format!(
                    "created BASE view '{view}' (target was absent); imported {}",
                    ui::plural(st.linked + st.copied, "object", "objects")
                ));
            }
            // Persist the watermark so the next run prunes unchanged subtrees.
            if let Some(w) = st.watermark {
                if let Err(e) = grovefs::import::save_watermark(&mbase, &wm_key, w) {
                    ui::warn(&format!(
                        "import: could not save the incremental watermark: {e}"
                    ));
                }
            }
            ui::success(&format!(
                "imported into view '{view}' under {}: {}, {} \
                 ({} hardlinked zero-copy, {} copied), {} skipped, {} failed",
                prefix,
                ui::plural(st.dirs, "dir", "dirs"),
                ui::plural(st.linked + st.copied, "object", "objects"),
                st.linked,
                st.copied,
                st.skipped,
                st.failed
            ));
            if st.failed > 0 {
                return Err(format!(
                    "{} failed to import",
                    ui::plural(st.failed, "entry", "entries")
                )
                .into());
            }
        }
        Cmd::Du {
            view,
            meta,
            meta_token,
            meta_api_key,
            store,
            db_path,
            data,
            data_root,
            api_key,
            token,
            path,
            depth,
            bytes,
            total,
        } => {
            let opts = grovefs::du::DuOpts {
                view,
                meta: meta.or_else(|| {
                    std::env::var("GROVEFS_META_SERVER")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                meta_token: meta_token.or_else(|| {
                    std::env::var("GROVEFS_TOKEN")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                meta_api_key: meta_api_key.or_else(|| {
                    std::env::var("GROVEFS_API_KEY")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                store: resolve_store(store),
                db_path: db_path.or_else(|| Some(base.clone())),
                data: data.or_else(|| {
                    std::env::var("GROVEFS_DATA_SERVERS")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                data_root,
                api_key: api_key.or_else(|| {
                    std::env::var("GROVEFS_API_KEY")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                token: token.or_else(|| {
                    std::env::var("GROVEFS_TOKEN")
                        .ok()
                        .filter(|s| !s.is_empty())
                }),
                path,
                depth: if total { 0 } else { depth },
                bytes,
            };
            grovefs::du::run(&opts).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Open the metadata store `grovefs import` writes into, for THIS deployment's
/// backend, creating the target view's tree when it is absent — the same "the
/// store owns its tree" contract as `view create`. Returns the store plus
/// `Some(true)` when a brand-new BASE view tree was created (SQLite / treedb;
/// `None` when the answer cannot be decided cheaply, e.g. PG). Under treedb the
/// placement comes from the shared `find_tree_home` / `plan_placement` helpers,
/// so the name lands on the SAME engine instance the meta-server routes it to —
/// the engine holds the tree, so a running server sees the imported nodes on its
/// very next read (no stale window beyond the dentry/attr TTL).
fn open_import_store(
    store: Option<&str>,
    mbase: &Path,
    view: &str,
) -> Result<(grovefs::meta::MetaStore, Option<bool>), Box<dyn std::error::Error>> {
    let uri = store.unwrap_or("");
    if grovefs::pg_store::is_pg_store(uri) {
        // open_pg builds the view's schema and its `node` table if absent; the
        // per-import "created" answer is not worth a second round trip, so the
        // INFO line about a newly created base view is simply skipped here.
        let meta = grovefs::meta::MetaStore::open_pg(uri, view, false)?;
        return Ok((meta, None));
    }
    #[cfg(feature = "treedb")]
    if grovefs::treedb_store::is_treedb_store(uri) {
        let addrs = grovefs::treedb_store::parse_addrs(uri)
            .map_err(|e| format!("import --meta-store: {e}"))?;
        if addrs.is_empty() {
            return Err("import --meta-store: no treedb addresses given".into());
        }
        let home = grovefs::treedb_store::find_tree_home(&addrs, view)
            .map_err(|e| format!("import --meta-store: {e}"))?;
        let created = home.is_none();
        let addr =
            home.unwrap_or_else(|| grovefs::treedb_store::plan_placement(&addrs, view).addr);
        let meta = grovefs::meta::MetaStore::open_treedb(&addr, view, false)?;
        return Ok((meta, Some(created)));
    }
    #[cfg(not(feature = "treedb"))]
    if uri.starts_with("treedb://") {
        return Err(format!(
            "import --meta-store '{uri}': this build cannot drive the treedb engine directly \
             (not compiled with `--features treedb`); import against the local SQLite \
             deployment or mount the view and copy the tree in"
        )
        .into());
    }
    // Everything else — no store, or a plain-path spec the `view` verbs also
    // treat as local (the same `is_pg_store` funnel as `rollback_imported_tree`
    // and the meta-server's store selection) — is the LOCAL SQLite deployment:
    // meta/<view>/<view>.db plus hardlinks/copies under blocks/.
    let created = !meta_path(mbase, view).exists();
    let meta = grovefs::meta::MetaStore::open_sqlite(mbase, view, false)?;
    Ok((meta, Some(created)))
}

// ---- Detached (background) mounts ------------------------------------------
//
// `mount` without `--foreground` re-execs this exact command line with
// `--foreground` appended, in a brand-new session (setsid) so the mount
// survives its shell/terminal. Readiness is confirmed by polling
// /proc/mounts: the parent returns only once the FUSE mount is actually
// live (or with the child's failure), which makes scripts deterministic —
// no sleep loops. Stdio: if the parent is attached to a terminal the
// child's output goes to a log file (`--log-file`, default
// `$TMPDIR/grovefs-mount-<mountpoint>.log` — deliberately NOT under
// `--home`, a detached mount still writes nothing there); if the parent's
// stdio is already redirected (a script's `> x.log 2>&1`), the child
// inherits those descriptors so existing sinks and greps keep working.
fn daemonize_and_await(
    view: &str,
    mountpoint: &Path,
    log_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let attach_tty = std::io::stdout().is_terminal() || std::io::stderr().is_terminal();
    let log_path = match log_file {
        Some(p) => Some(p.to_path_buf()),
        None if attach_tty => {
            let name = mountpoint
                .to_string_lossy()
                .replace(['/', ':'], "-")
                .trim_matches('-')
                .to_string();
            Some(std::env::temp_dir().join(format!(
                "grovefs-mount-{}.log",
                if name.is_empty() { "root" } else { &name }
            )))
        }
        None => None,
    };

    // Candidate strings /proc/mounts may show for this mountpoint (raw, cwd-
    // joined, kernel-canonical). The dir is created here ONLY so canonicalize
    // can resolve it pre-mount; the child re-creates it idempotently.
    let mut wants = vec![mountpoint.to_string_lossy().to_string()];
    if !mountpoint.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            wants.push(cwd.join(mountpoint).to_string_lossy().into_owned());
        }
    }
    let _ = std::fs::create_dir_all(mountpoint);
    if let Ok(c) = mountpoint.canonicalize() {
        let s = c.to_string_lossy().into_owned();
        if !wants.contains(&s) {
            wants.push(s);
        }
    }

    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1)).arg("--foreground");
    match &log_path {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::from(f.try_clone()?))
                .stderr(Stdio::from(f));
        }
        None => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn background mount: {e}"))?;
    let pid = child.id();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if mounted_at(&wants) {
            ui::success(&format!(
                "mounted view '{view}' at {}",
                mountpoint.display()
            ));
            match &log_path {
                Some(p) => ui::kv_block(
                    2,
                    &[("pid", pid.to_string()), ("log", p.display().to_string())],
                ),
                None => ui::kv_block(2, &[("pid", pid.to_string())]),
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // Surface the tail of the child's own error output when we have a
            // log file; with inherited stdio it already went to the terminal.
            if let Some(p) = &log_path {
                if let Ok(data) = std::fs::read(p) {
                    let start = data.len().saturating_sub(2000);
                    eprint!("{}", String::from_utf8_lossy(&data[start..]));
                }
            }
            return Err(format!(
                "grovefs: background mount exited ({status}) before mounting{}",
                log_path
                    .as_ref()
                    .map(|p| format!("; full log {}", p.display()))
                    .unwrap_or_default()
            )
            .into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "grovefs: mount not visible at {} within 30s (background pid {pid} still running)",
                mountpoint.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

/// True when /proc/mounts carries an entry at any of the given mountpoint
/// spellings (octal escapes \040 \011 \012 \134 decoded, per proc(5)).
fn mounted_at(wants: &[String]) -> bool {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    content.lines().any(|l| {
        let mut it = l.split_whitespace();
        let _dev = it.next();
        match it.next() {
            Some(mp) => {
                let mp = mp
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\");
                wants.contains(&mp)
            }
            None => false,
        }
    })
}

/// The first mountpoint on THIS machine whose FUSE source is `grovefs:<view>`
/// (the FSName every mount sets), if any. Used by `view rm --base` to refuse
/// deleting a tree out from under a live mount on THIS machine. Mounts served
/// by a remote meta-server on ANOTHER machine are not in this table —
/// documented, not hidden.
fn view_mounted_at(view: &str) -> Option<String> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return None;
    };
    let tag = format!("grovefs:{view}");
    for l in content.lines() {
        let mut it = l.split_whitespace();
        let (Some(dev), Some(mp)) = (it.next(), it.next()) else {
            continue;
        };
        if dev == tag {
            return Some(
                mp.replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\"),
            );
        }
    }
    None
}

/// Live mounts of THIS store that can write: every physical tree name (base or
/// fork) plus pure ALIASES resolving to one. A derived lens is read-only by
/// contract, so it cannot commit a node row and is not a gc blocker.
/// Mounts of another store's names are ignored (their db is not under `mbase`).
fn gc_live_mounts(mbase: &Path, roots: &[PathBuf], views: &[String]) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for l in content.lines() {
        let mut it = l.split_whitespace();
        let (Some(dev), Some(mp)) = (it.next(), it.next()) else {
            continue;
        };
        let Some(name) = dev.strip_prefix("grovefs:") else {
            continue;
        };
        let Some(reason) = gc_writer_reason(name, mbase, views) else {
            continue;
        };
        let mp = mp
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        let mpp = Path::new(&mp);
        // The same view NAME can mount in unrelated stores, so attribute the
        // entry to the process that serves it before calling it a writer of
        // THIS store. A daemon that names another store — or is a remote
        // `--manager`-bootstrapped client — never touches these pages.
        match gc_mount_daemon(mpp) {
            Some(pid) if !proc_store_hit(pid, mbase, roots) => continue,
            // No daemon visible: a stale FUSE entry whose mountpoint no longer
            // answers is dead. Anything else stays a conservative block.
            None if !mount_answers(mpp) => continue,
            _ => {}
        }
        out.push(format!("'{name}' at {mp} ({reason})"));
    }
    out.sort();
    out.dedup();
    out
}

/// The `grovefs mount` daemon serving `mp`, matched by its `-m/--mountpoint`
/// value (absolute, or relative to the daemon's cwd). `None` means the FUSE
/// entry outlived its daemon (or the daemon is not visible in /proc).
fn gc_mount_daemon(mp: &Path) -> Option<u32> {
    let want = std::fs::canonicalize(mp).unwrap_or_else(|_| mp.to_path_buf());
    for pid in proc_pids() {
        let Some(args) = proc_args(pid) else {
            continue;
        };
        if !args.iter().any(|a| a.contains("grovefs")) || !args.iter().any(|a| a == "mount") {
            continue;
        }
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let target = if a == "-m" || a == "--mountpoint" {
                let v = args.get(i + 1).cloned();
                i += 2;
                v
            } else if let Some(v) = a.strip_prefix("--mountpoint=") {
                i += 1;
                Some(v.to_string())
            } else {
                i += 1;
                None
            };
            let Some(target) = target else {
                continue;
            };
            let p = Path::new(&target);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(c) = &cwd {
                c.join(p)
            } else {
                continue;
            };
            let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
            if canon == want || abs == mp {
                return Some(pid);
            }
        }
    }
    None
}

/// Does `mp` still answer? A dead FUSE daemon leaves `/proc/mounts` entries
/// behind until someone unmounts them; probing the root returns ENOTCONN (107)
/// or EIO (5) for those, while a served mount always stats.
fn mount_answers(mp: &Path) -> bool {
    match std::fs::metadata(mp) {
        Ok(_) => true,
        Err(e) => !matches!(e.raw_os_error(), Some(107) | Some(5)),
    }
}

fn proc_pids() -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .collect()
}

fn proc_args(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

fn proc_env(pid: u32, key: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    for kv in raw.split(|b| *b == 0) {
        let kv = String::from_utf8_lossy(kv);
        if let Some((k, v)) = kv.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn gc_writer_reason(name: &str, mbase: &Path, views: &[String]) -> Option<String> {
    if views.iter().any(|v| v == name) {
        return Some("writable view tree".to_string());
    }
    let slot = match grovefs::meta::find_def_slot(mbase, name) {
        Some(s) => s,
        None => return None,
    };
    if !slot.is_file() {
        return None;
    }
    match grovefs::layout::LayoutRules::parse_def_file(&slot) {
        Ok(grovefs::layout::ViewDef::Alias(real)) if views.iter().any(|v| v == &real) => {
            Some(format!("alias of '{real}'"))
        }
        // Derived (read-only by contract) and foreign names cannot write.
        _ => None,
    }
}

/// Local `grovefs` processes that name this metadata root or one of its data
/// roots on the command line (or through the GROVEFS_* env knobs). Best-effort
/// `/proc` probe: a remote client of a meta-server on ANOTHER machine is not
/// visible here, which is why `--apply` is documented as strictly offline.
fn local_store_users(mbase: &Path, roots: &[PathBuf]) -> Vec<String> {
    let me = std::process::id();
    let mut out = Vec::new();
    for pid in proc_pids() {
        if pid == me || !proc_store_hit(pid, mbase, roots) {
            continue;
        }
        let mut cmd = proc_args(pid).unwrap_or_default().join(" ");
        if cmd.len() > 160 {
            cmd.truncate(157);
            cmd.push_str("...");
        }
        out.push(format!("pid {pid}: {cmd}"));
    }
    out.sort();
    out.dedup();
    out
}

/// Does one process reference this store? Args first (`--home`/`--meta-dir`/
/// `--data-dir`/`--data-dir`, in both `--flag value` and `--flag=value` form),
/// then the GROVEFS_* env knobs. A grovefs process that names NO store falls
/// back to `$HOME/.grovefs` unless it is a remote client (`--manager`, or the
/// retired `--meta-server`/`--data-servers` of an older running mount),
/// which never writes a local page tree.
fn proc_store_hit(pid: u32, mbase: &Path, roots: &[PathBuf]) -> bool {
    let Some(args) = proc_args(pid) else {
        return false;
    };
    if !args.iter().any(|a| a.contains("grovefs")) {
        return false;
    }
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    let mut named = false;
    let mut remote = false;
    // Which knobs were given on the command line; a flag overrides its env twin.
    let mut seen = (false, false, false); // --home, --meta-dir, --data-dir
    for (i, a) in args.iter().enumerate() {
        if a == "--manager"
            || a.starts_with("--manager=")
            || a == "--meta-server"
            || a == "--data-servers"
            || a.starts_with("--meta-server=")
            || a.starts_with("--data-servers=")
        {
            remote = true;
        }
        let (flag, val) = if let Some(eq) = a.find('=') {
            (&a[..eq], Some(a[eq + 1..].to_string()))
        } else if matches!(a.as_str(), "--home" | "--meta-dir" | "--data-dir") {
            (a.as_str(), args.get(i + 1).cloned())
        } else {
            continue;
        };
        if !matches!(flag, "--home" | "--meta-dir" | "--data-dir") {
            continue;
        }
        named = true;
        match flag {
            "--home" => seen.0 = true,
            "--meta-dir" => seen.1 = true,
            _ => seen.2 = true,
        }
        if let Some(v) = val {
            if arg_hits_store(flag, &v, cwd.as_deref(), mbase, roots) {
                return true;
            }
        }
    }
    // Env fallbacks, per knob: a flag on the command line overrides its env
    // counterpart, so a mount started with `--home other` is NOT attributed to
    // this store just because GROVEFS_HOME was inherited from the shell.
    for (given, key) in [
        (seen.0, "GROVEFS_HOME"),
        (seen.1, "GROVEFS_META_DIR"),
        (seen.2, "GROVEFS_DATA_SERVERS"),
    ] {
        if given {
            continue;
        }
        if let Some(v) = proc_env(pid, key) {
            named = true;
            if arg_hits_store("--home", &v, cwd.as_deref(), mbase, roots) {
                return true;
            }
        }
    }
    if named || remote {
        return false;
    }
    // No store named anywhere: the process uses the default home.
    proc_env(pid, "HOME")
        .map(|h| arg_hits_store("--home", &format!("{h}/.grovefs"), None, mbase, roots))
        .unwrap_or(false)
}

/// Does one flag value (a path or comma-separated path list) name `mbase` or
/// one of `roots`? Relative values resolve against the process's cwd.
fn arg_hits_store(
    flag: &str,
    val: &str,
    cwd: Option<&Path>,
    mbase: &Path,
    roots: &[PathBuf],
) -> bool {
    if !matches!(flag, "--home" | "--meta-dir" | "--data-dir") {
        return false;
    }
    val.split(',').any(|part| {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let p = Path::new(part);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(c) = cwd {
            c.join(p)
        } else {
            return false;
        };
        let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
        let same = |w: &Path| {
            let wc = std::fs::canonicalize(w).unwrap_or_else(|_| w.to_path_buf());
            canon == wc || abs == w
        };
        same(mbase) || roots.iter().any(|r| same(r))
    })
}

fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K {
        format!("{:.1} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.1} KiB", f / K)
    } else {
        format!("{n} B")
    }
}

fn fmt_age(secs: u64) -> String {
    if secs >= 86400 {
        let (d, h) = (secs / 86400, (secs % 86400) / 3600);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    } else if secs >= 3600 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else if secs >= 60 {
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s}s")
        }
    } else {
        format!("{secs}s")
    }
}

fn print_gc_report(rep: &grovefs::gc::Report, rec: Option<&grovefs::gc::Reclaimed>) {
    let roots: Vec<String> = rep
        .data_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    ui::heading("gc report");
    let mut facts: Vec<(&str, String)> = vec![
        ("store", rep.meta_root.display().to_string()),
        ("data root", roots.join(", ")),
        (
            "views",
            format!(
                "{} · {} · {}",
                ui::plural(rep.views.len(), "tree", "trees"),
                ui::plural(rep.refs as usize, "referenced key", "referenced keys"),
                ui::plural(
                    rep.striped_layouts as usize,
                    "striped layout",
                    "striped layouts"
                )
            ),
        ),
    ];
    if rep.objects == 0 && rep.striped_layouts > 0 {
        // A "0" here is not "empty store": the trees reference striped objects
        // whose chunks live on data-servers, and gc only audits local block
        // roots. Say so instead of letting the bare 0 read as "no bytes".
        facts.push((
            "objects",
            format!(
                "0 local blocks — {} place their chunks on data-servers, which gc does not audit (use `grovefs du` for their bytes)",
                ui::plural(rep.striped_layouts as usize, "striped layout", "striped layouts")
            ),
        ));
    } else {
        facts.push((
            "objects",
            format!("{} ({})", rep.objects, fmt_bytes(rep.objects_bytes)),
        ));
    }
    facts.push((
        "orphans",
        format!(
            "{} ({}) · {} reclaimable · {} within grace <{}",
            rep.orphans.len(),
            fmt_bytes(rep.orphan_bytes()),
            rep.reclaimable(),
            rep.within_grace(),
            fmt_age(rep.grace.as_secs())
        ),
    ));
    ui::kv_block(0, &facts);

    if !rep.orphans.is_empty() {
        ui::blank();
        ui::section(&format!("Orphans ({})", rep.orphans.len()));
        let mut t = ui::Table::new(vec![
            ui::Column::right("size").min(6),
            ui::Column::right("age").min(4),
            ui::Column::left("kind").min(4).max(8),
            ui::Column::left("key").min(12),
        ])
        .indent(2);
        for c in rep.orphans.iter().take(20) {
            t.row(vec![
                ui::Cell::new(fmt_bytes(c.size)),
                ui::Cell::muted(fmt_age(grovefs::gc::age_secs(c.mtime))),
                ui::Cell::new(c.kind_str()),
                ui::Cell::muted(&c.key),
            ]);
        }
        t.print();
        if rep.orphans.len() > 20 {
            ui::hint(&format!(
                "{} more — use --json for the full list",
                rep.orphans.len() - 20
            ));
        }
    }

    if !rep.held.is_empty() {
        ui::blank();
        ui::warn(&format!(
            "ledger-held: {} ({}) — NOT deleted; run `grovefs refs --meta-dir {} rebuild` first if these are stale",
            rep.held.len(),
            fmt_bytes(rep.held_bytes()),
            rep.meta_root.display()
        ));
        let mut t = ui::Table::new(vec![
            ui::Column::right("size").min(6),
            ui::Column::left("key").min(12),
        ])
        .indent(2);
        for c in rep.held.iter().take(5) {
            t.row(vec![
                ui::Cell::new(fmt_bytes(c.size)),
                ui::Cell::muted(&c.key),
            ]);
        }
        t.print();
        if rep.held.len() > 5 {
            ui::hint(&format!("{} more", rep.held.len() - 5));
        }
    }

    if !rep.skipped.is_empty() {
        ui::blank();
        ui::warn(&format!(
            "skipped: {} left untouched (never deleted)",
            ui::plural(rep.skipped.len(), "entry", "entries")
        ));
        for s in rep.skipped.iter().take(5) {
            ui::item(s);
        }
        if rep.skipped.len() > 5 {
            ui::hint(&format!("{} more", rep.skipped.len() - 5));
        }
    }

    ui::blank();
    match rec {
        None => {
            // Keep "nothing deleted" as the stable dry-run prefix, then give
            // the reason: only invite `--apply` when there is something to
            // reclaim, rather than suggesting a pointless re-run on 0 bytes.
            if rep.reclaimable() > 0 {
                ui::info("dry run: nothing deleted.");
                ui::hint(&format!(
                    "re-run with --apply (offline) to reclaim {}, {}",
                    ui::plural(rep.reclaimable(), "object", "objects"),
                    fmt_bytes(rep.reclaimable_bytes())
                ));
            } else if rep.orphans.is_empty() {
                ui::success("no unreferenced objects were found.");
            } else {
                ui::info(&format!(
                    "dry run: nothing deleted. {} within the {} grace; re-run after it elapses.",
                    ui::plural(rep.orphans.len(), "orphan", "orphans"),
                    fmt_age(rep.grace.as_secs())
                ));
            }
        }
        Some(r) => {
            if r.failed.is_empty() {
                ui::success(&format!(
                    "reclaimed {}, {} freed; {} within grace kept",
                    ui::plural(r.deleted as usize, "object", "objects"),
                    fmt_bytes(r.bytes),
                    r.within_grace
                ));
            } else {
                ui::warn(&format!(
                    "reclaimed {}, {} freed; {} within grace kept; {} failed",
                    ui::plural(r.deleted as usize, "object", "objects"),
                    fmt_bytes(r.bytes),
                    r.within_grace,
                    r.failed.len()
                ));
                for f in r.failed.iter().take(10) {
                    ui::item(f);
                }
            }
        }
    }
}

fn gc_json(rep: &grovefs::gc::Report, rec: Option<&grovefs::gc::Reclaimed>) -> serde_json::Value {
    let cand = |c: &grovefs::gc::Candidate| {
        serde_json::json!({
            "key": c.key,
            "data_root": c.data_root.display().to_string(),
            "size": c.size,
            "age_secs": grovefs::gc::age_secs(c.mtime),
            "kind": c.kind_str(),
            "within_grace": c.within_grace,
        })
    };
    serde_json::json!({
        "meta_root": rep.meta_root.display().to_string(),
        "data_roots": rep.data_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "views": &rep.views,
        "referenced_keys": rep.refs,
        "striped_layouts": rep.striped_layouts,
        "objects": rep.objects,
        "object_bytes": rep.objects_bytes,
        "matched_objects": rep.matched,
        "orphans": rep.orphans.iter().map(cand).collect::<Vec<_>>(),
        "orphan_bytes": rep.orphan_bytes(),
        "reclaimable": rep.reclaimable(),
        "reclaimable_bytes": rep.reclaimable_bytes(),
        "within_grace": rep.within_grace(),
        "ledger_held": rep.held.iter().map(cand).collect::<Vec<_>>(),
        "ledger_held_bytes": rep.held_bytes(),
        "skipped": &rep.skipped,
        "grace_secs": rep.grace.as_secs(),
        "applied": rec.is_some(),
        "deleted": rec.map(|r| r.deleted).unwrap_or(0),
        "deleted_bytes": rec.map(|r| r.bytes).unwrap_or(0),
        "delete_failed": rec.map(|r| r.failed.clone()).unwrap_or_default(),
    })
}

/// Render the control plane's `gc` report. The server answers in the shape
/// [`gc_json`] uses (plus where it ran and what it could see), so this prints
/// the lines the local report prints without re-deriving a single verdict here:
/// the deletion decision stays on the server that owns the trees.
fn print_gc_remote(rep: &serde_json::Value) {
    let num = |k: &str| rep.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let txt = |k: &str| rep.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let arr = |k: &str| -> Vec<serde_json::Value> {
        rep.get(k)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let servers: Vec<&str> = rep
        .get("data_roots")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    ui::heading(&format!(
        "gc report · {} (backend {})",
        txt("meta_root"),
        txt("backend")
    ));
    ui::kv_block(
        0,
        &[
            ("data server", servers.join(", ")),
            (
                "views",
                format!(
                    "{} · {} · {}",
                    ui::plural(arr("views").len(), "tree", "trees"),
                    ui::plural(
                        num("referenced_keys") as usize,
                        "referenced key",
                        "referenced keys"
                    ),
                    ui::plural(
                        num("striped_layouts") as usize,
                        "striped layout",
                        "striped layouts"
                    )
                ),
            ),
            (
                "objects",
                format!("{} ({})", num("objects"), fmt_bytes(num("object_bytes"))),
            ),
            (
                "orphans",
                format!(
                    "{} ({}) · {} reclaimable · {} within grace <{}",
                    num("orphans"),
                    fmt_bytes(num("orphan_bytes")),
                    num("reclaimable"),
                    num("within_grace"),
                    fmt_age(num("grace_secs"))
                ),
            ),
        ],
    );
    let orphans = arr("orphans");
    if !orphans.is_empty() {
        ui::blank();
        ui::section(&format!("Orphans ({})", orphans.len()));
        let mut t = ui::Table::new(vec![
            ui::Column::right("size").min(6),
            ui::Column::right("age").min(4),
            ui::Column::left("kind").min(4).max(8),
            ui::Column::left("key").min(12),
        ])
        .indent(2);
        for c in orphans.iter().take(20) {
            t.row(vec![
                ui::Cell::new(fmt_bytes(c["size"].as_u64().unwrap_or(0))),
                ui::Cell::muted(fmt_age(c["age_secs"].as_u64().unwrap_or(0))),
                ui::Cell::new(c["kind"].as_str().unwrap_or("?")),
                ui::Cell::muted(c["key"].as_str().unwrap_or("")),
            ]);
        }
        t.print();
        if orphans.len() > 20 {
            ui::hint(&format!(
                "{} more — use --json for the full list",
                orphans.len() - 20
            ));
        }
    }
    let held = arr("ledger_held");
    if !held.is_empty() {
        ui::blank();
        ui::warn(&format!(
            "ledger-held: {} ({}) — NOT deleted; run `grovefs refs rebuild --control-server …` first if these are stale",
            held.len(),
            fmt_bytes(num("ledger_held_bytes"))
        ));
        let mut t = ui::Table::new(vec![
            ui::Column::right("size").min(6),
            ui::Column::left("key").min(12),
        ])
        .indent(2);
        for c in held.iter().take(5) {
            t.row(vec![
                ui::Cell::new(fmt_bytes(c["size"].as_u64().unwrap_or(0))),
                ui::Cell::muted(c["key"].as_str().unwrap_or("")),
            ]);
        }
        t.print();
        if held.len() > 5 {
            ui::hint(&format!("{} more", held.len() - 5));
        }
    }
    let mounts = arr("server_mounts");
    let sessions = arr("live_sessions");
    if !mounts.is_empty() || !sessions.is_empty() {
        ui::blank();
        ui::warn(&format!(
            "in use: {} on the server host, {} there",
            ui::plural(mounts.len(), "mount", "mounts"),
            ui::plural(
                sessions.len(),
                "view with live sessions",
                "views with live sessions"
            )
        ));
        for m in mounts.iter().take(5) {
            ui::item(&format!(
                "mounted on the server: {}",
                m.as_str().unwrap_or("")
            ));
        }
        ui::note(
            "a mount on another machine is not visible from the server host; stop it too before --apply",
        );
    }
    ui::blank();
    if rep
        .get("applied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let failed = arr("delete_failed");
        if failed.is_empty() {
            ui::success(&format!(
                "reclaimed {}, {} freed; {} within grace kept",
                ui::plural(num("deleted") as usize, "object", "objects"),
                fmt_bytes(num("deleted_bytes")),
                num("within_grace")
            ));
        } else {
            ui::warn(&format!(
                "reclaimed {}, {} freed; {} within grace kept; {} failed",
                ui::plural(num("deleted") as usize, "object", "objects"),
                fmt_bytes(num("deleted_bytes")),
                num("within_grace"),
                failed.len()
            ));
            for f in failed.iter().take(10) {
                ui::item(f.as_str().unwrap_or(""));
            }
        }
    } else if num("reclaimable") > 0 {
        ui::info("dry run: nothing deleted.");
        ui::hint(&format!(
            "re-run with --apply to reclaim {}, {}",
            ui::plural(num("reclaimable") as usize, "object", "objects"),
            fmt_bytes(num("reclaimable_bytes"))
        ));
    } else if orphans.is_empty() {
        ui::success("no unreferenced objects were found.");
    } else {
        ui::info(&format!(
            "dry run: nothing deleted. {} within the {} grace; re-run after it elapses.",
            ui::plural(orphans.len(), "orphan", "orphans"),
            fmt_age(num("grace_secs"))
        ));
    }
}

fn run_fusermount(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new(fusermount_bin())
        .args(args)
        .status()?;
    if !status.success() {
        return Err(format!("{} exited with {status}", fusermount_bin()).into());
    }
    Ok(())
}

fn fusermount_bin() -> &'static str {
    if Path::new("/usr/bin/fusermount3").exists() {
        "fusermount3"
    } else {
        "fusermount"
    }
}

/// [`run_fusermount`] with a deadline. A plain `fusermount -u` on a busy FUSE
/// mount BLOCKS in the kernel's `umount()` until every in-flight request is
/// answered — and a daemon tearing itself down stops answering nothing only
/// while that helper blocks, so an unbounded wait here is one edge of a real
/// three-way deadlock (helper blocked on umount ⇄ reverse-inval blocked on
/// the busy directory ⇄ new ops queued behind it). Bounding the wait turns
/// the deadlock into a timeout that escalates to the lazy path.
fn run_fusermount_timeout(
    args: &[&str],
    wait: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Stdio;
    let mut child = std::process::Command::new(fusermount_bin())
        .args(args)
        .stdin(Stdio::null())
        .spawn()?;
    let born = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("{} exited with {status}", fusermount_bin()).into())
                }
            }
            None if born.elapsed() >= wait => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}ms", wait.as_millis()).into());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
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
    ui::info("received termination signal, unmounting cleanly...");
}

/// Spawn a watcher thread that unmounts `mountpoint` when the process gets
/// SIGINT/SIGTERM. Best-effort: if signal setup fails the thread simply idles.
///
/// The attempt is repeated and finally escalated to a lazy detach because a
/// termination can land while a request on this mount is still in flight — which
/// is exactly what a lost credential does: the refusal arrives AS the reply to an
/// operation, so the kernel is still finishing that open when we ask for the
/// umount and a plain umount answers `EBUSY`. Stopping there would leave a live
/// mountpoint whose backend is gone (and exiting anyway would leave the classic
/// stale `Transport endpoint is not connected`), so: retry (each attempt
/// TIME-BOUNDED — a busy umount does not merely answer EBUSY, it can block
/// forever inside the syscall, which is how a revoke landing on a mount under
/// live traffic once deadlocked its own teardown), then `-u -z`, which detaches
/// the mountpoint even while busy and still ends the session. If even the lazy
/// detach wedges (or errors), a detached watchdog repeats it — the helper's
/// blocked `umount()` completes on its own once the queued requests drain, so
/// re-asking without owning the session's exit path guarantees the mountpoint
/// eventually goes down instead of hanging the escalation here. If nothing
/// works we keep serving — a mount that answers with an error is recoverable by
/// an operator, a dead endpoint is not.
fn spawn_term_unmounter(mountpoint: PathBuf) {
    std::thread::spawn(move || {
        install_termination_handlers();
        wait_for_termination();
        let mp = mountpoint.to_string_lossy();
        // Go straight to the lazy detach. A plain `fusermount -u` is NOT a
        // safe first try on a terminating daemon: on a busy mountpoint its
        // `umount()` BLOCKS in the kernel until every queued request drains —
        // and the drain needs this very call to return, so a revoke landing
        // under live traffic once deadlocked its own teardown (helper in
        // `umount()` ⇄ reverse-inval on the busy directory ⇄ new ops queued
        // behind it). Killing a blocked helper is worse (it can strand
        // fusermount's mount lock). `umount2(MNT_DETACH)` never blocks on
        // busy refs and the kernel aborts the connection's pending requests
        // once the last opener goes away, which the watchdog below confirms.
        if run_fusermount_timeout(&["-u", "-z", &mp], std::time::Duration::from_secs(2)).is_err() {
            let mp = mountpoint.clone();
            std::thread::spawn(move || {
                let mp = mp.to_string_lossy().into_owned();
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let held = std::fs::read_to_string("/proc/mounts")
                        .map(|m| {
                            m.lines()
                                .any(|l| l.split_whitespace().nth(1) == Some(mp.as_str()))
                        })
                        .unwrap_or(false);
                    if !held {
                        break;
                    }
                    let _ = run_fusermount_timeout(
                        &["-u", "-z", &mp],
                        std::time::Duration::from_secs(2),
                    );
                }
            });
        }
    });
}
