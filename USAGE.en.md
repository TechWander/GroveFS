# GroveFS User Guide

> A user-view FUSE filesystem: physical bytes live centrally on disk, each user mounts
> a private file list of their own, the lists support custom folder nesting, and NFS
> is entirely unnecessary. This document explains **how to use it** and **how to handle data**.

[中文](USAGE.md)

---

## Feature index: find the entry point by "what do I want to do"

One table covers every capability. The left column is **what you want to accomplish**, the right column is the entry point (CLI and MCP tools), and the last column is the detailed section — every section follows the same recipe: **what problem it solves → full command and parameters → real output → boundaries and gotchas**.

| What I want to do | Entry point | Details |
| --- | --- | --- |
| See it work on one machine in 5 minutes (start the two services, then mount) | `meta-server` + `data-server` → `view create` → `mount` | §2, §15.1 |
| Bring an existing on-disk directory under management (**zero-byte copy**) | `grovefs import --view V --source DIR` | §3.3, §14.2 |
| Show the same view differently to A vs. B (affects only this mount) | `mount --layout rules` | §3.1 |
| Define a read-only shape server-side (consistent across all clients, hot-reload) | `view define` · MCP `define_derived_view` | §3.2 |
| Expose a different public name, whole tree still writable (name governance) | `view alias` (`--retire` rotates with no gap) | §3.4 |
| Independent tree: organize freely, content read-only (structure governance) | `view fork` · MCP `fork_view` | §3.4.1 |
| Install just a small slice of a base view, renaming as you go | `view graft` · MCP `graft_view` | §3.4.1 "Selective graft" |
| List views / see what a tree actually looks like | `view list` · MCP `list_views` / `get_view` / `list_view_tree` | §3.2, §3.5 |
| Migrate a single view across machines/OSes | `view export` + `view import` | §3.4.2 |
| Delete one definition / delete a whole base view and reclaim bytes | `view rm --name F` / `view rm --name F --base` | §3.4.3, §5.3 |
| Audit / reclaim orphan objects (offline) | `gc --meta-dir D [--data-dir R] [--apply]` | §12.6 |
| Let an agent customize data listings itself | `grovefs mcp` (stdio JSON-RPC) | §3.5 |
| Lock the interface and mount points, revoke per-caller | `apikey create/list/rotate/revoke/set-ro/calls/update/purge` | §3.5.1, §6 |
| Read-only mount / zero-cache strong consistency | `mount --ro` / `--no-cache` | §7 |
| Performance observation and rate limiting (per-stage latency / bandwidth / cache TTL) | `mount --stats*` / `--bw-read|--bw-write` / `--cache-ttl` | §8 |
| Share one view across several machines | `data-server` + `meta-server` | §4, §11 |
| Stop typing the two cluster coordinates on every client (mount bootstrap) | `manager define/serve/status` + `mount --manager` | §4.2 |
| Swap metadata to the PostgreSQL backend | `--meta-store postgres://…` everywhere | §4.1, §6.2, §15.3 |
| PG connections hit the ceiling: scale out / move an existing view to another instance | `--meta-store file:registry` + `--pg-instance` / `view move` | §6.2 |
| Mounted-but-idle views still hold 12 PG connections | `meta-server --pg-write-pool`/`--pg-read-pool` (warm floors) + each `-max` (burst ceiling) + `--pg-pool-idle-secs`; per view `grovefs view pool` | §6.2 |
| Govern the same metadata from any machine (never hand out the DB password) | `meta-server --control-listen` + `view`/`apikey`/`refs --control-server --api-key` | §3.5.2 |
| Manage views and keys from a browser, and delegate base views to accounts (create accounts / grant) | `grovefs web` + `web account` / `web grant` | §3.5.3 |
| Primary/standby HA with real fencing | `meta-server --role/--peer/--sync-repl/--witness` | §5.5, §11.8 |
| Reconcile a broken ledger / audit who is calling | `refs rebuild` / `apikey calls` | §5.3, §3.5.1 |
| Slim down long commands (env vars + config file) | `GROVEFS_*` / `~/.config/grovefs/grovefs.conf` | §1.1, §15.15 |

> **Don't confuse the two `import`s**: the top-level `grovefs import` "brings a directory under management" (a data entry point, §3.3); `grovefs view import` "loads back the metadata JSONL exported by `view export`" (a migration entry point, §3.4.2).
>
> **The CLI and MCP are the same implementation**: `view define/alias/fork/graft/rm` and their MCP tool counterparts share parameter semantics and refusal wording (byte-for-byte identical); only the entrance differs — humans use the CLI in a terminal, agents use MCP.

---

## 0. Core concepts (understand these three first)

| Concept | Description |
|------|------|
| **View** | Each user gets an independent directory tree (with custom nesting), stored in one SQLite DB **on the `meta-server` machine**: `<meta-dir>/meta/<view>/<view>.db` (under the PG backend, a `grovefs_v_<hex>` schema). Users cannot see each other's files. |
| **Object** | The real bytes of a file. One file = one object file under `blocks/`, named by UUID (or several chunks when striped); objects are auto-distributed over 256 bucket subdirectories by a hash of their key. The view's directory entries point at object keys. |
| **Base dir (storage root)** | The two services' own roots: `meta-server --meta-dir` holds `meta/<view>/<view>.db` (the trees), `meta/views/*.layout` (the definitions) and `meta/_control/` (the control plane: ledger, keys, accounts); `data-server --data-dir` holds `blocks/` (the content objects). The global `--home` is the **default for both** — one `--home` is enough when both services share a host, `--meta-dir`/`--data-dir` name them separately when meta and data are split. Defaults to `$GROVEFS_HOME` or `~/.grovefs`. **A mount never reads or writes any storage root**: the client keeps nothing on its own disk, tree and bytes both live server-side. |

Data and views are **separate by nature**: a view is just "a directory tree + pointers", the bytes always live in `blocks/`.

### 0.1 Terminology map (newcomer quick reference)

| Term in the docs | Meaning |
| --- | --- |
| `--home` | The global default **storage root**, read only by the node-side verbs (`view create`/`gc`/`import`/`apikey`/`du`) and by the two servers themselves (it is where a call lands when it names no `--meta-dir`/`--data-dir`). It has nothing to do with a mount — a mount's two coordinates are resolved ONLY from the deployment's signed roster (§4.2); hand-typed addresses were removed from `mount` |
| data-server / meta-server | The two central service processes: the former manages bytes (object storage), the latter manages the directory tree (metadata); a client never points at these addresses — it points `mount --manager` at the bootstrap service, which resolves them |
| base view / derived view / alias / fork | The four view forms: an independent storage tree (writable) / a read-only display lens defined by rules on the server / a renamed full-tree-writable entry / an independent tree that copies the directory but not the bytes (beginners: see the analogy and comparison table in §3.0 first; operational detail in §3.4–§3.4.1) |
| token / API key | A token is the cluster admission gate (one shared passphrase); an API key is a per-caller, revocable credential that can be bound to a view / marked read-only (§3.5.1) |
| control plane / mount plane | Control plane = `grovefs mcp`'s view-definition management interface; mount plane = the FUSE client's connections to meta/data — the two gates are independent and their scopes never cross (§6) |

> Readers who haven't yet built a "FUSE / mount / object storage" mental model should first read the web manual docs.html's "§0 zero-base primer" (six keywords, what you do and don't need to know, minimum hardware requirements).

### Common parameters: what they govern first, then how to write them

> In the commands below, `--home`, `--meta-dir` and the like are the **parameter forms** (copy them as-is); what each governs is taken from its **actual meaning**:

| What it governs (actual meaning) | Command-line form |
| --- | --- |
| **Default storage root** — where the node-side verbs and the two servers land when they name no root explicitly (`blocks/` + `meta/`); a mount does not read it | `--home <dir>` |
| **Server-side metadata root** — on the `meta-server` machine, the umbrella directory holding the authoritative view DBs, view definitions, and API keys | `--meta-dir <dir>` |
| **Where bytes land on disk** — the storage root `data-server` actually writes bytes to (defaults to the metadata root) | `--data-dir <dir>` |
| **The existing directory to be managed** — that batch of data already sitting on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory used to reach this tree | `-m <dir>` (i.e. `--mountpoint`) |
| **Mount bootstrap address** — the one service coordinate a client types by hand (host:port); both service addresses are resolved from its signed roster | `mount --manager <addr>` + `--roster-key-file <file>` |
| **View name** — an arbitrary label for a logical tree (unrelated to any Linux username) | `--view <name>` |
| **Cluster admission passphrase** — the shared secret the server sets, presented by the client on mount (one for the data plane, one for the metadata plane) | server `--token <str>` + client `--token <str>` |
| **Revocable access credential** — the server admits per-caller API keys instead (`grovefs apikey create --for mount`); listable, auditable per caller, revocable, and a revoke closes **already-established** mounts; `--view V` **binds a key to a single view** (opening another view is refused at the handshake), `--ro` is read-only enforced server-side (per write-intent op refused) | server `--require-api-key` + client `mount --api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) |
| **Account key (governance credential)** — a key minted with `--for admin --owner <account>` stands in for the DB password: whoever holds it can manage the same metadata from **any** machine that can reach the governance port, with the real reads/writes done on behalf by `meta-server`; one account may hold many keys, each minted and revoked individually | server `--control-listen <addr>` + client `--control-server <addr> --api-key <ID:SECRET>` (or env `GROVEFS_CONTROL_KEY`) |
| **Mount as a given identity** — lock arbitration and audit record ownership under this uid | `--run-as-uid <uid>` |
| **Read-only / zero-cache strong consistency** — mount read-only / disable client caching | `--ro` / `--no-cache` |
| **Fallback switch for directory-listing speed** — turn off READDIRPLUS (which carries attributes alongside the entries) and fall back to plain READDIR + a per-entry `LOOKUP`, for A/B performance troubleshooting only (paging is unaffected either way — whether PLUS is negotiated comes from the 2nd byte of the `M_OPEN` ack, i.e. `supports_paged_readdir()` of the server's **live engine**, and today every engine pages inside its own store, derived/lens shapes included) | `--no-readdirplus` |
| **Per-stage latency telemetry** — opt-in recording of the time spent in each read/write/metadata stage (aggregate p50/p90/p99/max offline to locate bottlenecks; printed on exit + periodic write to disk + a hidden node `.grove-stats`; not a throughput claim) | `--stats` / `--stats-file <path>` / `--stats-interval <secs>` / `GROVEFS_STATS=1` (see §8.1) |
| **Read-cache TTL tuning** — enlarge the kernel attr/dentry cache TTL (default 2 s) to cut repeated LOOKUP/GETATTR on read-heavy / immutable trees; cross-client freshness still relies on the default invalidation push plus the newer-kernel `AUTO_INVAL_DATA` backstop, `--no-cache` forces it to 0 | `--cache-ttl <secs>` (see §8.2) |
| **Per-mount content bandwidth shaping** — put a GCRA token slot on this mount's content reads and writes (non-overlapping time slots laid out by byte, so the long-run rate equals the cap exactly; overshoot only sleeps, never drops bytes; write-side latency is backpressure), `0` = unlimited | `--bw-read <rate>` / `--bw-write <rate>` (e.g. `50M`; env `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE`; see §8.2) |

### About `--view` and path capacity

- **`--view` is unrelated to Linux usernames** — it is only an **arbitrary label** in the view namespace. The binary never calls `getpwnam`, never checks uid, never requires `useradd`. `alice`, `team-a`, `u42`, `研究组` all work as a view name — as long as it is a legal filename on the target filesystem. It serves exactly two purposes: ① it lets the server locate the tree `meta/<view>/<view>.db`; ② it stamps a `grovefs:<view>` label on the FUSE mount for `mount`/`findmnt` to display. **All authentication is server-side** — the `--token` / `--require-api-key` gates — unrelated to a Linux user and to "who can read the storage root".
- **One tree per user, fully autonomous paths inside the tree** — once mounted, under that mountpoint you can `mkdir`/`touch` **any number** of files and directories, to **any depth**, with any nesting, no different from working in your own home. The granularity of isolation between users is "the whole view tree", not a single path. A manifest is only a convenient entry point for pre-building a skeleton, not a cap on the number of paths; after mounting you can `mkdir -p a/b/c/d/...` at any time.

---

## 1. Environment setup

```bash
# 依赖:只需 Rust 工具链(edition 2021);不需要 libfuse 开发包,也不需要 pkg-config
# (FUSE 客户端是纯 Rust 的 fuser;非 root 挂载时靠发行版的 setuid 助手 fusermount/fusermount3)
# export PKG_CONFIG="$PWD/.build-bin/pkg-config"   # no crate in the current dependency graph queries pkg-config; removing it still builds
export LC_ALL=C LANG=C
cargo build --release                                       # 产物:target/release/grovefs
```

Executable subcommands: `mount` / `umount` / `data-server` / `meta-server` / `manager` (mount bootstrap) / `view` / `mcp` / `apikey` / `web` / `refs` / `gc` / `import` / `du`. The only entry point that builds a view is `view create` (§3.0); a `mount` never creates a database.

### 1.1 Command shorthand: env vars and an auto-loaded config file

The long commands you type over and over can be slimmed down wholesale. The precedence for every knob is always:
**command-line argument > current environment variable > config file > built-in default**.

| Knob (env) | Parameter it stands in for | Description |
| --- | --- | --- |
| `GROVEFS_HOME` | `--home` | Local storage root |
| `GROVEFS_META_SERVER` | `du --meta-server` (node-side verb) | Metadata service address; **no longer drives `mount`** — a mount's two coordinates come only from the deployment's signed roster (§4.2) |
| `GROVEFS_DATA_SERVERS` | `du`/`gc --control-server`'s `--data-servers` (node-side verbs) | Data service address; **likewise no longer drives `mount`** (same as above) |
| `GROVEFS_MANAGER` | `mount --manager` | Address of the mount-bootstrap service; the **only** coordinate source for `mount` — it resolves both the meta- and the data service (§4.2); hand-typed `--meta-server`/`--data-servers` were removed from `mount` |
| `GROVEFS_ROSTER_KEY_FILE` | `mount --roster-key-file` | Key (64-hex file) that verifies the roster signature; `--manager` refuses the mount without it |
| `GROVEFS_REGISTER_WITH` | `meta-server/data-server --register-with` | Which manager a node registers with (§4.2) |
| `GROVEFS_JOIN_KEY_FILE` | `meta-server/data-server --join-key-file` | File holding the JOIN key that authorizes a registration (a client never holds it) |
| `GROVEFS_DEPLOYMENT` | `mount --deployment`, server-side `--deployment` | Which deployment this belongs to (`manager define --name`; default `default`) |
| `GROVEFS_STRIPE_BYTES` | `mount --stripe-bytes` | Stripe chunk size for cross-server striping (default 16 MiB; only takes effect when the data list resolved from the roster has multiple addresses; an illegal value refuses to start) |
| `GROVEFS_MEMBER_ID` | `data-server --member-id` | This data-server's stable identity id within a striped cluster (a 64-bit integer unique per member; an illegal value refuses to start; if not given, the node participates in stripes in the historical positional mode) |
| `GROVEFS_TOKEN` | `mount --token`, `meta-server/data-server --token` | Cluster admission passphrase |
| `GROVEFS_DATA_ACCESS_KEY` | `meta-server --data-access-key`, `data-server --data-access-key` | Shared HMAC key (hex, `openssl rand -hex 32`) for the Data-Access Capability that binds the data plane to a genuine meta admission; both servers must use the SAME value, `mount` needs none (the capability is minted by the meta-server in the `M_OPEN` ack and auto-replayed to the data plane); §6.1.1 |
| `GROVEFS_META_DIR` | `meta-server --meta-dir` | Server-side metadata root (precedence: `--meta-dir` argument > this env > `--home` > `~/.grovefs`) |
| `GROVEFS_META_STORE` | `--meta-store` (`view`/`apikey`/`mcp`/`meta-server`/`data-server`/`du`/`import`) | Non-SQLite backend addressing; when no flag is given this env is the fallback (source `resolve_store()`). `import` only writes a local SQLite tree, so if this env holds `postgres://…` it is **honestly refused** |
| `GROVEFS_API_KEY` | `mount --api-key` | env for an existing per-caller credential |
| `GROVEFS_MCP_API_KEY` | MCP `initialize` handshake | Whole-server pre-authentication |
| `GROVEFS_CONTROL_KEY` | `--api-key` (with `--control-server`) | The governance port's admin key (`<id>:<secret>`); if unset it falls back to `GROVEFS_API_KEY`, and either way the secret stays out of argv |
| `GROVEFS_STATS` | `--stats` | `1/true` enables telemetry |
| `GROVEFS_SQLITE_READ_POOL` | — (internal tuning) | Per-view SQLite **warm read-connection floor**, range 0–16; default `READ_POOL_SIZE = 1` (one idle read connection ≈ 3 fds), bursts retained up to `max(floor, READ_POOL_RETAIN = 8)` — the env raises the floor and the ceiling follows; `0` is fully lazy (open/close per read). It lives on the **server-side** per-view engines and on callers that embed the library: a mount process holds no engine, clients get frames like `M_READDIRP` that push `offset/limit` down to the server. Measured account in §8 "where a create actually goes": raising the server-side warm read floor from 1 to 8 does not change the shape of the concurrent file-creation curve — the bottleneck is on the write side |
| `GROVEFS_PG_WRITE_POOL` | — (internal tuning) | Per-view PG write-connection warm pool, default 8, range 0–64; when the pool is empty a connection is opened ad hoc, the hard ceiling is PG `max_connections` |
| `GROVEFS_PG_READ_POOL` | — (internal tuning) | Per-view PG read-connection warm pool, default 4, range 0–64 |
| `GROVEFS_PG_SYNC_COMMIT` | — (internal tuning) | Session-level `SET synchronous_commit` on PG write connections; default `on` (PG's own default); with `off`/`false`/`0`/`async` a COMMIT return no longer waits for the local WAL to hit disk. On the same test rig (with the write-statement cache already in place) the N=4 write path goes 5,225 → 6,142 ops/s (+18%), N=1/N=8 within run-to-run jitter, at the cost that a crash may lose the last several committed transactions and, with a synchronous standby configured, it also stops waiting for its ack (RPO=0 must keep it `on`) |
| `GROVEFS_GATEWAY_CTL_PIN` | — (`grovefs web` only) | The connection shape between the gateway and the governance port. By default (unset, or any value but `0`) the gateway uses a **resident control-connection pool**: one socket pinned to the admin credential, which removes the per-op TCP connect, server thread and the two queries of an authentication; `0` puts it back on one connection per request. The knob exists because it lets the A/B be run **inside one deployment on one binary**, and because an operator needs a kill switch that isn't a recompile. Measured (four-machine PG tier, both arms interleaved on one host): serial p50 −19…−43% and server CPU/op −10…−50%; concurrency only pays for real at P=16/32 (+18%/+25%, P=32 p50 26ms vs 83ms), while the P≤8 deltas sit inside the run-to-run spread. Both arms write the same number of audit rows and answer ops byte-for-byte identically (§3.5.2) |

On top of the env vars above, grovefs also **auto-loads a `KEY=VALUE` config file** at startup to fill any variable not yet set — so you don't even have to export each time:

```bash
# 查找顺序: $GROVEFS_CONFIG 指定路径 > $HOME/.config/grovefs/grovefs.conf
# (只认 GROVEFS_* 键,其余行提示后忽略)
cat > ~/.config/grovefs/grovefs.conf <<'EOF'
GROVEFS_MANAGER=192.168.1.10:50055
GROVEFS_ROSTER_KEY_FILE=/etc/grovefs/roster.key
GROVEFS_TOKEN=s3cret            # 文件里有口令就 chmod 600(与 env 里放口令同一契约)
EOF
chmod 600 ~/.config/grovefs/grovefs.conf

# 之前: grovefs mount --view alice -m /mnt/alice \
#         --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token s3cret
# 之后: 只剩"这次挂哪个视图到哪儿"(coordinates come from GROVEFS_MANAGER's signed roster)
grovefs mount --view alice -m /mnt/alice
```

Format details: `#` comments, blank lines, an optional `export ` prefix (the same file can double as a shell script), and single/double-quoted values are all supported; non-`GROVEFS_*` keys and lines missing `=` are ignored after a one-line warning carrying `file:line`; an explicitly specified `$GROVEFS_CONFIG` that can't be read warns and continues (non-fatal). The re-exec child of a background mount inherits the environment, so the shorthand still applies there. `--view`/`-m` are deliberately not included — they differ per mount, and baking them into defaults would be wrong.

Multi-cluster caution: these values are one set of **process-wide global coordinates**; a single command reads one file only and does not auto-match by mount point / view. To switch clusters, pass it per invocation via `GROVEFS_CONFIG=~/grovefs-B.conf grovefs …` (one file per cluster, `chmod 600`) rather than a persistent export — an already-exported variable of the same name **silently overrides** another cluster's config file (the file only fills keys not yet set); the mount banner `using remote metadata service at …` lets you verify the coordinates. The control-plane `--meta-dir` has no env fallback and must be given explicitly per cluster; `GROVEFS_META_STORE` is a global key, and a leftover export would point governance commands at a different PG set, so unset it before switching clusters.

Regression coverage: `bash .build-bin/run_all.sh config` (37 assertions: the precedence chain, parser edge cases, a real FUSE mount driven entirely by the file, and section 7 - the hard refusal when no source supplies a coordinate (exit 1, the sentence verbatim, nothing attached), plus the mount banner and its zero-footprint contract (`--home` stays nonexistent).

---

## 2. Quick start: the smallest cluster on one host

A view is **always served by two processes**: `meta-server` owns the tree, `data-server` owns
the bytes — but a mount gets those two coordinates ONLY from the `manager`'s signed roster,
never from a hand-typed IP:PORT. The smallest cluster runs `manager` plus those two services
on this host's loopback over one storage root — `meta/` holding one tree per view, `blocks/`
holding the content objects. Move them to two machines and the commands are word-for-word
identical, only the addresses differ. Why a mount has no single-disk fallback, and why the
coordinates can only be bootstrapped through the manager, is `docs/adr/0001-manager.md` and
`docs/adr/0002-no-local-mount.md`.

```bash
Y=./target/release/grovefs
BASE=/data/myfs                       # the storage root both services share

# 1) declare the topology and start the manager (the first `serve` mints join.key / roster.key under $BASE/manager/)
$Y --home $BASE manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
$Y --home $BASE manager serve --listen 127.0.0.1:50055 &

# 2) the two services report they are alive to the manager (one storage root = the minimal cluster)
$Y meta-server --meta-dir $BASE --listen 127.0.0.1:50053 --advertise-addr 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &
$Y data-server --data-dir $BASE --listen 127.0.0.1:50100 --advertise-addr 127.0.0.1:50100 --member-id 0 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &

# 3) create the two views explicitly (the only entry point that builds a view)
$Y view --meta-dir $BASE create --view alice
$Y view --meta-dir $BASE create --view bob

# 4) mount: both coordinates resolve from the manager's signed roster (detaches by default: it returns once the mountpoint is live;
#    --foreground stays attached, Ctrl-C unmounts)
$Y mount --view alice --mountpoint /mnt/alice --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key
$Y mount --view bob   --mountpoint /mnt/bob   --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key

# 5) read and write it like an ordinary directory
mkdir -p /mnt/alice/projects/2026
echo "report" > /mnt/alice/projects/2026/report.txt
ls /mnt/bob                               # bob cannot see alice's files

# 6) unmount
$Y umount --mountpoint /mnt/alice
fusermount -u /mnt/alice                  # equivalent
```

**View isolation**: `/mnt/alice` and `/mnt/bob` are two independent trees, invisible to each other. The hierarchy (nested directories you `mkdir`) is fully custom.

**No hand-typed coordinates**: a client never writes down a service address; a mount passes only
`--manager HOST:PORT --roster-key-file F`, and the signed roster resolves the two coordinates (§4.2).
The roster is read once, at mount time, and never sits on the IO path — `mount` no longer accepts a
hand-typed `--meta-server`/`--data-servers`. This is a **replacement**, not an addition.

**What happens without the bootstrap**: a `mount` given neither `--manager` (nor `GROVEFS_MANAGER`)
exits on the spot, with no "settle for less" fallback. Verbatim:

```text
$ grovefs mount --view team -m /mnt/team
error: a mount resolves its cluster coordinates from the deployment's manager: there is no longer a way to name a metadata or data service by hand. Pass --manager HOST:PORT --roster-key-file PATH (or the GROVEFS_MANAGER / GROVEFS_ROSTER_KEY_FILE env knobs) to fetch the deployment's SIGNED roster and dial the meta- and data-servers it declares. Directly naming the addresses was removed on purpose: one --home holding a view's tree AND its bytes would make every client an authoritative store, and hand-typed lists let two mounts of one view dial different (or half a) cluster; a manager declaring the topology once and every client verifying the same signature is what keeps them consistent. Start the services with --register-with the manager, then `grovefs manager define`/`serve` it. --home stays the store root of the node-side verbs (view create, gc, import, apikey, du) and of the servers themselves, never of a mount.
```

`GROVEFS_MANAGER` / `GROVEFS_ROSTER_KEY_FILE` (or the same keys in §1.1's config file) satisfy the
requirement too; the roster's meta list may be an ORDERED failover list and its data list the COMPLETE
stripe member set (§4, §5.5). Why a mount has no local fallback at all - the motivation, the rejected
alternatives and the price paid - is `docs/adr/0002-no-local-mount.md`.

---

## 3. View customization and governance: manifest, lens, derived, alias, fork

`view create --manifest` can lay down a directory skeleton in one shot when creating a view. A manifest supports only two directives per line, and its paths are **relative to the view root** — `mkdir projects` and `mkdir /projects` are the same directory:

```text
# 这是注释,行首 # 会被忽略
mkdir projects
mkdir projects/2026
touch projects/2026/README.txt
```

```bash
$Y view --meta-dir $BASE create --view alice --manifest ./seed.txt
created BASE view 'alice' at /data/myfs/meta/alice/alice.db
applied seed manifest ./seed.txt
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
$Y view --meta-dir $BASE create --view alice --manifest ./seed.txt   # the same command again
error: view 'alice' already exists — remove it first (`grovefs view rm --name alice --base`) or choose a different name
```

Two boundaries are worth stating: ① `view create` **refuses a duplicate name**, so "re-running is harmless"
describes the **seeder**, not this command — feeding the same manifest a second time skips each name
already present and plants only the new ones (the unit test
`seed_manifest_takes_relative_and_absolute_paths_alike` pins this; the second pass counts `0/0`);
② a `touch` object lands in the `blocks/` **sharing that meta root**, which is the assumption `view create`
and `meta-server` have in common — when the `data-server` runs on a different `--data-dir` it cannot see
those objects, so such a deployment seeds directories only (`mkdir` lines) and writes bytes through a
mount. Under the PG backend `--manifest` is refused outright (`seeds a SQLite db file; under
--meta-store postgres://… mount the view and write the files instead`) — it produces no local DB.

Once mounted you see the pre-built hierarchy.

### 3.0 The four views in one table (beginner edition)

> This section answers just three things: **what each of the four views is, which files/tables each command produces, and how to fill in the parameters**.
> §3.1–§3.4.1 are the operational detail; on first contact, read this section fully, then follow the examples in §3.2/§3.4.

> **Explicit creation (must read)**: `meta-server` has exactly one rule at `M_OPEN` — when `meta/<name>/<name>.db` (under PG, a `grovefs_v_<hex>` schema) does not exist it is refused outright with a readable reason, and an empty DB is **never** created at mount time; this rule has no switch. The server prints the contract in its startup log (several `.build-bin/test_*.sh` suites assert this line verbatim):
>
> ```text
> meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view --meta-dir <dir> create --view <name>`); no empty tree is ever auto-created
> ```

**First, correct the most common misconception: writing data is not creating a base view.** A `mkdir`/`echo` into a mountpoint only adds nodes to an **already existing** tree; when `mount --view <name>` hits a tree the server does not have, the handshake is refused and the server writes nothing:

```text
$ grovefs mount --view nosuch -m /mnt/nosuch --manager 127.0.0.1:50055 --roster-key-file ~/roster.key
error: view 'nosuch' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --meta-dir <meta-dir> create --view nosuch` (or seed a tree with `grovefs view import`)
```

The only entry point for creating a base view is `view create --view` (which first checks that the name is not already taken by a derived/alias/fork), plus `import --view V --source DIR` / `view import` (which build the tree when the target is absent).

#### 1. A one-line analogy

| Form | Analogy | One line to remember it |
| --- | --- | --- |
| **Base view** | The warehouse original | The original that owns the data; content writable, deleting a name reclaims bytes |
| **Derived view** | A read-only filter on the original | Rules stored on the control plane (SQLite local `meta/views/`, PG `grovefs_control`); mounts read-only; the data is still the base view's copy |
| **Pure alias** | A door plate | Gives the original a public name; what mounts is the original, whole tree writable |
| **fork** | A photocopied catalog | Directory entries copied once, tidiable at will; bytes still shared, content can't be written in |

#### 2. The four forms in one table

| | Base view | Derived view | Pure alias | fork |
| --- | --- | --- | --- | --- |
| **Essence** | A real physical tree | base tree + rules (builds no tree) | name → real view (builds no tree) | An independent physical tree (dir entries copied from the base tree) |
| **Create command** | `view create --view V` | `view define --name D --base B --rules F` | `view alias --name A --real R` | `view fork --name F --base B` |
| **New files** | `meta/V/V.db` | `meta/views/D.layout` | `meta/views/A.layout` | `meta/F/F.db` + `meta/views/F.layout` |
| **Can change dirs after mount** | Yes | No (read-only) | Yes | Yes |
| **Can write content after mount** | Yes | No (`EROFS`) | Yes | No (`EROFS`) |
| **When deleting one name** | Removes the base tree's name; bytes freed only when the ledger hits zero | Derived mount is read-only, can't delete (goes through base) | Equivalent to deleting on the real view; bytes freed only when the ledger hits zero | Removes only the fork's own name; base tree untouched; bytes freed only when the ledger hits zero |
| **When deleting the whole view** | `view rm --base`: drop the tree + reclaim bytes no one holds | `view rm --name`: deletes only the definition | `view rm --name`: deletes only the definition, real name becomes mountable again | `view rm --name`: deletes the definition + its own tree + rebuilds the ledger |
| **Typical use** | Actually storing data | A team sharing one read-only shape | Hiding the storage real name / swapping the public name | An tidiable independent branch over the base data |

**Choice in one line**: store/modify content → base view; change the shape read-only only → derived; swap the public name only → alias; tidy the directories independently without uploading → fork.

> "Can content be written" is a hard contract: a derived mount is a read-only mount on the client; fork allows structural ops, and `write`/`truncate` return `EROFS` from the client. The server adds a separate `EPERM` gate on write-intent against derived names, so bypassing the client still can't write. **None of derived/alias/fork is a security boundary** — anyone who can mount the base name still sees the whole tree; use independent views + token/API key for hard isolation (§6).

**When a write fails, at which step the error surfaces** (ops troubleshooting quick reference; per-`open` probe echo and causes in docs.html §12):

| Form | Structural write<br>mkdir/rename/unlink | Content write · truncate class<br>`O_TRUNC`/`truncate` | Content write · append/plain write<br>`write()` on `O_WRONLY`/`O_APPEND` | shell `>>` append appearance |
| --- | --- | --- | --- | --- |
| **Base view** (writable key) | allowed | allowed | allowed | rc=0, really hits disk |
| **Pure alias** A→R (underlying writable) | allowed (same tree) | allowed | allowed | rc=0, writing A writes R |
| **Derived lens** D over B | `EROFS` (whole tree read-only) | `EROFS` | `EROFS` | can't write, `>>` also errors |
| **fork** (structure writable / content read-only) | allowed | `EROFS` **refused at open** (`>` rc=1) | `EROFS` **returned on the first `write(2)`** (the first write after creating an empty file is likewise refused on the spot) | rc=1 reports "read-only filesystem", no false success |
| **Read-only key** | remount: same as the derived lens, refused whole-tree; a mount routed through the server flipped in place by `set-ro`: the server gate refuses, failure becomes visible on the fsync round-trip (a local `--home` has no credential gate, `set-ro` does not affect already-built mounts) | same two branches as the left | same two branches as the left | after remount no false success; only in the ≤1s recheck window at the instant of a `set-ro` flip is an existing (server-routed) mount's `>>` still possibly rc=0 (both ro-take-effect and restore each have a ≤1s recheck window) |

> ⚠ A content-read-only mount does not enable page-cache writeback, so **the first `write(2)` returns `EROFS`** and the shell `>>` rc is reliable. The one exception: **a running view mounted through the server (meta/data-server) flipped in place by `apikey set-ro`** — the kernel mount flag can't change mid-flight, so an append at the switch instant may still be rc=0 with bytes not landed (failure visible on the fsync round-trip, a ≤1s recheck window); to confirm such a flip took effect, remount and observe that open-then-refuse holds. A local `--home` mount has no server credential gate (admission is guarded by filesystem permissions), so `set-ro` does not affect its already-built mounts.

#### 2.1 Three common follow-ups (don't mistake a "filter" for a "copy")

| Follow-up | Answer | Where you can see it |
| --- | --- | --- |
| Does a derived view **copy** the base view, just read-only for the user? | **It's not a copy.** A derived view lands only one `.layout` text (`base B` + rules), copying no dir entries, copying no bytes. Each time a directory is read, the server/local computes the display shape **on the fly** on the base tree per the rules. | `view define` builds no `meta/D/D.db`; the directory has only `meta/views/D.layout`; `view list` prints `view 'D' -> base 'B': 1 hide, 1 only, 1 reloc rule(s) (read-only)` rather than "a tree"; after editing a rule it reshapes via the server within ≤2s. The cost is that the directory can't be changed either and content writes return `EROFS` — because it never had its own dir entries to change. |
| Is an alias just the base view with a different name? | **Yes.** An alias has no metadata of its own; at resolution the name is swapped to the real view `R`, and thereafter reading dirs, making dirs, writing files, deleting files all land on `R`'s **same tree, same bytes**. | An alias occupies only one line `alias R` in `meta/views/A.layout`, with no `meta/A/A.db`; after `view alias`, a file written into `A/` is immediately visible in `R/` (the same inode). The only difference is **name governance**: while the alias exists `mount --view R` is refused, and one real view can have only one alias at a time (rotate with `--retire`). |
| Is a fork a full metadata copy, where you can change dirs but can't delete the underlying data? | **First half correct; second half needs to separate "delete the name" from "delete the bytes".** A fork copies the base tree's `node` rows (dir entries, incl. inode/hardlink relations), so it has its own complete, modifiable directory tree; content writes return `EROFS`. Deleting a name in the fork removes only the fork's own dir entry, the base tree is untouched; whether bytes are reclaimed depends on the `refs.db` ledger — the object is really deleted only when no view holds it anymore. | `view fork` creates `meta/F/F.db` and records in `refs.db`; `rm --name F` deletes the fork's own tree + definition, the base tree and still-held bytes are untouched; files added to the base tree later do not automatically appear in the fork. |

#### 2.2 Is a derived view necessary? Can "alias + read-only API key" replace it

**It can replace it only when you want "the whole tree read-only"; the moment you need to change the shape, it can't.** The two manage **two orthogonal dimensions**: the key manages "can you write", the derived view manages "what you see, and at which path".

| Dimension | Alias `A -> R` + read-only key | Derived view `D over B` |
| --- | --- | --- |
| The tree you see after mounting | **the whole tree** (verified `ls` shows `docs raw secret`) | **the shape the rules keep** (same data, verified only `docs`) |
| The excluded names | visible, readable, just not writable | **the name doesn't exist**, `ls` reports `ENOENT`, not even existence is leaked |
| Can paths be reorganized | No (shape identical to `R`) | Yes (`map`/`link` hang `/raw` at `/archive/raw`) |
| Granularity of the key | only two levels — "bind a view + ro/rw" (`apikey create --view`, no path parameter) | the rules themselves are **path-level**, precise down to a subdirectory |
| Count and names | one real view **can have only one alias**, and while the alias exists the real name can't mount (a second alias is refused, verbatim: `one public name per real view`, rotate with `--retire`) | unlimited count, each derived view takes one name, the base view still mounts as before |
| Nature of read-only | **permission**: when mounted through the server it can be flipped hot with `set-ro` (both directions hot-effective; only for mounts whose read-only already landed in the kernel at handshake does the removal require a remount, see below) | **contract**: swap to an unrestricted rw key to mount, `mkdir` is still `EROFS` |
| Distribution cost | must hand out the secret, must rotate/revoke | one text on the server, change one line and it's effective within ≤2s, nothing to hand out |

The verified directions of read-only-key hot switching (important, all referring to **mounts via meta/data-server** — the switch relies on the server's live-session revalidation tick; a local `--home` mount has no server credential gate, `set-ro` does not affect already-built mounts): **rw → ro is hot-effective** (during a mount, after `set-ro` a `mkdir` becomes `EPERM`, no remount needed); **a mount established with an rw key, flipped by `set-ro` and then `--rw` back, restores write just as hot-effectively** — local verification (`.build-bin/repro_setro_resume.sh`): within the recheck window after the flip there is still at most one tick of residual refusal (`EROFS`), and once that tick passes the same mount directly reports `write+fsync ok`, no remount needed, and a failed write attempt leaves no bytes. **Only for a mount whose key was already ro at handshake, with read-only already landed in the kernel**, `set-ro --rw` can't change the kernel flag, `mkdir` still `EROFS`, and it must be remounted (or just remount with an rw key). The command output's "no reconnect, no remount" holds in both directions for server-gate flips, but not for the removal of kernel-level read-only.

**Conclusion**: for external sharing, when you have just two kinds of demand —
- only "don't let people write" → a read-only key suffices, no alias even needed (send the ro key directly for them to mount `R`);
- plus "don't let people see everything" or "give them a different path" → you must use a derived view (or an independent view);
- the two can stack: the derived view gives the shape, the key gives the permission. **The security boundary is always the key/view; `.layout` is only a presentation rule.**

#### 2.3 The four-step fork ledger, verified: deleting a name ≠ deleting bytes

One `hello world` (ledger `holders=1`), and where `fork` leads:

| Step | `refs.db` | `blocks/<bucket>/<key>` | Can the base view still read it |
| --- | --- | --- | --- |
| base view writes `big.bin` | `holders=1` | present | yes |
| `view fork --name exp --base team` | `holders=2` | present (bytes not copied) | yes |
| `rm big.bin` inside the fork | `holders=1` | **still there** | yes (`cat` still `hello world`) |
| the base view also `rm big.bin` | row gone (`0`) | **reclaimed**, bucket dir empty | no |

Two easily-missed points: a fork's "writable" reaches only the structural layer — `mkdir`/`rename`/`unlink` are allowed, **but even content into the empty file it `touch`ed itself can't be written** (`EROFS`); and the block path is not a prefix of `data_ref`, it's a `FNV-1a(key) % 256` bucket (`blocks/b4/<key>`), so looking by key prefix would make you think "the block is lost".

#### 2.4 Can "fork + delete rows" replace a derived view, without `.layout`

**You can materialize the same visible shape, but in exchange you get snapshot semantics.** Deleting rows in a fork makes the deleted names truly nonexistent (`ENOENT`) in that view, the base tree untouched — this works today, needing no new mechanism. The problem isn't "can you hide", it's "how does it live afterward":

| | fork + delete rows | derived (`.layout`) |
| --- | --- | --- |
| Visible shape | delete a row = hide, materialized in `meta/F/F.db` | computed by rules, no rows landed |
| Base tree add/rename/reattr | **does not follow** (the copy happened in an instant, must re-fork) | **follows immediately** (computed fresh on each readdir) |
| Cost (20,201 entries measured) | `fork` 0.61s, **20,201 rows / 3.5 MB** | `define` 0.00s, **47 bytes** |
| Cost (100,002 entries measured) | `fork` **0.65s**, `meta/F/F.db` **12 MB** + 13-byte definition | `define` 0.00s, 100 rules **2.9 KB** |
| To change "what's hidden" once | re-fork, or hand-edit rows (after editing they drift from the rules) | change one line, effective within ≤2s via the server |
| Read-only guarantee | structure is writable by default, needs an extra flag to be read-only | read-only by nature (still `EROFS` with an rw key) |
| Can you tell "what was hidden" | can't tell — the rows are gone, an audit can't distinguish "hidden" from "never existed" | `view list` reports `1 hide, 1 only, …` directly |
| Multiple external perspectives | N copies × N rows | N texts × tens of bytes |

Extrapolating from measurements (100,002 entries → 12 MB, 0.65s, ~125 B/row): for a view of 10 million entries, **every extra "fork-as-lens" costs about 1.25 GB more metadata and about 65 seconds of copying**; a derived view of 100 rules is only 2.9 KB.

> Want "the definition to also be just a table row": the PG backend already is a row in `grovefs_control.view_defs` (the SQLite side uses a text file so it can be hand-edited, `git diff`'d, and needs no schema migration).

**"Make the fork follow base writes" — can that be done by appending metadata to the fork? Not today, and there's no such entry point.** A fork's `node` rows have only their own `id/parent_id/name/ino/data_ref`, no back-pointer to "which base-tree row this came from", no change sequence; following would mean producing an increment on every base-tree create/rename/unlink, writing it into the fork, and resolving conflicts (base renamed vs. the fork having already tidied that entry elsewhere) — that's a whole new sync protocol, not "appending a few metadata rows". Today there are three ready paths:

- **Want a snapshot, want to tidy** → fork; when the base changes, re-`view fork` (100k: 0.65s / 12 MB). Rebuilding loses the fork-side tidying, so use "blue-green": fork to a new name `F2` → mount and verify → clients switch over → `view rm --name F`.
- **Want live-following, no independent tree** → derived (`.layout`): once the base changes, the next directory read shows the new shape; the cost is a read-only mount and untidiable directories.
- **Rebuild a fork on another machine** → `view export` + `view import --as-fork B` (§3.4.2), likewise a one-shot snapshot.

**Choice**: need "freeze one copy, decouple from the base tree" → fork (delete rows to hide); need "several read-only views over one live tree" → derived. The two differ in **time semantics** (snapshot vs. rule), not two spellings of one feature.

#### 3. What each command produces/changes

> Paths are shown with the SQLite storage root `D`. The PG backend has no local `.db`/`.layout` files; see the end of this section for the counterparts.

| Command | Produces/changes | What it doesn't touch |
| --- | --- | --- |
| `grovefs view --meta-dir D create --view V [--manifest F]` | creates `D/meta/V/V.db` (with `node`/`ctrl`/`locks` tables); `--manifest` additionally writes dir/empty-file nodes into the tree | doesn't change other views |
| `grovefs mount --view V --manager MGR --roster-key-file KEY` (the only mount form) | the server resolves `V`: a hit on `D/meta/views/V.layout` follows the definition (an alias swaps the name, a derived view applies its read-only lens, a fork opens its own tree), otherwise it opens `D/meta/V/V.db`; neither present → **refused outright** with a prompt to `view create` first. The client keeps nothing on disk | doesn't change an existing tree |
| `grovefs view --meta-dir D define --name X --base B --rules F` | writes/overwrites `D/meta/views/X.layout`, body containing a `base B` line, the rest being rules | builds no `X.db`, copies/moves no content |
| `grovefs view --meta-dir D alias --name A --real R [--retire OLD]` | writes/overwrites `D/meta/views/A.layout` (a single line `alias R`); `--retire` also deletes the old alias definition | builds no `A.db`; `R`'s data untouched |
| `grovefs view --meta-dir D fork --name X --base B` | creates `D/meta/X/X.db` (copying B's `node` rows, preserving inode/hardlink relations); writes `D/meta/views/X.layout` (a single line `fork B`); records the shared bytes in `D/meta/_control/refs.db` | copies no bytes; doesn't change B's tree |
| `grovefs view --meta-dir D rm --name X` | deletes `D/meta/views/X.layout`; if X is a fork, also deletes `D/meta/X/X.db` (+WAL/SHM) and rebuilds `refs.db` | derived/alias touch no data; fork doesn't touch the base tree or still-held bytes |
| `grovefs view --meta-dir D rm --name V --base` | deletes `D/meta/V/V.db` (+WAL/SHM), rebuilds `refs.db`, and reclaims objects **no one holds anymore** from `--data-dir` (default `D`) | bytes still held by other views are kept |
| `grovefs mount … --layout F` | the rules file itself is never landed; the client wraps `F` around the **already resolved** provider, **stacking** with the published derived definition (measured in §3.0 pitfall 5) | doesn't change server state; affects only this mount |
| `grovefs view --meta-dir D export/import --view V` | export: one JSONL file; import: creates `D/meta/V/V.db` (under `--meta-store postgres://…` the import opens that view's schema in the database, §15.9) | doesn't move bytes (bytes must be copied separately into `blocks/` or handed to the data-server's data root) |

A few easily-missed rules:

- **A definition is just a text slot**: `view define/alias/fork` write `meta/views/<name>.layout`; only fork additionally creates a `meta/<name>/<name>.db` tree.
- **A definition may precede the base tree**: `view define` allows the base to not be built yet (it warns), but **at mount time** both server and local enforce that the base must already be created, otherwise the derived name is refused mounting. Recommended order remains "`view create` the base view first, then define".
- **Deleting a definition is not hot-effective**: after `view rm --name`, new mounts immediately don't see the definition; already-mounted derived/alias connections keep the old resolution until remount (or until the server cache is cleared by an HA/snapshot event).
- **Deleting a base view has four guards**: the name itself must not still be governed by a definition; no other derived/alias/fork may point at it; it must not still be mounted on this machine; and the tree in PG must be deleted **with the same command** (`view --meta-store postgres://… rm --base`) — the ledger decrement and `DROP SCHEMA` finish in one transaction (§15.8); hand-`DROP SCHEMA` past the CLI leaves ledger rows that forever over-count.

#### 4. What lands on disk: files and tables

After one `view create` + `view define` + `view alias` + `view fork`, the storage root looks roughly like this:

```text
D/
├─ blocks/<2hex>/<key>         # 所有视图共享的文件字节(key 是 UUID,不是内容哈希)
└─ meta/
   ├─ team/                    # 基视图 team:一棵树一个目录
   │  └─ team.db               # node + ctrl + locks(+ -wal/-shm 随 checkpoint 出没)
   ├─ lab/                     # fork lab:独立元数据树
   │  └─ lab.db                # 自己的 node + ctrl + locks
   ├─ _control/                # 控制面:不是树(目录名是保留视图名,永不被当树枚举)
   │  ├─ refs.db               # 跨视图持有账本:谁还在引用哪个 key
   │  ├─ api_keys.db           # 可选:API key/审计
   │  └─ store.key             # 开 api_keys.db 那份密文副本的密钥(0600)
   └─ views/
      ├─ docsonly.layout       # base team + only /docs
      ├─ public.layout         # alias team
      └─ lab.layout            # fork team
```

`ls meta/` lists trees and nothing else: `_control/`, `views/` and the dot-prefixed `.xfer/` (the server-side export/import spool, present once those verbs have run) are not trees and can never be a view name.

The three `.layout` bodies look like this respectively (plain text, not SQLite DBs):

```text
# meta/views/docsonly.layout
base team
only /docs

# meta/views/public.layout
alias team

# meta/views/lab.layout
fork team
```

| On-disk artifact | What it is | Who produces/maintains it |
| --- | --- | --- |
| `meta/<view>/<view>.db` | a SQLite DB; the `node` table is the directory tree (one row per name), `ctrl` a key-value table (today only the HA `epoch`), `locks` the lock leases | `view create`/`view fork`/`view import` |
| `meta/views/<name>.layout` | **plain text** definition, not a database; three mutually exclusive forms: `base B`+rules / `alias R` / `fork B` | `view define`/`alias`/`fork`; deleted by `view rm` |
| `meta/_control/refs.db` | the shared-bytes ledger; when a name is deleted it decides "whether any other view still holds it" | auto-booked on every add/delete; `refs rebuild` recomputes |
| `blocks/<2hex>/<key>` | the file's real bytes; all views store only the key | produced on write/import; deleted on reclaim |
| `meta/_control/api_keys.db` | optional control plane: key digests and call audit | the `apikey` command / `mcp --require-api-key` |

A row of the `node` table contains: `id`, `ino`, `parent_id`, `name`, `is_dir`, `data_ref` (the key pointing into blocks), `link_target`, `mode/uid/gid`, `mtime/atime/ctime`, with a uniqueness constraint on `(parent_id, name)`. `ctrl` is a `(k, v)` key-value table holding today only the HA `epoch` (**not an inode counter** — new inodes aren't allocated from a cursor, they equal their own row's `id`); `locks` are POSIX/BSD lock leases. `refs.db` has a single `holders(data_ref TEXT PRIMARY KEY, holders INTEGER)` table recording how many views hold each object. `api_keys.db` (optional) has two tables `api_keys` and `mcp_calls`: the former carries `id, label, secret_hash, created_at, last_used_at, revoked_at, scope, view, ro, owner, note, expires_at, created_by, secret_hint, secret_enc` — beyond the digest, three bookkeeping fields (remark / deadline / minter), one head+tail fingerprint, and one **reversible ciphertext copy** `secret_enc` (NULL while no wrap material exists); the store still holds **no plaintext** anywhere — the latter is the call audit. The PG tier uses the same column names (`view_name`/`ro BOOLEAN`, plus an extra `seq` identity column). Old stores upgrade in place: opening the registry adds the missing columns with `ALTER TABLE … ADD COLUMN` and rewrites no row. **The difference between views is not the table structure, but "whether this name has its own `.db`"**.

**The PG backend's (`--meta-store postgres://…`) correspondence**

| SQLite on-disk artifact | PG counterpart |
| --- | --- |
| `meta/<view>/<view>.db` | a schema `grovefs_v_<hex(view)>` (with the same-shape `node`/`ctrl`/`locks`) |
| `meta/views/<name>.layout` | a row in the `grovefs_control.view_defs` table |
| `meta/_control/refs.db` | the shared table `grovefs_refs.holders` |
| `meta/_control/api_keys.db` | `grovefs_control.api_keys` + `mcp_calls` |

Under PG, `view define/alias/rm --name` works as usual (writing `view_defs`); `view --meta-store postgres://… create` explicitly builds that view's schema (`grovefs_v_<hex>`); `view --meta-store postgres://… rm --base` decrements the ledger (per-key `grovefs_refs.holders`) and runs `DROP SCHEMA … CASCADE` in one transaction, then reclaims `blocks/` bytes by the `data_ref` read before deletion (§15.8); `view fork` is likewise available on both backends (copying the `grovefs_v_<hex>` schema in a single transaction); zero-copy `import` is still based on the local file tree and refuses.

> **PG DB-creation entry point**: `grovefs view --meta-store postgres://… --meta-dir D create --view V` explicitly builds that view's `grovefs_v_<hex>` schema and root row. Mounting does not build a schema — when the schema is absent, `M_OPEN` refuses outright and prompts this command; an existing schema can also be restored from a dump.

#### 5. How to fill in the parameters

| Parameter | Used where | Meaning |
| --- | --- | --- |
| `--meta-dir D` | `view`/`apikey`/`mcp`/`refs`/`import`, and `meta-server` | the metadata root: has `meta/` under it; points to the same directory as `meta-server --meta-dir` |
| `--home D` | the global parameter (before the subcommand) | the default **storage root**: where a call lands when it names no `--meta-dir`/`--data-dir`. Only the node-side verbs (`view`/`gc`/`import`/`apikey`/`du`) and the two servers read it — it has **nothing to do with a mount** (§0) |
| `--meta-store postgres://…` | `view`/`apikey`/`mcp`/`meta-server`/`import` | the control plane switches to PG: `view_defs`/`api_keys`/`mcp_calls`; nothing local lands as `.layout`. An `import`'s product can only be a local SQLite tree, so a non-SQLite URI is refused with the alternative path spelled out (in-mount `cp`/`rsync`) |
| `--view V` | `view create`/`mount`/`import`/`du` | the view name (one safe path element) |
| `--name X` | `view define/fork/alias/rm` | the new definition's name (derived/fork/alias name, or name to delete) |
| `--base B` | `view define/fork` | the referenced **physical base view** name |
| `--real R` | `view alias` | the real physical view name the alias points to |
| `--retire OLD` | `view alias` | atomic rotation: after creating the new alias, delete the same real view's old alias |
| `--rules F` | `view define` | the rules file path (plain text, one verb per line) |
| `--manifest F` | `view create` | the seed manifest (`mkdir`/`touch` lines, paths relative to the view root), pre-laying the skeleton when building the tree |
| `--data-dir D` | `view rm --base`/`import`/`du` | the root where `blocks/` lives; defaults to the metadata root (same path as `--meta-dir`/`--home`), only needs naming when meta and data are separated; deleting a base view reclaims bytes from here, `du` measures bytes from here |
| `--layout F` | `mount` | this mount's client-side rules file; affects only this mount, writable; **stacks as a second lens** on top of a published derived definition (measured in §3.0 pitfall 5) |

**The four verbs of a rules file (`--rules` / `--layout`)**

| Form | Parameters | Effect | Beginner note |
| --- | --- | --- | --- |
| `hide /a/b` | one source path | this subtree is entirely invisible in this view | affects display only; source data still there |
| `only /a` | one source path, one per line | show only these subtrees; ancestor dirs along the way are auto-allowed | write more lines for more; not writable outside the allowlist |
| `map /a/b /x/y` | source path + target path | the entry vanishes from the source and appears at the target; intermediate dirs auto-synthesized | the source path is the real path in the base tree; you can't create inside a synthesized dir |
| `link /a/b /x/y` | source path + target path | visible in both places, same inode | copies no bytes; deleting either place removes only that name |

- All paths are **in-view absolute paths** (starting with `/`); `<src>` usually must be a path that **really exists in the base view**; `<dst>` may be a new path you invent.
- One rule per line, `#` starts a comment, blank lines ignored; the same `<dst>` may not appear in two `map`/`link` rules.
- The parsing stage **does not verify whether a source path exists**: a typo usually won't error, that content just won't appear. Check the base tree before writing rules — `find` after mounting, or MCP `list_view_tree`; don't guess from the source dir (`import` adds a prefix).
- **With no rules, a derived view matches the base view entry-for-entry**: when `.layout` has only a `base B` line, the diff between `find` output and the base view is empty (verified) — "identical" is a special case, not a default constraint; once you write `hide`/`only`/`map`, the shapes differ.
- **A synthesized directory's inode comes from reserved high bits**: `map`/`link` intermediate dirs, which don't exist in the base tree, are issued downward from `i64::MAX` (verified: after `map /raw /archive/raw`, `/archive`'s inode is `9223372036854775807`); `only`'s ancestor dirs are all real dirs in the base tree, merely "allowed", their attributes unchanged.
- **`..` behaves differently per verb**: a `map` leaf rewrites its own reported name/parent, `..` stays inside the lens; `link` keeps the source identity, `..` returns to the **source parent** (same inode, two names, path resolution uses the real coordinates).

#### 6. Which tree does `mount --view X` actually pick

Judged in order, stop at the first hit:

1. `meta/views/X.layout` is a single `alias R` line → swap to the real view `R`, resolve by `R` thereafter; `X` is not an independent tree, and `R` can no longer be mounted by its real name.
2. It is a single `fork B` line → open `X`'s own `meta/X/X.db` (the tree copied when the fork was built), unrelated to `B`'s later changes.
3. It is `base B` + rule lines → open `meta/B/B.db` and apply the following rules as a read-only lens.
4. No `X.layout` → `X` itself is a base view, open `meta/X/X.db` directly.
5. `meta/X/X.db` absent too → **refuse outright** (exit=1), never falling back to an empty tree:
   `error: view 'X' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --meta-dir <dir> create --view X` (or seed a tree with `grovefs view import`)`

> Resolution happens **only on the server**: it reads `<meta-dir>/meta/views/` (the PG backend reads `grovefs_control.view_defs`), and the client has no view namespace of its own. Both single-hop rules hold: an alias can't point at another alias, and a name that has a definition can't be a fork's base; the server additionally refuses "derived over derived", and a derived mount requires the base to already exist.

#### 7. The easiest pitfalls

1. **Writing data does not create a base view.** New trees can only be made with `view create` (or built in passing by `import`); a mount that finds no DB is refused outright and no empty tree is created.
2. **A derived view is read-only and is not a permission boundary.** Anyone who can mount the base name still sees the whole tree; use independent views + token/API key for isolation, don't rely on "hiding a few directories".
3. **An alias locks the real name.** While the alias exists, `mount --view R` is refused; new mounts use the alias. After deleting the alias the real name becomes mountable again (new mounts only). One real view can have only one alias at a time, rotate it with `--retire` in one command.
4. **A fork copies only directories, not bytes.** Content writes return `EROFS`; deleting the fork doesn't affect the base tree; files added to the base later don't automatically appear in the fork; both backends can fork, and it's the same verb in each (the store-holding process does the copy: SQLite copies file rows, PG across schemas, each committing by budget — one transaction if it fits, otherwise automatic batching).
5. **`--layout` and `meta/views/*.layout` are different things, and they stack.** The former affects only this mount — the client wraps it around the **already resolved** tree — and is writable; the latter is a server-named definition, read-only. Measured on one copy of the data: mounting `docsonly` (definition `only /docs`) shows `docs/{a.md,keep.md}`; adding `--layout` (rule `hide /docs/keep.md`) makes both layers take effect at once — `docs/{a.md}`, still read-only — with the mount log printing two lines back to back (`derived view 'docsonly' over base 'team': read-only lens` + `layout overlay client.rules: 1 hide, 0 only, 0 reloc rule(s)`). To let the client decide alone, mount the base name with `--layout` (measured: root lists `data docs photos`, writable).
6. **Bytes are truly reclaimed only when the ledger hits zero.** Deleting a derived/alias removes only the definition; deleting a fork removes only its own tree; deleting a name in any view frees the object only once no other view holds it in `refs.db`.
7. **`view define` may precede the base tree.** It warns base tree missing; it's enforced only at mount. `view create` first, then define, saves one round of troubleshooting.
8. **A misspelled view name errors outright, no empty tree grows.** The server refuses a name with neither a tree nor a definition (verbatim text in step 6 above), and so does an alias name retired by `--retire` — this is the only behavior, no switch.

**Error cross-reference**: `EROFS` = read-only filesystem (derived mounts, fork content writes); `EPERM` = operation not permitted (creating into a synthesized dir, the server's derived-write gate).

#### 8. Walk it through from scratch (copy as-is)

Every line of output below is a real artifact of this release build (`.bneck/probe_walkthrough.sh`; substitute your own view names and paths):

```bash
Y=./target/release/grovefs
D=/data/myfs                     # the storage root shared by manager, meta-server and data-server
GFS=(--manager 127.0.0.1:50055 --roster-key-file $D/manager/roster.key)   # mount coordinates resolve ONLY from the manager

# ⓪ start the three services (once is enough; cross-machine layout in §11): declare the topology, then register
$Y --home $D manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
$Y --home $D manager serve --listen 127.0.0.1:50055 &
$Y meta-server --meta-dir $D --listen 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file $D/manager/join.key &
$Y data-server --data-dir $D --listen 127.0.0.1:50100 --register-with 127.0.0.1:50055 --join-key-file $D/manager/join.key &

# ① create the base view (the tree that really stores data), mount it, write data
$Y view --meta-dir $D create --view team
created BASE view 'team' at /data/myfs/meta/team/team.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
$Y mount --view team -m /mnt/team "${GFS[@]}" &
mounting view 'team' at /mnt/team (fully remote: no local store)
mkdir -p /mnt/team/docs /mnt/team/raw
echo "hello" > /mnt/team/docs/a.md

# ② derived: read-only, showing only /docs
printf 'only /docs\n' > docsonly.rules
$Y view --meta-dir $D define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$Y mount --view docsonly -m /mnt/docsonly "${GFS[@]}" &
derived view 'docsonly' over base 'team': read-only lens
note: derived (virtual) views mount read-only — write to base view 'team'
mounting view 'docsonly' at /mnt/docsonly (fully remote: no local store, read-only)
ls /mnt/docsonly                  # docs
echo x > /mnt/docsonly/docs/b.md  # EROFS (every content write refused)

# ③ fork: an independent directory tree — structure writable, content read-only
$Y view --meta-dir $D fork --name lab --base team
forked view 'lab' from base 'team': 4 entries, 1 shared object(s)
note: 'lab' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch 'team'
note: content bytes are SHARED with 'team' (cross-view ledger): the object is freed only when no view holds a name for it anymore
$Y mount --view lab -m /mnt/lab "${GFS[@]}" &
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
mounting view 'lab' at /mnt/lab (fully remote: no local store)
mkdir /mnt/lab/review                        # allowed
mv /mnt/lab/docs/a.md /mnt/lab/review/a.md   # allowed
echo x > /mnt/lab/new.md                     # EROFS (every content write refused)

# ④ alias: a public name, whole tree writable; from now on `team` is not a mount entry point
$Y view --meta-dir $D alias --name public --real team
defined pure alias 'public' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (the meta-server refuses a mount of the real name); alias mounts show the FULL tree and are WRITABLE
$Y mount --view public -m /mnt/public "${GFS[@]}" &
alias view 'public' resolves to real view 'team' (full tree, writable)
mounting view 'public' at /mnt/public (fully remote: no local store)
echo hi > /mnt/public/docs/c.md              # allowed, and what it writes is `team`
$Y mount --view team -m /mnt/x "${GFS[@]}"   # the real name is governed: refused
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)

# ⑤ the listing and what landed on disk
$Y view --meta-dir $D list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
alias 'public' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
base view 'team' -> own tree meta/team/team.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --meta-dir /data/myfs list [--view NAME]` (add --view to see only that view's keys)
ls $D/meta $D/meta/views
  meta: _control lab team views
  views: docsonly.layout lab.layout public.layout
```

> `base view 'team'` stays in the listing — it is the storage identity; while the alias exists **new mounts** must use `public`, and `team` becomes mountable again only after the alias is deleted.

Cleanup order (delete the definitions first, then the base tree):

```bash
$Y view --meta-dir $D rm --name docsonly       # definition only; team's data untouched
$Y view --meta-dir $D rm --name lab            # drops the fork's own tree; team and the shared bytes untouched
$Y view --meta-dir $D rm --name public         # drops the alias; team becomes mountable again (new mounts only)
$Y view --meta-dir $D rm --name team --base --data-dir $D   # tree plus every byte no one holds anymore
```

### 3.1 Per-client display customization: `mount --layout` (same data, different shape)

A manifest fixes the shape of the **storage tree**; `--layout` fixes the shape **a particular mount sees** — data stays put, only the display changes. The rules file is one verb per line (`#` comments, all paths absolute, source paths point into the storage tree):

| Rule | Parameter meaning | Effect |
| --- | --- | --- |
| `hide <src>` | source path | this subtree is entirely hidden at this mountpoint |
| `only <src>` | allowlist source path, multiple allowed | show only these subtrees (ancestors leading to them are auto-allowed, but not writable outside the allowlist) |
| `map <src> <dst>` | source path + display path | reorganize: the source location vanishes, the entry appears at `<dst>`, intermediate dirs auto-synthesized |
| `link <src> <dst>` | source path + alias path | alias: both visible at once, same inode, no byte copy |

```bash
$ cat lens.txt
hide /photos/private
map /data/raw /team/raw
$Y mount --view alice -m /mnt/alice --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key --layout lens.txt
layout overlay lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at /mnt/alice (fully remote: no local store)

$ ls /mnt/alice            # 本客户端: data docs photos team
data  docs  photos  team
$ ls /mnt/alice/photos     # private 已隐藏
public
$ echo hi > /mnt/alice/team/raw/note.txt   # 写直通源对象,无透镜的其它挂载立即可见
$ mkdir /mnt/alice/team    # 透镜合成的脚手架名已占用
mkdir: 无法创建目录 “/mnt/alice/team”: 文件已存在
```

Key points (all backed by test assertions): the lens acts only on the display layer, **real entries keep their real inodes**, read/write/lock/stripe/GC all pass straight through; an `rm` through the lens deletes the source entry; other clients and other mounts are entirely unaffected (the lens lands nothing and changes no server state). It is **display customization, not a security boundary** — use independent view + token / view-bound key for hard isolation (§6.1). Limits: creating into a purely synthetic directory returns `EPERM`; symlink target text is not rewritten; under a shared meta-server, invalidation pushes for entries under a synthetic directory may miss the lens coordinates, so pair it with `--no-cache`; an illegal rules file makes the mount fail-fast.

### 3.2 Server-side derived views: the shape defined centrally on the server (`grovefs view`)

The lens rules in §3.1 are carried by each client; to give one set of shapes a **name and store it on the server** (a team sharing one display contract, authorized by name), use a derived view: `grovefs view define` writes the rules into the meta root's `<meta-dir>/meta/views/<name>.layout` (composed of a `base <physical view>` line plus the remaining rule lines, exactly the four verbs of §3.1; under `view --meta-store postgres://…` it writes into the PG `grovefs_control.view_defs` table, landing no local file), and clients just `mount --view <derived-name>` with **no local rules file needed**. Derived (virtual) views **always mount read-only** — the shape is centrally governed, distributed only, never modified; writes belong only to the original (base-name) view; for a writable custom shape use §3.1's client-carried `--layout` lens. The same `views/` directory also holds two other kinds of definitions — a **pure alias** (governing "what to call it", §3.4, `grovefs view alias`) and a **fork view** (an independent metadata tree: structure writable, content read-only, §3.4.1, `grovefs view fork`):

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D create --view V [--manifest F]` | V = the new **base view** (physical tree) name, F = optional seed manifest | explicitly creates a **physical tree** `D/meta/V/V.db` (the only entry point that builds a tree); **only a base view's deletion truly reclaims the underlying bytes** (cross-view reference ledger hits zero before physical GC, §5.3); derived views/aliases only reshape/rename, never participate in physical deletion |
| `grovefs view --meta-dir D define --name V --base B --rules F` | D = meta root (same as meta-server's `--meta-dir`), V = derived name, B = physical view name, F = rules file | writes/replaces `D/meta/views/V.layout` (under the PG backend `--meta-store`: edits a `grovefs_control.view_defs` row); `V`'s mount is **read-only** (writes go to `B`); for `meta-server`, every mount goes through the server, so established connections **reshape hot** within ≤2s (no remount) — for every mount |
| `grovefs view --meta-dir D list` | — | lists all view definitions: derived views (base/rule counts), pure aliases (→ real view) and forks (→ base view), plus **undefined bare base views** (→ their own `meta/<name>/<name>.db`), §3.4/§3.4.1 |
| `grovefs view --meta-dir D rm --name V` | V = derived, alias, or fork name | deletes only the definition, **touches no data**; exception: `rm --name <fork>` also deletes that fork's own `meta/<name>/<name>.db` (+WAL/SHM) and runs `refs rebuild` to recompute the ledger (base view and bytes unaffected); after deleting an alias the base name becomes mountable again (new mounts only) |

```bash
$ cat docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir $BASE define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir $BASE define --name team --base team --rules docsonly.rules   # 名与基同名
error: derived view name must differ from its base
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs mount --view docsonly --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/lens &   # only the derived name; the client carries no rules file
manager 10.0.0.5:50055: deployment 'default' epoch 1 resolved meta [10.0.0.5:50110] data [10.0.0.5:50100] (signature verified, valid for 300s more)
using remote metadata service at 10.0.0.5:50110
derived view 'docsonly' over base 'team': read-only lens
note: derived (virtual) views mount read-only — write to base view 'team'
using remote data service at 10.0.0.5:50100
mounting view 'docsonly' at /mnt/lens (fully remote: no local store, read-only)
$ ls /mnt/lens                       # only docs; mounting the base name `team` still shows the full tree, writable
docs
$ echo x > /mnt/lens/docs/new.md     # every write through a virtual view is refused (EROFS)
```

Semantics and guards (verified by the section-9 e2e): define rejects `name == base`, rejects the base itself being a derived view (no nesting), rejects shadowing a physical `<name>.db`, and fail-fasts on illegal rules; **virtual views always mount read-only (hard contract)** — writes belong only to the original (base-name) view, enforced at three layers: `meta-server` carries the derived flag in its `M_OPEN` handshake reply so the client lands a kernel-level read-only mount directly (write/`flock` write-lock returns `EROFS` immediately, zero round-trips); the server adds a separate authoritative gate on write-intent ops against derived names (returns `E_NOTPERM`→`EPERM`, deliberately not `E_ROFS` to avoid being misread as an HA failover signal), so old/spoofed clients can't bypass it; the client's read-only flag comes from the `M_OPEN` ack, not from its command line. **Editing a definition is hot-effective** — `meta-server` checks the definition version before dispatching each op (SQLite reads file mtime, PG reads `xmin`), so after a replacement an established mount auto-switches to the new shape within the client TTL (≤2s), no remount needed (verified ≤5s assertion, see section 9; the read-only contract is unchanged after reshaping), whereas `view rm` is not hot-effective (established mounts keep the old shape until remount, to avoid yanking a tree in use); multiple clients of the same name are rendered by the server's **same definition**, virtual inodes consistent across machines; under a shared meta-server a derived subscriber registers into both its own-name and base-name buckets and writes are double-broadcast, so base-side writes are immediately visible to derived mounts even without `--no-cache`; to limit a caller by name to reading only this derived view, mint a `--view docsonly --ro` key (writes always go to the base name, derived mounts are read-only anyway). Resolution happens only on the server: `mount --view <derived-name>` learns the shape from the `M_OPEN` ack and the client has no local `meta/views/` to read; adding `--layout` **stacks a client lens on top of it** (§3.0 pitfall 5), it is not either/or. Note: the SQLite tier's `grovefs view` operates server-local files and must run on the meta-server machine; under an HA deployment `meta/views/` must be synced to the standby manually along with the control-plane directory (`meta/_control/`); the PG tier keeps definitions in shared storage (`grovefs_control`), governed from any reachable node with the same `--meta-store`, no local files to sync (§6.2); it too is **not a security boundary**, mounting the base name directly still shows the whole tree (what can be blocked is the token / key, not the shape).

### 3.3 Zero-copy import of existing data (`grovefs import`): directories stay put, clients get it by connecting

When the server already has a batch of ready data (e.g. `/data/legacy`) that clients should fetch by view, you needn't load it in first: `grovefs import` registers an **existing local directory tree** into a view, bytes stay put — within the same filesystem the object is a hardlink to the source file (zero extra bytes, both paths the same inode, bidirectionally real-time visible), and across devices it auto-degrades to a streaming copy:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs import --view V --source DIR` | V = target view (created if absent), DIR = existing source dir | mirrors the whole tree into the view (default prefix `/<DIR name>`): dirs become nodes, files are hardlinked into `blocks/` and registered; mtime/atime/permissions/owner preserved |
| `grovefs import ... --prefix P` | P = target dir in the view (e.g. `/legacy`) | mirrors under the given prefix, `P` created automatically if absent |
| `grovefs import ... --meta-dir D [--data-dir R]` | D = meta root, R = storage root (default D) | under a meta-server deployment, run on the **storage machine**, writing directly into that deployment's authoritative SQLite tree `meta/V/V.db` and `R/blocks/`; already-mounted clients converge via TTL (≤2s) |
| `grovefs import ... --meta-store URI` (or `GROVEFS_META_STORE`) | URI = `postgres://…` | **refused**: an import can only produce a local SQLite tree, so writing under a PG deployment would be a shadow tree the server can't read; feed bytes through the mountpoint instead (§14.2), the error text names this alternative path |
| `grovefs import ... --dry-run` | — | reports only how many dirs/files would be imported, **not even creating the view db** |

```bash
$ grovefs --home /data/cluster import --view legacy --source /data/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home /data/cluster import --view legacy --source /data/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
$ grovefs mount --view legacy --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/da &   # 客户端连接即得
$ echo "new" > /mnt/da/legacy/docs/quarter.txt   # 经挂载写 → 源路径同 inode 同步可见
$ cat /data/legacy/docs/quarter.txt
new
$ rm /mnt/da/legacy/photos/2026/y.jpg            # 视图里删名字 → 只摘 blocks/ 链接,源文件毫发无损
```

Key semantics (the `import` suite: 35 assertions): **a re-run is incremental** (existing names skipped, new files registered); **GC removes only the link** (the source path is never deleted by view operations); entries after import have exactly the same rights as native writes (read/write/lock/snapshot/invalidation-push all apply), and can be composed by §3.2 derived views' `map`/`link` into any virtual directory shape — that shape mounts **read-only** (writes go to the imported original view and reach the virtual shape through double-broadcast push); `grovefs import` also refuses a derived name as its target (otherwise it writes a shadow `<view>.db` no one can read). Three implementation facts to note: hardlinking requires the source and `blocks/` to be on the same disk (across disks it auto-emits a copy and counts it honestly); when an external tool updates a source file via **rename/replace write** (vim's default), the view side sees the old inode's last content, so import must be re-run for the new content; symlinks/special files are deliberately skipped. Under a non-SQLite backend (`--meta-store postgres://…`, incl. the `GROVEFS_META_STORE` fallback) `import` **refuses outright** and gives the alternative path (mount the view, then feed bytes through the mountpoint, §14.2) — it can only produce a local `meta/<view>/<view>.db`+`blocks/`, and writing them down would be a shadow tree the server can't see (proven by the `import`/`govcheck` suites; `view fork` is not in this category — it can already copy inside each backend's authoritative store).

---

### 3.4 Pure alias: name governance (`grovefs view alias`)

§3.2's derived views govern "what it looks like" (which tree a view presents externally); a **pure alias** governs "what it's called" — hiding the storage real name on the server and issuing only a public name outward: the name is the alias, the mapped value is the real name. The definition file sits in the same `<meta-dir>/meta/views/<name>.layout`, its form mutually exclusive: a first line `alias <real view>` and **no rule lines at all**. The two faces of alias vs. derived view: **an alias is writable** (it is the real view, whole tree in original form), and **the real name is thereby no longer a mount entry point** — both local (`--home`) and `meta-server` refuse to mount a governed real name (the public name is decoupled from the storage identity, the outward name can rotate freely, and guessing the real name still can't get you in):

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D alias --name A --real R` | A = public name, R = physical view name | writes/replaces `D/meta/views/A.layout` (body only `alias R`; PG backend `--meta-store`: a `grovefs_control.view_defs` row); mounting `A` = whole tree **writable**; `R` is thereby **not directly mountable** (refused locally and on the server) |
| mount | no extra parameter at all | `mount --view A` gives the real view's whole tree; remote resolves server-side at `M_OPEN` (alias resolution precedes the admission decision, alias and base name go through the same gate) |
| `grovefs view --meta-dir D rm --name A` | — | deletes the alias, the real name becomes mountable again for new mounts (established connections unaffected) |
| `grovefs view --meta-dir D alias --name A2 --real R --retire A` | A2 = new public name, A = the leaked old alias | **atomic rotation**: writes `A2` first then deletes `A`, the real name `R` is governed throughout with **no exposed window** (no gap where `R` is briefly directly mountable between two steps); the old name `A` is immediately invalid (mounting it lands only in a brand-new empty view, never reaching the real tree); legitimate users just remount under `A2` |

```
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
alias 'teamwork' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/work --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key
mounting view 'teamwork' at /mnt/work (fully remote: no local store)
$ ls /mnt/work
docs photos
$ echo hello > /mnt/work/docs/2026/new.md   # 别名挂载可写,写入即真实树
$ ls /mnt_docs/docs/2026                    # 同一写经另一路派生名(docsonly)挂载穿透可见
a.md new.md
$ grovefs mount --view team -m /mnt/t --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)   # 直挂被治理的真实名:服务端拒挂(exit=1)
$ findmnt /mnt/work | tail -1
/mnt/work grovefs:teamwork fuse rw,nosuid,nodev,relatime   # 别名可写;派生名的挂载行是 ro
```

After a public name leaks, **one command rotates it atomically** (old name invalid at once, real name never directly mountable throughout, authorization unchanged):

```
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork   # 'teamwork' 已泄露,换成 'teamwork2'
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --meta-dir $BASE list
alias 'teamwork2' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/leak --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key   # 拿旧泄露名再挂
$ ls /mnt/leak          # 空:旧名不再解析到真实树(命名空间按需新建,数据不外泄)
$ grovefs mount --view team -m /mnt/probe --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key        # 猜真实名同样被拒(全程治理)
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

Guards (fail-fast at the `alias` command, 26 assertions in the layout suite sections 10/11): refuses `name == real`, refuses shadowing a physical `<name>.db`, refuses the target being a derived view (an alias only maps physical views), refuses an **alias chain** (`--real` pointing at another alias), allows only one outward name per real view at a time (the `--retire` rotation writes-then-deletes within the same command and is not blocked by this constraint); `--retire` additionally refuses pointing at a derived view or a nonexistent alias (it won't mis-delete a shape definition). The local (`--home`) case follows the same contract: a mount first resolves `views/<name>.layout`, and after an alias renames it mounts as usual and prints `alias view 'A' resolves to real view 'R' (full tree, writable)`; directly mounting a governed real name returns `error: view 'R' is governed by an alias — mount the alias name instead`; `grovefs import --view A` refuses an alias as target (an alias is not a storage identity, writing it out would be a shadow db no one can read). Implementation facts: alias resolution is a single hop, re-reading the definition on every `M_OPEN` (SQLite fully scans the `views/` directory — mounting is a rare event, reading a small directory fully is cheaper than maintaining an invalidation protocol; PG reads the same alias table in shared storage); rotation/`rm` affect only **new** mounts (established old-name connections persist until the old connection drops, the same semantics as the read-only gate); a rotated-away old name lands in an on-demand-created empty namespace, never reaching the real tree, so a leak is harmless; the SQLite tier's `meta/views/` are ordinary files, synced to the HA standby manually along with the control-plane directory (`meta/_control/`), `grovefs view`/`import` run on the storage machine (operating server-local files); the PG tier keeps aliases in shared storage (`grovefs_control`), governed from any reachable node with the same `--meta-store`, no local files to sync (§6.2). The refusal reason is **carried byte-for-byte into the protocol** (the `M_OPEN` response's data segment carries the server's verbatim text), so a remote refusal to mount the real name echoes exactly like a local one — the two `view 'team' is governed by an alias …` above are real runtime output; equally precise are the refusal reasons for token mismatch, api key missing/invalid/out-of-scope/revoked, and a view-bound key refusing another view (when talking to an **old server** the data segment is empty, and only then does the client fall back to generic wording). **Rotation is name governance (leak damage-control), not a security boundary**: hard isolation still rests on token+key — rotation only stops the old public name from reaching the real tree, it cannot revoke an existing holder's access.

### 3.4.1 fork view: an independent metadata tree, structure writable, content read-only (`grovefs view fork`)

A derived view is a read-only "display lens", an alias is a whole-tree-writable "public name"; between the two there is a real need still missing: **able to tidy the directory tree, but unable to upload content, and deleting your own name never harms the base view**. `grovefs view fork` is exactly this tier — **copying** the base view's dir entries into an independent metadata tree (SQLite: `<meta-dir>/meta/<name>/<name>.db`; PG: a new `grovefs_v_<hex>` schema), preserving inode and hardlinks, copying not a single content byte, sharing them via the cross-view ledger; the definition is only one line `fork <base view>`:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D fork --name V --base B` | V = new fork name, B = local SQLite physical base view name | creates `D/meta/V/V.db` (copying B's dir entries) and writes `D/meta/views/V.layout` (body only `fork B`); thereafter the two evolve independently, names added to the base view later are not injected into an already-built fork |
| `… fork --name V --base B --empty` (MCP: `fork_view` with `seed:false`) | `--empty` = build only the root, copy no dir entries | still writes a `fork B` definition, mount semantics identical, but the tree has only the root row; then use "graft" (`graft`, next subsection) to load a curated subset on demand — bytes still zero-copied |

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team
forked view 'lab' from base 'team': 7 entries, 4 shared object(s)
note: 'lab' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch 'team'
note: content bytes are SHARED with 'team' (cross-view ledger): the object is freed only when no view holds a name for it anymore
$ grovefs view --meta-dir $BASE list
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team/team.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --meta-dir $BASE list [--view NAME]` (add --view to see only that view's keys)
$ grovefs mount --view lab -m /mnt/lab --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key &
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
mounting view 'lab' at /mnt/lab (fully remote: no local store)
$ mkdir /mnt/lab/docs/2026/review      # organize the tree: allowed
$ mv /mnt/lab/docs/2026/a1.md /mnt/lab/docs/2026/review/a1.md   # rename: allowed (only the fork's own metadata moves)
$ echo x > /mnt/lab/docs/2026/new.md   # upload bytes: refused (EROFS)
$ ls /mnt/lab/docs/2026                # a2.md a3.md new.md review
                                       # the name WAS created (create passes), but new.md is 0 bytes
$ grovefs view --meta-dir $BASE rm --name lab
removed fork view 'lab' of base 'team' (its own metadata tree is gone; content bytes stay while any view — including 'team' — still holds a name for them)
note: a live mount of a removed fork keeps its already-resolved names until it remounts; new mounts are refused
```

Semantics and guards: `view fork` refuses `name == base`, refuses a target name that already has a definition or a physical db, refuses a base name that itself has a definition (derived/alias/fork can't be a base), refuses a base name with no physical tree in that store (one sentence across both tiers: SQLite looks for `meta/<base>/<base>.db`, PG for `grovefs_v_<hex>`); `alias` refuses pointing at a fork (a fork is a tree identity, not a public name — mint a key for it with `apikey create --view lab`), `import` refuses a fork target. **The structural face is writable, the content face read-only**: `mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/empty-object `create` are allowed, `write`/`truncate` always `EROFS`; the fork identity is carried to the client in the 3rd flag byte of the `M_OPEN` reply and the content gate sits in the client's FUSE layer — since every mount goes through the server, this judgement is identical for every mount. Deletion removes only the fork's own `node` rows: a shared object is judged by the cross-view ledger and reclaimed only after the **last holding view** removes its name; `view rm --name <fork>` deletes the definition + its own `<name>.db` (+WAL/SHM) and runs `refs rebuild` to recompute the ledger, the base view and bytes untouched by a hair. Honest boundary: the content-write gate is a **client gate**, a raw-protocol client bypassing FUSE to connect directly to the data-server can still write bytes — the hard boundary remains the server's token+API key (§6). Visibility: `view list` lists both definition slots (derived/alias/fork) and **bare base views** (physical trees with no definition); `apikey list --view V` lists only keys that can open V (a pure alias also matches a key bound to the real name), and tallies the count of unbound keys. e2e in the `fork` suite (76 assertions, incl. the `--empty` empty seed and `view graft` grafting) and the `forksrv` suite (43, the same contract on the PG backend).

#### Customizing a fork view: seven actions from creation to deletion

A fork can customize only two things: the **tree** (the copied dir entries, tidiable at will) and the **name** (view name, mountpoint, key binding); content bytes are always read-only and shared with the base view. Walk one real tidying through:

| # | Action | Command | Result / criterion |
| --- | --- | --- | --- |
| 1 | Pick the base and name it | `view list` to confirm the base is a **bare base view** (no derived/alias/fork definition) | a fork copies the **physical tree in that store** (SQLite file rows / PG schema); a derived name, alias, or another fork is refused; the name can't equal the base name nor clash with an existing db |
| 2 | Build the tree | `grovefs view --meta-dir D fork --name F --base B` | 100,002-entry measurement **0.65s**; produces `meta/F/F.db` (**12 MB**) + `meta/views/F.layout` (13 B, body only `fork B`) |
| 3 | Mount | `grovefs mount --view F -m /mnt/F --manager MGR --roster-key-file KEY` |
| 4 | Tidy the skeleton | `mkdir`/`mv`/`ln -s`/`rm`/`chmod`/`chown`/`touch` (empty file) | changes only `meta/F/F.db`; `B`'s dir entries and bytes untouched by a hair |
| 5 | Decide how to "hide" | `rm` inside the fork (permanent, changes the tree) or overlay `--layout` at mount (affects only this mount, doesn't change the tree) | after deleting rows, an audit can't tell anything was "hidden"; `--layout` lets the same fork show different shapes to different clients |
| 6 | Issue the credential | `grovefs apikey --meta-dir D create --label team-a --view F [--ro]` | the key is valid only for F; even rw, content writes still `EROFS` (only structure writable) |
| 7 | Rotate / delete | fork a new snapshot to `F2` → switch the mount → `view rm --name F` | `rm` deletes F's own tree + definition and `refs rebuild`; shared bytes reclaimed per the ledger (§2.3) |

One complete tidy (steps 4–5) can be copied as-is:

```bash
# 4) 重排骨架:归档、分组、软链、清理
mkdir -p /mnt/F/archive/2025
mv /mnt/F/raw/*.log /mnt/F/archive/2025/     # 改名/移动:只动 fork 的 node 行
ln -s archive/2025 /mnt/F/latest             # 符号链接:放行(目标文本不改写)
rm -rf /mnt/F/tmp                            # 删名字:只影响 F,基树照旧
# 5) 只想改「这次挂载看到什么」而不动 fork 的树:叠一层客户端规则
printf 'hide /tmp\nonly /archive\n' > hide-tmp.layout
grovefs mount --view F -m /mnt/F2 --manager MGR --roster-key-file KEY --layout hide-tmp.layout
```

**Can do / can't do (one table to draw the line)**:

| What you want to do | How on a fork | Boundary |
| --- | --- | --- |
| Copy a freely-tidable catalog | `view fork` | copies only dir entries; bytes shared, not copied |
| Hide certain names | `rm` inside the fork | base tree unaffected; but that something was "hidden" is not visible to an audit |
| Let different teams see different shapes | the same fork over multiple mountpoints each with `--layout`; or fork another copy | `--layout` is a client display layer, **not a security boundary** |
| Follow the base tree's additions/renames | **not supported**; re-fork (blue-green rename) | a fork is a **point-in-time snapshot** (§2.4) |
| Upload a new file inside the fork | `touch` empty file allowed; writing content `EROFS` | the content gate is a client gate, the hard boundary remains the server's token/key |
| Base view on PG | `view fork` still works | PG does a cross-schema `INSERT … SELECT`; bytes via the shared ledger, the same contract as SQLite (`forksrv` verified) |
| Treat the fork as a public name/alias | no, `alias` refuses pointing at a fork | a fork is a tree identity; for a public name use `apikey create --view F` |
| Move to another machine | `view export --view F` + `import --as-fork B` | bytes `rsync blocks/` separately (§3.4.2) |
| Delete the fork and also reclaim bytes | `view rm --name F` | only bytes whose ledger hit zero are truly reclaimed; `--base` on a fork is refused |

**Four common pitfalls**: ① `view rm --name F --base` is refused by the "name governed by a definition" guard — a fork is not a base view, deleting it **may only** omit `--base`; ② rebuilding a snapshot loses all fork-side tidying, so for rotation use a new name `F2` and switch the mountpoint, don't `rm` in place and rebuild; ③ every fork is a full catalog (100k entries 12 MB), N perspectives means N × 12 MB, not tens of bytes; ④ don't hand-edit `meta/F/F.db` — the ledger `refs.db` is updated as a set only at `fork`/`rm`/`import`, a hand-made `data_ref` has no holder record, and both the reclaim decision and `view export`'s reference count would disagree with reality.

**On "the base view's identity": a view's identity is its name** (for SQLite the `meta/<name>/<name>.db` filename, for PG the schema name), **there is no separate UUID**. The `fork B` in `views/F.layout` is just a name reference, used for `view list` display, `view rm --base`'s dependency guard, and mount prompts; mounting a fork reads **only its own `meta/F/F.db`**, never re-checking the base tree at runtime. Hence:

- If the base tree is renamed/moved at the file layer, an already-built fork **mounts as usual, content unchanged** (it's a self-contained snapshot); what changes is that the `fork B` line points at a nonexistent name, and `view list` no longer lists B's tree.
- The supported way to "change the outward name" is **alias rotation** (`view alias --retire <old-name>`): the real name stays, fork/derived/alias proceed as usual; don't hand-`mv meta/<base>/<base>.db`.
- The dependency guard works by name: as long as some `fork B`/`base B`/`alias B` still references B, `view rm --name B --base` refuses to delete.
- The cross-view ledger `refs.db` counts **per object** (`holders(data_ref, holders)`) and is independent of view names, so a rename won't mis-count references; if a real drift appears, recompute with `grovefs refs --meta-dir D rebuild`.

#### Selective grafting: install only the part you want, renaming as you go (`graft`)

A full fork is a "whole snapshot"; but when actually delivering a tidied view, the common case is **wanting only a small part of the base view, and renaming it** (`data/4f2c-9a10.fits` → `M31_2025.fits`). Two actions combine, still **copying no bytes**:

1. **Empty fork**: `fork_view` with `seed:false` (CLI form `view fork --empty`) — builds only the root of `meta/F/F.db`, moving no 100k dir-entry rows.
2. **Graft**: feed a **plan** (plain text, one entry per line) to `graft_view` (CLI form `view graft --map PLAN`, file or `-` reads stdin), grafting base-view entries into any directory of F on demand, renaming line by line.

**The target may be a fork or a bare base view** — the only criterion is: **it must own its `node` table**. A derived lens and a pure alias are just someone else's display layer and are refused by role (refusal verbatim below).

Plan format (easiest for external programs to generate, or pipe it in directly):

```text
# 只有「整行以 # 开头」才是注释;行尾 # 不是注释(会被当成文件名的一部分!)
src/data/4f2c-9a10.fits	by_target/M31_2025.fits
src/docs/readme.md	src/docs/README.md
src/calib
```

Four semantics (each backed by a measurement):

| Form | Landing spot | Note |
| --- | --- | --- |
| `src` (no TAB) | the **same full relative path** in the target view | **not the basename**: `src/docs/readme.md` does not become `/readme.md`; it rebuilds the two-level parents `src/docs/` in the target |
| `src<TAB>dst` | exactly the "dst" | **the only means of renaming**: a differing basename to the right of the TAB is a rename, may carry a directory (missing parents auto-created, 0755) |
| `--dest D` | both landing spots above get an extra `D/` layer | prefixes every line (**lines with a TAB too**); affects only "where it goes", not the name |
| `--skip-existing` | existing same-name entries **skipped in place** | existing directories **merged**, same-name files **skipped**, the rest copied; without it any conflict **refuses the whole plan** |

- All paths are relative to each view's root: leading/trailing `/` and `.` are optional, **`..` is refused outright** (the plan is data, no traversal allowed).
- **If the source is a directory → its whole subtree comes with it** (names kept); if the source is a file/symlink → only itself.
- The `entries` count **includes auto-created parent dirs**: a 4-line plan may echo `13 entries (8 directories)` (measured below), not an over-report.
- The whole plan lands in one transaction: an error on any line (typo path, target already exists, wrong target role) means **not one line lands**.
- Zero byte copy: a grafted name and the source view point at the **same object**, `refs.db` records one more holder; each side deletes its own name, and the last holder truly reclaims it (§2.3). Hardlink relations are kept too: two source names sharing an inode still share one inode after grafting (`nlink=2`).

#### Graft run-through: five forms + refusal verbatim (echo word-for-word)

Prepare a base view (make a `/src` prefix with `import`) and an empty fork:

```text
$ grovefs import --meta-dir /tmp/gv --view team --source /tmp/gv/src
created BASE view 'team' (target was absent); imported 6 object(s)
imported into view 'team' under /src: 6 dir(s), 6 object(s) (6 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grovefs view --meta-dir /tmp/gv fork --name lab --base team --empty
forked view 'lab' from base 'team' as an EMPTY tree: root only, nothing copied
note: place the subset you want with `grovefs view graft --name lab --src team --map plan.tsv` (or the `graft_view` tool, same implementation) — a directory source brings its subtree; each destination basename is the rename
note: 'lab' has the same fork semantics — structure writable, content writes (write/truncate) refused with EROFS, bytes shared through the cross-view ledger
```

**Form ①: rename + move a subtree + default `--src` (taken from the fork definition's base name)**

```text
$ cat plan.tsv
# 注释行:整行以 # 开头才被忽略(行尾 # 不是注释)
src/docs/2026/a.md	notes/A.md
src/docs/2026/readme.md	src/docs/2026/README.md
src/calib
src/data/4f2c.fits	by_target/M31.fits

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv
grafted 13 entries (8 directories) from 'team' into 'lab' at /: 5 shared object(s), 0 skipped
note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
```

Why 13 entries from a 4-line plan? — **parent directories are created for you**: `/src`, `/src/docs`, `/src/docs/2026` (3), `/notes` (1), `/by_target` (1), the whole `/src/calib` subtree (`calib`+`x`+`y` = 3) = **8 directories**; 5 files (`A.md`, `README.md`, `c1.txt`, `c2.txt`, `M31.fits`). Use MCP's `list_view_tree` to check directly (paths are metadata, bytes untouched):

```text
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"doc","version":"1"},"capabilities":{}}}' \
             '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"lab","depth":9}}}' \
  | grovefs mcp --meta-dir /tmp/gv
 /by_target/M31.fits
 /notes/A.md
 /src/calib/x/c1.txt
 /src/calib/y/c2.txt
 /src/docs/2026/README.md          # 只列文件;dirs=8 files=5
```

Note `/src/docs/2026/b.md` isn't in there — the plan didn't name it, grafting **moves only named entries** (exactly its difference from a whole fork).

**Form ②: re-run conflict → `--skip-existing` idempotent merge**

```text
$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv
error: 'A.md' already exists in the destination — rename it in the plan or pass --skip-existing
$ echo $?
1

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv --skip-existing
grafted 0 entries (0 directories) from 'team' into 'lab' at /: 0 shared object(s), 5 skipped
```

→ placement scripts can **rerun blindly**: a conflict skips just that one entry, the rest copy, exit code 0.

**Form ③: `--dest` is a prefix (added even to lines with a TAB), while "lines with no TAB keep the full relative path"**

```text
$ cat plan2.tsv
src/docs/2026/b.md
src/docs/2026/b.md	custom/kept.md

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --dest staging --map /tmp/gv/plan2.tsv
grafted 7 entries (5 directories) from 'team' into 'lab' at /staging: 1 shared object(s), 0 skipped

$ grovefs mcp ... list_view_tree {"view":"lab"}        # 实测落点
 /staging
 /staging/custom
 /staging/custom/kept.md
 /staging/src
 /staging/src/docs
 /staging/src/docs/2026
 /staging/src/docs/2026/b.md
```

Both lines took `--dest staging`: line 1 (no TAB) lands at `staging/` + the **source's full path** `src/docs/2026/b.md`; line 2 (with TAB) lands at `staging/` + the `custom/kept.md` written in the plan. **To give the same data a different name, the only way is the TAB column.**

**Form ④ `--map -`: an external program generates the plan, the whole thing piped in**

```text
$ printf 'src/data/4f2c.fits\tvia_stdin.fits\n' | grovefs view --meta-dir /tmp/gv graft --name lab --src team --map -
grafted 1 entry (0 directories) from 'team' into 'lab' at /: 1 shared object(s), 0 skipped
```

A generating script can go straight `... | grovefs view graft --name F --map -`, without landing a file first.

**Form ⑤: the target is a bare base view (reverse grafting) — must give `--src` explicitly**

A base view has no inferable "source base name", so omitting `--src` is refused outright; giving `--src lab` lets you move tidied names from the fork back into the base view:

```text
$ printf 'src/docs/2026/b.md\n' | grovefs view --meta-dir /tmp/gv graft --name team --map -
error: 'team' is a base view — pass --src to name the view its entries come from

$ printf 'notes/A.md\tfrom_lab/A.md\n' | grovefs view --meta-dir /tmp/gv graft --name team --src lab --map -
grafted 2 entries (1 directory) from 'lab' into 'team' at /: 1 shared object(s), 0 skipped
note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
note: the destination is a base view — content stays writable there; re-running the same plan with --skip-existing merges into what is already there
```

Note the last note **changes by target type**: a `fork` target says content read-only (EROFS), a `base` target says content writable — the same tool won't misdiagnose you.

**Refusal verbatim (all measured, use them to match logs)**

| Situation | Refusal verbatim |
| --- | --- |
| Target already has the same name (no `--skip-existing`) | `error: 'A.md' already exists in the destination — rename it in the plan or pass --skip-existing` |
| Target name doesn't exist | `error: no tree for graft destination 'ghost' in /tmp/gv (local files) — create it first (…)` |
| Target/source is a derived lens | `error: graft destination 'lens' is a derived lens: it renders another tree read-only and owns no nodes — use a base view or a fork` |
| Target/source is a pure alias | `error: graft destination 'pub' is an alias of 'team' — use 'team' (an alias has no tree of its own)` |
| Plan empty (only comments/blank lines) | `error: empty graft plan — one '<src>' or '<src>TAB<dst>' line per entry (blank lines and '#' comments are ignored)` |
| Source path doesn't exist in the current source view | `error: 'src/docs/nope.md' does not exist in view 'team'` (the whole plan **lands not one line**) |

**The MCP entry to the same capability** (`graft_view`'s `map` takes the plan text directly, `\t` is a TAB):

```text
[graft_view] {"name":"lab2","map":"src/docs/2026/a.md\tby_target/M31_a.md\nsrc/calib"}
← grafted 8 entries (5 directories) from 'team' into 'lab2' at /: 3 shared object(s), 0 skipped
  note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
  note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
```

**Can / can't**

| What you want to do | Possible? | Boundary |
| --- | --- | --- |
| Graft entries from a different source view (even another fork) | **yes** | one `--src` per call; multiple sources means multiple calls, each atomic |
| Graft one object into multiple names | **yes** | hardlink semantics: same `data_ref`, ledger records one more holder |
| Upload/modify bytes via grafting | **no** | if the target is a fork, content is still `EROFS`; grafting straight into a base view keeps it writable as usual |
| Use a derived/alias as source or target | **no** | they own no `node` table, refused by role (verbatim above) |
| PG backend | **no** (same reason as fork) | the authoritative tree is in PG, there's no readable `meta/<name>/<name>.db` |

The CLI (`grovefs view graft --name F --map PLAN|-`) and MCP (`graft_view`) share one implementation: parameter semantics and refusal wording byte-for-byte identical, only the entrance differs.

### 3.4.2 Single-view metadata export/import: migration across machines and OSes (`grovefs view export / import`)

GroveFS's metadata is **not hardware-bound**: a `node` row stores only `data_ref` (the object key), the object path is a pure function of the key (`blocks/<two hex of FNV1a(key)%256>/<key>`), with no absolute path, device number, hostname, or host inode; GroveFS's own `ino` is in the DB too. So moving a whole DB/directory to another machine works directly — provided **metadata and content bytes go together**. To migrate per view, across machines, selectively, use `view export` to write a tree as host-agnostic JSONL, then `view import` at the target storage root:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D export --view V --out F [--force]` | D = source storage root, V = **physical base view or fork** name, F = output file | writes a header (format/version/kind/definition verbatim/node and blob counts) + one node per line (id/inode/name/is_dir/data_ref/link_target/mode/uid/gid/three timestamps); **contains no bytes**; `--force` needed to overwrite an existing F; can export while mounted (a read-only snapshot) |
| `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` | D = target storage root, V = new base view name, F = the file exported above, B = an existing bare base view on the target machine | rebuilds the whole tree in `D/meta/V/V.db` (preserving id/inode/hardlink/permissions/owner/symlink), then rebuilds the `refs.db` ledger; an existing tree requires `--replace`; `--as-fork B` additionally writes a `fork B` definition (restoring "structure writable, content EROFS") |

```bash
# 源机(挂载中也可;停写后更稳)
$ grovefs view --meta-dir /data/src export --view team --out /tmp/team.jsonl
exported view 'team' metadata: 42 node(s), 37 blob reference(s) -> /tmp/team.jsonl
note: metadata only — copy the store's `blocks/` tree (the whole data root) as well
# 只拷内容字节(元数据已在上面的 JSONL 里)
$ rsync -a -H /data/src/blocks/ /data/dst/blocks/
# 目标机
$ grovefs view --meta-dir /data/dst import --view team --in /tmp/team.jsonl
imported BASE view 'team' from /tmp/team.jsonl: 42 node(s), 37 blob reference(s); cross-view ledger adjusted
$ grovefs --home /data/dst mount --view team -m /mnt/team     # 硬链接仍是同一 inode,权限/时间原样
```

**The same pair of commands also crosses backends** (SQLite ⇄ PostgreSQL, no third tool needed): a JSONL exported from a SQLite tree is exactly the input for PG's `view import`, and vice versa — `view --meta-store postgres://… import` **opens a schema of that name** in the database (the only tree-building entry besides `view create`), `view --meta-store postgres://… export` writes a schema as the same JSONL format. Bytes still go through their own object stores (either `rsync -a` the data root, or have the data-server serve the same objects directly):

```bash
S=/data/sqlite-store; C=/data/cluster; PG=postgres://grove@10.0.0.9:5432/grovefs
grovefs view --meta-dir $S export --view team --out /tmp/team.jsonl
rsync -a -H $S/blocks/ $C/blocks/                        # 内容字节另走,元数据已在 JSONL 里
grovefs view --meta-store $PG import --view team --in /tmp/team.jsonl
# 回滚方向:同一份 PG dump 再搬回一棵 SQLite 树
grovefs view --meta-store $PG export --view team --out /tmp/team2.jsonl
grovefs view --meta-dir $S import --view team_rolledback --in /tmp/team2.jsonl
```

The round trip rewrites no rows: a dump re-exported from the target store after import is **line-for-line identical** to the source dump (the `diff` criterion), so id/ino/hardlink/mode/owner/the three timestamps/symlink text all carry over as original values rather than being reassigned; at import the allocator watermark is raised to the export's maximum (SQLite's two auto-increment watermarks, PG's `setval`), so node ids created after migration are strictly greater than the imported side's max id, never colliding. The shared-bytes ledger (SQLite's `meta/_control/refs.db`, PG's `grovefs_refs.holders`) is updated in the same transaction with the import/`--replace`, and a failure rolls it all back — a tree is either untouched or wholly the new tree (single-transaction semantics). During the migration window the **source store is not written**, the original tree remains, and a rollback is the same pair of commands run in reverse. Whole-DB backup has its own routes: SQLite copies the file tree, PG goes through `pg_dump`/logical replication (§15.7).

e2e: the `migrate` suite asserts this chain line by line between two local SQLite trees (74 checks), the `xfer` suite runs the local and remote shapes once per backend on SQLite/PG (96 checks); the cross-backend copy (the SQLite dump → PG schema → re-export → back into SQLite above) was run on this machine with the commands of this section and both `diff` passes were line-for-line identical.

**Import atomicity**: a temporary tree is first built at `D/meta/.V.import-<pid>-<time>`, and only after all rows and checks pass and the WAL merges does it `rename` to `D/meta/V/V.db` — a corrupt file / dangling parent / count mismatch / failed `--replace` never damages the old tree (the old tree is either untouched or wholly the new tree, no intermediate state).

**Guards**: virtual names (alias/derived) own no tree, so `export` refuses outright and prompts exporting their physical base; an existing output file needs `--force`; if the import target already has a definition slot, `view rm` it first; a header format/version mismatch, a root that isn't an `id=1` directory, a dangling parent, or a header count disagreeing with the body are all whole-job refusals; `--as-fork` requires the base view to already exist **on the target machine** and the base name itself to have no definition.

**Migration's topology constraints (data distribution, not hardware binding)**:

- Multi-disk `--data-dir`: an object picks its disk by `FNV1a(key) % number of roots`; on the target the root **count and order must match**; you may also merge into a single root (merging each disk's `blocks/`).
- Cross-server striping (`s<N>`): block i is at `servers[i % N]`. With `--member-id` configured, addressing is by **member identity**: the `--data-servers` order is irrelevant (any order listing the same machines reads/writes correctly), but you must keep **N members with an unchanged set** — swapping out any one machine (even holding the count) is caught by the member-set fingerprint and refused at mount, rather than silently mis-reading someone else's block. The positional mode without `--member-id` addresses by the client list's **index**, so **the order is itself part of the layout**; reordering the list silently mis-routes existing objects (no handshake before reading/writing an existing object can detect a reorder) — this is an inherent hazard of the positional mode; to root it out, give each data-server a unique `--member-id`. **Changing machines / changing IPs is not itself a constraint**: in identity mode, replacing that item in `-o data=` in place (keeping its id) is auto-recognized, metadata unchanged, already-written objects need no `rebal` (the rotating placement `i % N` only decides where newly-written objects land). A count mismatch is refused outright (reporting `laid out across N servers but this mount has M; refusing to remap`).
- HA `epoch` (each view's `ctrl` table): a whole-directory copy carries it over, so the old primary must be stopped after migration; `view export/import` and `view fork` deliberately do not copy `ctrl` or `locks`.
- The PG backend can't use file copying; for whole-DB backup use `pg_dump`/logical replication; but `view export/import` works on both backends — a PG tree is in a schema, export goes through a REPEATABLE READ read-only snapshot, import rebuilds in a single transaction (§15.9, proven by the `xfer` suite's PG tier).
- `uid/gid` is numeric metadata (advisory), kept by number; symlink target text is user data, kept verbatim.
- For a whole-directory cold copy, first stop all mounts and the `meta-server`; if you can't stop, use `sqlite3 <db> ".backup"` / `VACUUM INTO`, **don't raw-copy a running DB** (WAL).
- **`st_ino` travels with the DB and is unchanged across machines** (`node.ino` is GroveFS's own numbering), but **`st_dev` is freshly assigned by FUSE on each mount**: a backup/sync tool that dedups by the `(dev, ino)` tuple must rebuild its mapping after a remount or DB move; lookup by ino alone (`find -inum`, `stat -c %i`) is unaffected.
- **How to confirm nothing is missing after copying**: object names are UUIDs, not content hashes, the read side has no verification step, so a missed copy can only be actively compared — the number of keys referenced by the tree (`sqlite3 D/meta/V/V.db 'SELECT COUNT(DISTINCT data_ref) FROM node WHERE data_ref IS NOT NULL;'`) against the `matched` in `grovefs gc --meta-dir D --data-dir R`'s output; `matched < refs` means there are keys with no object. Use `rsync -a` to preserve mtime (`gc --older-than` relies on it for the new-object grace), and a store that has used `import` must carry `-H` (imported objects and source files are hardlinks).

**Multiple base views**: create any number of base views (`view create --view A`, the only entry point that builds a tree), each its own `meta/A/A.db`, trees/bytes/permissions/deletions not affecting each other; `view list` lists all bare base views. `view export/import` works per view, so at migration you may move just one of them, and different base views may bind different keys (`apikey create --view A`).

e2e in the `migrate` suite (74 assertions). Coverage: export while mounted / host-agnosticism / virtual-name and overwrite guards / import across absolute paths / hardlink and permission fidelity / atomic replace / corrupt-input refusal / `--as-fork` ledger sharing / multi-base-view independence `fork`/`--as-fork` share the same contract on both backends, see §15.9).

**Scale measured** (CLUSTER tier — a `meta-server` + `data-server` pair on this host's loopback, default IO, rerunnable via `bash .build-bin/perf_migrate_scale.sh`): migrating a view of 5052 nodes, 5000 objects — `export` **0.07s** producing 1,161,499 B of JSONL (**1.10×** the 1,056,768 B view DB, ~230 B/node); `import` to a new storage path **0.53s** (incl. the cross-view ledger adjustment); with the services up, mounting answers a full enumeration of the 5000 files in **0.22s**, content byte-identical, permissions and owner preserved (`664 uid=1005`). Registering the same batch of files into the source view originally (zero-copy `import`) took **1.2s** — that per-object storage overhead is entirely off the export/import path. **Migration cost is set by the namespace size, not the data volume**; bytes go separately via `rsync -a -H blocks/` (0.1s on the same rig).

### 3.4.3 Deleting a whole base view: reclaiming bytes along (`grovefs view rm --base`)

A `view rm` without `--base` deletes only a **definition slot** (derived/alias/fork) and never touches data; to delete a **physical base view** from end to end — its metadata tree gone, the cross-view ledger rebuilt, **content bytes no other view still holds reclaimed from `blocks/`** — use `--base` to say this explicitly (to avoid a mis-deletion):

| Command | Effect |
| --- | --- |
| `grovefs view --meta-dir D rm --name V --base [--data-dir R]` | deletes `D/meta/V/V.db` (+WAL/SHM) → `refs rebuild` recomputes holders over the surviving trees → for each blob in V's tree **held by no other view** calls `backend.remove`; `--data-dir` points at the root actually storing `blocks/` (default the metadata root `D`, same convention as `import --data-dir`, for meta-server/data-server split deployments); the echo honestly reports the freed/held/not-found/failed counts |

```
$ grovefs view --meta-dir .home rm --name alice --base
removed base view 'alice': 2 node(s) gone, 2 blob reference(s) dropped, 2 object(s) freed from .home/blocks, 0 still held by other views
```

**Four-fold guards** (any hit refuses, the tree untouched by a hair): ① the name is governed by a definition (is a derived/alias/fork definition slot) — first run a `view rm` without `--base` to delete the definition; ② some alias/lens/fork **definition** still resolves through this base (otherwise deleting the base would leave that mount dangling), reports `view 'lens1' is governed by a definition over base 'alice' — remove it first`; ③ the local `/proc/mounts` has a live `grovefs:<V>` mount, reports `view 'alice' is mounted here at …  — unmount it first` (mounts **on other machines** against the same meta-server are invisible to this machine and must be stopped first yourself); ④ **the PG tier goes through the same command** (`view --meta-store postgres://… rm --base`): the base tree is a schema, `remove_view` decrements `grovefs_refs.holders` per key (DELETE at zero) in one transaction, then runs `DROP SCHEMA … CASCADE`, and only after commit reclaims `blocks/` bytes per the returned `unbooked_refs` — there's no SQLite-tier "delete the db then rebuild the ledger" intermediate window. **A hand `DROP SCHEMA` past the CLI does not count as deletion**: the ledger is a global table with no view column, a bare drop deletes only the tree without booking it off; more importantly, which objects that view subscribed to is **known only to that schema**, the ref list vanishes with the schema, and those bytes thereafter have no record to reclaim (proven in section E of §15.8's measurement).

**Freed only when the last namer dies**: a fork's blob refs come from the base view's own keys, the ledger holds them at >1, so when deleting the base the bytes still named by the fork **must not be touched**; bytes are truly reclaimed only once the base and all dependent definitions are gone and the ledger has no holder left. Striped keys `s<N>:<stripe>[:<fp>]:<uuid>` live on the data-server, an ordinary local root can't reclaim them — honestly counted into failed and skipped, never half-deleted. **MCP boundary**: `remove_view` is control-plane only (equivalent to a `view rm` without `--base`), deleting a whole base view goes only through the CLI — the interface never touches data bytes.

e2e in the `viewrm` suite (42 assertions: seeding the tree / mount guard / multi-guard / shared bytes lost from deletion / reclaim after the last namer dies / split root `--data-dir` / re-mount-empty semantics after deletion); the same PG-tier sequence in `.bneck/pg_view_teardown.sh` (44 assertions: real FUSE-mount writes → three-way reconciliation of schema/ledger/bytes → transactional deletion → a second deletion refused → the two directions of `refs rebuild` after an out-of-band drop → residue exactly equal to the unreclaimable part).

### 3.5 Agents customizing data listings: the MCP interface (`grovefs mcp`)

View governance (§3.2/§3.4/§3.4.1) is simultaneously a set of **MCP (Model Context Protocol) tools**. `grovefs mcp` speaks newline-delimited JSON-RPC 2.0 on stdio (implementing the common method surface of the 2025-06-18 protocol: lifecycle `initialize`/`ping`, tools `tools/list`/`tools/call`, resources `resources/list`/`resources/templates/list`/`resources/read`, prompts `prompts/list`/`prompts/get`, completion `completion/complete`, logging `logging/setLevel` and server-side `notifications/message`; client notifications are always silently unanswered), any MCP client (Claude Desktop, an IDE agent, etc.) spawns it as a subprocess, and the agent can then **customize data listings automatically through the interface**: query views, **list the directory a view actually renders** (a derived view via its lens, and preview without landing what a to-be-published rule would grow into), publish a read-only derived lens with inline rule text (the four verbs same as `--layout`, rules ride as text lines, landing no file), rotate the public name, delete a definition, **dry-run a whole definition through all governance guards before publishing (writing nothing)**. It shares the same library functions (`grovefs::mcp`) with the `grovefs view` subcommands — validation, refusal wording, on-disk actions byte-for-byte identical, one contract two entrances, an agent can do nothing the CLI can't and bypass no guard. **Probe is read-only**: `list_view_tree`/`get_view` open the local `<meta-dir>/meta/<view>/<view>.db` read-only, render the listing through the same `LayoutMeta` lens a mount uses (no mount, no object bytes read; remote/PG-backend trees are not walked here); the `size` in the reply is a metadata field not a byte count (directory `4096`, file `0`, symlink the target length), for real bytes mount and `stat`. **Control-plane only**: no mount, no data writes. By default it trusts whoever can spawn this process (= already holds the storage root's OS-level permission), cross-network auth is still the server's token / API key's job; when it is fronted to several different callers, `grovefs mcp --require-api-key` **optionally** puts an API-key gate on the interface (see §3.5.1). All echoes below are from real runs:

```
# 1) 存储机:建一个物理视图(智能体稍后要在它的树上定制列表)
$ grovefs view --meta-dir $HOME_DIR create --view team --manifest team.manifest
created BASE view 'team' at /data/xy/grovefs/.demomcp/home/meta/team/team.db
applied seed manifest team.manifest

# 2) MCP 客户端配置里登记 stdio 服务器(以通用 mcpServers 写法为例):
#    "grovefs": { "command": "grovefs", "args": ["mcp", "--meta-dir", "/data/xy/grovefs/.demomcp/home"] }
$ grovefs mcp --meta-dir $HOME_DIR       # 客户端实际这样拉起:stdin 进帧、stdout 出帧

# 3) 智能体侧 tools/call 与工具结果文本(原样回显):
[list_views]   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{
  "db_path": "/data/xy/grovefs/.demomcp/home",
  "defined": [],
  "physical": [
    "team"
  ]
}
[preview_rules] "rules": "bog /x"                    # 先发后落:非法动词在发布前被抓住
bad layout rules: line 1: unknown layout verb 'bog'
[define_derived_view] {"name":"release","base":"team","rules":"# public release: no private photos\nhide /photos/private"}
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
[define_derived_view] {"name":"team","base":"team",...}                     # 名与基同名
isError → derived view name must differ from its base      # 与 CLI 逐字节同文案(治理失败按 MCP 惯例回在结果内)
[list_views] 再次                                    # 结构化 JSON 回执,字段: name/kind/real_view/规则计数
view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)   # CLI 对照:`grovefs view list` 同文案

# 4) 用户侧:直接挂智能体定的名(无 --layout、无本地规则文件)
$ grovefs mount --view release -m /mnt/release --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key &
derived view 'release' over base 'team': read-only lens
note: derived (virtual) views mount read-only — write to base view 'team'
mounting view 'release' at /mnt/release (fully remote: no local store, read-only)
$ ls /mnt/release
docs  photos

# 5) 探查与预览(只读元数据,不挂载、不碰数据字节):
[list_view_tree] {"view":"team"}              # 先摸清真实路径:智能体据此写规则而非猜
{"kind":"physical","rendered_from":"team","dir_count":4,"file_count":3,"truncated":false,"entries":[
  {"path":"/docs","kind":"dir"},{"path":"/docs/2026","kind":"dir"},{"path":"/docs/2026/b.md","kind":"file"},
  {"path":"/docs/a.md","kind":"file"},{"path":"/photos","kind":"dir"},{"path":"/photos/private","kind":"dir"},
  {"path":"/photos/private/x.jpg","kind":"file"}]}
[list_view_tree] {"view":"release"}           # 经已发布透镜渲染:显示路径 /docs /docs/2026 /docs/2026/b.md /docs/a.md /photos(/photos/private 被规则剪除,与挂载所见一致)
[list_view_tree] {"base":"team","rules":"only /docs"}   # 不落盘预览一副待发布规则:仅 /docs 子树呈现,不写任何 .layout
[validate_definition] {"name":"release","base":"team","rules":"hide /photos/private"}   # 发布前干跑:把 name+base+rules 走完整套治理守卫,不写盘,回显确切判定
{"accepted":true,"dry_run":true,"kind":"derived","hide":1,"only":0,"reloc":0,"would_replace_existing_definition":false,"base_view_db_present":true,"note":""}
[get_view] {"name":"release"}                 # 读回单个透镜原始规则,供“读—改—写”迭代
{"kind":"derived","base":"team","hide":1,"only":0,"reloc":0,"definition":"base team\nhide /photos/private","mount":"read-only (virtual shape; writes go to the base view)"}
```

Ten tools and their parameters: eight governance ones — `list_views` (optional `db_path`), `define_derived_view` (`name`/`base`/`rules`, the latter inline rule text), `alias_view` (`name`/`real`/optional `retire` atomic rotation), `fork_view` (`name`/`base`, optional `seed` — `false` builds only an empty root, leaving "whether to copy dir entries" to the caller; copying the base view's dir entries into an independent metadata tree — structure writable, content EROFS), `graft_view` (`name`/`map` plan text, optional `src` (defaults to the fork definition's base name)/`dest`/`skip_existing` — grafting named entries from another view (a directory source brings its whole subtree) into any position of the target view, renaming line by line, zero byte copy, the whole plan in one transaction), `remove_view` (`name`, deleting a fork also deletes its own tree), `preview_rules` (`rules`, dry-run checking verb syntax without landing), `validate_definition` (`name`/`base`/`rules`, running a whole definition through define's full governance guards before publishing — illegal name, name equals base, shadowing a physical db, overwriting a pure alias, rule syntax — on refusal returning text byte-for-byte identical to a real publish, on acceptance returning a JSON verdict, never landing to disk); two read-only probes — `list_view_tree` (`view` or `base`+`rules` simulation, optional `path`/`depth`/`max`/`db_path`; renders the path listing a mount actually sees, either via a published lens or by simulating a rule set without landing), `get_view` (`name`, returning one definition's raw `definition` text and counts). The same governance is secondarily exposed via MCP's other common method surfaces, reusing the **exact same renderers**, so resources/prompts/completion match tool results byte-for-byte: **resources** `resources/list`/`resources/templates/list`/`resources/read` expose views and the manual as attachable context — `grovefs://guide` returns the sectioned ops manual verbatim, `grovefs://views` the same snapshot as `list_views`, `grovefs://view/<name>/definition` (governance text verbatim) and `/tree` (the listing a mount actually sees, the template carrying `{?path,depth,max}`, synonyms of `list_view_tree`'s parameters — a big tree can attach one subtree at a time, `truncated:true` when bounded off); **prompts** `prompts/list`/`prompts/get` offer two parameterized governance playbooks (`reshape-view` taking `base`/`goal`, `rotate-alias` taking `real`/`new`/`old`); **completion** `completion/complete` is role-aware per `ref` (`reshape-view.base` / `rotate-alias.real` give only physical views, `rotate-alias.new`/`old` give only aliases, the rest give all view names); **logging** after `logging/setLevel` sets a session threshold, the server pushes `notifications/message` by severity (success `info`, failure `warning`) after each dispatched call, `data` carrying the method and the object operated on (e.g. `tools/call list_views ok`). Runtime-facing metadata: each tool carries a `title` and `annotations` (`readOnlyHint` marking `list_views`/`preview_rules`/`validate_definition`/`list_view_tree`/`get_view`, `destructiveHint` only `alias_view`/`remove_view`); the four JSON-returning tools carry an `outputSchema` and results also give `structuredContent`; `initialize`'s `instructions` span seven sections (Discover/Reshape/Curate/Validate before publishing/Golden order/Beyond tools/Mechanics), a client injecting context can follow them. One more protocol detail: negotiated per `2025-06-18`/`2025-03-26`/`2024-11-05`, an unrecognized version returns `2025-06-18`; `arguments` also accepts a JSON string; a missing parameter appends `(tool 'x' requires: …)`, a misspelled tool name appends `did you mean '…'?` and an `available:` list. Semantics: all governance guards apply as usual (derived mounts read-only, the real name refused while an alias exists, `--retire` writes-then-deletes, fork structure writes allowed / content EROFS; hot reshape still only at the `meta-server` entrance, a mount resolves its definition once, at `M_OPEN`). Protocol details: notifications (no id) are never answered; an illegal frame returns `-32700` and continues serving; a missing parameter and an unknown tool return `isError` per MCP convention rather than a protocol error. `db_path` defaults to `grovefs mcp --meta-dir` (falling back to `--home`), also overridable per call. e2e in the `mcp` suite (77 assertions: handshake capability declaration and protocol negotiation / discovery (`instructions` naming all ten tools one by one with sections, `title`+`annotations`+`outputSchema`) / inline-rule reshaping / path probe and shape preview / dry-run validation before publish / alias rotation / empty fork `seed:false` + `graft_view` grafting / graft-guard surface 11 refusal verbatim (self-graft · empty plan · `..` · root path · base view missing `src` · target or source is derived/alias · nameless tree · illegal name · duplicate-name whole-batch rollback · `skip_existing` skipping) / CLI same text / resource enumeration and read (incl. `grovefs://guide`, `grovefs://views`, tree query-parameter drill-down and `max` truncation) / prompt instantiation / role-aware view-name completion per `ref` / `structuredContent` consistent with text / unknown-tool suggestion and missing-parameter prompt / log threshold→notification with `data` carrying the tool name / protocol-error paths). The agent-side wiring manual (in English, for internationalized deployment) is already distilled into the repo as a project skill (`skills/grovefs-mcp/`): `SKILL.md` (how to spawn, the ten-tool parameter table, the golden workflow, an exact refusal-text table per case, hard boundaries), `reference.md` (the full method surface / auth-gate matrix / error codes / on-disk effects / CLI cross-reference), `examples.md` (real echoes), plus two scripts: `scripts/mcp_call.py` (a zero-dependency stdio JSON-RPC client, splitting the reply into stdout/stderr, exit code 0 success / 2 protocol error / 3 tool refusal / 4 transport problem) and `scripts/smoke.sh` (running the full chain on a `mktemp -d` temporary storage root with per-line assertions, never touching real data).

### 3.5.1 Locking the interface and the mountpoints: API-key authentication, scope partitioning and call audit (`grovefs apikey`)

One server often faces **multiple different callers** — an agent runtime fronting one `grovefs mcp` service out, several client machines mounting the same `meta-server` over LAN — when you need revocable credentials and attribution audit. `grovefs apikey` mints API keys (**authentication consults only the stored SHA-256 digest**, the plaintext is echoed by `create`/`rotate` and the store also keeps a reversible ciphertext copy of it that needs the store key to open, so `api_keys.db` alone still can't be turned into a usable credential), and tags each key with a **scope** (`--for admin` is the credential for the governance port that stands in for the DB password, see §3.5.2) and an **owner** (`--owner <account>`, required for `--for admin`): `--for mcp` (opens only the governance interface), `--for mount` (opens only a `--require-api-key` meta/data server), `--for admin` (opens only the governance port `meta-server --control-listen`, and also admits mount+mcp), `--for any` (**default**, mount+mcp only — `any` never upgrades to `admin`; a row whose scope is absent or unparseable authenticates as this tier). The two entrances share one registry (`<meta-dir>/meta/_control/api_keys.db`), isolated by scope: a mount credential can't be replayed to the governance API, and an agent key can't open the data plane either. A mount key may also carry **two access-control dimensions**: `--view V` **binds the credential to a single physical view** V — using it to `M_OPEN` another view is refused with `E_AUTH` at the handshake (one without `--view` is a system-level key, able to open any view); `--ro` marks it **read-only** — the server refuses write-intent ops per-op with `E_NOTPERM` on the meta plane and content writes with `EROFS` on the data plane, and hints the client a read-only mount in the `M_OPEN` reply ("read-only" is enforced server-side, not relying on the client's `-o ro` self-discipline); read-only/read-write **can be flipped in place** — `apikey set-ro --id <ID> --rw` flips a read-only key back to read-write, without `--rw` (or with `--ro`) back to read-only, a live mount adopts the new verdict on its next second-level revalidation tick, no remint, no reconnect (the one exception: a mount whose read-only already landed in the kernel at handshake still needs a remount to lift the OS-level cap, see set-ro's paragraph in 3.5.1; and the switch holds only for server-routed mounts, a local `--home` mount has no server credential gate and is unaffected). **Governance-plane gate**: `grovefs mcp --require-api-key` gates all **state-reading/writing methods** accordingly — `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `completion/complete` (`initialize` / `ping` / `prompts/*` / `logging/*` are always open so a client can complete the handshake, fetch static playbooks, and control how verbose the server is). **Mount-plane gate**: `meta-server --require-api-key` gates `M_OPEN`, `data-server --require-api-key` gates `OP_OPEN`, the client presents via `mount --api-key <ID:SECRET>` (if you'd rather not put it on the command line, export `GROVEFS_API_KEY='<ID:SECRET>'`, the secret then stays out of argv); **the enabling order matters**: a server **without** `--require-api-key` does **no** key verification at all (a client presenting a made-up key still mounts, verified `MOUNTED`); with it, it is **fail-closed** — when `api_keys.db` has no usable key **no one (including you) gets in**, so mint the keys first, then restart with the flag; **audit surface**: every `tools/call`, every `resources/read`, and every mount connection (incl. refused ones) appends to the same audit table (time, key, plane method, tool tool, success, detail), read back by `grovefs apikey calls`. A revoke only marks the row revoked (past calls keep attribution), a revoked key authenticates against nothing immediately, **and already-established mounts are closed too** (see below). Storage is one separate local SQLite (`<meta-dir>/meta/_control/api_keys.db`): the control-plane state. Keys minted now use the short format `<16-hex id>:<32-hex secret>` — 49 characters total, the secret carrying 122 random bits (offline brute force and online replay remain out of reach); long-form keys still verify — the check compares digests, never lengths — and `apikey rotate` mints a fresh secret for the SAME id when one is lost (label/scope/view binding/audit history all survive; the new plaintext is echoed too, and the stored copy is replaced with the new one — for a row the store holds no copy of (nothing was wrapped when it was minted), `rotate` remains the only way back). **Each key also carries its bookkeeping into the same row**: a `--note TEXT` remark, an absolute `expires_at` the server computed from `--ttl DUR` (units `s`/`m`/`h`/`d` only — a misspelt duration is refused rather than silently minting a key that never expires), and the `created_by` account that minted it, plus a `secret_hint` column holding an irreversible head-4 + `…` + tail-4 fingerprint. The `list` key column prints exactly that stored fingerprint, so "seeing the key in the list" and "holding a usable credential" remain two different things (to get the plaintext again use `apikey secret --id`; a row the store holds no copy of can only be `rotate`d). The echoes below are from real runs:

```
# 1) 铸造一个 key(回显全文并在库里落一份可逆副本;scope 默认 any)
$ grovefs apikey --meta-dir $HOME_DIR create --label claude-desktop
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    01fcf6bb65a742c4
  owner: <none>
  label: claude-desktop
  scope: any
  view:  <any>
  rw:    rw (read-write)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   01fcf6bb65a742c4:49d5f536592d49c090ddb10624f05c7e

Present it to a `--require-api-key` server as
  grovefs mount --manager HOST:PORT --roster-key-file F --api-key '01fcf6bb65a742c4:49d5f536592d49c090ddb10624f05c7e' -m /mnt/x
(or export GROVEFS_API_KEY='01fcf6bb65a742c4:49d5f536592d49c090ddb10624f05c7e' so it stays out of the command line).
Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount
credential out of the governance API (and vice versa); `--for admin` is the
governance credential and is never implied by `any`.
Revoke later with: grovefs apikey --meta-dir $HOME_DIR revoke --id 01fcf6bb65a742c4

# 1b) 备注与有效期一起入账:三笔台账(备注/有效期/铸造者)都随 key 落库,期限由服务端算
$ grovefs apikey --meta-dir $HOME_DIR create --label nightly --for mount --note "staging box, hand over end of March" --ttl 30d
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    be55fdb6147a4f79
  owner: <none>
  label: nightly
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  note:  staging box, hand over end of March
  expires: 2026-12-18T15:10:06.069199511+00:00
  key:   be55fdb6147a4f79:0d4fb3aff1584c74b10630148e42eb4e

# 2) 铸造一个只能挂载的 key;scope 写错直接拒绝(打错一个词不会静默放宽权限)
$ grovefs apikey --meta-dir $HOME_DIR create --label laptop-alice --for mount
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    18c4bb4091764ad6
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   18c4bb4091764ad6:3d6997d607be4cf7ba0024a85afedbe6
$ grovefs apikey --meta-dir $HOME_DIR create --label x --for both
error: unknown api key scope 'both' (expected any|mcp|mount|admin)      # exit=1

# 2b) 视图绑定 + 只读的 key(访问控制维度)
$ grovefs apikey --meta-dir $HOME_DIR create --label reader --for mount --view alice --ro
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    a026316b216946dc
  owner: <none>
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   a026316b216946dc:4611085b2d714a519af0a762acad76c6
$ grovefs mount --view bob -m /mnt/b --manager HOST:PORT --roster-key-file F --api-key "$READER_KEY"   # 拿它去挂别的视图
error: permission denied: this api key is bound to view 'alice', not 'bob'    # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key 挂载
mkdir: 无法创建目录 “/mnt/a/should_fail”: 只读文件系统                        # 据握手标志落内核只读;服务端另逐写拒 E_NOTPERM/EROFS

# 2c) 原地切换只读/读写(热生效,不重铸不重连)
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id a026 --rw
apikey: id='a026316b216946dc' (label 'reader') is now rw (read-write)
  a LIVE mount presenting this key resumes admitting writes on its next re-check window (≤1s) — no reconnect, no remount.
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id a026            # 切回只读(默认方向)
apikey: id='a026316b216946dc' (label 'reader') is now ro (read-only)
  a LIVE mount presenting this key starts refusing writes on its next re-check window (≤1s) — no reconnect, no remount; its reads keep flowing.
# 前缀歧义/已撤销/未知 id 与 revoke 同样处理:歧义前缀报错、revoked 行不回活、未知 id 报错(exit=1)。

# 2d) secret 丢了不换身份:rotate 为同一 id 重铸 secret(绑定/归属/审计/台账全保留,新明文再展示一次)
$ grovefs apikey --meta-dir $HOME_DIR rotate --id a026
apikey rotated (the new credential's copy is stored — `grovefs apikey secret --id <id>` reads it back):
  id:    a026316b216946dc
  owner: <none>
  label: reader
  scope: mount
  view:  alice
  key:   a026316b216946dc:942266559f824286b450b4e8a67ad04c

  the id and every binding are unchanged — only the secret was replaced;
  the OLD secret fails its next handshake, and a live mount established on it
  stays open until it reconnects — `apikey revoke` is the kill-now verb.
# 已撤销的 id 拒绝 rotate(revoke 是终态,轮换不复活);要立刻封杀用 revoke。列表里的指纹同步换成新 secret 的头尾。

# 2e) Handing a credential over later: `secret` reads the full credential back from the stored copy
#     (byte-for-byte what `create`/`rotate` printed at the time)
$ grovefs apikey --meta-dir $HOME_DIR secret --id a026
apikey secret — the stored copy of this credential:
  id:    a026316b216946dc
  owner: <none>
  label: reader
  scope: mount
  view:  alice
  key:   a026316b216946dc:942266559f824286b450b4e8a67ad04c

  authentication still runs on the stored digest; this copy is for
  handover.
  a direct read of the local store is NOT audited — whoever can
  open that file already owns it; a read through a meta-server
  (`--control-server`) is, and shows in `apikey calls`.
# Through a meta-server or the console only the row's owner (or a superuser) gets it back; another account is refused.
# A local read like the one above is governed by file permissions instead — which is why it records nothing.
# A row with no stored copy answers never stored, a copy wrapped under different material answers unopenable,
# a copy disagreeing with the digest answers stale — all three refuse rather than guess.

# 3) 列表:直接显示 key 指纹(头4…尾4,库里存的那一段,读取时不反推),绝不落明文;台账随行带出
$ grovefs apikey --meta-dir $HOME_DIR list
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   9622e52b060e41d8   772a…636e    -                agent            mcp     <any>        rw   2026-11-18T15:10:06.161151695+00:00 -                         never
2   b381a1693c9b4165   1c5f…1d10    -                lab-agent        any     lab          rw   2026-11-18T15:10:06.124367225+00:00 -                         never
3   be55fdb6147a4f79   0d4f…eb4e    -                nightly          mount   <any>        rw   2026-11-18T15:10:06.069303270+00:00 -                         expires 2026-12-18T15:10:06.069199511+00:00 | note: staging box, hand over end of March
4   a026316b216946dc   9422…d04c    -                reader           mount   alice        ro   2026-11-18T15:10:06.032809367+00:00 2026-11-18T15:10:11.311280050+00:00 never
5   18c4bb4091764ad6   3d69…dbe6    -                laptop-alice     mount   <any>        rw   2026-11-18T15:10:05.984742303+00:00 2026-11-18T15:10:11.259186132+00:00 never
6   01fcf6bb65a742c4   49d5…5c7e    -                claude-desktop   any     <any>        rw   2026-11-18T15:10:05.965729039+00:00 -                         never

# 3b) 按视图过滤:只列能开 lab 的 key(纯别名也匹配绑定到真实名的 key),并提示系统级 key 数
$ grovefs apikey --meta-dir $HOME_DIR list --view lab
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   b381a1693c9b4165   1c5f…1d10    -                lab-agent        any     lab          rw   2026-11-18T15:10:06.124367225+00:00 -                         never
1 bound key(s) for view 'lab' of 6 total
  note: 4 unbound key(s) admit every view too — plain `apikey list` shows them

# 3c) 凭据多了直接检索:`--label/--owner/--note` 大小写不敏感子串,`--scope` 精确到一档入口
$ grovefs apikey --meta-dir $HOME_DIR list --label alice --scope mount
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   18c4bb4091764ad6   3d69…dbe6    -                laptop-alice     mount   <any>        rw   2026-11-18T15:10:05.984742303+00:00 2026-11-18T15:10:11.259186132+00:00 never
$ grovefs apikey --meta-dir $HOME_DIR list --note staging
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   be55fdb6147a4f79   0d4f…eb4e    -                nightly          mount   <any>        rw   2026-11-18T15:10:06.069303270+00:00 -                         expires 2026-12-18T15:10:06.069199511+00:00 | note: staging box, hand over end of March

# 3d) 台账就地可改:`update` 只动备注与期限两笔,其余(身份/绑定/审计)一概不碰
$ grovefs apikey --meta-dir $HOME_DIR update --id be55 --note "handed to ops" --ttl never
apikey updated: id='be55fdb6147a4f79' (label 'nightly')
  note:    handed to ops
  expires: never
$ grovefs apikey --meta-dir $HOME_DIR update --id be55 --ttl "3 weeks"     # 时长只认 s/m/h/d 四个单位
error: bad ttl '3 weeks': expected a count and a unit, like 45m, 24h or 7d   # exit=1
$ grovefs apikey --meta-dir $HOME_DIR update --id be55                      # 两个 flag 都不给:拒,而不是空转一次
error: nothing to update: pass --note TEXT and/or --ttl DUR (`--ttl never` removes a deadline)   # exit=1
# 期限由服务端判:过了 expires 的 key 在 list 里标 EXPIRED(行仍列出)、每一次鉴权都被拒;
# 给它一个新的 --ttl 就是唯一恢复路径(同一把 key 重新鉴上);revoked 行拒改——吊销是终态。

# 4) 服务端开门时自报门槛;不开门的默认路径行为与从前逐字节一致
$ grovefs meta-server --meta-dir $HOME_DIR --listen 127.0.0.1:15310 --require-api-key
meta-server listening on 127.0.0.1:15310 (base $HOME_DIR, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view --meta-dir <dir> create --view <name>`); no empty tree is ever auto-created
$ grovefs data-server --listen 127.0.0.1:15311 --data-dir /data/cluster --require-api-key --meta-dir $HOME_DIR
data-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live connections on the next op

# 5) 客户端拿各种坏凭据去挂:拒因逐字节的原文回到本地 stderr(退出码 1,不留脏挂载)
$ grovefs mount --view alice -m /mnt/a --manager HOST:PORT --roster-key-file F                      # without a key
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --manager HOST:PORT --roster-key-file F --api-key "18c4bb40...:deadbeef"
error: authentication failed: invalid api key secret
$ grovefs mount --view alice -m /mnt/a --manager HOST:PORT --roster-key-file F --api-key "0000000000000000:whatever"
error: authentication failed: unknown api key id
$ grovefs mount --view alice -m /mnt/a --manager HOST:PORT --roster-key-file F --api-key "no-colon-here"
error: malformed api key (expected '<id>:<secret>')
$ grovefs mount --view alice -m /mnt/a --manager HOST:PORT --roster-key-file F --api-key "$MCP_KEY"   # a governance key presented to a mount
error: authentication failed: this api key is scoped to 'mcp', not 'mount'
#   反方向同样被拒:把 mount key 塞进 MCP initialize 的 apiKey,tools/list 回 -32001,
#   且这条跨入口尝试同样被审计成 scope 拒因。

# 6) 挂载连接与治理调用同表审计(method 分平面、tool 分握手;被拒的也入账)
$ grovefs apikey --meta-dir $HOME_DIR calls --limit 8
last 8 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-11-18T15:10:11.569517381+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'
2026-11-18T15:10:11.535856955+00:00 -                                  meta         M_OPEN                 no   view='alice': malformed api key (expected '<id>:<secret>')
2026-11-18T15:10:11.502511379+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: unknown api key id
2026-11-18T15:10:11.485540205+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: invalid api key secret
2026-11-18T15:10:11.462438861+00:00 -                                  meta         M_OPEN                 no   view='alice': no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
2026-11-18T15:10:11.344410213+00:00 a026316b216946dc                   meta         M_OPEN                 no   view='bob': key bound to view 'alice'
2026-11-18T15:10:11.335343137+00:00 a026316b216946dc                   meta         M_OPEN                 yes  view='bob' uid=1005
2026-11-18T15:10:11.276881451+00:00 18c4bb4091764ad6                   data         OP_OPEN                yes

# 7) 撤销一行输出说清两面契约;历史调用仍保留归属
$ grovefs apikey --meta-dir $HOME_DIR revoke --id 18c4bb4091764ad6
apikey: revoked id='18c4bb4091764ad6' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# 8) 封停活挂载:实测撤销后一次写的遭遇(无需运维介入)
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: 输入/输出错误        # EIO:后台复验 tick 先落地,daemon 正在自卸载
$ cat /tmp/grovefs-mount--mnt-a.log        # 分离挂载的日志默认落 $TMPDIR,按挂载点命名(`--log-file` 可指定)
manager 127.0.0.1:50055: deployment 'default' epoch 1 resolved meta [127.0.0.1:15310] data [127.0.0.1:15311] (signature verified, valid for 300s more)
using remote metadata service at 127.0.0.1:15310
using remote data service at 127.0.0.1:15311
grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
received termination signal, unmounting cleanly...
$ ls /mnt/a                            # 干净的空目录——不是 "Transport endpoint is not connected"
.  ..
#   errno 取决于写与自卸载谁先到:那趟写正是复验的载体时拿到服务端的拒因、报 权限不够(EACCES),
#   复验先落地则报 输入/输出错误(EIO);两者都不是假成功(套件钉的是 rc≠0)。
#   同期另一个 key 的挂载照旧读写(逐调用方撤销,不像轮换一份共享 token 那样断全部人);
#   拿被撤销的 key 重挂则 fail-closed:"authentication failed: this api key has been revoked"。

# 9) 治理面门的形状:门只卡状态方法面——不带 apiKey 的 initialize 照常应答,随后 tools/list 被拒
$ grovefs mcp --meta-dir $HOME_DIR --require-api-key      # 客户端 initialize 不带 apiKey
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize 应答(门只卡状态方法面)
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}

# 10) 客户端 initialize 携带 "apiKey":"<id>:<secret>"(且 scope 不是 mount)→ 该会话鉴权通过,工具面放行:
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
```

Nine subcommands: `create --label L [--for any|mcp|mount|admin] [--owner ACCOUNT] [--view V] [--ro] [--note TEXT] [--ttl DUR]` (mints `<id>:<secret>` and echoes it — the short 49-char format; the row keeps the SHA-256 digest authentication consults plus a wrapped copy that `secret` reads back; `--view`/`--ro` are meaningful only for mount/any scope, `--owner` required for admin; `--note` is a remark stored with the row, `--ttl` is turned into an absolute `expires_at` by the server — units `s`/`m`/`h`/`d` only, a misspelling is refused), `list [--view V] [--label S] [--owner S] [--scope P] [--note S]` (columns id/key/owner/label/scope/view/rw/created/last_used plus one combined `expires / revoked / note` column, a past-deadline row marked `EXPIRED` — still listed, refused at auth; the key column prints the stored head-4…tail-4 fingerprint, neither plaintext nor ciphertext; with `--view` lists only keys that can open that view, a pure alias also matching a key bound to the real name, and adds how many system-level keys stay unbound; `--label`/`--owner`/`--note` are case-insensitive substrings, `--scope` is exact), `revoke --id ID` (accepts an exact id or a unique prefix), `rotate --id ID` (exact id or unique prefix; mints a fresh secret for an active key's SAME id and echoes the new key — the stored copy is rewritten by the same statement, so a row that held none gains one; the old secret fails its next handshake, an established session lives until it reconnects, a revoked id refuses rotation — rotate replaces a credential, revoke kills one; the fingerprint the list prints follows the new secret), `secret --id ID` (exact id or unique prefix; reads the stored copy back as the full `<id>:<secret>`. That column holds an `sbx1.<nonce>.<ct>.<mac>` envelope (HMAC-CTR keystream + encrypt-then-MAC, no new dependency) and opening it needs the wrapping material: env `GROVEFS_STORE_KEY` (64 hex) first, else `<meta-dir>/meta/_control/store.key` (created 0600 with `create_new` at the first mint, a racer reads the winner's copy). Four answers, four sentences: copy + material → the credential; no copy stored → *never stored* (rotate is the way back); copy under different material → *unopenable* (a failed MAC refuses, it does not guess); copy disagreeing with the digest → *stale*, refused — it never hands out a string the authentication path would not accept. With no material in force a mint simply stores no copy; it never degrades to plaintext. A read through a meta-server or the console writes an `apikey.secret` audit row carrying the id it read; **a direct local read records nothing** — whoever can open that file already sees the whole file), `set-ro --id ID [--rw|--ro]` (flips an active key's read-only flag in place: default and `--ro` set read-only, `--rw` restores read-write; moves only the write permission, not liveness/auth, a server-established mount becomes hot-effective within ≤1s no reconnect — a local `--home` mount has no server credential gate and is unaffected by the switch; a revoked id refuses to flip), `update --id ID [--note TEXT] [--ttl DUR]` (edits only those two bookkeeping fields — identity, bindings and audit history are untouched: `--note ""` clears a remark, `--ttl never` drops a deadline; passing neither flag is refused instead of no-oping; **it is also the only recovery path for an expired key** — give an `EXPIRED` key a new `--ttl` and the same key authenticates again; a revoked id refuses the edit, revocation stays final), `calls --limit N` (default 20, read back in reverse order), `purge --before DUR [--dry-run]` (deletes the audit rows older than that window and reports how many went, reporting the count the table held when the cut began; `--dry-run` counts without touching anything, which is the question to ask before destroying history). That table does not grow towards forever: a server holding a `--control-listen` port always carries a window, silence means 7d, `meta-server --audit-retention 90d` sets another one and only `never` keeps everything; purge is a SUPERUSER verb on the governance port, and the console deliberately has **no** button for it (§3.5.2). An MCP client that can't customize the handshake may have the operator export `GROVEFS_MCP_API_KEY='<id>:<secret>'` to **pre-authenticate the whole service process** (`grovefs mcp` establishes its identity with that key at startup, the client's initialize need not carry one again); the mount-side counterpart env is `GROVEFS_API_KEY` (takes effect when `--api-key` isn't passed, keeping the secret off the command line). The `data-server`'s key-DB path is given by `--meta-dir <metadata root>`, **defaulting to the node's first `--data-dir`** — no extra parameter for a same-machine deployment, but a data-disk/metadata-root split deployment must point it right (pointing at a directory with no key ever minted is fail-closed, admitting none). Honest boundary: this key authenticates **the caller at the entrance** — it lets a caller (or not) issue governance write commands, let it (or not) open a view tree, and attributes each admission to a specific key, but does not change the governance semantics itself; the view-binding/read-only dimensions are enforced server-side; live-connection revalidation is **op-driven** (checked only with traffic), so the revoke's closure boundary is "that mount's first server round-trip after the revoke" — a read answered entirely by the kernel cache needs no round-trip and is thus not interrupted (closed on the next round-trip); `--token` and `--require-api-key` may coexist, both must pass. Without `--require-api-key`, server behavior is exactly as with no gate (only the audit still lands as usual). e2e in the `apikey` suite (61 assertions: mint / list / the plaintext never reaching the disk — only an `sbx1` envelope — with a 0600 `store.key` / `secret` reading back byte-for-byte what `create` printed · an unknown id refused / no-key-refused · wrong-secret-refused · right-key-admitted / call attribution / revoke-instant-and-row-kept / short-id-prefix revoke / env pre-auth / `list --view` filter / default-no-gate regression / `set-ro` in-place ro↔rw flip · still-authentic-after-flip / short 16+32-hex format / `rotate` prefix-rotation-keeps-id · old-secret-refused · new-secret-admitted · digest-only-on-disk · revoked-refuses-rotation / `list --label·--scope·--note` filters / `create --note·--ttl` echoing the remark and the server-computed deadline · `list` carrying both for the row · the key column being the stored head+tail fingerprint while the plaintext never reaches the disk · the fingerprint itself stored (derived from no secret at read time) · `update` clearing a remark and `--ttl never` dropping a deadline · refusing when neither flag is given · a past-deadline row listed as `EXPIRED` and refused at every gate · a new `--ttl` letting the SAME key authenticate again · `update` refusing a revoked id) and the `mountkey` suite (76 assertions: scope minting and typo refusal / six refusal-reason classes carried into the protocol / admission and env fallback (argv doesn't leak the secret) / connection audit down to a specific key and view / scope two-way isolation / revoke closes live mounts with no dirty endpoint / other keys unaffected / token-and-key double-gate AND / the data plane's independent gate / failover carrying the credential / no-gate default path no regression / view-bound key refusing across views with audit attribution / read-only key's server-enforced write refusal / `set-ro` on the same live mount rw→ro refuses write · ro→rw admits · no reconnect).

### 3.5.2 Governing the same metadata from any machine: the remote control plane (`--control-listen` / `--control-server`)

Governance commands (`view create/define/alias/list/rm`, `fork`/`graft`, `export`/`import`, `apikey`, `refs rebuild`, `gc`) must be able to read and write **the metadata store itself**. On the SQLite backend a tree is `<meta-dir>/meta/<view>/<view>.db`, reachable only by a process on the disk-holding host; on the PG backend any machine that can connect to the DB can govern it, but at the cost of handing `postgres://user:pw@…` to every management machine — one shared password that neither says who's editing nor can revoke just one person.

The remote control plane splits these two: `meta-server --control-listen HOST:PORT` opens one more listener that **runs only governance verbs**, and governance commands point at it with `--control-server HOST:PORT --api-key '<id>:<secret>'`, the real operations done on behalf by that already-store-holding process. A management machine has no DB URI, needn't reach `<meta-dir>`, and SQLite and PG take the same road. The credential is an **account key** (§3.5.1's `--for admin --owner <account>`): one account may hold many keys, each minted and revoked individually, each call landing in the audit.

Startup (the disk-holding host; the governance port and the mount port are two ports, each firewallable separately):

```bash
$ grovefs meta-server --meta-dir /srv/gfs --listen 0.0.0.0:50110 --control-listen 0.0.0.0:50111
meta-server listening on 0.0.0.0:50110 (base /srv/gfs, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view --meta-dir <dir> create --view <name>`); no empty tree is ever auto-created
meta-server: CONTROL plane listening on 0.0.0.0:50111 (admin-scope api keys only; every call is audited as method=control — mint with `grovefs apikey create --for admin --owner <account>` on this host)
```

The first admin key is minted on the disk-holding host (`apikey` reads the local DB directly; the governance port won't mint an admin for itself — that would make the gate pointless):

```bash
$ grovefs apikey --meta-dir /srv/gfs create --label ops-alice --for admin --owner alice
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    6e0f9e874f3c4bdc
  owner: alice
  label: ops-alice
  scope: admin
  view:  <any>
  rw:    rw (read-write)
  key:   6e0f9e874f3c4bdc:8516e6eaf85045178490e81d995d92e6

Admin scope also governs a `meta-server --control-listen` from any machine, with
  grovefs view --control-server HOST:PORT --api-key '<id>:<secret>' list
  grovefs apikey --control-server HOST:PORT --api-key '<id>:<secret>' create --label k2 --for mount
```

Any management machine (the secret into env, not into argv):

```bash
$ export GROVEFS_CONTROL_KEY='6e0f9e874f3c4bdc:8516e6eaf85045178490e81d995d92e6'
$ grovefs view --control-server 10.0.0.5:50111 list
no alias/fork/lens definitions in remote control plane at 10.0.0.5:50111 (backend sqlite) — the 1 base view(s) below each map to their own tree
base view 'alice' -> own tree meta/alice/alice.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --control-server 10.0.0.5:50111 --api-key <admin-key> list [--view NAME]` (add --view to see only that view's keys)

$ grovefs view --control-server 10.0.0.5:50111 create --view bob
created BASE view 'bob' at /srv/gfs/meta/bob/bob.db (via 10.0.0.5:50111)
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.

$ grovefs view --control-server 10.0.0.5:50111 rm --name bob --base
removed base view 'bob': 1 node(s) gone, 0 blob reference(s) dropped, 0 still held by other views; 0 object(s) freed from /srv/gfs/blocks, 0 not found under that data root (they live on another store?), 0 could not be freed (striped keys move through a data-server)

$ grovefs refs --control-server 10.0.0.5:50111 rebuild
refs ledger rebuilt at /srv/gfs/meta/_control/refs.db: 0 holder entries across the surviving views
  run it QUIESCED: a booking committed between the server's read pass and write pass would be overwritten with the pre-booking count
```

The audit is the same table as local: the `method` column says `control`, the `tool` column the verb, `detail` carries the account; attempts that fail auth land a row with an empty `key_id`, so "who tried with the wrong credential" is also visible.

```bash
$ grovefs apikey --control-server 10.0.0.5:50111 calls --limit 4
last 4 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-11-11T13:35:53.830004726+00:00 6e0f9e874f3c4bdc                   control      info                   yes  account=alice
2026-11-11T13:35:51.145091131+00:00 -                                  control      info                   no   authentication failed: this api key is scoped to 'mount', not 'admin'
2026-11-11T13:35:43.825879139+00:00 6e0f9e874f3c4bdc                   control      apikey.list            yes  account=alice
2026-11-11T13:35:43.820049671+00:00 6e0f9e874f3c4bdc                   control      refs.rebuild           yes  account=alice
```

Refusal verbatim (exit code 1):

```bash
$ grovefs view --control-server 10.0.0.5:50111 list                     # 不带 key
error: --control-server 10.0.0.5:50111 needs an admin account key: pass --api-key '<id>:<secret>' (or export GROVEFS_CONTROL_KEY). The first one is minted ON THE SERVER HOST with `grovefs apikey --meta-dir <server-meta-dir> create --for admin --owner <account>`.
$ grovefs view --control-server 10.0.0.5:50111 --api-key "$MOUNT_KEY" list
error: authentication failed: this api key is scoped to 'mount', not 'admin'
$ grovefs view --control-server 10.0.0.5:50111 --meta-dir /tmp list      # 本地 flag 与远程并用
error: --control-server 10.0.0.5:50111 cannot be combined with --meta-dir/--meta-store: a remote control plane governs the store ITS server owns, so a local path here would describe a different store. Drop the local flags — the account key alone decides what is governed.
$ grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" --data-dir /data/cluster   # the in-place form scanning this machine
error: --control-server 10.0.0.5:50111 audits bytes on the DATA SERVERS, so --data-dir cannot be combined with it — name the roots with --data-servers HOST:PORT instead
```

Coverage is split by verb (identical across both backends, see "both backends take one road" below):

| Via the governance port (any machine) | Must be on the disk-holding host (`--meta-dir` / `--meta-store`) |
| --- | --- |
| `view create` (incl. `--manifest` text) / `define` / `alias` / `list` / `rm` (delete a definition, `--base` delete a whole tree) | `mount` / `umount` (the mountpoint is local) |
| `view fork` (whole-tree copy, incl. `--empty`) / `graft` (pick a subset + rename/`--dest`) | `gc --data-dir R` (the SQLite-only form scanning this machine's `blocks/` in place) |
| `view export` / `import` (chunked up/down; locally only `--out` and the input file) | `du --meta-dir` (local tree files) |
| `apikey create` / `list` / `rotate` / `revoke` / `set-ro` / `calls` / `update` / `purge` | |
| `refs rebuild` | |
| `gc --data-servers … [--apply]` (the server reads its own tree, bytes enumerated against the data-server) | |

Semantics and boundaries:

- **Same set of guards**: behind the governance port is the same `ControlStore`; refusal reasons for illegal names, existing names, names governed by an alias/lens/fork, etc. are byte-for-byte identical to local, only the executing process changes.
- **Recognizes only admin**: keys of the `mount`/`mcp`/`any` scopes are all refused on that port (`any` never upgrades to admin); there's no anonymous mode, and when `api_keys.db` is empty no one gets in.
- **Revoke lands within a second**: a fresh connection checks and authenticates per call, so after `apikey revoke` the very next verb returns `this api key has been revoked`; a pinned connection is re-verified on the `KEY_RECHECK` tick (1 s), so a live session can do at most one more second of work before it gets that same sentence and ends — that second is the entire price of reusing a connection, and the refusal is audited like any other.
- **Rotation keeps the identity**: `apikey rotate` replaces only the secret — id/label/scope/view binding/ro/owner/audit history all survive; the old secret fails its next handshake (an already-established session lives until it reconnects — use revoke to kill now); a revoked id refuses rotation.
- **Connections can be pinned, identities are not lent out**: the framed protocol is still one-request-one-response with no lease that can be extended, but a governance client may send `"pin": true`, and the server then pins that credential to the socket: later requests carry no key and pay no re-authentication, which is exactly where the TCP connect, a server thread and the two queries of an authentication (the `api_keys` SELECT plus the `last_used_at` UPDATE) used to go. The reply carries `"pinned": true` only when the socket really is reusable, so the presence of that field IS the capability negotiation and either kind of endpoint can talk to either. Pinning is bounded: the server unpins after 512 requests or 300 s of silence, the client reopens after 60 s idle — and because the client yields first, a bound never cuts into a request in flight. **What is pinned is the credential, not the identity**: every op is still attributed and judged by the account in its own request (three different accounts can pass over one connection), and not one audit row is saved. `grovefs web` uses the pooled connection by default; `GROVEFS_GATEWAY_CTL_PIN=0` puts it back on one connection per request, and a CLI — one process, one call — still uses the one-request path. The secret appears only in a frame, the server compares only SHA-256 digests.
- **The audit table grows by itself**: a server holding a control port always carries a retention window: silence means **7d**, and saying `never` out loud is the only way to keep everything — one row per governance call, per MCP tool call and per mount handshake, so a deployment that lives on the console accumulates this table whether anyone reads it or not. `meta-server --audit-retention 90d` runs the same cut in the background every 1/24 of the window (bounded to 5 min..1 h) and **sweeps once at start**, so a restart against a shorter window applies it immediately; `grovefs apikey purge --before 90d [--dry-run]` runs it once by hand and reports the count it started from. purge is a SUPERUSER verb and **deliberately not a console button**: the account whose actions are recorded here is not the account allowed to trim them. The console's audit page states the current retention out loud — a superuser sees "the server sweeps rows older than X", a delegated operator simply gets no such line, and `auditRetention` in `store.info` is where that sentence comes from.
- **The port is the boundary**: the governance port opens only to the internal subnet, the mount port still uses `--listen`; a `--control-listen` bind failure makes `meta-server` fail at startup, not come up unhealthy.
- **Both backends take one road**: `fork`/`graft`/`export`/`import`/`gc` are executed by the store-holding `meta-server`, whether the backend is a SQLite file or a PG `grovefs_v_<hex>` schema doesn't change the command shape; under a PG deployment the server is already the only process that can touch the schema, and a management machine needs neither a `postgres://` connection string (so there's no leakage of a shared DB password).
- **A dump carries metadata only**: `export` renders server-side into the `<meta-dir>/meta/.xfer/` staging area and returns **line-aligned chunks** (never splitting a line), `import` pushes chunks back in reverse and installs in a single transaction; a cross-machine import moves only metadata, the bytes themselves must be moved separately — the CLI prints this reminder every time.
- **e2e**: `forksrv` (43 checks, fork/graft over the governance port on the PG tier; the SQLite tier is covered by the `fork` suite), `xfer` (96 checks, export/import on both tiers × local/remote), `gcremote` (44 checks, gc on both tiers) each assert line by line "the management machine writes nothing locally + the ledger is exact + consistency across tiers".
- **Defaults unchanged**: a `meta-server` without `--control-listen` is exactly as in the other sections; a governance command without `--control-server` still edits the local / current-URI store directly.

e2e in the `ctlplane` suite (startup banner / the first admin key must be minted on the host / remote create·define·alias·list·rm·refs·apikey full set with no local writes on the management machine / a second home still empty (proving the tree is only on the server) / `--for any`·`--for mount` and a junk key refused / remote revoke-instant / two refusal classes for no-key and local-flag mixing / guards still effective after a remote tree delete / audit retention: `apikey purge` counts without deleting under `--dry-run`, deletes and names the count it started from under `--before 1s`, refuses `--before never` and a nonsense duration, refuses a non-admin key, and a delegated account cannot reach `apikey.purge` even speaking straight to the socket — plus the two refusals of `--audit-retention` (no `--control-listen`, unreadable duration) plus `never` admitted as the explicit keep-everything, the 7d default and its startup banner reported too / the PG tier's same assertions); the two connection shapes of pinning are asserted in the `web` suite (the default arm keeps ≥1 live connection to the governance port, the `GROVEFS_GATEWAY_CTL_PIN=0` arm keeps none, and both answer the same op byte-for-byte identically — the knob swaps sockets, not semantics); the remote paths of fork/graft and export/import are covered by the `forksrv` and `xfer` suites respectively (both backends × local/remote).

### 3.5.3 Managing views and keys from a browser, and delegating base views to accounts (`grovefs web`)

§3.5.2 let any machine govern the same metadata, but the door is still a terminal: verbs to remember, a key to carry, refusal reasons to read. The three highest-frequency things — look at the views, hand out a key, give a colleague a patch of ground — are cheaper to click. And "**this** account governs **this** patch, including the forks and aliases derived from it and the keys bound to it" is a claim about ownership that one shared admin key cannot express and the terminal has no shape for.

The console adds exactly that, and it **does not add a second body of governance logic**. `grovefs web` is an HTTP gateway in front of `meta-server --control-listen`: an embedded single-page app (Overview / Views / Keys / Accounts / Audit — `include_str!`'d into the binary at build time, so no CDN, no node_modules, and it opens on an air-gapped host), which turns a browser session into an **account** and every click into one governance op forwarded onward. "Who may touch which tree" is still decided in exactly one place, server-side `gates()`/`authorize()`; the gateway only **subtracts** — the verbs it does not route (`gc.scan`/`gc.apply`/`refs.rebuild`/`view.rm_base`/`fork.graft`) can never reach the server from a browser, even where the server itself would have allowed them. Accounts, grants and password digests live on the **server** (SQLite: `<meta-dir>/meta/_control/web_accounts.db`; PG: tables in the control DB; PBKDF2-HMAC-SHA256, 210,000 rounds), and sign-ins are verified by the process that holds the store — the gateway never sees the password library, which is exactly why it can afford to be only a translation layer. The page follows the deployment too: on a PG multi-instance registry (§6.2's `--meta-store file:` shape), the new-base-view form on the Views page offers a **target-instance dropdown** fed dynamically by `store.info`'s `pgInstances` — the same expanded book the startup banner prints — and the default "auto" places exactly like the CLI without `--pg-instance` (least loaded);  on a single-instance or non-PG store the row simply isn't rendered, because there is no decision to make. The overview page adds an **instance ledger** on a PG store — placed-view counts per instance, the control-plane flag, and the views that predate the placement table — read from the same `view_placement` table the dropdown and the server-side balancer consult, so the three faces cannot drift apart; and the "metadata root" line renames itself to "local anchor (--meta-dir)" on PG/treedb deployments, quoting the banner's own layout sentence for where the trees actually live.

**Install time: the first administrator can only be placed by the process that holds the store.** If the governance entry point could mint its own administrators, the gate would be decorative — so bootstrap hangs off `meta-server`, not off `web`. Both flags apply only while the accounts table is **empty**; a restart that re-reads the same unit file is idempotent, while a different `--admin-user` refuses to start rather than quietly adding a second administrator. The password comes only from `--admin-password-file`, never `--admin-password` — argv is visible to every process on the machine and lands in the shell history.

```bash
$ grovefs meta-server --meta-dir .webdoc/srv --listen 127.0.0.1:15698 --control-listen 127.0.0.1:15699 \
    --admin-user rootops --admin-password-file ./root.pw
meta-server listening on 127.0.0.1:15698 (base .webdoc/srv, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view --meta-dir <dir> create --view <name>`); no empty tree is ever auto-created
meta-server: CONTROL plane listening on 127.0.0.1:15699 (admin-scope api keys only; every call is audited as method=control — mint with `grovefs apikey create --for admin --owner <account>` on this host)
meta-server: web accounts — created the first web account 'rootops' (role admin) from the install-time flags — sign in with it, then delegate base views
meta-server: web console sign-ins are verified HERE (`grovefs web --control-server 127.0.0.1:15699`); accounts and their base-view delegation live in meta/_control/web_accounts.db
```

**Start the gateway.** The credential is the same `--for admin` account key from §3.5.2, and `--api-key-file` is the recommended shape (the file itself `0600`, mounted as a secret under systemd):

```bash
$ grovefs apikey --meta-dir .webdoc/srv create --label web-console --for admin --owner rootops   # shape: see §3.5.1/§3.5.2
$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key --listen 127.0.0.1:15687
grovefs web: management console on http://127.0.0.1:15687/
  control plane : 127.0.0.1:15699 (backend sqlite)
  sign-in       : accounts of that server's web store; the first owner comes from its 
                    --admin-user flag
  sessions      : 8h idle window, 4 worker thread(s)
```

Open that address in a browser; for a remote host use an SSH tunnel (`ssh -L 8787:127.0.0.1:15687 store-host`) or a reverse proxy (§11, `docs/deploy.html`).

**Creating accounts and delegating** — an account may exist before it governs anything; `grant add` is what gives it ground, and the object of a grant is a **base view**, not a leaf:

```bash
$ grovefs view --control-server 127.0.0.1:15699 create --view main
created BASE view 'main' at .webdoc/srv/meta/main/main.db (via 127.0.0.1:15699)
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key account create --account bob --password-file ./bob.pw
web account created: bob
  role:  user
  bases: none — it can sign in and govern nothing yet
  note:  delegate a base view before this account can manage anything

Delegate a base view (a user governs that tree and everything derived from it):
  grovefs web --control-server 127.0.0.1:15699 grant add --account bob --base VIEW

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key grant add --account bob --base main
bob now governs 'main' and everything derived from it, as granted by rootops.
That covers its forks, aliases and lenses, and the keys bound to any of those names.

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key account list
account              role   created                last sign-in           disabled               base views governed
bob                  user   2026-11-17T16:39:25.333958529+00:00 -                      -                      main
rootops              admin  2026-11-17T16:39:24.081640099+00:00 -                      -                      <all: an admin is not bounded by grants>

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key grant list
account              base view                perm     granted                by
bob                  main                     manage   2026-11-17T16:39:25.384892614+00:00 rootops
```

The remaining account verbs: `account passwd` / `account role --role user|admin` / `account disable` (stops at the next call; `account enable` restores it with its grants untouched) / `account rm` (deletes the grants too, **but deliberately leaves its api keys** — removing a person should not silently stop a production mount that is running, and the output reports how many keys are still outstanding) / `grant rm`. Passwords always arrive via `--password-file` or `--password-stdin` (`pass generate … | grovefs web account create --account alice --password-stdin`); the two are mutually exclusive and there is no `--password`.

Refusal reasons, verbatim (exit code 1; the five below are password policy, granting to an account that does not exist, a missing control plane, an invalid key, and plain HTTP on a routable address):

```bash
$ … account create --account tiny --password-stdin          # feeding "abc"
error: password too short: use at least 10 characters
$ … grant add --account ghost --base main
error: no web account 'ghost' — create it first (`grovefs web account create`)
$ grovefs web --listen 127.0.0.1:15687
error: grovefs web needs --control-server HOST:PORT. The console is a gateway in 
  front of `meta-server --control-listen HOST:PORT`: it holds no accounts, views, keys 
  or audit of its own, so there is nothing for it to serve without that address.
  Try: grovefs web --control-server 127.0.0.1:9502 --api-key-file /etc/grovefs/control.key
$ grovefs web --control-server 127.0.0.1:15699 --api-key 'nope:not-a-key' --listen 127.0.0.1:15687
error: cannot reach the control plane at 127.0.0.1:15699 with this key: authentication failed: unknown api key id
The console needs `meta-server --control-listen HOST:PORT` and an `admin`-scope 
account key (`grovefs apikey create --for admin --owner <account>`), supplied to 
this process by --api-key / --api-key-file / $GROVEFS_CONTROL_KEY.
$ grovefs web … --listen 0.0.0.0:15690
error: refusing to bind 0.0.0.0:15690 — this console speaks plain HTTP, so a routable address puts every sign-in and every governance call on the wire unencrypted. Either bind 127.0.0.1 and terminate TLS in a reverse proxy (and pass --use-x-forwarded-for so sign-ins are throttled per CLIENT rather than per proxy), or pass 
             --allow-insecure-http if this network is trusted end to end.
```

**What a signed-in session can do.** The session is one `gfs_sid` cookie, `HttpOnly; SameSite=Strict`; writes additionally carry `X-GroveFS-CSRF` (handed out by the login response), and `Origin` must equal `Host`. Signing in as `bob` returns his own reach:

```json
{ "ok": true, "account": "bob", "role": "user", "bases": ["main"], "csrf": "…" }
```

| Page | admin | user (granted `main`) |
| --- | --- | --- |
| Overview | backend, role, totals of views / definitions / live keys / live sessions | only its own actor/role/bases — deployment-level numbers are never shown |
| Views | every view | `physical.list`/`def.list` return only his own tree and what derives from it; may define a lens/alias, create and delete forks, read `view.stats` |
| Keys | every key, may mint any scope | sees only his own keys and his view's; may mint `--for mount`/`--for mcp`, which **must** bind a view; may set-ro; may rotate a secret (same id, bindings untouched); may revoke his own |
| Accounts & grants | create accounts, change passwords, promote and demote, disable and delete, add and revoke base-view grants | the form is not rendered at all (he cannot create an account at any moment); the page becomes "your account + change your own password" |
| Audit | recent calls line by line (`ts/tool/ok/detail`, detail carrying `account=<person>@<ip>`) | sees only his own lines |

What `bob` gets back while working inside his own patch, from a real run (defining a lens, forking his base view, minting a key for a view of his own):

```json
{ "ok": true, "result": null }                       // def.put  {name:"team", text:"base main\nhide /secret\n"}
{ "ok": true,                                        // fork.create {base:"main", name:"lab", with_def:true}
  "result": { "blobs": 0, "definition": "fork main\n", "nodes": 1 } }
{ "ok": true,                                        // view.stats {view:"main"}
  "result": { "name": "main", "kind": "base", "base": "main", "tree": true, "forkTree": true,
              "family": ["lab", "team"], "keys": 1, "sessions": 0, "resident": false,
              "definition": null, "stamp": null } }
{ "ok": true, "id": "22e14911182f4d4a", "scope": "mount", "owner": "bob" }   // apikey.create
```

An attempt past his patch comes back with the server's own sentence (not something the gateway invented):

```json
{ "ok": false, "error": "account 'bob' is not an administrator, and 'view.create' is not per-view governance: a new base view picks a storage home (PG instance / data root), which is a deployment decision — an admin must run it (or grant you the base view it acts on)" }
{ "ok": false, "error": "account 'bob' is not an administrator, and 'account.list' is not per-view governance: this describes the deployment, not one view — an admin must run it (or grant you the base view it acts on)" }
{ "ok": false, "error": "account 'bob' may mint `--for mount` or `--for mcp` keys, not `--for admin`: those scopes govern the whole deployment, and only an administrator may hand that out" }
{ "ok": false, "error": "'gc.scan' is not available from the console: it stays an operator CLI command, where its dry-run report and confirmations are part of calling it" }
```

Revoking does not wait for a session to expire — the server re-reads the principal per request — so after `grant rm`, bob's **next call** hits the wall, and the audit keeps names on both sides:

```bash
$ grovefs web … grant rm --account bob --base main
bob no longer governs 'main'.

# bob's session is still valid, but the next action is refused by the server:
{ "ok": false, "error": "account 'bob' is not delegated base view 'main' (the name 'main' resolves to it) — an admin grants it with `grant bob main`" }
```

```text   ← the audit page's data source (`apikey.calls`, tab-separated, newest first)
2026-11-17T16:39:26.776732901+00:00  grant.remove   true   account=rootops
2026-11-17T16:39:26.692261124+00:00  view.stats     true   account=bob@127.0.0.1
2026-11-17T16:39:26.641644723+00:00  apikey.create  true   account=bob@127.0.0.1
2026-11-17T16:39:26.575385677+00:00  apikey.create  false  account=bob@127.0.0.1 account 'bob' may mint `--for mount` or `--for mcp` keys, not `--for admin`: …
2026-11-17T16:39:26.541581154+00:00  fork.create    true   account=bob@127.0.0.1
```

Semantics and boundaries:

- **What is granted is a base view, not a leaf**: if `grant add --base` is given a lens/alias/fork name, the server walks `resolve_base` (≤ 4 hops) up to its base view and **says so**; from then on every fork, rename or extra lens that grows under that base is automatically inside the grant, with no per-name additions — that is the difference between giving someone a patch of ground and giving them a name.
- **The gateway subtracts capabilities, it never adds them**: the admin key it holds outranks any signed-in account, so the whitelist of verbs a page may press is **hard-coded at 32** (an op not on it answers `… is not available from the console`), and the five destructive/cross-tenant governance verbs plus `login` are not routed at all; `/api/call` matches exactly, so a prefix trick like `apikey.list/../gc.apply` gets nowhere. Those two lists (the gateway's 32 and the server's ACL table) are maintained in two files, so one unit test, `every_routed_op_has_an_access_rule`, checks that every name in the whitelist has a server-side rule, and another, `destructive_ops_are_not_routable`, pins the verbs that are deliberately not routed — drift is caught by `cargo test --lib` rather than by someone clicking a button.
- **Identity comes from the server, not from the cookie**: the cookie only says "this browser signed in"; every request is re-authorized as its own account, so revoking a grant or `account disable` takes effect on the **next call**, while `account passwd` does not kick out sessions that are already open (that is the gateway's session table; to stop someone immediately, use `disable`).
- **Anti-enumeration and throttling**: "no such account" and "wrong password" return byte-identical wording and status codes, with timing padded out; the same client IP that fails 5 times within 15 minutes starts receiving 429 (30s baseline, doubling for each further failure), **even if that attempt's password was correct** — the 429 body carries `retryAfter`.
- **The control store is not a view**: under `meta/` the **directory name is the type** — `_control/` is the control plane (ledger, keys, accounts), `views/` holds the derived/alias definitions, and every other directory is one physical view tree (carrying its own `<name>.db` inside). The judgement lives in exactly **one** place (`meta::list_view_trees`) — otherwise a console account library would appear in `view list` as a mountable fake view and drag `refs rebuild`/`gc`/`fork drop` down with it.
- **Export and import are two dedicated routes** (`/api/export`, `/api/import`), not generic ops: they ship a file body and take session + CSRF the same way. Importing a **new base view** remains admin-only (`view.create` is a deployment decision); a `user` importing a tree they govern uses `as_fork=<their own base view>`.
- **A routable bind address must be admitted explicitly**: over plain HTTP, `--listen 0.0.0.0` puts every sign-in and every governance call on the wire unencrypted, so the gateway refuses to start by default. Either terminate TLS in a reverse proxy in front (and pass `--use-x-forwarded-for`, otherwise the throttling and the audit will show the proxy's address for everyone), or pass `--allow-insecure-http` to state that the network is trusted end to end. With `--use-x-forwarded-for` on, make sure only that proxy can reach the port — otherwise the client address in the header is whatever the caller chose.
- **The last lock cannot be destroyed**: demoting or deleting the **only enabled administrator** is refused (`'rootops' is the only enabled administrator — create a second admin and verify you can sign in as it before demoting this one`); create the second admin and verify you can sign in as it before touching the first.
- **Feature flag**: `web` is a default feature on top of `native`; `cargo build --no-default-features --features remote-only` produces a client-only binary without the gateway or the page (and therefore without tiny_http).

e2e in the `web` suite (189 checks: the four install-time flag combinations and "a refused startup does not open the port" / five classes of refusal before the gateway binds / security headers and no anonymous reach / the two wrong-login replies byte-identical (anti-enumeration) / CSRF, Origin and the whitelist verb by verb / headless account creation and delegation (including `--password-stdin`, password policy, `view list` not containing the three control stores, and `refs rebuild` still succeeding) / `bob`'s positive capabilities and each over-reach refusal / rotating his own key's secret through the page route `apikey.rotate` (the id survives, the new plaintext appears in the reply and in the copy only its owner can open, and neither plaintext is findable on disk) and refusing to rotate someone else's key / reading a stored copy back through the page route `apikey.secret` (byte-for-byte the credential `rotate` printed, the reveal writing no plaintext into the store, another account's row refused with `belongs to`) / `store.info` shipping the `pgInstances` key (null on SQLite, the instance names on a multi-instance registry — the placement dropdown's server contract) / a **counting** assertion on the routed whitelist (the page routes exactly 32 governance ops with `apikey.update` among them — an op added to the server's ACL table and forgotten here fails red instead of becoming a broken button in the browser) / the key page's bookkeeping: the remark and the **server-computed** deadline travel in the create reply, the minter is stored, listings carry only the head+tail fingerprint and the stored fingerprint equals the head-4/tail-4 of the credential echoed at creation (no listing ever carries the plaintext) / expiry is the server's verdict shipped in the row, a new ttl is the recovery path, a misspelt ttl is refused instead of minting a never-expiring key, editing someone else's bookkeeping is refused and `update` refuses a revoked row / revocation and disable effective on the next call / audit attribution `account=bob@ip` with no bleeding into other people's rows / the export→import tri-state / the last-admin protection tri-state and the second admin unlocking it / the 6th sign-in attempt returning 429 with `retryAfter` / a restart with the same bootstrap flags neither re-seeds nor locks itself, while a different `--admin-user` refuses to start); the server-side storage of accounts and grants on both backends (including PG) is covered by the `webauth`/`meta` unit tests in `cargo test --lib`.

---

## 4. Production topology: central storage + multiple client LAN mounts

Real scenario: data sits on a central cluster, several client machines mount over LAN, the same user across machines sees **one authoritative view**.
This requires splitting the two services:

```bash
# ===== 中心节点 =====
# 数据服务:把物理对象存到本节点的硬盘目录
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --advertise-addr 10.0.0.5:50100 --member-id 0 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key

# 元数据服务:托管每用户权威视图(多客户机共享同一棵,不再各存各的副本)
# 引导服务 manager + 元数据服务：拓扑在服务端声明一次、签名分发，客户机只指 manager 一个地址
$Y --home /srv/mgr manager define --meta 10.0.0.5:50110 --data 10.0.0.5:50100
$Y --home /srv/mgr manager serve --listen 0.0.0.0:50055 &
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --advertise-addr 10.0.0.5:50110 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key
#   --meta-dir 就是元数据根:视图库在其 meta/ 下。视图名必须显式创建——先建树再挂:
$Y view --meta-dir /data/cluster create --view alice
#   (不建就挂会被 M_OPEN 拒:view 'alice' does not exist on this meta-server …)

# ===== 每台客户机(远程挂载不需要 --home,本地什么都不落) =====
$Y mount --view alice \
     --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

- `--manager HOST:PORT` + `--roster-key-file F`: the client's ONLY source of coordinates — the resolved data list sends file IO over the network to the data-server (never touching the client's local disk), the resolved meta list has the view tree served by the central meta-server (**multiple clients mounting the same user = sharing one authoritative tree**). Hand-typed `--meta-server`/`--data-servers` were removed from `mount`.
- The roster's meta list may be a failover list and its data list the complete stripe member set — this topology is declared once server-side by `manager define` instead of being retyped on every client machine (§4.2).
- The client keeps **no view copy locally and writes nothing locally**: a remote mount **needs no `--home`** (passing one writes nothing) — the authoritative data is on the meta-server, the bytes on the data-server, access control all happens server-side at `M_OPEN`/per-op, the client doesn't consult a local DB.

> **A nonexistent view name is refused outright — this is the only behavior, no switch.**
>
> **Rule**: the name must already exist — a physical tree, or a governance definition (derived/alias/fork) — otherwise `M_OPEN` refuses outright and **writes nothing**. The refusal travels as a **payload-bearing `E_DB`** (not an empty packet), so the client prints the reason rather than a bare `Input/output error`, and the server log has the same sentence (verified verbatim, SQLite):
>
> ```text
> meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition …; no empty tree is ever auto-created
> $ grovefs mount --view typo1 --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/t
> error: view 'typo1' does not exist on this meta-server (sqlite): no tree and no definition —
>   views are created explicitly, so unknown names are refused instead of becoming an empty tree;
>   create it first: `grovefs view --meta-dir /data/cluster create --view typo1` …
> meta: M_OPEN view='typo1' refused: …                          # 服务端日志
> $ ls /data/cluster/meta | grep typo1                          # 空:没有 typo1.db,也没有 -wal/-shm
> ```
>
> **There is no switch**: an unknown view name is always refused, no parameter or environment variable can change this rule.
>
> With `--require-api-key` on, this refusal also enters the audit (`grovefs apikey calls`): `… M_OPEN view='typo1' refused: …`. Existing base views, derived views, pure aliases, forks mount as usual; the PG backend probes the PG schema itself (`information_schema.schemata`), so a refusal **creates no schema**.
>
> **For a new view, always create-then-mount**: SQLite `grovefs view --meta-dir <meta-dir> create --view <name>` (or a tree seeded by `view import`); PG `grovefs view --meta-store postgres://… --meta-dir <meta-dir> create --view <name>` builds the schema explicitly (or restores from `pg_dump`).
>
> **The orthogonal other lock is binding a key to a view**: a key from `apikey create --view team` mounting a different name is refused at the **binding check** (earlier than the existence check), verified `permission denied: this api key is bound to view 'team', not 'ghost3'`, likewise building no tree. Division of labor: the existence check governs "does this name exist", the bound key governs "which tree may this caller open".
>
> Cleaning up a mistakenly created empty tree (first `umount` all mounts of that name): for a bare base view (a physical tree with no definition) use `grovefs view --meta-dir D rm --name <base> --base` in one command — it deletes `meta/<base>/<base>.db` (+WAL/SHM), runs `refs rebuild` to recompute the ledger, and reclaims bytes no other view holds (§3.4.3). The manual equivalent: `rm -f meta/<name>/<name>.db meta/<name>/<name>.db-wal meta/<name>/<name>.db-shm` then `grovefs refs --meta-dir D rebuild`; the PG backend is the **same command** with `--meta-store` (`view --meta-store postgres://… rm --name <base> --base`) — the booking-off (per-key decrement of `grovefs_refs.holders`) and `DROP SCHEMA … CASCADE` complete in one transaction, and after commit bytes are reclaimed per the `data_ref` read before deletion (§15.8). A hand `DROP SCHEMA` past the CLI does not count as deletion: the ref list vanishes with the schema, the ledger rows thereafter can be reclaimed by no one (`refs --meta-store postgres://… rebuild` only deletes the over-counted rows, the bytes are already unlocatable).

### 4.1 Does multi-machine sharing really need PG? — getting-started selection

> **Direct answer: no.** The ability to share one tree across machines over LAN comes from "the same `meta-server` + the same `--view`", unrelated to whether the backend stores SQLite or PG. The default embedded SQLite (compiled into the binary as `rusqlite bundled`, zero external dependencies) already suffices for multi-client mounts, write serialization, and real-time invalidation push. PG is only an **optional backend** for meta-server, a pure server-side config (`--meta-store postgres://…` given at meta-server startup, clients have no such option and are unaware). The two are a **pick-one** replacement relation, not stacked: the same authoritative tree lands on one backend only — with SQLite you needn't install PG; after switching to PG the view tree goes only into a PG schema, the server no longer creates/reads/writes any `<meta-dir>/meta/<view>/<view>.db` (what stays in the directory is the old pre-switch copy); the control plane follows the backend too — `grovefs view`/`apikey`/`mcp`/`data-server` also accept `--meta-store postgres://…` (or env `GROVEFS_META_STORE`), keys and audit go into `grovefs_control.api_keys`/`mcp_calls`, view definitions into `grovefs_control.view_defs`, and a PG deployment lands no control-plane file locally (proven by the `control` suite). Go to PG only if you want to hand the authoritative tree's persistence/HA/operations **to a mature database**.

| Topology | Backend | Applicable scenario | Command |
| --- | --- | --- | --- |
| Single machine local | SQLite (embedded) | development, testing, single user | start `data-server` + `meta-server` locally, then `mount --manager 127.0.0.1:50055 --roster-key-file ~/roster.key` (§15.1) |
| **Central node + multi-client LAN** | **SQLite (default)** | **small/medium team sharing, zero external-service dependency** | `meta-server --listen 0.0.0.0:50110` + client `mount --manager MGR:50055 --roster-key-file F --view alice` |
| Central + multi-client LAN | SQLite + HA (`--role`+`--peer`+`--witness`) | auto failover after a single meta-server dies, double-write protection | add `--role primary --peer …` etc. |
| Central + multi-client LAN | **PostgreSQL** (`--meta-store postgres://…`) | want to hand HA/backup/PITR/million-node index tuning **to PG**, and already have PG ops capability | `meta-server --meta-store postgres://user:pw@host:5432/db` (can't add `--peer`/`--witness` at once) |

> **Common misconceptions corrected**:
> - "PG must be installed on both client and server" → wrong, **server-side only** (the meta-server process connects to PG), the client installs only the `grovefs` binary and speaks only GroveFS's TCP framed protocol.
> - "Going to PG gives higher write concurrency" → **partly true**: the default SQLite is still single-writer; but `--meta-store postgres://…` changes writes to a SERIALIZABLE write-connection pool (at most 8 connections, the HopsFS route), concurrent writes on disjoint rows commit in parallel, conflicts transparently retried by SSI (verified by the `pg_ssi`/`pg_parallel` suites). Parallelism is only within a single PG cluster, not partitioning metadata authority across clusters; cross-cluster scaling is still left to PG's own replication/sharding.
> - "You must have PG before sharing a tree" → wrong, the default SQLite supports it, the three suites `metasrv`/`freshness`/`locks` are all pure SQLite.

### Cross-client consistency: two modes

| Mode | Command | Behavior |
|------|------|------|
| **Real-time invalidation push (default, recommended)** | every remote mount (meta coordinates resolved from the manager) | after one client writes successfully, the meta-server broadcasts invalidation (`inval_entry`+`inval_inode`) over an event bus to the same user's other subscribers, the peer's next access is **immediately visible**, no polling. |
| **`--no-cache` strong-consistency backstop** | add `--no-cache` | disables kernel attr/dentry caching and reads directly via `FOPEN_DIRECT_IO`, writes immediately visible. Cost: zero client cache, more metadata/data round-trips. For scenarios with extremely strict freshness demands. |

> **Default is fresh**: the non-`--no-cache` push-invalidation model already guarantees "A writes successfully → B sees it on next access immediately", no polling, no `--no-cache`. Proven end-to-end by `.build-bin/test_freshness.sh` (sequential append of 100 lines, truncate, reverse read all become peer-visible within <500ms). `--no-cache` is **not** the only channel to keep fresh; it merely fully bypasses the kernel cache as a more conservative backstop in the extreme race window where writeback dirty-page flush and remote invalidation interleave.

> **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` are arbitrated centrally via the meta-server's `locks` table (SQLite), **across processes and machines**; after a primary/standby switch the lock state survives via snapshot replication. Lock reclamation has two paths: ① **the lock-holding process crashes** — the kernel closing its fd triggers FUSE `release` (carrying a matching `lock_owner`), the lock row is **deleted immediately**, far less than the lease; ② **the whole mount daemon machine crashes** — no `release`, no renewal, the lock expires and is swept **within a short lease (default 15s)**. A live holder keeps renewing via the mount daemon's **renewal heartbeat** (bumping `expires_at` every lease/3), **never timing out**. Non-cooperative apps (cp/dd) still don't block, consistent with NFS/SMB advisory semantics. Proven by `.build-bin/test_locks.sh` (segment 4 crash-immediate-reclaim / segment 4b renewal across the lease). **Scalability boundary**: lock-waiting (blocking F_SETLK / `flock -w`) polls synchronously in the mount process (every 100ms, up to 10 minutes), occupying a FUSE worker thread; under the default `--threads 4`, if ~4 processes are waiting on locks at the same mountpoint at once, other requests to that mount queue — for lock-heavy scenarios increase `--threads`, event-driven wakeup is on the roadmap.

#### 4.1.1 Both tiers' hard metrics: write parallelism is just the most visible one

| Metric | SQLite (default) | PostgreSQL |
| --- | --- | --- |
| Write model | single writer (`BEGIN IMMEDIATE` mutex); reads parallel via WAL | 8 SERIALIZABLE write connections + 4 read-only connections per view, disjoint rows commit in parallel, conflicts transparently retried by SSI |
| Write throughput (this repo measured) | **767–1,109 ops/s** (N=1 1,109 / N=4 811 / N=8 767; concurrent writes still single-writer serialized, and dip slightly with mount count) | single-view plateau **~4.6k–5.2k ops/s** (N=4 peak 5,235; N≥8 dips with SSI retries; after the write-statement cache N=1/4/8 are 3,048–3,118 / 4,784–5,202 / 4,518–4,680, §6.2) |
| Single-transaction cap | no hard gate (bounded only by memory/disk) | no hard gate |
| Availability | native HA: `--role primary/standby` + `--peer` + `--witness`, snapshot forwarding; single writer, switching has a window | left to PG itself (streaming replication / PITR), GroveFS does not manage it |
| View scale | one `meta/<view>/<view>.db` + one engine instance per view, about 10 + (read-pool−1)×~1.3 fds/view (server side: default **warm 1** read connection, bursts retained up to 8 as needed; `GROVEFS_SQLITE_READ_POOL` raises the warm floor), under 65536 about four thousand views | 12 connections per view (default 8 write + 4 read, tunable via `GROVEFS_PG_*_POOL`); after `--engine-idle-secs` eviction counted by "active views in the last N minutes" |

The conclusion is clear: **the gap between SQLite and PG is the single-writer model itself (one write transaction at a time), measured about 4–6×**.

> Ledger and main DB share one durability tier: the cross-view ledger `refs.db` is `ATTACH`ed into the view DB, and switching `journal_mode=WAL` resets **the attached DB's own** `synchronous` back to FULL — left untreated, every naming operation would pay an extra 17ms-class fsync on `refs.db-wal`. `attach_refs_ledger` therefore mirrors the view DB's `synchronous` onto the ledger; the invariant is pinned by the unit test `refs_ledger_mirrors_the_view_synchronous_mode`.

Under a remote mount (`meta-server`), `M_CREATE` / `M_SYMLINK` inline the just-committed row into the reply (`MetaProvider::create_node_with_attr`; the local engine uses a default implementation that reads one extra row without an extra round-trip), so one create pays only two metadata round-trips — the kernel's `LOOKUP` plus `M_CREATE` — and the client fills the FUSE reply from the row itself: on the same remote mountpoint, 1500 sequential creates, `--stats`' `remeta.rpc` per create **2.003**, **0.360–0.407ms/op**. The attributes come from the just-written row itself, mode/uid/gid/nlink/link-target pinned by `create_and_symlink_replies_carry_the_row_in_one_round_trip` and `.build-bin/probe_create_attr.sh`; when an old server's reply carries no row the client auto-falls back to one `M_GETATTR` — the one round-trip that cannot be saved is the price of correctness, not a version switch.

#### 4.1.2 What can be replaced is the feature surface, what can't is the form

**On the feature surface SQLite is a superset**: all 20 governance capabilities (see the §10.1 matrix) are available on SQLite — `fork`/`graft`, `export`/`import --as-fork`, `refs rebuild`, `rm --base`, remote `gc`, `du`, `apikey`+audit, and even **native HA exists only on SQLite** (PG refuses). The few REFUSED entries in the PG column are differences of form rather than feature gaps: zero-copy onboarding of a directory, the `gc` that scans this machine's `blocks/` in place, and native primary/standby snapshot forwarding — the alternatives are spelled out row by row in §15.9.

**What can't be replaced is these four** (all in "form", not "feature"):

| Dimension | SQLite's real state | What differs in PG's form |
| --- | --- | --- |
| Availability to the storage layer | primary/standby + witness + snapshot forwarding: single writer, switching has a window, depends on two external processes and the network | replicas and recovery are handed to PG itself (streaming replication / PITR), GroveFS does not manage it |
| View scale | one file, one engine per view, fd/memory grow linearly, no trick besides "eviction" | one connection pool per view, reclaimed by `--engine-idle-secs` when idle; the ceiling is PG's `max_connections`, not this machine's fd table |
| Multi-machine shared write | one `--meta-dir` may have only one writer (file lock); putting the `.db` on a shared filesystem doesn't work | writes on disjoint rows commit in parallel (transparently retried by SSI); still a single `meta-server` serving one cluster |
| Independent metadata scaling | metadata bound to the meta-server machine's disk | metadata lands on a separate DB host, decoupled from the bytes (data-server / object store) |

#### 4.1.3 Backend-exclusive / semi-exclusive commands

"Exclusive" means only one tier can run it; "semi-exclusive" means both tiers share the name but differ in landing spot or form. Detail and refusal verbatim in §15.9.

| Command | SQLite | PG | Note |
| --- | --- | --- | --- |
| `grovefs import --view V --source DIR` | available | refused | the top-level import is "bring an on-disk directory under management", relying on same-disk hardlink zero-copy; a remote DB has no manageable local disk, it refuses and points to `view import` |
| `gc --meta-dir D --data-dir R [--apply]` | available | refused | offline orphan scanning must see both the DB and the object directory; a remote deployment uses `gc --control-server … --data-servers …` instead |
| `meta-server --role primary/standby --peer … [--witness …]` | available | refused | native HA is SQLite-only; PG uses streaming replication / PITR |
| `view define / alias / list` | definition lands `meta/views/*.layout` | definition lands `grovefs_control.view_defs` | each tier lands its own control-plane store, same one discipline |
| `apikey … / calls` | `meta/_control/api_keys.db` | `grovefs_control` | capability OK on both tiers, only the landing spot differs |
| `refs rebuild` / `rm --base` / `du` | `--meta-dir` | `--meta-store postgres://…` | same name different shell; `du` on a nonexistent view name gives the same `no tree for view …` on both tiers |

#### 4.1.4 How to choose: two lines, don't pick by "does a feature exist"

- **Single center + hundreds of active views + metadata within a few GB** → **SQLite** (default). Least fuss, largest transactions, single binary; add primary/standby HA as needed.
- **Need 8-way parallel writes / hand backup and PITR to the database / already have PG ops** → **PostgreSQL**. The connection wall has been dissolved by `--engine-idle-secs` eviction; write parallelism is its only hard advantage over SQLite (~4.6k vs ~0.8–1.1k, §4.1.1).

```
你的部署形态？
├─ 单机 / 单中心，活跃视图 < 千级，能接受主备切换 ─────► SQLite（默认）
└─ 要多写并行，或想把 HA / PITR 外包给数据库 ───────────► PostgreSQL
```

### 4.2 Mount bootstrap: the manager — declare the coordinates once, point clients at one address

A `mount` resolves two cluster coordinates: a metadata service (possibly an ORDERED failover list) and a data service (possibly the COMPLETE stripe member set). The second is not a candidate pool — a striped object is pinned to its member-id set, so an object is unreadable through a shorter list. Both coordinates are therefore **provisioning facts**: an operator decides them, they do not emerge at runtime. Constants like these do not have to be typed on every client machine: `manager` declares them once server-side and distributes them signed, and a client points at the manager alone — hand-typed `--meta-server`/`--data-servers` were removed from `mount`.

The manager is a **bootstrap service and never a data path**: a mount consults it once, resolves the two address lists, then connects to the meta- and data-servers directly and never asks again.

```bash
# ===== (1) server side: declare the topology, then start the daemon =====
$Y manager --state-dir /srv/mgr define --name prod \
     --meta 10.0.0.5:50110 --data 10.0.0.5:50100,10.0.0.6:50100
deployment 'prod' declared in /srv/mgr/manager/deployments.json
  meta (failover order): 10.0.0.5:50110
  data (complete member set): 10.0.0.5:50100, 10.0.0.6:50100
  declaration epoch 1
  next: every address above must be a RUNNING server started with
--register-with — a roster ships the full declared set (a stripe's
members are not a candidate pool), so one declared-but-down
data-server fails mounts instead of being skipped. `manager status`
shows who has actually registered.
$Y manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055
manager: join key   /srv/mgr/manager/join.key — copy to every node started with --register-with (authorizes a registration; a client never holds it)
manager: roster key /srv/mgr/manager/roster.key — copy to every client as `mount --roster-key-file` (signs the answer; symmetric, so keep the manager's copy secret)
manager listening on 0.0.0.0:50055 (host-readable state /srv/mgr/manager/deployments.json)
manager: deployment 'prod' (epoch 1) — meta [10.0.0.5:50110] data [10.0.0.5:50100, 10.0.0.6:50100]
manager: rosters valid 300s after issue; registrations lapse after 90s of silence; lookups answer from the declaration, live or not
manager: neither key leaves this process — a node proves itself with the JOIN key, a client checks the ROSTER key; heartbeat is ~30s, and a node that cannot reach this service keeps serving its own mounts

# ===== (2) nodes: register at startup (registration is NOT a prerequisite for serving) =====
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --member-id 0 \
     --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50100 \
     --join-key-file /etc/grovefs/join.key --deployment prod
manager: registered data 10.0.0.5:50100 of deployment 'prod', member-id 0 — live 90s
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50110 \
     --join-key-file /etc/grovefs/join.key --deployment prod

# ===== (3) each client: one address =====
$Y mount --view alice --manager 10.0.0.9:50055 --deployment prod \
     --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
manager 10.0.0.9:50055: deployment 'prod' epoch 1 resolved meta [10.0.0.5:50110] data [10.0.0.5:50100,10.0.0.6:50100] (signature verified, valid for 300s more)
using remote metadata service at 10.0.0.5:50110
using sharded remote data service across 2 servers (stripe 16777216 bytes): 10.0.0.5:50100,10.0.0.6:50100
mounting view 'alice' at /mnt/alice (fully remote: no local store)
```

Reading the topology back does not need a mount — the operator asks with their own credential:

```bash
$Y manager status --server 10.0.0.9:50055 --deployment prod --join-key-file /etc/grovefs/join.key
```

The JSON carries, for every **declared** address, `registered` / `seen_secs_ago` (since its last heartbeat) / `member_for_secs` (how long this address has been part of the fleet) / `live_secs_left` / `member_id` — a diagnostic view, never a routing input.

**Four semantics worth remembering**

- **A roster is the DECLARED set, never "the ones alive right now".** Heartbeats influence exactly two things: one column in `status`, and whether `manager serve --require-registered` refuses the lookup outright (it is a **precheck gate, not a filter** — with it on, the roster is still the full declared set, because handing a client half of a stripe's members hands it objects it cannot read). `--require-registered` is off by default; use it where a half-started cluster should fail mounts loudly instead of letting them discover the gap by timing out.
- **Two keys, two questions.** `--join-key-file` (64 hex, `0600`) authorizes a REGISTRATION and is held only by meta-/data-servers — a client never holds it. `--roster-key-file` signs the ANSWER: the manager signs with it, the client verifies with it. **Registration is not admission**: a node that cannot reach the manager keeps serving, retrying with backoff (one log line per state change, not per attempt).
- **An undeclared address cannot get in.** A registration is accepted only for an address the operator DECLARED for that role and deployment (`Denial::NotDeclared`), so a leaked join key still **cannot steer clients to an attacker's machine** — the refusal, verbatim from a real run: `manager: registration with 10.0.0.9:50055 failed (data address 10.0.0.7:50100 is not declared for deployment 'prod'; a registration can never ADD an address to a roster — correct --advertise-addr or declare it (declared for this role: [10.0.0.5:50100, 10.0.0.6:50100])); the node serves regardless, retrying (next note only on a change)`.
- **Fail-closed on verification.** A missing `--roster-key-file`, a bad signature, an expired roster or an unknown roster version exits `mount --manager` non-zero and leaves no mountpoint behind; it never "mounts then warns". `--manager` is the **only** coordinate source for `mount`: the hand-typed `--meta-server`/`--data-servers` flags were removed outright (there is no "mutually exclusive" question left, only one road remains), and the two resolved address lists feed the very same backend-construction path — **a replacement, not an addition**.

**Failure boundary**: kill the manager and existing mounts keep reading and writing (the data plane never goes through it); what is affected is NEW mounts, which fail closed. Declarations live in `<state-dir>/manager/deployments.json`, and `serve` stats its mtime before every request and re-reads only on change — which is why `define` can be a standalone short command (no daemon-side credential, no restart) and why a restarted manager needs no re-define. **Topology cost**: one more listening port reachable from every node AND every client (a separate port from the meta/data planes, so a cross-machine deployment opens it explicitly), carrying roughly one request/response per node per ~30s plus one per mount; **latency cost** = one extra TCP round-trip per mount plus one local HMAC-SHA256 verify; **IO-path delta = 0**.

The honest strength ceiling (an HMAC is symmetric, so a client that can verify can also sign), what the Data-Access Capability already disarms, and every rejected alternative and residual risk: [`docs/adr/0001-manager.md`](adr/0001-manager.md). End-to-end assertions: `.build-bin/test_manager.sh` (§0–§10).

---

## 5. How to handle data (key focus)

### 5.0 Anatomy of view metadata (one-screen quick reference)

What exactly is stored in one storage root (`<meta-dir>` or a local `--home`), and how a name is parsed when mounted:

```text
<meta-dir>/
├─ blocks/<2hex>/<key>      内容字节,一文件一对象(key=UUID,按哈希扇出 256 桶)
└─ meta/
   ├─ <view>/                每个物理视图一棵权威树,自成一个目录
   │  └─ <view>.db           (SQLite WAL;-wal/-shm 同目录,随 checkpoint 出没)
   │     node(id INTEGER PRIMARY KEY, ino, parent_id, name, is_dir, data_ref,
   │          link_target, mode, uid, gid, mtime, atime, ctime,
   │          UNIQUE(parent_id, name))   ← 树的全部:名字、类型、指向 blocks 的 key、POSIX 属性
   │     ctrl               (k,v) 键值表:今天只有 HA 的 epoch(0/缺行=没启用 HA)
   │     locks              POSIX/BSD 锁表(租约到期自动回收,release 是持有者终止信号)
   ├─ _control/              控制面:不是树(目录名是保留视图名,永不被当树枚举)
   │  ├─ refs.db             跨视图共享对象持有者账本(GC 判「最后一个名字」的唯一依据)
   │  └─ api_keys.db         API key SHA-256 摘要 + 调用审计(SQLite 后端的控制面;PG 后端进 grovefs_control)
   └─ views/*.layout        派生/别名/fork 定义(派生=`base <B>` 行+规则行;别名=单行 `alias <R>`;fork=单行 `fork <B>`(三种行可写在文件任意位置,注释/空行跳过))
```

- **Field-by-field glossary**: `node`'s 13 fields (incl. the division between `ino` and `id`, `mode` storing only permission bits, times in seconds), `ctrl.epoch`, `locks`' 9 fields (the lease and `sid` renewal), `refs.holders`' counting semantics, and the read path each of the 4 indexes supports, see the web manual `docs.html` §8.1.1 (with real `sqlite3` query echoes); this section gives only a one-screen reference.
- **The name-resolution chain**: `mount --view X` → `M_OPEN(X)` → the server looks up `meta/views/X.layout`:
  contains a `base <B>` line (derived) → opens the physical tree `meta/<B>/<B>.db`, applies the rule lines as a `LayoutMeta` lens, mounts read-only;
  contains an `alias <R>` line → applies no rules, the name lands directly on the real tree `meta/<R>/<R>.db`, whole tree writable, the real name refused to mount;
  contains a `fork <B>` line → opens its own `meta/X/X.db` directly (an independent tree);
  no definition → X is a physical view, opens `meta/X/X.db` directly. The client needs no rules file locally.
- **Two backends of the same tree**: SQLite (default, one directory and one `.db` per view) and PG (`meta-server --meta-store postgres://…`, one schema `grovefs_v_<hex(view)>` per view, the ledger a shared table `grovefs_refs.holders`) are a **pick-one** replacement relation; the control plane follows the backend too: under `--meta-store postgres://…` keys/audit/view definitions all go into the `grovefs_control` schema (`api_keys`/`mcp_calls`/`view_defs`), nothing lands as a local control-plane file; the CLI `view`/`apikey`/`mcp`/`data-server` use `--meta-store` (or env `GROVEFS_META_STORE`) to reach the same system record.
- **The backup closed loop**: `blocks/` + one `meta/<view>/<view>.db` per tree + `meta/_control/refs.db` (recomputable with `grovefs refs --meta-dir D rebuild`) + `meta/views/*.layout` + (as needed) `meta/_control/api_keys.db` — under the PG backend one `pg_dump` covers both `grovefs_v_*` and `grovefs_control`, leaving only `blocks/` + env to back up locally — §5.4 gives the full list and both the stop-service and hot-backup approaches.
- **The customization↔storage mapping**: the full cross-reference of the four forms (command→files/tables produced, parameters, common pitfalls) is in §3.0; §5.0's `.layout` is the on-disk form of "how to customize a view" (the rule's four verbs in README "the three view questions" and §3.1/§3.2); fork is the only customization that creates a physical tree (`view fork` copies `node` rows, bytes shared and booked into the ledger).

### 5.1 Physical layout

- **Single object**: `blocks/<2hex>/<key>`, one file = one object file. Objects fan out by key hash automatically into 256 bucket subdirectories (the bucket name is the two lowercase hex digits of `FNV-1a(key) % 256`, not a prefix of the key), one level deep, no deeper tree — the per-directory entry count doesn't grow with the total, at tens of millions of objects each bucket holds only tens of thousands, `ls`/`rsync`/backup tools don't degrade. The bucket name is a pure function of the key, the 256 are pre-built at startup, no mkdir on the hot path.
- **Key naming**: on a view write a UUID is generated as the object key, stored in the view's `data_ref` field.
- **Striping (a big file across multiple data-servers)**: `data_ref` is encoded as `s<N>:<stripe>[:<fp>]:<uuid>` (`<fp>` is the fingerprint of the member-id set, appearing only in identity mode; the positional mode keeps three fields),
  the i-th chunk key = `{uuid}#{i}`, landing at `servers[i % N]` — when `--member-id` is enabled the slot is determined by the **identity** after sorting member ids (the list order is irrelevant), when not enabled it is the **list index**, not the address. **A key without the `s` prefix = a single object** (the storage form when striping is not enabled).

### 5.2 Multi-data-server striping (aggregating capacity/bandwidth)

```bash
# Declare the topology once server-side: comma-separated data-server addresses → the roster resolves a multi-address list, deterministic striping turns on
$Y --home /srv/mgr manager define --meta 10.0.0.5:50110 --data 10.0.0.5:50100,10.0.0.6:50100
$Y mount --view alice --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

- A file is split into chunks only if it's ≥ the chunk size; the chunk size is configured by `--stripe-bytes` (env `GROVEFS_STRIPE_BYTES`), default 16 MiB — it's booked per-object into `data_ref`, so changing the config affects only objects created afterward, old objects remain readable at their own recorded sizes, and a cluster can mix different values. Small files naturally land only in chunk0 (a single node).
- **Value**: spreading a large file over multiple nodes' **aggregate capacity**. Note: on a single already-saturated link (loopback/single NIC) there is no throughput gain, real gains require genuinely independent links.
- A data list that resolves to a single `host:port` is striping-not-enabled: the whole object lands on that one machine, the key has no `s` prefix (giving `--stripe-bytes` here is honestly warned as ineffective).
- **Member identity `--member-id` (eliminating the order hazard)**: once each data-server starts with a unique `--member-id N` (or `GROVEFS_MEMBER_ID`), stripe placement is by **sorted member identity** rather than list order — `manager define --data` listing the same machines in any order reads/writes correctly, machine/IP changes replace in place and get re-`define`d. Keeping the count but **changing the set** (swapping one machine with a new id) is caught by the member-set fingerprint in `data_ref` and the mount is refused, rather than silently reading the wrong chunk. Either all members configure ids or none do: mixing the two modes in the same key space is refused at mount. Without ids it stays in the historical positional mode by list index, where reordering `manager define --data` silently mis-routes existing objects (see §3.4.2 migration constraints).

### 5.3 Deletion / GC

- After `unlink` (deleting a file), the view drops the reference, the backend best-effort deletes all chunks by key, a striped remove expands to delete all chunks, **idempotent** (leftover chunks can be cleared by GC).
- **The reference ledger `refs` for cross-view shared objects**: one `data_ref` names exactly one underlying object yet may be simultaneously named by multiple physical views (`view fork` and `view export/import --as-fork` copy the same batch of `data_ref`, derived views/aliases share the base view's bytes; whereas `grovefs import` bringing the same source file into two views gives each an **independent key** — both `blocks/` entries are hardlinks to the source file, bytes still the same inode, zero extra usage, but the ledger counts per key separately). A view's internal reference count can't see other views, so taken alone it would mis-delete objects another view still references. Hence the booking lands in `<base>/meta/_control/refs.db` (PG backend a shared table `grovefs_refs.holders`): each view ATTACHes this ledger, the holder count and node add/delete go into the **same IMMEDIATE transaction**, atomically committed across files. The precise physical-deletion rule is "**the last name for this key within the view is removed, and the global ledger has no other view holding it**" — only then `backend.remove`. An out-of-band deletion of a view db (`rm meta/x/x.db`, a new `--home`) only makes the ledger **over-count**, objects linger and are never mis-deleted; both tiers have this command, all doing "read the tree, recompute, don't read the ledger": `grovefs refs --meta-dir D rebuild` (rewrites `meta/_control/refs.db`), `refs --meta-store postgres://… rebuild` (rewrites `grovefs_refs.holders`); the direction is always "linger bytes" rather than under-count, see §15.8. A key with no row in the ledger is that view's private (an empty ledger needs no initialization step).

### 5.4 Backup and migration

- **Metadata is not hardware-bound**: object location = `blocks/<two hex of FNV1a(key)%256>/<key>`, derived as a pure function of `data_ref`; a `node` row stores no absolute path/device number/hostname, and `ino` is GroveFS's own numbering. It works directly across machines/OSes, as long as metadata and bytes go together.
- **The list to migrate**: `blocks/` (bytes), `meta/<view>/<view>.db` (each view tree), `meta/_control/refs.db` (cross-view ledger, recomputable with `grovefs refs rebuild`), `meta/views/*.layout` (derived/alias/fork definitions), `meta/_control/api_keys.db` (needed only if keys were used). That's the SQLite-backend list; under the PG backend trees, ledger, definitions, keys, audit are all inside the DB and migrate with `pg_dump`/`pg_restore`, locally you need only move `blocks/` + env.
- **Whole-directory cold copy**: after stopping all mounts and the `meta-server`, `rsync -a -H` the whole storage root; **don't raw-copy a running SQLite** (WAL), if you can't stop use `sqlite3 <db> ".backup"` / `VACUUM INTO`.
- **Single-view export/import**: `grovefs view --meta-dir D export --view V --out F` → on the target after copying `blocks/`, `grovefs view --meta-dir D2 import --view V --in F`; preserves id/inode/hardlink/permissions/owner/symlink, atomic replace (a failure doesn't harm the old tree), `--as-fork B` restores fork semantics. Details in §3.4.2.
- **Topology boundary**: multi-disk `--data-dir` count and order must match (an equal-disk clone replacing in place doesn't affect location, only adding/removing/reordering remaps; or merge into a single root); cross-server striping `s<N>:` must keep N members; with `--member-id` the order is irrelevant but the set must not change, without it the order is also part of the layout (§5.2); a whole-directory copy carries the HA `epoch` over, the old primary must be stopped first; the PG backend uses `pg_dump`/logical replication.
- **Multiple base views**: any number of base views each their own `meta/<name>/<name>.db`, you may migrate just one, and keys may bind per view.
- **Under HA** the meta-server can additionally replicate via the backup API (see 5.5).

### 5.5 HA primary/standby (meta-server)

```bash
# 主:读写,并把快照周期推送给备
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110
# 备:只读服务,主心跳丢失后自动提升
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主(可选 RPO=0):加 --sync-repl,写返回前同步等备落盘 ACK
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110 --sync-repl
# 真 fencing(可选,opt-in):加 --witness <逗号分隔的 data-server 仲裁者列表,推荐奇数 N>,多数派授权
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --peer 10.0.0.6:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
```

- The two nodes must use **their own separate `--meta-dir`** (each keeps a copy of the views, replicated between nodes via snapshots).
- The roster's meta list may be a failover list — declare it once with `manager define --meta 10.0.0.5:50110,10.0.0.6:50111`; when the primary dies the client auto-switches to the standby to continue reads/writes (§4.2).
- **`--sync-repl` (optional, opt-in, default off)**: once enabled, each write op, before returning to the client, synchronously pushes the snapshot and waits for the standby's fsync + atomic-rename ACK, dropping **RPO from ~500ms to 0**. Cost: write latency adds one control-channel RTT + one `VACUUM INTO` fsync. When the standby is unreachable/times out it **honestly degrades to async and logs a warn** (that op's RPO falls back to REPL_INTERVAL, not falsely reported as 0). Proven by `.build-bin/test_ha.sh` segment 7 (RPO=0) / segment 8 (partition degradation).
- **`--witness` (optional, opt-in, default off)**: designates one or more `data-server`s (`host:port`, a comma-separated list, an odd N recommended; N=1 requires only 1 arbitrator) reachable by both primary and standby as an **independent arbitrator set**. Authority needs a **strict majority** `quorum_size(N)=N/2+1` arbitrators granting at once (the meta side fans out concurrently to the whole list and judges by majority `witness_quorum`; two majorities must intersect at ≥ one arbitrator, so an equal-value collision naturally leaves one primary). Each data-server maintains a monotonic `fence` token (`OP_FENCE` folds in via a `fetch_max` CAS and returns **the pre-fold value `prev`**): a claim succeeds iff `prev < my epoch` (I raised the token); `prev >= my epoch` means an equal or higher epoch already exists (the peer won, or an equal-value collision) → fenced. Using `prev` rather than the post-max is exactly to **distinguish an equal-value collision** — when arbitrators recover and both halves claim as candidate, the first wins, the second sees `prev == candidate` and is blocked, never dual-primary. The incumbent primary renews a **bounded lease (default 3s)** with the majority each tick (if a majority reports `prev > me` it self-demotes); each arbitrator **refuses overlapping leases** — at any instant at most one live lease, so **a live lease itself is proof of exclusive authority**, write requests are admitted directly by the primary's **local lease validity** (no per-write arbitrator RTT, only one per renewal, amortized across the window), a fenced stale primary is refused at the next revalidation after its lease expires; a would-be-promote standby is also **non-overlapping** — it must wait for the predecessor's lease to expire before it can seize (`prev >= candidate` collision / a higher peer, or the predecessor's lease still live → refuse/defer). The metadata-plane revalidation can't stop content bytes (`write()` hits the backend first, then a debounced meta touch), so **the data plane is gated too**: the client stamps the live epoch on every write-intent data frame (create/write/truncate/remove/fsync), a data-server that is a witness directly returns `EROFS` for frames with `epoch < token`, so a stale primary cannot silently write content bypassing the metadata plane (a blocked frame re-fetches the epoch from the authority and auto-retries once); and the data-server linearizes the epoch check and the disk write against the arbitrator claim (a claim takes an exclusive lock that drains in-flight writes, and witness and storage are the same node), so stale-write bytes cannot overlap with the new primary's writes — content tearing-write is truly closed on the arbitrator node (note: the data-plane gating applies only to data-servers inside the --witness list, i.e. those whose tokens the quorum raises; so deploy witnesses co-located with the content disks clients actually write to, content disks outside the list keep token 0 and their data-plane gate is inactive, leaving only the metadata-plane fence as backstop). **FAIL-SAFE**: once witnesses are configured but a **lease majority can't be reached and it exceeds the grace continuously (=PROMOTE_TIMEOUT 3s)**, a node can't prove exclusive authority and **gives up write permission** — the primary self-demotes to standby, the standby refuses to promote (**writes freeze**, sacrificing availability rather than risking dual-write). Each data-server persists its token independently to `<root>/.fence_token`, **a restart never zeroes the epoch**, letting a stale primary stay out. **The availability dividend over a single arbitrator**: downing **few** arbitrators still forms a majority and renews, not freezing writes; losing a few disks also doesn't revive a stale primary (the majority still keeps a monotonic token); only **losing the majority** fails-safe freezes. This bounds the dual-write window to **within the lease safety margin** (rather than the unbounded peer-only case): non-overlapping leases mean the peer can only take the token after our lease expires, and we stop trusting the cached lease ahead by the margin; a transient arbitrator error doesn't harm single-write, sustained unreachability is still backstopped by the grace (=PROMOTE_TIMEOUT 3s). Proven by `test_ha.sh` segment 9 (a stale primary restart blocked into standby by the witness + token persistence), segment 10 (kill one witness→primary self-demotes & standby refuses to blind-promote→restore witness→exactly one primary), segment 11 (3 arbitrators, down 1 still renews without freezing, down 2 breaks the majority before fail-safe).
- **Honest annotations**:
  - The default (async) **RPO ≈ 500ms** (the primary pushes a snapshot every ~500ms), on an extreme failure the last <500ms of writes may be lost; adding `--sync-repl` drops those writes' RPO to 0.
  - Fencing needs **arbitrators**: with strictly two nodes and **no `--witness`**, split-brain cannot be prevented (both halves cut off from each other each think they're primary → unbounded dual-write), this is a hard CAP/consensus boundary, needing a third-party arbitrator (a witness or a third node) to converge. **With `--witness` it's fail-safe**: when the arbitrator **majority** is unreachable, writes freeze rather than blind-write/blind-promote; the primary holds a non-overlapping bounded **lease** (arbitrators refuse overlapping leases, a majority live lease is proof of exclusivity), writes pass on local lease validation with one fan-out to the whole list per renewal, and content data frames are gated by token, bounding the dual-write window **within the lease safety margin** and guaranteeing convergence to a single primary. **The availability dividend of majority arbitration**: `--witness` takes a comma-separated list of N independent arbitrators (odd recommended), authority needs a majority `N/2+1` — **downing a few arbitrators still doesn't freeze writes** (only losing the majority stops writes), losing a few disks doesn't revive a stale primary, eliminating the single-point availability dependency of the witness over a single arbitrator (N=1 is a single arbitrator, the witness itself being the availability dependency: it dies→both primary and standby stop writing).

---

## 6. Security: admission (token / API key) and view access control

**A one-screen reference of the permission stack** (details expand per the in-row pointers):

```text
层  机制                       谁强制             粒度与语义                              详见
1  token(共享口令)            服务端握手         集群准入,不匹配 E_AUTH;uid 自报属 advisory   §6
2  --view 名                  服务端路由         树选择,不是身份:同 view 即同树               §6/§0
3  API key(--require-api-key) 服务端逐 op        逐调用方:scope 分平面(mcp|mount|any)、      §3.5.1/§6.1
                                                  --view 绑定拒开它树、--ro 拒写、
                                                  可撤销(活挂载 ≤1s 封停)、全量审计
4  DAC(--data-access-key)     data 面握手        数据面准入绑定:能力由真 meta 的              §6.1.1
                                                  M_OPEN 签发、data-server 校验,
                                                  挡住 rogue meta 复用合法 data-server
✗  显示治理(派生/别名/fork)  ——                 不是安全边界:只改呈现,可见即可读、          §3.2-§3.4.1
                                                  可读即可写回(fork 内容除外)
```

If a central server listens bare on the LAN, any host on the same segment can read/write objects by key / open another's view. Add a shared passphrase:

```bash
# 两个 server 用同一 token
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token s3cr3t
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token s3cr3t

# 客户端挂载带同一 token(同时覆盖数据面 OP_OPEN 与元数据面 M_OPEN)
$Y mount --view alice \
     --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key --token s3cr3t -m /mnt/alice
```

- token mismatch → permission denied directly at connect, no retry.
- empty token (default) = LAN-trusted, no auth, trusted intranet only.

**But `--token` is one shared secret**: any host holding it can mount, the audit can't tell "who mounted", and kicking one caller means rotating the token and cutting everyone. When you need **per-caller revocable credentials**, use the same `grovefs apikey` registry as the mount-plane gate (full mint/scope/audit/revoke usage in §3.5.1):

```bash
# 服务端:把 key 门与 token 叠加(两道都得过),或只上 key 门
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
# 客户端:呈递一个 `--for mount`(或 `--for any`)的 key
$Y mount --view alice --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key \
     --api-key '<id>:<secret>' -m /mnt/alice          # 或 export GROVEFS_API_KEY 不进 argv
```

The two key differences from token: ① **revocable per caller and closes live connections** — the server revalidates once per second (checked only with traffic), a revoked key is refused on that mount's first server round-trip after the revoke (E_AUTH/EACCES carrying the refusal verbatim), the mount prints the reason then **unmounts itself** (leaving no stale endpoint), while other keys' mounts are served as usual; ② **auditable** — every connect (incl. refused) lands in `apikey calls` (method=meta/data, tool=M_OPEN/OP_OPEN, detail records view and uid). Both gates are **entrance gates**; after adding the key's `--view`/`--ro` two access-control dimensions, "who may touch which tree, read-write or read-only" is carried by the credential itself and enforced per-op server-side.

### 6.1 Sharing a view and per-caller differentiated permissions (view-bound / read-only key)

**By default `--view` does not isolate uid**: any uid that does `mount --manager <mgr> --roster-key-file F --view alice` lands on alice's same authoritative tree, seeing each other's writes. That is the out-of-the-box "many people share one view" form — metadata central, content central, real-time invalidation push guarantees convergence.

> To distinguish roles within the same shared tree (e.g. a read-only reviewer, or restricting a caller to only touching a certain view), mint each role a **view-bound / read-only API key**: `grovefs apikey create --for mount --view V [--ro]` — revocable, attributable, `--view` lets the key open only that one physical view (opening another is refused with `E_AUTH` at `M_OPEN`), `--ro` makes the server refuse per write-intent op (details §3.5.1).

The overall logic flow (the isolation unit = the view name, not the client):

```text
客户端 A · uid=1000   mount --view alice ─┐
客户端 B · uid=2000   mount --view alice ─┤ 同一个 --view → 落进同一棵树；
客户端 C · uid=1000   mount --view bob   ─┘ 一个客户端也可同时挂多个 view（多对多）
        │
        ▼ TCP 帧 M_OPEN(view, token, uid, api_key)   （客户端从不开 db 文件 / 不持 PG 连接串）
┌─ meta-server（单一权威写者）—— 按序过门，再路由
│  ① token 校验     不匹配 → E_AUTH（集群准入，唯一确定性门；uid 自报属 advisory）
│  ①' api-key 门    服务端 --require-api-key 时：scope 非 mount/any、缺失/无效/已撤销 → E_AUTH（带拒因原文）
│                   key 若 --view 绑定：开其它视图同样在 M_OPEN 处 E_AUTH 拒
│                   活连接每秒复证一次（有流量才查），撤销即断开该挂载并使其自卸载
│  ② ro 写门        key 若 --ro：每个写意图 op 直接 E_NOTPERM（data 面内容写 EROFS），M_OPEN 回包告知只读挂载
│  ③ view 名 → 引擎缓存  首次 M_OPEN 打开已存在的树，之后同 view 命中同一实例
└──┬──────────────────────────────────
   ▼ view=alice                        ▼ view=bob
  alice 权威树（A、B 共享，写串行化）    bob 权威树（仅 C，与 alice 互不可见）
   ├─ SQLite（默认）：       一 view = 一棵独立树目录  <meta-dir>/meta/alice/alice.db · bob/bob.db
   └─ PostgreSQL（--meta-store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62
```

> The independent-storage unit in the database is "the view", not "each client": SQLite one `.db` file per view, PG one schema per view (neither is a single flat table); data bytes go through the data-server → stored into its `blocks/<key>` under `--data-dir`, the tree stores only the `data_ref` reference.

The uid a client reports defaults to `geteuid()`, overridable explicitly with `mount --run-as-uid N` — it is recorded as the entry's owner in lock arbitration and audit only, **advisory** (a TCP client can forge it), and **never an access-control gate**. Determinism rests on `--token`/`--require-api-key` (admission), "who may touch which tree, read-write or read-only" on the key's `--view`/`--ro` binding enforced per-op server-side. Two behavior boundaries you must know: ① **lock-class ops (`flock`/`F_SETLK`, incl. lock renewal) are classified as write-intent** (they write the central `locks` table), so a read-only key (`--ro`) has their requests refused uniformly (`E_NOTPERM`, landing `EPERM` on the client FUSE side, distinct from the HA standby's `E_ROFS` switch signal) — a read-only role that needs a shared lock for read coordination must use a read-write key; ② **revoke is op-driven** (the server revalidates once per second on traffic), a revoked/demoted key is refused on that mount's first server round-trip after the revoke, a read answered entirely by the kernel cache needs no round-trip and is thus not interrupted (closed on the next round-trip).

### 6.1.1 Data-plane admission binding: Data-Access Capability (stopping a rogue meta-server from reusing the honest data-server)

**Threat model**: the token is a static cluster passphrase shared by the data and metadata planes; `meta-server` is the ONLY place view isolation is enforced, while `data-server` is a flat KV (opaque keys, no view ownership, no server-side identity auth, cleartext TCP). So an attacker who holds a valid `--token` can stand up a **malicious meta-server** anywhere (its own authoritative tree, a forged ACL) while pointing clients at the **honest data-server** — the data plane only checks the token, not "whether this `OP_OPEN` passed a genuine meta `M_OPEN` admission", so a rogue meta can drive a client to read/write content bytes it should never touch. This hole is covered by NEITHER of the token/api-key **entrance gates**: they authenticate "may this caller in at all", but do not bind "the meta plane and the data plane are the same trusted server".

**Mechanism (Data-Access Capability, DAC)**: give both servers a **shared HMAC key K** (hex, `openssl rand -hex 32`); nothing is configured on `mount`:

```bash
# Both servers use the SAME K; the client mounts normally, the capability flows between the handshakes
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token s3cr3t --data-access-key "$K"
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token s3cr3t --data-access-key "$K"
$Y mount --view alice --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key --token s3cr3t -m /mnt/alice
```

Flow: every time `meta-server` accepts an `M_OPEN`, it signs a capability string with K (`<expiry>.<hex(HMAC-SHA256_K(expiry))>`) into the tail of the ack; the client (`RemoteMeta`) stores it, `mount` forwards it to the data plane, and the client (`RemoteBackend`) replays it on every `OP_OPEN` to `data-server`; a `data-server` with `--data-access-key` verifies it under the same K and refuses an `OP_OPEN` without a valid capability with `EACCES`. A **rogue meta does not have K**, so it cannot sign a valid capability — its mounts, even holding the correct token, cannot open the honest data-server. That is the link that gets closed.

**Lifetime**: `meta-server --data-access-ttl SECS` (default `0`) sets the capability's expiry. `0` mints a **non-expiring** capability so a long-lived mount is not cut off mid-session across a data-server restart/reconnect (default, best compatibility); a nonzero TTL stamps `now+TTL` on each `M_OPEN`, tightening the replay window at the cost of a remount once it lapses — for stricter deployments.

**Default off = no capability field on the wire**: with no `--data-access-key` on either server, the ack carries no capability and `OP_OPEN` neither sends nor checks the field (endpoints ignore the tail field, so capable and incapable peers interoperate). Enabling one side while the other's key differs fails closed and refuses; a malformed hex makes the server refuse to start.

> ⚠ **Honest boundary**: DAC is **proof-of-admission through a genuine meta** (a meta↔data channel binding), **NOT a per-view data ACL**. It binds "this data access came from a legitimate meta admission"; the capability string **does not encode the view** — the data plane is a flat KV, an `OP_OPEN` key carries no view ownership, so a data-server cannot judge by view. It stops "a rogue meta arming a client against the honest data-server", but it does **not** replace the §6.1 `--view`-bound key for "this caller may only touch this view" fine-grained isolation — that still rests on the meta plane's `--require-api-key` + `--view`/`--ro`. True per-view content isolation still needs separate views + separate data storage roots. See `grovefs::remote::dac` for the source and verdict semantics, and `tests/meta_roundtrip.rs::genuine_meta_arms_data_but_rogue_meta_does_not` for the end-to-end proof.

### 6.2 PostgreSQL metadata backend (optional)

`meta-server` stores the authoritative view tree in embedded SQLite by default (one `<meta-dir>/meta/<view>/<view>.db` per view); you can switch it to PostgreSQL:

```bash
$Y meta-server --meta-store postgres://user:pw@10.0.0.9:5432/grovefs --listen 0.0.0.0:50110   # under PG no --meta-dir is needed: trees and control plane live in the database
```

The client mount command only swaps its bootstrap (`--manager host:port --roster-key-file F`), the wire protocol unchanged. Internally the server maps each view to a `grovefs_v_<hex(view)>` schema (hex guarantees identifier safety, injection-agnostic), the root node fixed at id=1, `ROOT_INO` consistent with SQLite behavior. Table creation is idempotent `CREATE ... IF NOT EXISTS`, no pre-migration script needed. Ops needing multi-statement atomicity (rename/unlink/lock) go through explicit transactions.

**Notes**:
- **Incompatible with the in-house HA**: `--meta-store postgres://…` + any `--role standby` / `--peer` / `--witness` fail-fast at startup (the `VACUUM INTO` snapshot mechanism applies only to SQLite). Leave PG high availability to **PostgreSQL's own streaming replication / logical replication / PITR** — stronger than file snapshots anyway.
- **Deleting a view is one command**: `view --meta-store postgres://… rm --name V --base --data-dir R` decrements `grovefs_refs.holders` per key (DELETE at zero) then `DROP SCHEMA … CASCADE` in **one transaction**, reclaims unheld `blocks/` objects after commit; `refs --meta-store … rebuild` recomputes the whole ledger over the surviving trees when quiesced. PG's DDL is transactional, so this tier has none of the SQLite tier's "delete the db → rebuild the ledger" intermediate state (§15.8).
- **Idle views auto-release connections (two layers, each with its own job)**: **layer one reclaims connections and keeps the engine** — every mounted view keeps a resident PG engine pool in the meta-server, shaped as a **warm floor + retention ceiling**: `--pg-write-pool`/`--pg-read-pool` (default 8 / 4, env `GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL`) are the floor, pre-filled at engine open and **never reclaimed**; `--pg-write-pool-max`/`--pg-read-pool-max` (default = floor) are the ceiling — on burst an empty pool opens connections on demand, and on return keeps at most the ceiling, closing the excess. After a view is silent for `--pg-pool-idle-secs` (default 60s, `0` disables), the reaper thread peels above-floor connections from the **least-recently-used end**, stopping at the first connection inside the window (hysteresis — it never takes one just used). **Layer two reclaims the whole engine** — `meta-server --engine-idle-secs N` (default 300, `0` disables) reclaims the engine after the view's last mount session ends and N seconds of silence, closing all connections; the next access auto-reopens, transparent to clients (`evict` suite: 6 views 76 connections → 4 after unmount reclaim). The division of labor is exactly why layer one exists: the mount-session count **pins the engine**, so a "mounted but nobody reads/writes" view can never be taken by layer two — with layer one it only holds the floor.
- **Connection planning formula**: `max_connections ≥ active physical trees × (write floor + read floor) + retained burst + ops connections`. At the default tier floor = ceiling (8 write + 4 read), one active view always holds 12 connections (`test_pg.sh` section 9 pins this with the "undeclared view still 12" assertion). Lower the floor, keep the ceiling — measured on the same rig (`.build-bin/probe_pool_floor.sh`, 2–3 rounds per config; connection counts from `pg_stat_activity` counting only the probe's own fresh database minus baseline, so other suites' connections never mix in):

  | Config | Write/read floor:ceiling | create-heavy (4 mounts × 8 threads, 8s) | Burst peak connections | Retained after burst | After the window | Steady 1 mount × 2 threads |
  |---|---|---|---|---|---|---|
  | `default` (no flags) | 8:8 / 4:4 | 3,842 / 4,000 ops/s | 30–31 | 12 | 12 | idle 12 |
  | `elastic` | 2:8 / 1:4 | 3,991 / 3,827 ops/s | 31–33 | 12 | **3** | idle **3** |
  | `tight` (floor = ceiling, shrinking the pool directly) | 2:2 / 1:1 | 2,064 / 2,056 ops/s (**−47%**) | 13 | 3 | 3 | idle 3 |

  The `elastic` vs `default` throughput gap is within round-to-round jitter (the two rounds trade places), i.e. **dropping an idle view from 12 connections to 3 has no measurable throughput cost**; `tight` with the same 3-connection floor loses nearly half, because every burst overflow re-pays connect + SERIALIZABLE pragma + prepared re-parse — that is the net win of floor/ceiling separation over shrinking the pool. The steady-state three tiers plus a read-floor-only control (`r4`: 2:8 / 4:4, idle 6) sit within noise of each other on client `remeta.rpc` quantiles (three rounds p50 273–324µs, p99 1,318–1,580µs, ops gaps <10% with unstable direction): **no evidence that reclaiming the read floor hurts tail latency**, and none that adding the 4 read connections back buys anything. Honest boundaries: ① the ceiling is not a hard concurrency gate — burst peaks measured 31–33 connections, far above the ceiling of 12, because 32 in-flight write threads open on demand when the pool is empty, so plan capacity as "floor + burst concurrency"; ② the per-view window's actual cadence is bound by the process sweep (wakes every `min(engine_idle/2, idle/2)` seconds, clamped to 1–60s), so the default tier's worst case is a 60s window + 30s cadence ≈ 90s before reclaim; ③ the table uses a 6s window so the probe can wait it out; the production default is 60s.
- **Per-view override: `grovefs view pool`** (changes one view, not the whole process):

  ```bash
  grovefs view --meta-store postgres://… pool --view lab --write 1:4 --read 1:2 --idle-secs 5
  view 'lab' pool bounds: write 1:4  read 1:2  reap-after 5s
    takes effect on the view's NEXT engine open: unmount it, or wait out --engine-idle-secs — a live mount keeps the pool it already holds (…)
  grovefs view --meta-store postgres://… pool --view lab          # bare run = inspect
  grovefs view --meta-store postgres://… pool --view lab --inherit  # drop the override, follow the process again
  ```

  These five declarations live in the **view's own schema `ctrl` table** (`pg_write_warm`/`pg_write_max`/`pg_read_warm`/`pg_read_max`/`pg_pool_idle_secs`), so they travel with `view move`, add no tables, never disturb the zero-query path of a single-instance deployment, and write no credentials into the database. `--write N:M` with `M < N` is refused outright (`error: ceiling 2 is below floor 8 — the floor is always retained, raise the ceiling or drop it`), and a view name never created is refused too — **a typo never mints a schema on the side**. Effect lands on the view's next engine open: `meta-server` reads the view's declarations first and pre-fills to the view's own floor (it never fills the process floor first and waits for the reaper); unreadable or malformed declarations silently fall back to the process defaults (fail-safe). Fork views are this command's main user: content writes are stopped by `EROFS` (§3.4.1), a write floor of 1–2 suffices, while `view fork`/`import` bursts can still climb to the ceiling.
- **The write path also caches prepared statements**: the read path caches ≤16 per connection (§8 above), the write path is on par — each write connection caches ≤32 (`WRITE_STMT_CACHE_CAP`, `INSERT`/by-name `lookup`/`DELETE`/`SET ino`/four `setattr`/`rename`/lock ops/three ledger statements). `tokio-postgres`/`postgres` 0.19 has **no** `prepare_cached`, so the cache is hand-built: `WriteConn` holds a `HashMap<String, Statement>`, `WriteTx` shadows `Transaction::query/execute` with inherent methods, so **SQL inside the transaction body, unchanged to a character**, hits the cache; a prepared statement survives `COMMIT`/`ROLLBACK` (measured), only DDL changing the result type triggers `0A000`, when the entry is evicted and one unnamed fallback taken. Measured (`bench_pg_conc.sh`, N∈{1,4,8}, T=8, 6s, default `sync_commit=on`, two rounds): FUSE-rig write throughput **3,048–3,118 / 4,784–5,202 / 4,518–4,680 ops/s**; the cache is built into the code (`WRITE_STMT_CACHE_CAP`, no switch), the control group is the same rig, same op sequence, readings without the cache build **2,556–2,577 / 4,131–4,200 / 3,350–3,497**, single-connection one transaction 577–670µs → 316–330µs, i.e. N=1 +18–21%, N=4 +15–24%, N=8 **+30–36%**, stacking `GROVEFS_PG_SYNC_COMMIT=off` is stably beneficial only at N=4 (5,225 → 6,142, +18%; N=1/N=8 within jitter, see §8); contention cases 180/180 single winner, `test_pg.sh` 77/77.
- **Multiple PG instances: instance registry + view placement (solving the connection ceiling)**: the PG tier holds 12 connections per active view by default (8 write + 4 read; with lowered floors count "write floor + read floor", see the two bullets above), so the number of simultaneously live views on one PG cluster is capped by `max_connections`; the scale-out answer is **putting different views on different databases** (a view's tree always lands on exactly one instance, never replicated), capacity growing linearly with instances. Two `--meta-store` shapes: an inline comma list `postgres://…/db1,postgres://…/db2` (auto-named `pg1..pgN`, names stable only while the list order holds — for small sets that will not grow) or a **`file:` instance registry** (recommended):

  ```text
  # /etc/grovefs/instances.conf  — one `name = postgres://…` per line, `#` comments; first entry = control plane
  main  = postgres://grove:pw@10.0.0.1:5433/grovefs
  team1 = postgres://grove:pw@10.0.0.2:5433/grovefs
  ```

  The registry keeps credentials in the file (not in argv, not in the journal, not in the database — the placement table stores **names** only) and gives each instance a stable name for placement rows to reference. An unreadable registry = fail-fast refusal to start, never a silent downgrade; the meta-server startup banner prints `N PG instance(s) [main, team1] — control plane on 'main'` (names only, no URIs).

  **Placement**: `view create` automatically picks the instance with the **fewest placements** (ties take registry order); `view create --pg-instance team1` pins explicitly (the console's new-base-view form makes the same choice as a dropdown, §3.5.3; `view move` is the other entry point that rewrites a placement row, see below); fork follows its base view, `import` auto-places, `view rm` forgets the placement row. The placement table `grovefs_control.view_placement` lives on the control-plane instance; `write`/`delete` serialize under the same advisory lock as control-plane DDL. **How a mount finds the right database**: the client only ever connects to the meta-server and never sees instances — at `M_OPEN` the meta-server resolves the view to an instance through the placement table, and the **server** fans PG connections out to the instances (so the feared "all load through one service" is only metadata RPC load, inherently the light path; PG connections are not relayed through it — it routes for you). View→instance resolution order: placement row → **scale-out self-heal** (no row but the schema sits on the control-plane instance = a view from before a single instance grew into a registry, pinned back once) → FNV-1a hash fallback for old hash-sharded deployments (see `probe_pg_shard.sh`); an unreachable control plane means **refuse, not guess** — guessing wrong plants a schema nobody can find again. A single URI passes through unchanged; a one-instance deployment pays zero extra queries.

  Semantic invariants: `view list` merges across instances; `refs --meta-store … rebuild` **recomputes per instance and sums** (a multi-URI is not refused); server-side `gc` (`gc.scan`/`gc.apply`) **scans the cross-instance union** — every instance's ledger and trees are counted before an orphan is declared; fork/graft source and target must sit on the same instance (one `INSERT … SELECT` cannot cross databases), different instances are refused naming both landing spots rather than connecting to the wrong server. Adding a machine to the instance set **does not move** already-placed views (placement rows rule); it only affects the balancing of new views.

  Boundaries (deliberately so): ① **one instance = one independent "metadata + content" domain** — `grovefs_refs.holders` has one ledger per instance (SQL transactions cannot cross databases), but fork/graft refusing cross-instance pairs guarantees **the ledger is complete for every sharing it can see** (same transaction as the tree write), so each instance must have its own data-server; ② capacity formula: `Σ instances (max_connections − ops headroom) / (write floor + read floor)` (default tier `/ 12`) simultaneously live views — **a second database on the same cluster buys no capacity** (`max_connections` is per cluster process); real scale-out is separate PG processes/hosts; ③ the "base view deletes a file and references dangle" problem does not exist (derived/alias/fork place with their base on the same instance; cross-instance refusal comes first); ④ **high availability is per-instance**: streaming replication / PITR configured independently per instance, GroveFS provides no cross-instance failover; the control-plane instance additionally carries the placement table and control-plane tables, and moving it is a manual operation (`pg_dump` the tables + edit the registry's first line, see docs.html §24.7).

  **Relocation: moving an existing view to another instance (`view move`)** — placement is chosen automatically only at creation; after the fact, use this command:

  ```bash
  $Y view --meta-store file:/etc/grovefs/instances.conf move --view V --pg-instance team1
  $Y view --meta-store file:/etc/grovefs/instances.conf move --view V --pg-instance team1 --keep-source   # keep the source as a rollback point
  ```

  Four steps (snapshot → build → flip → retire): ① a RepeatableRead snapshot export of the source schema (no tearing even while mounts keep writing); ② build the replica on the target instance and self-verify against the dump header's row count (**placement untouched** — a failure leaves no observable change); ③ rewrite the placement row, the **only atomic switch point** (same advisory lock as control-plane DDL; a failure here `DROP SCHEMA CASCADE`s the fresh replica, retry-safe); ④ retire the source by default (schema + its ledger holdings, one transaction) — unless `--keep-source`. **Metadata only** end to end: content bytes stay in their respective data domains (gc scans the cross-instance union, so source-domain bytes are never declared orphans); mounted clients are unaffected, the next `M_OPEN` routes to the new home.

  Rollback path: the source schema left by `--keep-source` IS the rollback point — `move` the same view back to the original instance. A reverse move that finds a same-named schema on the target **with matching row counts** skips the build and flips the row directly (idempotent reuse), so a rollback copies no data and touches one placement row. To clean the leftover source once stable: run move again without `--keep-source`, or `rm` it directly (no placement row points at the source schema anymore, so no double-live).

  Guards and boundaries: refused while the view is mounted (unmount first; mounts on other machines must be quiesced by you); must run on the storage host (refused with `--control-server`); refused on the SQLite tier (each view already has exactly one home); alias/derived refused (they own no tree); **a fork can move, but its base does not follow** — after the move the fork and its base sit on two instances, and a later `graft` of that pair is refused cross-instance (a corollary of boundary ① above; think before moving).

  The full scale-out flow: ① provision the new PG instance's database and role; ② add one line to the registry file; ③ restart the meta-server (the banner reports the new instance list); ④ **new views** balance toward the new machine from then on, **old views stay put** (placement rows / self-heal pins rule); ⑤ to offload a hot view, use `view move` above. Adding machines to the instance set never moves placed views; capacity grows only with new views.

  Evidence: the hash-routing mechanism (`.build-bin/probe_pg_shard.sh`, two databases 8 views: schema "one and only one", the same meta-server serving both sides, no cross-shard ledger bleed); registry + placement + relocation (`test_pg.sh` section 8, 55 assertions: banner reports two instances, auto-placement lands on main, `--pg-instance team` lands on the team database, cross-instance mounts read/write through the **same** meta-server, rm clears schema and placement row together, unknown instance names refused; move's four steps measured — main→team switch with the source schema gone, already-lives refused naming the state, move-back succeeds, `--keep-source` leaves schema=1, rollback reuses the replica and only flips the row, post-move mounts write and read fully); 13 pure-logic unit tests on parsing/placement (`shard_tests`: registry parsing with error line numbers, single-instance passthrough, control plane pinned to the first entry, unreachable control plane refused, auto-naming, backend verdict).
- Why not offer Redis: K/V+TTL is not isomorphic to the view tree (transactions/row locks/multi-column indexes are all naturally needed, forcing it would just rebuild a weaker PG on the same instance).
- Scenario-by-scenario runnable commands (verifying on one host / production systemd / inspection / backup-restore / delete-view / capability boundary) in **§15.3–§15.9**.

---

## 7. Read-only mount

```bash
$Y mount --view alice --mountpoint /mnt/alice_ro --manager 127.0.0.1:50055 --roster-key-file ~/roster.key --ro
```
Every write operation (create/write/mkdir/unlink/rename/chmod/truncate, etc.) returns `EROFS` (a double guard of kernel `MS_RDONLY` plus a filesystem-side check). Suited to distribution/archive views.

### 7.1 Two boundaries that "look POSIX but aren't" (verified empirically)

- **Permission bits are stored and displayed but not enforced**: `chmod`/`ls -l` faithfully record and echo `node.mode`, but mount admission **does not look at the in-tree mode** — the same uid appending to a `chmod 444` file still returns `rc=0` with the bytes hitting disk (rereading after `echo more >>` includes the new line; measured on a cluster mount — a `meta-server` + `data-server` pair on this host's loopback). The real access control is: server-side token/API key (§6), `--ro` mounts (previous section), and mount visibility decided by whether `allow_other` is set; the file permissions on `blocks/` constrain only **processes that can reach the storage machine or the data-server directly**, never a client that already holds a mount. **Don't treat `chmod` as an isolation mechanism inside a view**; cross-uid reads being refused is FUSE mount visibility (without `allow_other` other users can't see the mountpoint at all), not the mode taking effect.
- **Sparse holes don't save space**: the backend stores bytes as a full object, so `truncate -s 1M` extending a hole actually occupies the full 1 MiB (`stat` echoes `size=1048576 blocks=2048`, i.e. `st_blocks*512 == st_size`, no hole). Don't expect sparse image files (qcow2, etc.) to use less network/disk; to save space, compress or transpose the format first, then import.

Both are current-version design trade-offs (the object-storage model doesn't maintain extents, and admission doesn't go through the POSIX mode-check path); they're listed as standing observations (§5-1/§5-2) and don't affect data integrity.

---

## 8. Performance knobs

- `--threads N` (default 4): FUSE concurrent-dispatch worker threads, each with its own `/dev/fuse` fd (`clone_fd`, Linux 4.5+). `0` = single-thread mode (usable as an A/B baseline). On kernels below 4.5 it automatically falls back to a single worker thread (no hard failure).
- Large client IO is auto-chunked and parallelized: reads ≥1MiB are sliced into 1MiB chunks and run concurrently, writes ≥4MiB are sharded and run concurrently (no protocol change).
- **Small-file creation: one mount = one RPC = one server-side commit**: `create` calls in the same parent directory are serialized by the kernel's parent-inode lock, so "multithreaded file creation" can only parallelize across **different parent directories** (`cp -r`/`tar -x` unfolding a multi-level subtree is exactly this shape, and it's the only shape with a parallel payoff). **There is no client-side group commit** — a mount holds no authoritative tree, so it cannot fold N requests into one transaction; batching would only spread one RPC's latency over N operations without reducing the commit count. `MetaEngine::create_nodes_batch`/`unlink_nodes_batch` (an engine-level capability: the caller hands the whole batch to the engine, which pays one ledger lock and one `BEGIN IMMEDIATE` for the group - a batch of one is just a single call; a duplicate name or a "directory not empty" inside a batch voids only that one entry, not its siblings) is therefore an **engine-level API** for callers embedding the library, and the engine-level microbenchmark (same transaction, same batch of statements, N=1200) reads: a single create **83µs/row**, group of 3 **38µs/row**, group of 8 **25µs/row**. On that same path another fixed cost was long ago removed: the CREATE/UNLINK/ledger SQL now lives on a resident connection (`prepare_cached`) instead of being parsed per row. One observation unrelated to batching: a mount with `WRITEBACK_CACHE` backfills size and mtime at close, and when both equal the value already in the DB (same length, same epoch second) it produces **no write transaction** — `mtime` has only epoch-**second** precision, so rewriting the same value within the second only amplifies. The cluster-tier concurrency curves are the two measurements below.
- **Parallel deletion**: same basis as creation — no batching on the mount, the win comes from **commits overlapping on the server**: SQLite writes still serialize on the same authoritative connection, but WAL lets the concurrent commits' fsyncs overlap, and no op has to wait for someone else's transaction either; byte reclamation (`backend.remove`) already happens inside the data-server's process. Same mountpoint, `--threads 16`, one parent dir per thread, 400 files each (python driver, `.build-bin/probe_unlink_cluster.sh`): T=1 **2,082** / T=2 **4,270** / T=4 **5,062** / T=8 **5,120** / T=16 **5,767** files/s; single-threaded `rm` of 2000 small files **3,013 files/s** (`meta.unlink` p50 **101µs**, mean 126µs). The two numbers come from different driver shapes (fresh parent dirs, batch length, in-process loop), so only the slope within a tier is comparable. Batch transactions remain valid **as an engine capability**: deleting N rows in one transaction (N=2400) at batch 1/2/4/8/16 = **53.9 / 30.0 / 18.7 / 13.2 / 10.8 µs/row**.
- **Where one `create` actually goes (SQLite tier, broken down by `--stats`)**: CLUSTER tier (a `meta-server` + `data-server` pair on this host's loopback), N=2000, three arms on the same fresh view, and `--stats` samples on the **client**, so these are end-to-end round trips: **a name-only file create costs 0.281ms/op** (3,560 ops/s, `meta.create` p50 127µs, `remeta.rpc` p50 86µs), **`mkdir` 0.211ms** (4,741 ops/s, p50 117µs), **`utime` 0.391ms** (2,561 ops/s, `meta.setattr` p50 109µs). The first two pay two RPCs per op (`M_CREATE`'s reply inlines the row it just committed, saving a follow-up `M_GETATTR`), `utime` pays four (`lookup` + two `getattr` + `M_SETTIMES`) — so the `mkdir → utime` gap is mostly **round-trip count**, and `file − mkdir ≈ 70µs` is the server-side work of naming the data_ref plus the cross-view ledger entry. **`create` doesn't touch the data plane**: the name lands in the DB and returns; the backend object is opened on first `write`/`truncate` (their respective `O_CREAT`), so "create only, no write" (`touch`, an empty file tree `cp -r`) neither pays an open/close nor occupies a cache fd. The semantic invariants are unchanged: a never-written file's `getattr`/`read` are both 0 bytes (a nonexistent object **is** an empty file), `fsync` succeeds, `unlink` only deletes the name (nothing in the ledger to reclaim), `du` counts 0 bytes, `gc` has no orphan to clean. The failure surface is therefore cleaner too: when `data-server` is unreachable `touch` still succeeds (the name only crosses the metadata plane), and the first op that actually crosses the data plane reports the error — `fsync` gets `EIO` (the `resilience` suite asserts both halves; a write without `fsync` already sits in the page cache, so the application side can't see this failure anyway). The "create only, no write" concurrency curve (same mountpoint, one parent dir per thread, `--threads 16`, files/s): T=1 **3,201** / T=2 **6,329** / T=4 **8,683** / T=8 **4,846** / T=16 **4,640** — up to T=4 the gain is real commit overlap, from T=8 on the single writer connection and the RPC thread queue cancel it out; raising the server's warm READ floor to 8 does not change the shape (the same arm reads **3,213 / 6,344 / 4,864 / 6,101 / 4,337**), because the bottleneck is on the write side, not in the read pool. **SQL statements aren't the big cost; commits and fsync on the volume are**: same volume, same pragma, raw SQLite doing "one INSERT + one ledger INSERT + one COMMIT" (`.build-bin/probe_wal_modes.sh`, N=500 per tier, `/data` is xfs, libsqlite3 3.31.1) — `WAL+NORMAL` per commit **p50 46µs / mean 404µs** (no fsync in the median; the mean and `meta.create`'s occasional 59ms tail spike are both contributed by the periodic checkpoint's fsync, 2,470 commits/s), `synchronous=OFF` **p50 30µs / mean 55µs** (18,159/s), `synchronous=FULL` **p50 16.7ms** (fsync every commit, 53/s), `journal_mode=DELETE+NORMAL` **p50 20.5ms** (37/s); the same code on tmpfs collapses entirely to **~26µs** (37k/s) — fsync is that order-of-magnitude gap. So this tier **provides no durability-lowering knob like `synchronous=OFF`**: it can be 3–7× faster, but the price is "a commit may still roll back after returning" rather than "lose only the most recent few commits", inconsistent with the durability standard of the other tiers in this section. To go faster, change the medium or the backend (PG write parallelism, see §6.2), not the durability. Both scripts are rerunnable: `.build-bin/probe_sqlite_create_breakdown.sh`, `.build-bin/probe_wal_modes.sh`.
- **Metadata read parallelism** (SQLite writes don't parallelize; PG write parallelism, see §6.2): besides the single authoritative write connection, each view's `MetaEngine` holds a pool of warm read connections — **1 warm by default** (`READ_POOL_SIZE = 1`, one idle read connection costs about 3 fds) with **bursts retained up to 8** (`READ_POOL_RETAIN = 8`; the measured gain flattens after roughly 8 concurrent readers). The `GROVEFS_SQLITE_READ_POOL` environment variable raises the **warm floor**, the retention ceiling takes the larger of the two, and `0` closes the pool entirely; when the pool is empty a connection is opened on demand, returned after use, and closed once the ceiling is exceeded. This pool lives on the **server-side storage engine** (and on callers embedding the library in their own process) — there is no engine inside a mount process, so `meta-server`'s concurrent RPC handlers each use their own backend's pool and never wait on each other. `lookup`/`getattr`/`readdir` and nlink resolution go through the pool, so concurrent reads on the same view needn't queue behind the write lock; SQLite **writes still serialize on the same authoritative connection** (single-writer semantics unchanged), PG writes take a separate SERIALIZABLE write pool (§6.2). Pool connections read committed data under a WAL read transaction, and their own writes are immediately visible (`cargo test`'s `reads_through_the_pool_observe_committed_writes` pins this). Engines opened on HA standbys/read-only don't pre-open a pool; they establish on first read on demand and return it, equally bounded. The PG engine (`PgEngine`) likewise holds a peer `PG_READ_POOL_SIZE = 4` read-only **warm floor** (`GROVEFS_PG_READ_POOL`; `GROVEFS_PG_READ_POOL_MAX` only sets the burst retention ceiling, defaulting to the floor — §6.2), reads committed data via PG transactions, falling back to the write connection when no read connection is available so failures surface truthfully, and each read connection caches its own prepared statements (≤ 16 per connection: lookup/getattr/two readdir pages/lock probe/epoch read), no longer sending an unnamed statement on every read for PG to re-parse and re-plan — measured on the same mountpoint under `--no-cache`, consecutive `stat` runs **0.358–0.360 ms/op** (≈2.79k/s, `.build-bin/probe_pg_read.sh`); the PG write side is separately parallelized by a `PG_WRITE_POOL_SIZE = 8` SERIALIZABLE write pool (§6.2).
- **PG commit durability tier** (`GROVEFS_PG_SYNC_COMMIT=off`, default `on`): the PG write connection runs `SET synchronous_commit = off` at session level, so COMMIT no longer waits for local WAL to hit disk when it returns. Same-rack comparison (`bench_pg_conc.sh`, N∈{1,4,8}, T=8, 6s, write-statement cache already in place): **3,305 / 5,225 / 4,284 → 3,194 / 6,142 / 4,180 ops/s** — the only stable gain is **+18% at N=4 (write pool saturated)**, while N=1/N=8 fall within single-run jitter (−3% / −2%; an earlier round showed the same pattern with N=4 winning +29% alone). The price is that **a crash may lose the most recent few committed transactions**, and with `synchronous_standby_names` configured it also stops waiting for standby acknowledgment — deployments promising RPO=0 must keep the default `on`. SSI isolation, the cross-view ledger, governance invariants, and `--sync-repl` snapshot forwarding are all unaffected (the last is mutually exclusive with the PG backend anyway); under the default tier `test_pg.sh` passes 77/77, and under `GROVEFS_PG_SYNC_COMMIT=off` too, 77/77.
- **Directory listing pagination + READDIRPLUS** (in effect without any switch): `MetaProvider::readdir_page(parent, offset, limit)` is pushed down into a SQL page in both the SQLite and PG engines, and the FUSE callback fills the reply buffer in geometric blocks (READDIRPLUS 32→1024, plain READDIR 128→1024), so one kernel READDIR/READDIRPLUS request pays `O(buffer)` rather than `O(whole directory)`; attributes are carried back once alongside the directory entries (the `FUSE_DO_READDIRPLUS` of kernel ≥4.6, a separate all-or-nothing negotiation group from the capability bits above; unsupported kernels naturally fall back to plain READDIR). Three key implementation points: ① **keyset resumption** — the kernel resumes at "where the reply buffer fills", and the offset doesn't line up with our page boundary, so every 32 rows a cursor `(parent, n) -> name of the (n-1)th row` is recorded: any resumption first locates with `WHERE parent_id=? AND name>? ORDER BY name LIMIT n`, then discards in-memory any prefix shorter than 32 rows; OFFSET is only a fallback when there's no cursor (seekdir, cursor cleared), and any name change (create/unlink/rename/link) immediately clears the cursor, never returning the old order. ② **query plan pinned** — every listing SQL carries an always-true `name>''` (a name can never be empty); its only job is to pin the plan onto `UNIQUE(parent_id,name)`, otherwise SQLite falls back to the `(parent_id,is_dir)` prefix index + a temp B-tree sort, 13–55ms per OFFSET page. ③ **directory nlink via covering index** — `getattr` and READDIRPLUS's `.`/`..` must compute a directory's nlink (`2 + subdirectory count`), done via a pure index scan on `idx_node_parent_kind(parent_id, is_dir)`, 100k sub-items in **0.02ms** (per-row table lookup would be 9.6ms each, and it's computed for every reply). Measured (CLUSTER tier — a `meta-server` + `data-server` pair on this host's loopback, same mountpoint, a fresh mount per pass, an empty kernel dentry cache, `.build-bin/probe_readdir_cluster.sh`): a 5001-entry directory, `ls -l` **0.270–0.274s** (`--no-readdirplus`, leaving only pagination, **0.575–0.629s**, the two listings byte-identical after sorting); a 10,002-entry directory, `ls -f` **0.116s**, `ls -l` **0.547s**; a 100,002-entry directory, `ls -f` **0.925s**, the same directory through a fork view **1.156s**, through a 100-rule lens **1.612s** (a reread inside the TTL **0.429s**).
- **Scope** (same basis as the previous item): both the read-only connection pool and SQL pagination live on the **server-side storage engine** — the SQLite `MetaEngine` (1 warm read connection, bursts retained up to 8), the PG `PgEngine` (a 4-connection read-only warm floor, each caching its own prepared statements) and the treedb engine (using its own offset/limit for readdir); both floors are widen-able with `GROVEFS_SQLITE_READ_POOL` / `GROVEFS_PG_READ_POOL`. On the client side what arrives is the `M_READDIRP` frame being pushed down: `offset/limit` go to the server, the server runs that page on its own read pool, and the client takes one page per kernel buffer and negotiates PLUS. Whether a store can page at all is told to the client by the 2nd byte of the `M_OPEN` ack (the live engine's `supports_paged_readdir()`) — the client never guesses. The `LayoutMeta` of `--layout`/derived views also overrides `readdir_page`: it **materializes the "rule-pruned shape" per parent directory** (up to 4 directories / 400k rows, TTL 2s), cutting one slice per kernel buffer — so 100k + 100 rules takes 1.612s first, 0.429s reread inside the TTL; large directories over 400k rows are still computed per buffer (correct, just not cached). Materialization IS a cache, so it **must be flushed by the invalidation push through `flush_listing`**, otherwise freshness would be squashed back to the TTL (the `disp_fresh` suite is the gate on exactly that contract). Metadata **write** parallelism exists only on the PG backend (a SERIALIZABLE write pool within a single cluster, see §6.2); SQLite writes still serialize; multi-authority across nodes isn't done, while **multi-PG-instance placement** under one authority is a supported surface (`--meta-store file:registry`, different views landing on different instances; boundaries in §6.2). The criteria are pinned by `cargo test`'s `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` and `readdir_page_resumes_at_arbitrary_offsets_and_drops_stale_checkpoints`.
- `--no-readdirplus`: closes the READDIRPLUS negotiation, forcing a fallback to plain READDIR + per-entry `LOOKUP`, for A/B benchmark troubleshooting (this is already the behavior when the kernel doesn't recognize the capability; the switch only forces it).
- Extra fuser options such as `--o allow_other` are passed through to the mount.

### 8.1 Per-stage latency telemetry (opt-in, for bottleneck hunting)

Adding `--stats` at mount time enables **per-stage latency telemetry**: it records the time spent in each stage of a read/write/metadata operation into separate HDR histograms, aggregating p50/p90/p99/max offline to **locate which stage the bottleneck is in** (key resolution vs. data backend vs. remote TCP round-trip vs. stripe fan-out vs. SQLite/PG query vs. SSI retry…), instead of only seeing one total wall clock. This is **a white-box diagnostic facility, not a throughput/scalability claim** — when off, each instrumentation point is skipped by a single relaxed atomic read (no clock, no lock, no allocation), with no measurable effect on the hot path.

```bash
# 卸载时把汇总报告打到 stderr(挂载进程 stderr)
$Y mount --view alice -m /mnt/alice --manager 127.0.0.1:50055 --roster-key-file ~/roster.key --stats
# 额外周期性 + 退出时原子落盘到文件(CI/长时间采样)
$Y mount --view alice -m /mnt/alice --manager 127.0.0.1:50055 --roster-key-file ~/roster.key --stats --stats-file /var/log/grovefs.stats --stats-interval 10
# 或不改命令行,用环境开关(GROVEFS_STATS=1)
GROVEFS_STATS=1 $Y mount --view alice -m /mnt/alice --manager 127.0.0.1:50055 --roster-key-file ~/roster.key
```

- `--stats`: enable telemetry (prints the report on exit; also writes a copy to disk if `--stats-file` is given).
- `--stats-file PATH`: write the report to that file (writes a temp file then `rename`, so a concurrent `cat` never reads a half-written file); also refreshed on exit.
- `--stats-interval SECS`: if >0, starts a daemon thread that rewrites `--stats-file` every SECS seconds (skips the periodic rewrite if no file is given), so you can sample without unmounting.
- Hidden control node **`.grove-stats`**: only when `--stats` is on, this read-only synthetic file appears at the root; `cat`-ing it gives the current snapshot; when unmounted or not enabled it **never appears** — it's a purely client-side synthetic node, never written into the metadata store, so it doesn't touch the single-identity / no-orphan `data_ref` / atomic-rename governance invariants, nor add a visible item to default mounts.

Stages covered (`group` categories): `op` (read/write's op_total — the end-to-end single-op latency as the kernel sees it, minus the sub-stages below gives the FUSE reply/dispatch overhead, key_resolve, backend_total, write.touch_mtime), `data-io` (local/remote read/write/len round-trips, remote.pool_wait, remote.chunked, shard's read_total/write_total/server_op/split/concat), `meta` (lookup/getattr/readdir/readdir_page/create/unlink/rename/setattr/statfs, db.sqlite_read/write_txn/finalize_count, db.pg_query/write_txn/pool_wait/ssi_retry, remeta.rpc). Non-timed counters: `sqlite read-pool lazy-opens`, `pg ssi-retries`, `remote reconnects`, `broadcast send timeouts`, `ha fencing demotions`, `ha quorum refusals`, `lock lease renewals`, `lock expired reclaims`, `gc objects freed`, `bytes read`, `bytes written` — used to **provide explanatory signal** for the latency curves above (e.g. pool_wait tail rising → pool too small; ssi_retry count climbing → concurrent write conflicts; remote reconnects growing → backend restarting/drifting). The report provides both a text table and CSV (both sections written into `--stats-file`), for offline plotting or A/B comparison. The `bytes=` column only accumulates at backend-boundary stages (`read`/`write.backend_total`, `io.*`), so an `op_total` row showing `bytes=0` is expected — it means "this stage doesn't count bytes", not "no data read"; a latency quantile showing `0` means it's below the histogram's µs resolution (sub-µs), also not a missed sample. The report header states both of these conventions.

> Note the basis: the telemetry numbers are the measured per-stage distribution **within a single mount process**, for troubleshooting; they are not equal to, and must not be used for, the throughput/scale conclusions in the paper (this system deliberately makes no throughput/scalability claim). The `errno count` is not yet in the fixed registry (would need to extend the `Counter` set and report schema), listed as a future item.

### 8.2 Read/write/metadata acceleration (kernel-adaptive + automatic downgrade)

All acceleration changes follow the capability-negotiation principle of **fast path on new kernels, automatic downgrade on old ones**: FUSE capability bits are requested **bit by bit** at the `INIT` stage (fuser's `add_capabilities` is per-group all-or-nothing, so requesting bit by bit means a missing kernel bit only drops that bit, not the rest); an unsupported kernel bit simply doesn't take effect and never affects other paths; the SQLite/PG-side optimizations are independent of kernel version. Every mount (whether or not `--stats`) faithfully echoes the capability bits the kernel **actually granted**:

```
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
```

On this machine's 5.4 and the cluster's 6.8 (Ubuntu 24.04) this line is measured all `true`; on a content-read-only (fork) mount `WRITEBACK=false`, so a refused write lands on the first synchronous `write()`. With `ATOMIC_O_TRUNC=true`, an open with `O_TRUNC` completes the truncation inside `open`, saving the one `FUSE_SETATTR` round-trip the kernel would otherwise send afterward (measured: an `O_WRONLY|O_TRUNC` open dropped from 131µs/op on 5.4 to 84µs/op); on old kernels that don't grant the bit, truncation falls back to the SETATTR path, and both paths truncate correctly — there's no "requested it but treated it as granted". The `FUSE_DEV_IOC_CLONE` that multithreaded `clone_fd` depends on is Linux 4.5+; on a running kernel below that version the mount falls back to a single worker thread and echoes a notice line (rather than hard-failing).

**Metadata write tail latency (P0)**: each writable view engine starts a **background `wal_checkpoint(PASSIVE)` thread** (once per second, once `TRUNCATE` to shrink the `-wal` file every ~30s), and sets the write connection's `wal_autocheckpoint` to 0 — moving the synchronous checkpoint stall originally inline in commits (measured to once contribute a **125ms** single spike to `create`) off the write hot path. The thread is closed via `Arc<StopSignal>` on the engine's `Drop` (no join, no blocking the unmount), and read-only/standby engines don't start it. Coupled with `prepare_cached` statement reuse on the hot read path (avoiding recompiling SQL each time). Before/after comparison on the same 3000-file create workload with `--stats`:

| Metric (µs) | Before | After |
| --- | --- | --- |
| `meta.create` total | 1.64 s | **0.38 s**(↓4.3×) |
| `meta.create` p99 / max | 1151 / **125439** | **230 / 33535**(spike gone) |
| `meta.lookup` p50 | 39 | **17** |
| `meta.getattr` p50 | 66 | **36** |
| `db.sqlite_read` p50 | 44 | **21** |

**Read-cache TTL (P1)**: `--cache-ttl <secs>` enlarges the kernel attr/dentry cache TTL from the default 2s, cutting repeated `LOOKUP`/`GETATTR` round-trips to the same directory/file — suited to read-heavy or immutable trees (e.g. an astronomy "data-doesn't-land" one-pass stream read). Safety boundary: cross-client freshness is **still guaranteed by the default real-time `notify` invalidation push**, and on newer kernels (≥3.15) once `AUTO_INVAL_DATA` is negotiated a size/mtime change also discards the peer's stale page data; `--no-cache` always pins the TTL to 0 (full bypass). `run_all.sh`'s `freshness`/`disp_fresh` suites continuously prove <500ms peer visibility with the default TTL unchanged. **The lens's own materialized listing is also a cache**: derived/`--layout` mounts, to compress one directory traversal from O(n²) to O(n), materialize the shape per parent directory (4 dirs / 400k rows / TTL 2s) and serve slices, but events where the peer modifies the base tree don't pass through this lens, so the invalidation push, while dropping kernel entries, must also call `flush_listing(coord)` (a client-owned lens goes through `run_event_stream`, a server-rendered derived view through `broadcast_derived_display`) — otherwise the "pushed invalidation" is pushed back to TTL magnitude by the cache. The interface semantics are pinned by `tests/layout.rs::flush_listing_drops_the_materialized_shape`, end-to-end convergence by the `disp_fresh` suite (first asserting "without flush it goes stale", then asserting that after flush it's immediately visible, avoiding a no-op run).

**Content bandwidth shaping (P1)**: `--bw-read <rate>` / `--bw-write <rate>` put a client-side GCRA token slot on this mount's content reads and writes — laying out non-overlapping time slots across the actual bytes delivered (reads before the reply, writes after they land), so the long-run rate equals the cap exactly, and an op larger than a 1-second allotment is released at most 1 second early; an overshooting op only sleeps to its own time slot, never dropping bytes or truncating, and the write-side latency naturally forms backpressure. Syntax: a plain byte count or a `K`/`M`/`G` suffix (base-1024, e.g. `500000`, `50M`, `1G`), `0`/absent = unlimited (the hot path just adds one `rate == 0` branch). Fallback: flag > `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE` > config file; an illegal rate is refused at parameter parsing (silently ignoring a cap an operator believes is in effect would be worse). Observation: the `bytes read`/`bytes written` counters in the `--stats` report can verify actual throughput (with shaping on, `op_total` includes the intentional wait, `backend_total` doesn't); the `bwlimit` regression suite pins it in effect with a lower-bound assertion of 24MiB@8MiB/s. **Honest boundary**: this is client self-rate-limiting, not isolation — bypassing FUSE to reach the data-server directly, or multiple mounts of the same view each taking a full allotment; the server cannot attribute content-addressed bytes to a view, so it is never a cluster-level quota. Metadata ops are not shaped.

**Large-block IO (P2, kernel-adaptive)**: `max_write`/`max_readahead` request 4MiB, each truncated by fuser and the kernel per `FUSE_MAX_MAX_PAGES` — old kernels (e.g. 5.4) automatically drop to the page limit, 6.x+ grant larger blocks = fewer round-trips; `PARALLEL_DIROPS` (concurrent directory ops) and `WRITEBACK_CACHE` (batched merged writes) were long since negotiated. Large-block reads/writes already approach the underlying filesystem's floor, measured at only ~50–70µs added per FUSE op.

> Honest boundary: the table above is this machine's single-process `--stats` measured distribution (run-to-run jitter exists, but the 4.3× create-total reduction and the 125ms→33ms tail-spike convergence far exceed the noise, and the mechanism is attributable); these are troubleshooting/tuning observations, **not a throughput or scalability claim**. The remote/PG backend bottleneck is network RTT and must be measured separately with `bench_remote.sh`.

### 8.3 Tuning loop and knob-effect layers (methodology)

Find the bottleneck first, then turn a knob; **turn only one knob at a time**, compare `--stats` data across two windows under the same workload before concluding.

**Knobs are grouped by the layer they take effect on; turning the wrong layer = turning for nothing**:

| Effect layer | Knob | Notes |
| --- | --- | --- |
| Application layer (the mount process itself) | `--threads N` (default 4, each worker a separate `/dev/fuse` fd), `--stats`/`--stats-interval` (pure observation, zero overhead), `--log-file` | Changes GroveFS's own concurrency and observability |
| FUSE negotiation layer (clamped by the kernel) | `max_write`/`readahead`, `--cache-ttl`, `--no-cache` | The cap is set by the kernel's `FUSE_MAX_MAX_PAGES`: an old kernel (e.g. 5.4) requesting 4MiB naturally degrades, not a bug |
| Backend capability layer (server config) | `manager define --data A1,A2[,A3…]` declares the full stripe-member set, ≥`--stripe-bytes` (default 16MiB) auto-striping, `--meta-store postgres://…` | Changes capacity and parallelism (§5.2, §6.2) |
| Structural (code change) | swap the `MetaProvider`/`StorageBackend` implementation (§16.1) | Only reach here when the layers above aren't enough |

**Minimum loop**: ① locate the symptom — mount with `--stats`, run a real workload, read the report: `[io]` for the backend_total/pool_wait share, `[meta]` for each op's p50/p99 and ops/s, `[counters]` for parallel-slice and readdir-prefetch counts; ② judge the bottleneck — backend_total/pool_wait dominating = network RTT or the backend; `meta.create` p50 high and ops/s not rising = SQLite single-writer serialization; a high `dup>0` share = kernel cache not being hit (consider `--cache-ttl`); ③ turn only one knob, restart to take effect (for a systemd unit: `daemon-reload` + restart the mount's or the server's own unit; a client-side env only needs a remount); ④ compare two windows, gain claims based on same-machine same-workload measurement — everything given in the manual is a floor line, not a gain number.

**Existing measured floors (local loopback; must re-measure across machines)**: large-block read/write adds only ~50–70µs per op from FUSE itself (§8.2); under background WAL checkpoint, `lookup` p50 17µs, `getattr` p50 36µs (§10); the remote-link bottleneck is network RTT and must be measured separately with `bench_remote.sh`. A number below the floor means you're not measuring the mechanism itself — before cross-machine comparison, first run `dd` on an ext4 baseline to subtract the disk's own floor.

---

## 9. Common operations

```bash
$Y umount --mountpoint /mnt/alice        # 卸载(后台挂载进程随之退出)
fusermount -u /mnt/alice                 # 等价(前台挂载则 Ctrl-C 即卸载)
ps aux | grep grovefs                     # 查看在跑的 server / mount
```

> `mount` **goes to the background by default**: it self-re-execs into an independent session process, returning only once the mountpoint is actually live in `/proc/mounts` (scripts need no more sleep-polling), printing just a one-line summary in the terminal (with pid and log path); the background process's output appends to `--log-file` (default `$TMPDIR/grovefs-mount-<mountpoint>.log`, `--log-file /dev/null` fully silences it); `grovefs umount -m MP` unmounts and terminates it. `--foreground` keeps the traditional terminal-attached style (for systemd `Type=exec` units, debugging, or when you want live logs). Kernel implementation note: the session still uses the blocking `Session::notifier() + run()` style; **don't** use `spawn + BackgroundSession::join()` as a blocking wait — `join()` unmounts immediately.
>
> For the full deployment tutorial see **§11**; for the inspection/backup/upgrade/failure-handling manual see **§12**.

---

## 10. Verification and regression

The full board is **43 stages** in all: `build` → `unit` → **40 suites** → `bench`. **The first 42 stages gate GREEN; `bench` only prints throughput reference numbers and never gates** (for pure functional regression only, use `SKIP_BENCH=1`). `pg`/`pg_parallel`/`evict`/`control`/`forksrv` auto-SKIP their whole stage on hosts with no reachable PostgreSQL, and `xfer`/`gcremote` skip just their PG arm and run the SQLite arm — and `client` skips its whole stage on a host without the sibling `grovefs-client` checkout, its binary, or the `fusermount` helper — none of them lower GREEN. The experimental backend takes two stages: `treedb` is a shape self-check (53 assertions), and `treedbmatrix` replays the whole `posix`+`e2e` assertion matrix against `--meta-store treedb://` label by label (49 assertions = 39 OK + 10 SKIP + 0 FAIL; those 10 SKIPs are all hardlinks — the priced cost of an engine with no `Link` op), so it **gates on the measured triple**: `pass=39 fail=0 skip=10` is PASS and any move of that triple (one more FAIL, one missing assertion, a gap closing by itself) is red. Running either stage for real needs a feature build — `FEATURES_TREEDB=1 bash .build-bin/run_all.sh …` makes the board's `build` step pass `--features treedb`; on a default build both SELF-SKIP to INFO without lowering GREEN.

```bash
export LC_ALL=C LANG=C                                 # 仓库根目录下执行(PKG_CONFIG 不必设:无 crate 查 pkg-config,§1)
bash .build-bin/run_all.sh                # 全板 43 段(42 门控 + bench)
                                          #   build unit
                                          #   posix edge e2e remote errors resilience concurrent
                                          #   metasrv ha striping locks freshness disp_fresh
                                          #   layout fork forksrv viewrm gc gcremote xfer
                                          #   bwlimit migrate import govcheck govtime
                                          #   grovesci mcp apikey ctlplane web mountkey manager
                                          #   config pg pg_parallel evict control client
                                          #   treedb treedbmatrix
                                          #   bench(只打吞吐,不门控)
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 同上,去掉 bench
bash .build-bin/run_all.sh fork mcp       # 只跑指定套件(build 自动前置)
```

The end-to-end demo script (view isolation + real-time invalidation push) is `.build-bin/test_metasrv.sh`.

### 10.1 Two-engine completeness matrix and cross-machine specials (not in the default board)

The default board's `pg`/`pg_parallel`/`control` can only answer "can this tier mount, read, and write"; they can't answer "is the same task covered on both backends". The latter is given by the three scripts below, all rerunnable and all cleaning up after themselves:

| Script | What it measures | Latest measurement |
| --- | --- | --- |
| `bash .bneck/engine_matrix.sh` | Runs **the same governance sequence** once each on SQLite/PG, arranging 20 capabilities side by side into one table: OK=works, REFUSED=by-design unsupported (quotes the refusal wording), ERROR=gap. It runs the **no-FUSE** pure governance plane, so `graft`'s source is a hand-written seed dump (empty `data_ref`, measuring only tree movement); the bytes-and-ledger path belongs to `forksrv`/`xfer` | All 20 rows OK/REFUSED, **no ERROR**; the trailing `post-teardown` check verifies `sqlite tree files 0 / pg schema and ledger back to baseline` → `TEARDOWN CLEAN` |
| `bash .bneck/pg_view_teardown.sh` | PG base-view transactional deletion + bidirectional ledger repair, throughout with real FUSE mounts, real data-server, real PG; runs in a database it creates and drops itself (the role needs `CREATE DATABASE` — the script asserts ABSOLUTE zeros, and on the shared `grovefs` another suite's stranded rows fail the baseline first) | 44 pass / 0 fail (including `schema dropped with 6 name(s), 4 object(s) no longer held by any view`, `3 row(s) deleted`, `2 row(s) corrected`, second deletion refused) |
| `bash .build-bin/test_pg.sh` / `test_pg_parallel.sh` | PG-tier e2e (concurrency, eviction, control plane); ends with `view rm --base` to self-clean | 77 / 41 assertions all pass; leaves no **this-view** schema or ledger rows after running (on a shared cluster, orphans left behind by other suites don't make this assertion falsely red) |

> **PG cross-machine deployment has also been measured** (a one-off isolation probe, not in the default board): an independent `initdb`'s PG placed on the HOST (`192.168.122.1:5439`, `listen_addresses` only that subnet, not touching the shared loopback `5433`), guest `gfs1` (`192.168.122.124`) running `meta-server --meta-store postgres://…@192.168.122.1:5439/grovefs` + `data-server` + a client FUSE mount. Evidence: HOST-side `pg_stat_activity` shows 20 resident connections from the guest IP `192.168.122.124` (per view 8 write + 4 read warm pool + control plane/ledger), and the 4 `node` rows (`/`, `hello.txt`, `sub`, `sub/n.txt`) plus 2 `grovefs_refs.holders` holders written by the guest mount all land in the HOST's PG, with write/read/`mkdir` all passing and bytes staying on the guest's `data-server`. So "the metadata DB on another machine" holds for PG. The probe script, isolated DB, and guest-side artifacts have all been cleaned up; the shared `5433`/`5434` were unaffected.

The matrix's conclusion (also the answer to "is completeness met") — 20 rows, **zero ERROR** across both tiers:

| Capability | SQLite | PG |
| --- | --- | --- |
| `view create` (build base tree) | OK | OK |
| `view define` (server-side derived lens) | OK | OK |
| `view alias` (pure alias) | OK | OK |
| `view fork` (independent metadata tree) | OK | OK |
| `view list` (definitions + bare base views) | OK | OK |
| `view export` (metadata dump) | OK | OK |
| `view import` (dump → tree) | OK | OK |
| `view import --as-fork` (dump → fork, keeping fork semantics) | OK | OK |
| `view import` (hand-written seed dump, empty `data_ref`) | OK | OK |
| `grovefs import` (directory → view, zero-copy) | OK | REFUSED |
| `view graft` (pick a subtree onto another tree) | OK | OK |
| `refs rebuild` (cross-view ledger recompute) | OK | OK |
| `gc --data-dir …` (in-place scan of local `blocks/`) | OK | REFUSED |
| `gc --data-servers …` (enumerate orphans via data-server, incl. `--apply`) | OK | OK |
| `du` (byte total, tree from backend, bytes asked of data-server) | OK | OK |
| `du --view <nonexistent name>` (no-tree guard) | REFUSED | REFUSED |
| `apikey list` (credential register) | OK | OK |
| `apikey calls` (MCP call audit) | OK | OK |
| native HA (`--role standby`) | OK | REFUSED |
| `view rm --base` (tear down the tree with its bytes) | OK | OK |

The two `apikey`/`audit` rows' "OK" are **the same two commands, two destinations**: SQLite writes `<meta-dir>/meta/_control/api_keys.db`, PG writes the `grovefs_control` schema; the `du`/`dumiss` trees likewise come from these two places (bytes uniformly asked of the data-server).

> **This table answers "can it run", not "how to write it"**: the command shapes for the same task across the two tiers, and the three things only one tier can do, see §4.1.3; why to pick a given tier (write throughput / availability / scale) see §4.1.1.

The remaining REFUSEDs fall into two shapes, both **design boundaries** rather than gaps, each with a "why" and an "alternative" (§15.9 written out row by row):

- **Two verbs that run on the local data root**: `grovefs import` (zero-copy onboarding of a directory) and `gc --data-dir` (in-place scan of `<data-dir>/blocks/`) inherently depend on "tree and bytes on the same machine". A PG deployment's bytes belong to the data-server, the corresponding form is the remote `gc --data-servers` (OK on both tiers), and directory onboarding goes through the mountpoint via `rsync`/`cp` or the three steps "local import → `view export` → `view import`".
- **Native HA's snapshot forwarding** is specific to the SQLite-file form; a PG deployment hands replicas and recovery to its own toolchain (`pg_basebackup`/streaming replication/PITR, §12.3), and giving `--meta-store postgres://…` together with `--role standby`/`--peer`/`--witness` fails at startup.

`du --view <nonexistent name>`'s REFUSED is **one shared refusal wording across both tiers**: a nonexistent view name is refused explicitly, never confused with "a genuinely empty view" (the matrix's `dumiss` row pins the wording):

```
error: no tree for view 'mxnone' in <store> — `grovefs view create --view mxnone` makes one,
       and `grovefs view list` shows the names that exist
```

What both tiers share is **control plane + tree + ledger + base-view teardown + fork/graft + export/import + remote gc + du + apikey/audit** — that is, at minimum what a metadata engine must have (the `forksrv`/`xfer`/`gcremote` e2e suites assert per tier, with `xfer` covering `--as-fork`/`--replace`).

> Before the matrix runs, confirm there are no leftover grovefs mounts/services on this machine, otherwise the PG column's schema count will include the concurrent run. The script `unset GROVEFS_META_STORE` itself (every arm passes its store explicitly per command), so it runs the same from any shell; it also needs a reachable PostgreSQL (`PGSTORE` can override, by default the `grovefs` database on `127.0.0.1:5433`). The `/tmp/engmx/*.log` after running are kept for per-cell wording.

---

## 11. Hands-on deployment tutorial (production, from zero to acceptance)

> This chapter walks a real deployment in order: pre-check → build and install → plan → token → systemd daemons → client mount → firewall → acceptance; §11.8 is the HA primary/standby topology, §11.9 the PostgreSQL backend topology. Replace the example IPs/paths with your actual ones.

### 11.0 Pre-deployment pre-check (run once on every node)

Kernel ≥ 4.5 is required for multithreaded dispatch (`clone_fd` depends on `FUSE_DEV_IOC_CLONE`); below 4.5 `mount` automatically falls back to a single worker thread (echoing a notice line), still mounting and reading/writing normally, just not parallel. `/dev/fuse` present and read-write, `fusermount`/`fusermount3` in PATH or hittable from an absolute path (**both mounting and unmounting as non-root go through this setuid helper**; at runtime only libfuse's userspace tools are needed, **not** the libfuse dev package; with `CAP_SYS_ADMIN` it connects to `mount(2)` directly, no helper needed). The build machine additionally needs the Rust toolchain (edition 2021) — **no pkg-config and no libfuse dev package needed** (measured: `PKG_CONFIG=/nonexistent cargo build --release` fully passes, the artifact's `DT_NEEDED` contains no fuse library).

```bash
uname -r                                   # 期望 >= 4.5
ls -l /dev/fuse                            # 存在且当前用户可读写
which fusermount fusermount3 || true       # 至少有一个
```

Data-disk requirement: the filesystem that `data-server --data-dir` sits on must have real capacity — all file bytes land as objects under its `blocks/`, so **don't** build the data root on tmpfs.

### 11.1 Build and install the binary

```bash
cargo build --release                                    # 产物:target/release/grovefs
sudo install -m 755 target/release/grovefs /usr/local/bin/grovefs
grovefs --version
```

> The repo's `.build-bin/pkg-config` shim is **a historical leftover** (only used when early fuser went through the libfuse backend); no crate in the current dependency graph queries pkg-config, so whether it's set or not the build succeeds all the same.
> Installing the same binary on every node (center + clients) is enough; the subcommand decides the role.

### 11.2 Plan: nodes, directories, ports

This tutorial's topology (the most common form: a single center node + multiple clients):

| Node | Role | Directory/port |
| --- | --- | --- |
| center `10.0.0.5` | `data-server` | data root `/data/cluster/blocks`, listening `0.0.0.0:50100` |
| center `10.0.0.5` | `meta-server` | `--meta-dir /data/cluster` (authoritative views at `/data/cluster/meta/<view>/<view>.db`), listening `0.0.0.0:50110` |
| client `10.0.0.21…` | `mount` | a remote mount **needs no `--home`** (writes nothing locally), mountpoint `/mnt/<view>` |

The two servers' `--listen` defaults are `127.0.0.1` (data 50052 / meta 50053); **production must explicitly give `0.0.0.0:<port>`**, otherwise clients can't connect.

### 11.3 Generate a shared token (strongly recommended)

When the center server listens bare on the LAN, any host on the subnet can read/write objects by key / open others' views. Generate a random passphrase, identical across the three parties (the two servers + all clients):

```bash
sudo mkdir -p /etc/grovefs
sudo bash -c 'umask 077; echo "GROVE_TOKEN=$(openssl rand -hex 16)" > /etc/grovefs/grovefs.env'
sudo chmod 600 /etc/grovefs/grovefs.env
```

Distribute the same `grovefs.env` to every node (root-owned 0600). The token just needs to match; an empty token = LAN-trusted, no auth, only for a fully trusted intranet.

### 11.4 Center node: two systemd daemon services

Both servers are foreground blocking processes; `SIGTERM` exits them cleanly; systemd `Type=simple` manages them directly.

```ini
# /etc/systemd/system/grove-data-server.service
[Unit]
Description=GroveFS data-server
After=local-fs.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/grove-meta-server.service
[Unit]
Description=GroveFS meta-server
After=local-fs.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-data-server grove-meta-server
systemctl status grove-data-server grove-meta-server    # 两个 active (running)
ss -lntp | grep -E '50100|50110'                        # 两个端口在听
```

> `meta-server`'s metadata root uses the subcommand parameter `--meta-dir` (taking precedence over the global `--home`; `--home` is the global default root); `data-server`'s data root is `--data-dir`; the two are unrelated, and neither needs the global `--home`.

If you want the web console (§3.5.3), the center node becomes **three** services: the two units above keep running exactly as they do, you only have `meta-server` open one more governance port and place an administrator on **first install**, then add a `grovefs web` that listens on loopback only.

```ini
# /etc/systemd/system/grove-meta-server.service —— only this ExecStart line changes
ExecStart=/usr/local/bin/grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token ${GROVE_TOKEN} --control-listen 127.0.0.1:50111 --admin-user rootops --admin-password-file /etc/grovefs/web-owner.pw
```

```ini
# /etc/systemd/system/grove-web.service   —— the console gateway (loopback only; browsers arrive via the reverse proxy or an SSH tunnel)
[Unit]
Description=GroveFS management console
After=grove-meta-server.service
Requires=grove-meta-server.service

[Service]
ExecStart=/usr/local/bin/grovefs web --control-server 127.0.0.1:50111 --api-key-file /etc/grovefs/control.key --listen 127.0.0.1:8787
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-meta-server grove-web
ssh -L 8787:127.0.0.1:8787 10.0.0.5          # then open http://127.0.0.1:8787/ from your own workstation
```

Both secrets are root-owned 0600: `web-owner.pw` is read once and only while the accounts table is **empty** (editing that file afterwards changes no password — `grovefs web account passwd` is the verb that changes one), and `control.key` is the gateway's governance credential (revoking it takes the whole console dark, while mounts and every other CLI path keep working). Open the governance port to the **intranet** only per §11.6 — or keep it on `127.0.0.1` and run the proxy and the gateway on the same host; the full reverse-proxy + TLS units and nginx config are in `docs/deploy.html`.


The topology can also carry a third resident daemon, the **mount-bootstrap manager** (§4.2): consulted once at mount time, never on any IO path; `join.key` and `roster.key` are minted on first `serve` under `<state-dir>/manager/` (0600) — the former is handed to servers' `--register-with`, the latter to clients' `mount --manager` to verify the roster. Its outage never moves a live mount; it only makes NEW mounts fail-closed:

```ini
# /etc/systemd/system/grove-manager.service
[Unit]
Description=GroveFS manager (mount bootstrap)
After=local-fs.target
[Service]
ExecStart=/usr/local/bin/grovefs manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```
To attach the bootstrap, append a stanza to each of the two ExecStart lines above — registration is a status report only; a server that cannot reach the manager keeps serving and retries with backoff (§4.2):

```text
# appended to the data-server ExecStart:
  --member-id 0 --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50100 \
  --join-key-file /etc/grovefs/join.key --deployment prod
# appended to the meta-server ExecStart:
  --member-id 1 --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50110 \
  --join-key-file /etc/grovefs/join.key --deployment prod
```

### 11.5 Clients: hand mounting to systemd too (template instance, one per view)

The mount unit carries `--foreground` to keep the process attached (`Type=exec` must watch the service process itself, so it can't be allowed to background-and-exit); a built-in SIGTERM handler means `systemctl stop` automatically does a clean `fusermount -u`, leaving no dead mount.

```ini
# /etc/systemd/system/grove-mount@.service   —— %i = 视图名,挂载点固定 /mnt/%i
[Unit]
Description=GroveFS mount view %i
After=network-online.target grove-data-server.service
Wants=network-online.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
# 上次异常退出可能留个死挂载,先清掉(- 表示失败也继续)
ExecStartPre=-/usr/bin/fusermount -u /mnt/%i
# Mount coordinates resolve ONLY from the manager (§4.2); there are no hand-typed --meta-server/--data-servers on the ExecStart below:
ExecStart=/usr/local/bin/grovefs mount --view %i --foreground --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key --token ${GROVE_TOKEN} -m /mnt/%i
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl start grove-mount@alice          # 挂 alice 视图到 /mnt/alice
sudo systemctl start grove-mount@bob            # 同一台机再挂 bob,互不可见
findmnt -T /mnt/alice                           # 源显示 grovefs:alice,FUSE 类型
```

Note two points (both uid-related, see §6.1):

- systemd starts the mount process as root by default, so the uid reported to meta-server is **0**; to mount as a fixed role, add `--run-as-uid N` to ExecStart to specify that mount's ownership slot (recorded in lock arbitration and audit).
- When **other users also need access** to the mountpoint, add `--o allow_other` and confirm `user_allow_other` is enabled in `/etc/fuse.conf`.

To switch to **revocable per-caller credentials** (kicking one machine no longer requires rotating the shared token and breaking everyone): add `--require-api-key` to each server's ExecStart (the data side additionally needs `--meta-dir` pointing at the metadata root, if it differs from the first `--data-dir`), and on the client replace `--token ${GROVE_TOKEN}` with `--api-key ${GROVE_API_KEY}`, minting the key via `grovefs apikey create --label <hostname> --for mount` and writing it into the same root-owned 0600 `grovefs.env` (so the secret never enters the `ps`-visible unit text). The real behavior after a revoke (measured): the mount, on its first server round-trip after the revoke, prints the server's refusal reason (`grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view`), **cleanly self-unmounts and exits with code 0** — a deliberate withdrawal, not a crash, so `Restart=on-failure` does **not** respawn it and the unit lands in `inactive (dead)` (`systemctl status` looks like an operator unmounted it manually; to tell "kicked" apart, look for that refusal line in the journal, or hook `OnFailure=`/`ExecStopPost=` for alerting); remounting with a revoked key fails fast. If you want automatic retry after being kicked (e.g. an operator will re-mint one), use `Restart=always`; to keep a one-and-done stop, no extra config is needed. Mounts on other hosts are entirely unaffected throughout — this is exactly the whole value of per-caller credentials over a shared token.

### 11.6 Firewall: open the two service ports only to the intranet subnet

```bash
# ufw 示例(按实际网段改);两个 server 同为帧协议,端口不同
sudo ufw allow from 10.0.0.0/24 to any port 50100 proto tcp
sudo ufw allow from 10.0.0.0/24 to any port 50110 proto tcp
# firewalld 等价:
# sudo firewall-cmd --add-rich-rule='rule family=ipv4 source address=10.0.0.0/24 port port=50100 protocol=tcp accept' --permanent
# sudo firewall-cmd --add-rich-rule='rule family=ipv4 source address=10.0.0.0/24 port port=50110 protocol=tcp accept' --permanent && sudo firewall-cmd --reload
```

Don't expose 50100/50110 to the public internet; the token is only a second line of defense, not transport encryption (the frame protocol is unencrypted; TLS/tunneling are unimplemented items).

### 11.7 Acceptance checklist (tick each item; every command is a real command)

```bash
# 1) 两个 server 存活且监听
systemctl is-active grove-data-server grove-meta-server && ss -lntp | grep -E '50100|50110'

# 2) 视图已挂上、打了 grovefs 标签
findmnt -T /mnt/alice          # 期望:FUSE / grovefs:alice

# 3) 新视图需先显式创建:名字不在时 M_OPEN 直接拒绝(无开关),
#    挂载本身从不建树。建完再挂:
#    grovefs view --meta-dir /data/cluster create --view carol
#    预建层级(--manifest)只在本地模式用,见 §3
sudo grovefs view --meta-dir /data/cluster create --view carol
sudo systemctl start grove-mount@carol && ls /mnt/carol

# 4) 跨客户端实时可见(两台客户机挂同一 --view,核心卖点)
#    客户机 A:
echo hi-$(date +%s) > /mnt/alice/from_a.txt
#    客户机 B(写完立刻,不等轮询):
cat /mnt/alice/from_a.txt      # 期望:即时看到同内容

# 5) 分布式锁跨机生效
#    A:flock -x /mnt/alice/lk -c 'sleep 30' &   # B 此时 flock 应阻塞,30s 后放行
#    B:flock -x /mnt/alice/lk -c 'echo got'      # 期望:等 A 退出后才打印

# 6) 字节确实落在中心,不碰客户机盘
ls /data/cluster/blocks | head       # 期望:256 个桶目录已在服务首启时建好,对象按 key 哈希落在其中增长(find /data/cluster/blocks -type f 看总量)

# 7) 数据服务重启对客户机透明(断→拉起,客户机 IO 自动重连)
sudo systemctl restart grove-data-server && cat /mnt/alice/from_a.txt

# 8) 卸载干净、无残留
sudo systemctl stop grove-mount@alice && findmnt -T /mnt/alice | grep -q grovefs && echo "残留!" || echo "已卸载"
```

All passing means the deployment is complete. To run the full regression see §10 and §12.7.

### 11.8 HA primary/standby topology (modified from §11.4)

Key point: the two meta-servers use **their own separate `--meta-dir`s** (snapshot replication, not a shared disk! It's `blocks/` that's shared — the data address resolved from the roster never changes during failover, it always points at the same set of data-servers):

```bash
# nodeA(10.0.0.5):primary
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --token $TOK
# nodeB(10.0.0.6):standby(同样写成 systemd unit,只改 --meta-dir/--listen/--role/--peer)
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --token $TOK
# the manager declares the ORDERED failover list (§4.2); a client points only at the manager:
$Y --home /srv/mgr manager define --meta 10.0.0.5:50110,10.0.0.6:50111 --data 10.0.0.5:50100
$Y mount --view alice --manager 10.0.0.5:50055 --roster-key-file /etc/grovefs/roster.key --token $TOK -m /mnt/alice
```

Preventing double-writes needs a third-party arbitrator: write **3 (odd number) data-servers** into the `--witness` list, and these data-servers should be deployed **on the same nodes** as the content disks clients actually write to (the data-plane gate only takes effect on data-servers in the list, see §5.5). Acceptance:

```bash
sudo ssh nodeA systemctl stop grove-meta-server     # 杀主
journalctl -u grove-meta-server -n5 --since '-1min' # nodeB 期望出现:meta HA: promoted to primary at epoch N
# 客户机继续可写(切到备的地址列表自动故障转移)
sudo ssh nodeA systemctl start grove-meta-server    # 旧主回来 → 见更高 epoch 自动降级为 standby
```

For the RPO-vs-fencing trade-off and the expected behavior of fail-safe write freeze, see §5.5; for the log keywords to watch during inspection, see §12.1.

### 11.9 PostgreSQL backend deployment (only one change, to meta-server)

```bash
# 1) 准备一个 PG(自建或发行版包均可),建库并给连接角色建 schema 的权限:
#    createdb grovefs;  psql -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grovefs;"
# 2) meta-server 启动参数加 --meta-store(与 --role standby/--peer/--witness 互斥,同时给会启动即 fail-fast):
$Y meta-server --meta-store postgres://grove:pw@10.0.0.9:5432/grovefs --listen 0.0.0.0:50110 --token $TOK
```

Client commands are unchanged (see §4.1: clients don't install PG, don't hold a connection string, don't sense the backend). Each view's `grovefs_v_<hex(view)>` schema must first be explicitly created via `view --meta-store … create` (a mount only opens an existing schema, never creating it in place), no pre-migration script needed; the control plane follows `--meta-store` into PG too: keys and audit land in `grovefs_control.api_keys`/`mcp_calls`, view definitions in `grovefs_control.view_defs`, and `grovefs view`/`apikey`/`mcp` with the same `--meta-store` (or env `GROVEFS_META_STORE`) connect to the same record, writing no control-plane files locally (the `control` suite proves it section by section). Backup/HA move to PG's own toolchain (one `pg_dump` covers tree + locks + control plane together; `pg_basebackup`/streaming replication/PITR), see §12.3. For the honest boundary on why to/not to go with PG, see §4.1. **For inspection SQL, backup/restore, view deletion, and the PG capability boundaries (incl. measured refusal wording), see §15.6–§15.9.**

---

## 12. Maintenance manual (inspection / backup-restore / upgrade / failure handling)

### 12.1 Daily inspection

```bash
# 进程与端口
systemctl is-active grove-data-server grove-meta-server
# 所有 grove-mount@* 实例都在跑?
systemctl list-units 'grove-mount@*' --state=running

# 挂载点都在、标签对
findmnt -t fuse | grep grovefs

# 中心容量(对象直存于 blocks 下 256 个桶子目录,直接 du 整目录即可)
# 判异常要看斜率而不是看大小:每周同一天追一行带日期的读数
du -sh /data/cluster/blocks; df -h /data/cluster
echo "$(date +%F) $(du -sh /data/cluster/blocks | cut -f1)" >> /var/log/grovefs_capacity.log

# 昨夜到现在有没有降级/冻结/提升事件
journalctl -u grove-meta-server --since yesterday | grep -E 'meta HA|sync-repl|fenced|promotion'

# 元数据面是否在集群上:meta-server 日志的 backend 横幅行(下表)比任何本地目录都可靠
journalctl -u grove-meta-server | grep -E 'metadata backend'
```

Log keyword → meaning → action (strings per the source print statements):

| Log keyword | Meaning | Action |
| --- | --- | --- |
| `meta sync-repl: ... degraded to async for this op` | `--sync-repl` standby unreachable / ACK timeout, that write's RPO falls back to ~500ms (honest degradation, not a false 0) | Check standby liveness / network; if persistent, either fix the standby or accept async RPO |
| `meta HA: promoted to primary at epoch N` | standby promotion succeeded | Confirm the old primary is stopped / will demote, investigate afterward why the primary was lost |
| `meta HA: demoted to standby (peer epoch N)` / `(a quorum ... holds ... > our ...)` | Found a peer/quorum holding a higher epoch, self-demoted | Normal convergence; if frequent on both sides, check network jitter |
| `meta HA: cannot form lease quorum ... fail-safe self-demote` | witness majority unreachable past the grace (3s), **write freeze** (fail-safe, prefer halting writes to double-writing) | Expected protection, not a fault: restoring arbitrator reachability auto-unfreezes |
| `meta-server: evicted N idle engine(s) (>Xs idle, no sessions)` | idle-engine reclamation (`--engine-idle-secs`, §6.2): N views with no mount session and quiet past the threshold released their engines (PG connections / file handles) | Normal reclamation noise, no action; to keep a view hot, shorten the threshold or confirm sessions are gone; `0` disables eviction (resident model) |
| `meta HA: promotion refused/deferred` | promotion denied (peer holds a higher token or the predecessor lease is still alive) | Normal: never two primaries; if both nodes refuse to promote, check the witness majority |
| `invalidation listener stopped: token rejected` | the client invalidation-push channel was rejected by token (the center token changed) | Restart the affected grove-mount@ instance with the new token |
| `meta-server: metadata backend sqlite — one db file per view under <base>/meta/` | the server reads local SQLite trees: one `<meta-dir>/meta/<view>/<view>.db` per view, `--meta-dir` is both the config directory and the trees' landing spot | Normal. When hunting "where did my tree go", read this line first: it says `one db file per view`, so the `.db` files belong under the local `meta/` |
| `meta-server: metadata backend postgres — one schema per view in PostgreSQL; <base> holds no trees` | tree and control plane are both in the database (`grovefs_v_<hex>` schemas + `grovefs_control`), `--meta-dir` keeps only logs and scratch (§6.2) | Normal. If this line appears yet you are still looking for `.db` under `meta/`, what you are reading is a pre-migration copy (§4.1); confirm the in-database landing spot with the inspection SQL of §15.6 |

### 12.2 Logs and observability

All processes write logs to foreground stderr; `RUST_LOG` controls the tracing level (default `warn`). A background mount (the default form) sends all its output to its `--log-file`/`$TMPDIR/grovefs-mount-<mountpoint>.log`; only foreground mounts (--foreground/systemd units) reach the terminal or journal:

```bash
RUST_LOG=info   # meta-server 常规运维建议级别;debug 仅在排障时短开(帧级日志量大)
journalctl -u grove-meta-server -f        # systemd 下实时追日志
```

There's no centralized metrics interface (unimplemented); health is judged by: process active + port listening + `findmnt` mounted + the keyword table above showing no new anomalous lines.

### 12.3 Backup and restore

What to back up (each item complements the others):

| Item | Location | Method |
| --- | --- | --- |
| Data objects | `blocks/` under the data-server `--data-dir` | A plain directory, back up directly with `rsync -a`/filesystem snapshot; if you ever used `grovefs import` (§3.3), some objects are hardlinks shared with the source directory — use `rsync -a -H` or `cp -a` to preserve the links and avoid doubling content, and the source directory itself should be in the same backup |
| Authoritative views | SQLite: `<meta-dir>/meta/<view>/<view>.db` (incl. WAL); treedb backend: **not under `<meta-dir>`**, see the "treedb backend" section below | See the three flows below; **don't bare-copy under write activity** |
| Cross-view ledger | `<meta-dir>/meta/_control/refs.db` | Copy with `meta/`; after loss/replacement recompute from the existing views via `grovefs refs --meta-dir D rebuild` (only ever overestimates, never wrongly deletes) |
| API key registry | SQLite: `<meta-dir>/meta/_control/api_keys.db` (incl. WAL); PG: `grovefs_control.api_keys`/`mcp_calls` | SQLite as above, cold copy; PG goes into the DB with `pg_dump`; none has a separate file to back up |
| View definitions (derived/alias) | SQLite: `<meta-dir>/meta/views/*.layout`; PG: `grovefs_control.view_defs` | SQLite plain text files, must be copied along (HA standbys also need manual sync, §3.2/§3.4); PG replicates with the DB, no manual sync needed |
| token | `/etc/grovefs/grovefs.env` | Store separately, encrypted (lost = no client can mount) |
| API key registry and audit | SQLite: `<meta-dir>/meta/_control/api_keys.db` (incl. `-wal`; exists only after `apikey create`) / PG: `grovefs_control.api_keys`+`mcp_calls` (via `pg_dump`) | SQLite **cold-copy** with `meta/` (stop writes first); with `--require-api-key` enabled it is the sole admission basis — losing it = no key authenticates, historical audit gaps. **Never delete `-wal` alone** (it also deletes not-yet-checkpointed key rows and audit rows); in a split-disk deployment `data-server --meta-dir` must point at the metadata root holding this DB (it doesn't copy, only reads that one), a wrong target fails closed; on PG deployments just give `data-server` the same `--meta-store` URI, no local DB to point at |

**One storage machine (the metadata root and the data root are the same directory)**: first `umount` to stop writes, stop `meta-server` and `data-server`, then `cp -a`/`rsync` the whole storage root. Restore: copy back into place → start the two services → clients remount → `ls`/`cat` spot-check. The binary itself isn't in the backup scope — just reinstall `grovefs`, the data and the metadata are all inside that one root. **A split deployment must be copied twice**: the two roots are the products of two independent commands (`meta-server --meta-dir` and `data-server --data-dir`), and copying only one of them cannot be restored — `meta/` holds no bytes, `blocks/` holds no names.

**SQLite + in-house HA**: what the standby receives every ~500ms is a full `VACUUM INTO` snapshot (online-backup semantics), but it's **a replica, not an archival backup** (still recommended to take periodic off-site snapshots of either the primary or standby). Node-repair flows:

- Standby broken → after fixing, just `systemctl start` the standby meta-server: it resumes receiving snapshots from the primary and catches up, no manual data injection.
- Primary broken → the standby auto-promotes (see the §12.1 logs); after the old primary is fixed and `start`ed: it finds a higher epoch and **auto-demotes to standby**, never reviving into two primaries (with `--witness` configured, even an old primary restarting with a higher epoch is held at standby by the quorum, `test_ha.sh` section 9 proves it).
- Both halves lost and rebuilding: restore `<meta-dir>/meta/` from the most recent archival snapshot, the precondition for the epoch starting from scratch is **all data-servers also turned over at once** (otherwise leftover `.fence_token` will block writes — this is fail-safe's correct behavior, not corruption).

**PostgreSQL backend**: view-tree backup is entirely handed to the PG toolchain (`pg_dump`/`pg_basebackup` + WAL archiving/PITR); on the GroveFS side only back up `blocks/` + the env file — keys, audit, and view definitions are already in the `grovefs_control` schema, one `pg_dump` covers tree + control plane together (proven by the `control` suite), no local control-plane files to cold-copy. In `--meta-store` mode the in-house snapshot backup API deliberately returns an error (by design). For directly copyable dump/restore/drill commands (incl. DBA database creation and post-restore verification items) see **§15.7**.

**treedb backend** (`--meta-store treedb://host:port`): the metadata comes in **two halves, and backing up only one loses the other** (both locations are measured — see the end-to-end board at the end of this section). **Half of it is in the engine**: that URI is only a TCP coordinate, the tree lives in the `treedb-server` process's `--dir` directory (one snapshot + one WAL), and **which views exist is in the engine too** (under this backend the authoritative source of `view list` is the engine's `Op::ListViews`) — `<meta-dir>/meta/<view>/<view>.db` **does not exist** here, so following the SQLite recipe and cold-copying `<meta-dir>` alone captures no view trees at all. **The other half is local SQLite beside the meta-server**: api keys in `<meta-dir>/meta/api_keys.db`, web accounts and grants in `<meta-dir>/meta/web_accounts.db` (each with its `-wal`/`-shm`), and the alias/fork/lens definitions in that same local control plane — none of it **travels with an engine backup**. So what gets backed up is **the engine directory (plus its archive directory once archiving is on) AND that `<meta-dir>`**; the content bytes are always in the data server's `blocks/`, independent of both halves. Three shapes:

- **Cold copy**: stop `treedb-server`, then `cp -a` its `--dir` (a graceful stop performs the last checkpoint itself, so the restore never depends on replaying tail records); RPO = the moment you stopped it. Restore with `treedb-load --in F --dir D` (both directories must be exclusive, a non-empty target is refused — it reports `refusing to restore into <dir>: it already has N entrie(s)`). Copy BOTH halves: the `<meta-dir>` side is the same stop-then-`cp -a` the SQLite row of the table above already describes.
- **Online cut**: `treedb-backup --primary HOST:PORT --out F.dump` — the primary neither stops nor needs to have checkpointed, and `--out -` pipes straight into gzip (the summary goes to stderr, the byte stream stays clean). The cut is pinned at **one seq**; it is not continuous catch-up.
- **Point in time**: start `treedb-server` with `--archive-dir A` (a checkpoint then copies the WAL segment and the snapshot it is about to overwrite into the archive before touching anything), after which `treedb-pitr --dir D --archive A --to-seq N --out NEW` lands at the end of **any single op between two checkpoints** — the only shape that can do that, with `--dry-run` printing the plan without writing bytes. Take N from the `seq` LINE of `treedb-server --stats` (it prints `seq = 24 (snapshot 12)`; there is **no** `wal_seq` field in the output): one op may produce several log records while the WAL carries no group boundary, so a point inside a group yields a state where that op "took effect halfway".

Three things it does not do: **no incremental backup, no encrypted backup, no archive retention policy** (the archive only grows — before pruning old segments, confirm the snapshots serving as anchors are still there). Two traps: ① `--archive-dir` must be **on from day one**, because checkpoints made while it was off leave a hole in the archive and a restore past it reports the gap instead of splicing the two sides of history; ② **upgrade server and integrating side in pairs** — `treedb-migrate` bumps the on-disk format but not the wire protocol, and **with authentication off a mixed-generation run raises no error, it just reads back garbage** (four measured arms in the engine's DESIGN §4.4: the same `--stats` decodes as `protocol = 0`, `wal = 0 B` against the wrong generation) — to get that gate, turn authentication on (`GROVEFS_TREEDB_KEY_FILE`).

Honest boundary: the shapes above are measured in the treedb repository (its DESIGN §6.7 / §6.10 / §6.11, rerun 2026-11-24 at rev `a749266`: cold copy equal item by item, online cut self-consistent, and a restore containing v0.7.0's three bulk records is **byte-identical to a cold dump**). **This repository runs the drill end to end as well**: `bash .build-bin/test_treedb_backup.sh` brings up a real cluster (engine started with `--archive-dir`, `meta-server --meta-store treedb://`, a real FUSE mount writing real files) and restores each of the three shapes into a NEW cluster that is then mounted — **55 assertions, all green** (2026-11-24, rev `3b2da1b`, engine pinned v0.7.0). Two of them are exactly what the two-halves paragraph above rests on: a view created AFTER the watermark disappears together with the engine (watermark 24 → 38; after the restore `view list` shows only `team` and mounting that view is refused with the no-base-tree error), while an api key minted BEFORE it still authenticates (it sits in the local store and does not roll back) — so after an engine rollback the control plane and the tree can disagree, and both halves have to be rewound to the same point. The `blocks/` content-byte row of the table above is backend-independent.

**Pin gate** (do not copy these commands blindly — the branches of this repo pin different engine versions): `treedb-backup`, `treedb-dump` / `treedb-load`, `--archive-dir` and `treedb-pitr` are **all binaries or flags that first appear in engine v0.6.0**, and `treedb-standby --promote` needs v0.4.0. Read the actual `treedb = { ..., tag = ... }` line in the branch's `Cargo.toml`: `master` still pins `v0.3.7` (so on that branch **only the cold-copy and replica-directory shapes exist** — the other commands are simply not in that build), while the integration frontier `feat/treedb-hardlink` pins `v0.7.0`. Change the engine binary **in pairs** with the server, for the mixed-generation reason above. The pin is executable too: `test_treedb_backup.sh` starts by counting which engine binaries are missing, so a branch pinned below v0.6.0 self-skips and names the absent ones instead of pretending to pass.

**Migrating to a new machine / switching OS**: metadata isn't bound to hardware (stores only object keys, paths are a pure function of keys), a whole-directory cold copy works as-is; for selective per-view migration use `view export/import` (details in §3.4.2). Three topology boundaries: the multi-disk `--data-dir` count/order must match (an equal-size clone swapped in place is fine, add/remove/reorder remaps; or merge into a single root); striping `s<N>:` must keep the N data-servers and their order; a whole-directory copy carries the HA `epoch` over, the old primary must be stopped first. PG backend uses `pg_dump`/logical replication.

### 12.4 Upgrade and rolling changes

**First, check whether the metadata layout changed generation**: while a storage root is still in the v1 flat shape (`team.db`/`refs.db` lying flat under `meta/`), every command that has to read a view tree refuses and names the migration verb — **there is deliberately no dual-read transition** (the reason lives in §4's "the directory name is the type": under the v2 enumerator a v1 store looks like an empty shop, so the ledger books zero holders and `gc --apply` reclaims the whole store's bytes). Inspect the shape with `grovefs layout --meta-dir D show`, the plan with `grovefs layout --meta-dir D migrate --dry-run`, and migrate with `grovefs layout --meta-dir D migrate`. Migration requires no live mount on that storage root and no grovefs process on this machine (two silent gates); it checkpoints each file's WAL first, then moves it into `meta/<view>/<view>.db` and `meta/_control/`; the ledger books content keys, not paths, so it is **not** rewritten. Verify with `show` afterwards (v2), then start the meta-server. The HA standby — and every host that receives `meta/views/` by hand — must migrate **on its own**.

Client binary upgrade (interrupt-free on one machine): `systemctl stop grove-mount@alice` → replace `/usr/local/bin/grovefs` → `start`. (stop unmounts cleanly; directly killing the binary won't do — a dead mount must be cleared by hand with `fusermount -u`.)

Center server upgrade (no-HA topology, accept a seconds-level IO pause): replace the binary → `systemctl restart grove-data-server grove-meta-server`. Client mount processes **don't** need to be restarted along with it — the remote link is transparent to server restarts (the resilience suite proves it: after the connection pool is wholly dropped, it reconnects with bounded backoff); only remount when a client-side protocol/feature genuinely needs the new version.

HA topology rolling upgrade (writes uninterrupted, except the instant of switchover):

```bash
# 1) 先升级并拉起 standby(追平快照)
# 2) stop primary → standby 在 ≤3s(PROMOTE_TIMEOUT)内提升,客户端自动切地址
# 3) 升级旧主并以 standby 角色拉起 → 见高 epoch 自动降级,回到初始形态
```

> Version-match requirement: the wire protocol is a custom frame and **carries no version number** — compatibility depends entirely on the capability-byte width and parse strictness of the `M_OPEN` ack packet. The packet fixedly carries three capability bytes (read-only / pageable / content-read-only). A new client strictly validates all three bytes; a missing one (a peer from the early two-byte generation) reports the plain-worded error `meta-server handshake ack is missing the capability flags — client and server builds must match`, the mount doesn't establish, never guessing defaults. Measured matrix: same-generation deployments with all three bytes present (new↔old three-byte versions in any direction) handshakes consistently and reads/writes fine; the only window to flag is "a two-byte-era old client connecting to a current server" — it leniently parses the ack as `get(i).unwrap_or(0)`, ignoring the third byte (content-read-only), so an old client mounting a **fork view** won't get the content-read-only `EROFS` at the FUSE layer (the server doesn't gate the byte stream, see §7). Hence the upgrade order: first bring all storage and server sides up to the three-byte generation, then don't use two-byte-generation clients to mount fork views.

> **Write-retry requirement in the switchover window**: between stopping the primary and the standby finishing promotion (≤3s `PROMOTE_TIMEOUT`), in-flight/first write round-trips may fail (`EIO`, or `EROFS`/`EPERM` via the server gate path) — the normal manifestation of a stale connection within the handoff window, measured as state-dependent (the same operation is sometimes `rc=0`, taking ~0.8s). Callers should retry the first write within the switchover window (an application-level rerun suffices, except idempotent appends — before retrying confirm the previous one didn't land); the uninterrupted-write promise holds only for the new primary after switchover completes.

### 12.5 Failure-handling quick reference

| Symptom | First reaction | Explanation |
| --- | --- | --- |
| `ls` reports `Transport endpoint is not connected` | `fusermount -u /mnt/x` (or `grovefs umount -m /mnt/x`) then restart the affected mount service | The mount daemon died / was killed, leaving a dead mountpoint |
| Mount immediately reports `Permission denied` | Read the **refusal reason verbatim** in the error (returned byte-for-byte with the handshake): `meta-server rejected the access token` / `no api key presented` / `unknown api key id` / `invalid api key secret` / `malformed api key` / `scoped to 'mcp', not 'mount'` / `this api key has been revoked` | `OP_OPEN`/`M_OPEN` handshake failures don't retry, fail-fast; the former compares the token, the latter checks the scope/revoked columns of `grovefs apikey list` and the attribution rows of `apikey calls` |
| A mount unmounts itself while in use (log `access refused by the server … — unmounting this view`) | `grovefs apikey calls` to see whether that key was revoked / revoked just under a minute ago | Expected behavior: the server revalidates live connections each second, and after a revoke the first round-trip closes the mount (§6), avoiding a still-readable view the server no longer recognizes |
| All writes `EPERM`, reads normal | `grovefs apikey list` to see whether the key is `--ro` (read-only) | A read-only key is enforced `EPERM` per-op server-side, immediate (not cached); **this is a permission, not a fault**, deliberately distinct from `EROFS`. Another source: an old/modified client writing under a derived (virtual) view name (the server's derived read-only gate also returns `EPERM`, §3.2) |
| Mounting the base name is refused `"governed by an alias"` (local and remote echo the same wording, §3.4) | `grovefs view list` to check whether the real name is governed by a pure alias | Expected behavior: the alias is its sole mount entry; to restore direct base-name mounting, `grovefs view rm --name <alias>` (affects only new mounts) |
| Remote mount writes all `EROFS` | `journalctl -u grove-meta-server` for `fenced/write-freeze/promotion refused` or the role; `findmnt` for whether the mount line carries `ro` | Three normal cases: ① the node is a standby (read-only, awaiting promotion); ② witness-majority unreachable triggered fail-safe write freeze — fix arbitrator reachability, prefer halting writes to double-writing; ③ what's mounted is a **derived (virtual) view name** (read-only contract, §3.2) — remount the base name to write |
| `flock` looks held, the holder's whole machine crashed | Wait for the ≤15s (`LOCK_LEASE_SECS`) lease to expire, auto-swept | Without a `release` signal the lease is the only backstop; a normal process crash is reclaimed immediately |
| After a data-server restart, client IO stalls a moment then recovers | No action needed | Reconnect with bounded backoff after the connection pool is wholly dropped (resilience proves it) |
| A non-root user can't see the mountpoint / `allow_other` doesn't take effect | Check `user_allow_other` in `/etc/fuse.conf`; check the fusermount SUID bit | A FUSE convention limit, unrelated to GroveFS |
| `--meta-store postgres://…` plus `--peer` exits immediately on startup | Remove the HA parameters | PG being mutually exclusive with in-house snapshot HA is a design decision (§6.2); use PG streaming replication for HA |
| No stripe chunks seen for large files | Confirm the file is ≥16MiB and the manager declared a multi-address data list (`manager define --data A,A2`) | Small files naturally land only in chunk0, by design not a bug |
| Mountpoint disk full | First enlarge the disk holding `blocks/`; afterward use §12.6's three criteria to check "workload volume or unreclaimed bytes" | Capacity governance has no magic: objects only add/delete without compression, long-term rely on `du` slope + physical/logical ratio |
| Any command that has to read a view tree reports `still uses the flat pre-v2 metadata layout` | `grovefs layout --meta-dir D show` for the shape, `grovefs layout --meta-dir D migrate` to migrate | The old storage root is still v1 flat and no command reads it (§12.4); stop the mounts on that storage and the grovefs processes on this machine before migrating |

### 12.6 Honest words on capacity and reclamation

- On `unlink` (the last link disappearing) the backend best-effort deletes the object/all shards by key, idempotent. **Pages named by no live view tree and no longer booked in the ledger** (orphans) are audited/reclaimed by the offline command `grovefs gc`: by default it only scans, listing the largest orphans, keys the ledger still holds (run `refs rebuild` first), and skipped non-object items; only `--apply` really deletes, and only pages older than `--older-than` (default `24h`), never deleting keys the ledger still holds, and requiring no live mount on this storage and no grovefs process on this machine (`--apply` must be offline; hardlink `import` inherits the source file's old mtime, so the age gate is a hard constraint, not a mere reference).

  ```bash
  grovefs gc --meta-dir D                        # 只扫描:孤儿 / 账本持有 / 跳过项
  grovefs gc --meta-dir D --data-dir /data      # meta 与 data 分家时指向真正存 blocks/ 的根
  grovefs gc --meta-dir D --apply                # 回收 24h 以上的孤儿(离线)
  grovefs gc --meta-dir D --apply --older-than 0 --json   # 立即回收 + 机器可读
  ```

  Guards: if any view tree can't be opened it aborts wholesale (skipping one tree would misjudge all its objects as orphans); keys the ledger still holds are only reported, not deleted; refused when `--data-dir` has zero intersection with all trees (guards against a misconfigured `--meta-dir`/`--data-dir`, `--force` overrides); PG deployments refuse outright (the tree is a schema, bytes belong to data-server). e2e in the `gc` suite (42 assertions: clean DB / out-of-band tree deletion→held→rebuild→orphan→grace→real delete / non-object not deleted / same-name-on-this-DB mount refused, other DB not blocked / wrong-root foolproof / PG and illegal duration refused / `--json` fields).

  **The form common to both backend tiers is the remote one**: the server reads its own tree (SQLite/PG), bytes enumerated toward the data-server — the operator's machine needs neither `meta/` nor `blocks/`, and orphan reclamation on a PG deployment goes through it:

  ```bash
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100,10.0.0.5:50101            # 只扫描
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100 --apply --older-than 24h   # 真删(离线)
  ```

  The same semantics as the local form (`--older-than`/`--force`/`--json`/wrong-root foolproof all judged server-side), but two "absence" facts must be stated rather than assumed: *silence proof only recognizes the server's own sessions* — what `--apply` refuses is "this server still serves a live session" or "these views are still mounted on this machine", returning `gc --apply refused: this deployment is in use — <view> (N live session(s) on this server); …; stop those mounts first (reclaim must run offline, and a mount on another machine is not visible from here, so stop it too)`; it cannot see mounts on another machine, so the second half of that sentence is an operator instruction, not a guarantee. `--data-dir` and `--control-server` are mutually exclusive (the bytes aren't on this machine). e2e in the `gcremote` suite (44 assertions, both tiers: match/orphan count, grace period enforced via the data plane, session silence proof, wrong-root foolproof, `--json` shape and local/remote flag-combination refusals).
- Turn `du` into three decidable criteria: ① **record slope, not size** (append a dated reading the same weekday each week; only a slope that doesn't match business growth is an anomaly); ② **divide by the logical amount** — `blocks/` is physical bytes, what you see inside a mount is logical bytes, only same-order-of-magnitude is normal. Use `grovefs du` for the logical amount (one batch fetch of all object bytes, tens of milliseconds for tens of thousands of files; `find` inside a mount does per-file stat, two orders of magnitude slower):

  ```bash
  grovefs du --home H --view V --total          # 本地 SQLite 部署:一个参数就够
  grovefs du --view V --meta-server M --data-servers A1,A2 --total   # 条带部署:给全 data-server 列表
  find /mnt/da -type f -printf '%s\n' | awk '{t+=$1} END{printf "logical %.2f GiB\n", t/1073741824}'
  ```

  When the byte source can't be determined, `du` refuses to output a number rather than emitting a version of 0: `--data-servers` > `--data-dir` > metadata root; if the tree comes from meta/PG and neither address is given it errors; when the address-list count doesn't match the object `s<N>` layout's number of servers (incl. a tree with both pre- and post-expansion generations) it reports by direction which addresses to give, and a matching count but **reversed order** also errors without a number (otherwise the whole tree would be measured as a pile of 0s). **Only confirmed-missing objects** count into `warning: N of M object(s) are not in the data source and count as 0 bytes`; timeouts, refused connections, read failures all error without a number, because bytes with no answer aren't 0 bytes.

  ③ **a ratio > 1 has only two causes**: something that should have been deleted wasn't (go confirm in the view that the file was truly unlinked), or bytes shared by import/fork are still held by another view (`grovefs view --meta-dir D list` to see who else references it, only physical reclamation when it reaches zero, §5.3).
- The most common third case is actually an accident: **bypassing the mount to `rm meta/<view>/<view>.db` directly doesn't trigger a backend delete**, objects stay forever in `blocks/` (the reference ledger only overestimates, never wrongly deletes, so it only takes space, loses no data); the reclaim ledger can be reset with `grovefs refs --meta-dir D rebuild`. After resetting, run `grovefs gc --meta-dir D` again and these pages change from `ledger-held` to orphans, only then does `--apply` reclaim them; without rebuilding, `gc` conservatively never deletes.
- SQLite view-DB reclamation is handled inside the engine (lock-table transaction reclamation, hardlink refcount GC); in PG mode it's handed to VACUUM.

**When objects are lost / the data source is wrong, which commands speak up** (ops quick reference; full output and causes in docs.html §11.5):

| Situation | Command / path | Does it speak up? |
| --- | --- | --- |
| Whole object not in the selected root | `du --data-dir`/`--data-servers` | **Yes**: stderr `warning: N of M object(s) are not in the data source and count as 0 bytes` |
| No data source given at all | `du` | **Yes (refuses to measure)**: `no data source to measure: pass --data-servers … or --data-dir …` |
| Address-list count/order mismatches the `s<N>` layout | `du --data-servers` | **Yes (refuses to measure)**: refuses by direction before probing, never calling a layout mismatch object loss |
| A striped object's **middle chunk** truly lost | `du` | **Yes (judged corrupt)**: stderr `warning: N of M object(s) are damaged striped objects and are excluded from the total (their true size is unknown)`, corrupt bytes not counted in the total |
| A striped object's **leading chunk** truly lost (later chunks still present) | `du` | **Yes (judged corrupt)**: enters the `damaged` warning as above (`missing its leading chunk #0 …`), no longer giving a misplaced tiny value |
| A striped object's **middle chunk** truly lost, read through the mount | `read`/`stat` inside a mount | **No (known defect)**: `stat size` truncated to the first lost chunk's offset (e.g. a 40 MiB object reported as 16 MiB), a full read rc=0 with content purely the pre-gap prefix, surviving chunks after the gap also invisible, no `EIO`; only `du` judges corruption |
| Reading a file whose object was lost (after remount / fd cache cleared) | `read`/`stat` inside a mount | **No (known defect)**: `stat size=0`, a full read rc=0 empty content, no `EIO` |
| Object externally `mv`/`rm`'d, but the mount already established | a stale fd inside the mount | **No (and unobservable)**: the data-server caches the old fd by key, only measurable after clearing the cache / restarting |
| Data root can't be opened during `view rm --base` | `view rm --base` | **Yes**: `error: cannot open data root …` + an offline `gc` reclaim hint (stating "tree deleted" and "bytes unreclaimed" separately) |
| Striped objects all remote, no local `blocks/` | `gc --meta-dir D` (audit) | **Yes (not falsely reported empty)**: `objects: 0 local block(s) — N striped layout(s) … use \`grovefs du\` for their bytes` |

> One-line selection: to "notice bytes were lost" use `du` (whole objects not in the data source, list mismatches, striped chunk corruption all speak up; but a mount reading a loss is still silent — an unimplemented item); to "reclaim pages no one references" use offline `gc`; to "reset the ledger" use `refs rebuild`. The mount read itself doesn't serve as integrity verification.

### 12.7 Regression verification after changes

After any upgrade/config change, run the full board (stage list in §10) on a machine with a build environment, or only the planes you care about:

```bash
bash .build-bin/run_all.sh                    # 全板(build 自动前置)
SKIP_BENCH=1 bash .build-bin/run_all.sh       # 纯功能回归,跳过吞吐基准
bash .build-bin/run_all.sh metasrv ha locks   # 只跑指定套件(build 自动补)
```

GREEN verdict: all suites PASS (`pg` auto-SKIPs on a host without PG, not lowering GREEN; `bench` is reference-only and never gates the result).

---

## 13. Full worked case: one storage machine + one read-write + one read-only client (hands-on, every parameter explained)

> Target topology (fixed IPs, copy as-is):
>
> ```
>                      存储服务器 192.168.1.10
>         ┌───────────────────────────────────┐
>         │ data-server :50100   字节存 /data/blocks/       │
>         │ meta-server :50110   权威树存 /data/meta/<view>/   │
>         │          (默认 embedded SQLite3,无需安装任何数据库)    │
>         │ manager   :50055   挂载坐标只从它签发的 roster 解析    │
>         └─────────┬─────────────────┬────────┘
>             ▲ 读写                   ▲ 只读
>  客户端A 192.168.1.11            客户端B 192.168.1.12
>  挂载到 /mnt/da(读写)           挂载到 /mnt/db(只读)
>         两台挂同一个 --view share → 同一棵权威树,A 写 B 秒可见
> ```
>
> The two clients mount **the same view `share`** (sharing one tree), only B is read-only. If A/B should see **two mutually invisible** trees, just change B's `--view` to another name (§2 view isolation).

### 13.0 On "the database is sqlite3" — first clear up a misconception

GroveFS's metadata engine **is SQLite by default**, and it **compiles SQLite's source into the `grovefs` binary** via `rusqlite`'s `bundled` feature — it's an in-process engine, not a database service that needs installing. So:

- You **don't need** to install the `sqlite3` command-line tool or any database software on 192.168.1.10;
- **Don't** pass `--meta-store postgres://…` to `meta-server` (only then does it switch to PG) — not passing it = the SQLite this case wants;
- The authoritative tree's physical form is just a plain file `/data/meta/share/share.db` (+ same-named `-wal`/`-shm` companion files, normal for WAL mode);
- **Don't** use the sqlite3 CLI to open/copy this file under write activity (it collides with a WAL checkpoint); to inspect content, stop writes first, backup flow in §12.3.

### 13.1 Step one: install the same binary on all three machines

Compile on the **build machine** (or the storage machine itself), then copy to each machine:

```bash
# 构建机(只需 Rust 工具链;无需 pkg-config 与 libfuse 开发包,详见 §1)
cargo build --release
# 分发到三台机器并安装
for H in 192.168.1.10 192.168.1.11 192.168.1.12; do
  scp target/release/grovefs $H:/tmp/ && ssh $H 'sudo install -m 755 /tmp/grovefs /usr/local/bin/grovefs'
done
# 每台验证
grovefs --version
```

**Meaning**: GroveFS is a single binary with multiple roles — the same `grovefs` program, the subcommand deciding whether it's the storage end (`data-server`/`meta-server`) or a client (`mount`). All three must be the same version (the handshake strictly validates capability bytes, mixed versions error immediately). The binary statically links SQLite, so clients have **no extra dependency beyond `fusermount`**.

### 13.2 Step two: on the storage machine (192.168.1.10), create directories and the token

```bash
sudo mkdir -p /data                             # 数据基目录;blocks/ 与 meta/ 都在服务首次启动时自动建
sudo mkdir -p /etc/grovefs                      # 配置目录
sudo bash -c 'umask 077; echo "GROVE_TOKEN=$(openssl rand -hex 16)" > /etc/grovefs/grovefs.env'
cat /etc/grovefs/grovefs.env                    # 记下这串 token,下一步 scp 给两台客户端
```

**Meaning and directory layout**: the `/data` disk will eventually look like this —

```
/data/
├─ blocks/            ← 文件的真实字节,一个文件=一个 UUID 命名的对象(或条带分块),按 key 哈希扇出在 256 个桶子目录里
└─ meta/              ← 一个视图一个目录
   └─ share/          ← 视图 share 的树目录
      └─ share.db     ← 它的权威目录树(目录项、权限、data_ref 指针)
```

"Views (tree directories) separated from objects (bytes)" is GroveFS's fundamental design: the hierarchical tree applications see is stored in one `meta/<view>/<view>.db` per tree, the bytes are always in `blocks/`, the two linked by the `data_ref` pointers in directory entries. The token is the cluster admission passphrase: all three machines must carry the same one, and every later `--token` is taken from it.

```bash
# 把同一份 env 分发到两台客户端(也可手工重复 13.2 生成后三处一致——用 scp 最保)
scp /etc/grovefs/grovefs.env 192.168.1.11:/tmp/ && ssh 192.168.1.11 'sudo mkdir -p /etc/grovefs && sudo mv /tmp/grovefs.env /etc/grovefs/ && sudo chmod 600 /etc/grovefs/grovefs.env'
scp /etc/grovefs/grovefs.env 192.168.1.12:/tmp/ && ssh 192.168.1.12 'sudo mkdir -p /etc/grovefs && sudo mv /tmp/grovefs.env /etc/grovefs/ && sudo chmod 600 /etc/grovefs/grovefs.env'
```

### 13.3 Step three: on the storage machine, start the two services by hand (verify before making them resident)

Open two SSH windows, one per service (foreground makes errors easy to see; switch to systemd after verifying):

```bash
# 窗口 1:数据服务。TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token "$TOK"
# 窗口 2:元数据服务
grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token "$TOK"
```

Per-parameter explanation — `data-server`:

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--listen` | `0.0.0.0:50100` | Listen address. **The default is `127.0.0.1:50052` (accepts only the local machine)** — to serve the two clients you must explicitly write `0.0.0.0`; the port number itself is your choice, the manager just has to declare it and the client picks it up from the roster (50100/50110 are this document's conventional values) |
| `--data-dir` | `/data` | Data base directory: the service auto-creates `blocks/` and 256 buckets under it, file bytes stored by UUID as key in `blocks/<2hex>/<key>`. **A comma-separated multi-path list is allowed (`/data,/mnt/d2,...`) = automatic placement across several in-machine disks**: each whole object picks one disk by a hash of its key (the client and `data_ref` are entirely unaware, statfs auto-aggregates capacity). But the root list must be **fixed** for a given store — placement is hash mod number-of-roots, neither the list nor the object records a disk ordinal, so a failed disk is **swapped in place** by an equal-size clone (same path, same position, same root count) and still locates; adding/removing disks or reordering `--data-dir` remaps every key and makes old objects unreachable under the new layout (the placement layer doesn't error, it silently ENOENTs); for expansion, provision multiple roots at once, or merge disks at the OS layer with LVM/mdadm and mount a single-path `--data-dir`. The server only knows this layout, a wrong base directory = data "lost" (really just not looked for) |
| `--token` | the string in grovefs.env | The data-plane handshake (`OP_OPEN`) passphrase. A client not carrying the same string → the connection gets `PermissionDenied`, no retry. Not giving it = LAN-trusted, no auth, only for a fully trusted intranet |
| (optionally `--require-api-key`) | — | Turns data-plane admission into **revocable per-caller API keys** (client `mount --api-key`): only accepts live keys minted by `grovefs apikey create --for mount` (or `any`), logs each connect (incl. refusals) to audit, and revalidates live connections each second — a revoke breaks established connections and makes them self-unmount. Given alongside `--token`, both gates must pass; not given = behavior as before (just still auditing). The key-DB path is set by `--meta-dir <metadata root>`, **defaulting to the first `--data-dir`** (in a split-disk deployment you must point it right; pointing at a directory where no key was minted = admits none) |
| (optionally `--data-access-key HEX`) | — | Requires every `OP_OPEN` to carry a **Data-Access Capability**, verified under this shared HMAC key (the SAME value as the cluster's `meta-server --data-access-key`); an `OP_OPEN` without a valid one is refused (`EACCES`). This closes the hole of a rogue meta-server reusing this honest data-server — a client that cannot mint a valid capability cannot open the data plane even holding the correct token. Independent of `--token`/`--require-api-key`; when several are enabled all must pass. Not given = no capability field on the wire, no check. NOT a per-view ACL (the data plane is a flat KV); see §6.1.1 |

Per-parameter explanation — `meta-server`:

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--meta-dir` | `/data` | **Metadata root** (subcommand parameter): view DBs land in `<meta-dir>/meta/` (i.e. `/data/meta/share/share.db`). Takes precedence over the global `--home` — the latter is the default root; when both are given `--meta-dir` wins |
| `--listen` | `0.0.0.0:50110` | As above, the default `127.0.0.1:50053` accepts only the local machine; the metadata plane takes its own port, not crossing the data plane's 50100 |
| `--token` | the same string | The metadata-plane handshake (`M_OPEN`) passphrase, validated independently of the data plane; recommended to use the same one for easier management |
| (optionally `--require-api-key`) | — | The metadata plane admits by the API key in `M_OPEN` (`mount --api-key ID:SECRET`, or client env `GROVEFS_API_KEY`): it reads the same registry (SQLite: `<meta-dir>/meta/_control/api_keys.db`; PG: `grovefs_control.api_keys`) — **the same registry** as `grovefs mcp --require-api-key` / `grovefs apikey --meta-dir`, separated into entrances by scope; a revoke closes established mounts within the first round-trip (the refusal reason is returned with the frame, the mount self-unmounts). Details in §6 / §3.5.1 |
| (optionally `--data-access-key HEX`) | — | On every accepted `M_OPEN`, mints a Data-Access Capability with this shared HMAC key (hex) into the tail of the ack for the client to replay on each `OP_OPEN` to `data-server`. **Must match the SAME value every `data-server --data-access-key` verifies with** — a client that mounted through THIS meta can therefore open the honest data-server; a rogue/foreign meta without the key cannot sign a valid capability, so its clients are refused. Not given = no capability minted, data admission gated by token/api-key only (unchanged behavior). See §6.1.1 |
| (optionally `--data-access-ttl SECS`) | `0` | Lifetime (seconds) of the capability minted above; only meaningful with `--data-access-key`. `0` (default) = non-expiring, so a long-lived mount is not cut off mid-session across a data-server restart/reconnect; nonzero = each `M_OPEN` stamps `now+TTL`, tightening the replay window at the cost of a remount once it lapses |
| (not passing `--meta-store`) | — | **This is "the database is sqlite3"** — not passing it means embedded SQLite, one `.db` per view; passing `postgres://…` is what switches to PG (and mutually exclusive with HA, see §11.9) |
| (not passing `--role/--peer/--witness`) | — | Single-authority node form: this case does no primary/standby/quorum, `192.168.1.10` down = metadata plane paused (on-disk data not lost). To add HA, extend per §11.8 |

Verification (a fourth machine or another window on this one):

```bash
ss -lntp | grep -E '50100|50110'      # 两行都在,进程名 grovefs
```

The two foreground windows should each print a banner line — `data-server listening on 0.0.0.0:50100 (host <storage-hostname>, 1 data root(s): /data)` and `meta-server listening on 0.0.0.0:50110 (base /data, role Primary)`; the data line echoes the hostname and root list because nodes often share the same root path names — when reading logs, first confirm the host, then the path. For what the full terminal looks like, see §13.11 record ①.


The third unit is the manager — **required**: `mount` no longer accepts hand-typed `--meta-server`/`--data-servers`, so a mount's coordinates resolve only from the manager's signed roster (§4.2). It is asked once, at the moment of mounting, and never sits on the IO path; an outage leaves live mounts untouched and only makes **new** mounts fail-closed. `join.key`/`roster.key` are minted on the first `serve` under `<state-dir>/manager/`, 0600:

```ini
# /etc/systemd/system/grove-manager.service
[Unit]
Description=GroveFS manager (mount bootstrap)
After=local-fs.target
[Service]
ExecStart=/usr/local/bin/grovefs manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```


Declare the topology once at the manager (it lands in `<state-dir>/manager/deployments.json`, which `serve` hot-reloads by mtime — changing the topology needs no daemon restart):

```bash
sudo grovefs manager --state-dir /srv/mgr define --meta 192.168.1.10:50110 --data 192.168.1.10:50100
```

A single deployment stays on the default `default` (a client that omits `--deployment` points at it); only managing several clusters at once needs separate `--name prod` declarations. Inside a declaration the meta list is an **ORDERED failover list** and the data list is the **complete member set** (not a candidate pool).

As for registration: append `--member-id N --register-with 192.168.1.10:50055 --advertise-addr <this host IP:port> --join-key-file /etc/grovefs/join.key` to the end of each of the two ExecStart lines above (servers keep serving when the manager is unreachable, §4.2). Registration only decides whether `manager status` can report who is actually alive; it never changes the roster content — the roster always answers exactly as declared, which is precisely why a declared-but-not-running data-server drags a mount down rather than being skipped. Open one more firewall port 50055 reachable by the nodes and by every client that will mount — the meta/data planes themselves never depend on it.

### 13.4 Step four: make the storage services systemd-resident + firewall

After confirming they start by hand, Ctrl-C the two foreground processes, hand them to systemd (full unit text in §11.4, paths swapped to this case's):

```ini
# /etc/systemd/system/grove-data-server.service
[Unit]
Description=GroveFS data-server
After=local-fs.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target

# /etc/systemd/system/grove-meta-server.service
[Unit]
Description=GroveFS meta-server
After=local-fs.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-data-server grove-meta-server grove-manager   # enable=开机 自起,now=立刻起

# 建应用要挂的视图:视图只能显式创建,名字不存在就挂会被 M_OPEN 拒
sudo grovefs view --meta-dir /data create --view share
```

**Meaning of every setting in the unit**: `EnvironmentFile` keeps the token out of both the unit file and the ps command line (a passphrase on the command line is visible to any same-machine user via `ps aux`); not writing `Type` means simple — the two servers are foreground blocking processes anyway, systemd just watches process liveness; `Restart=on-failure` respawns on crash (OOM/segfault etc. nonzero exits), watch §12.1's log inspection for repeated respawns; `After=local-fs.target` ensures the service starts only after the disk holding /data is mounted.

Firewall (run on the storage machine, opening the two service ports plus the one bootstrap port only to the intranet subnet):

```bash
sudo ufw allow from 192.168.1.0/24 to any port 50100 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 50110 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 50055 proto tcp   # manager: clients must be able to ask for a roster
# firewalld 等价:
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50100 protocol=tcp accept"
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50110 protocol=tcp accept"
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50055 protocol=tcp accept" && sudo firewall-cmd --reload
```

### 13.5 Step five: client A (192.168.1.11) read-write mount

First the pre-check + mountpoint directory, then fetch the roster verification key from the storage machine:

```bash
ls -l /dev/fuse && which fusermount fusermount3   # §11.0 三件套
sudo mkdir -p /mnt/da
# 存储机首次 `manager serve` 会把 roster.key 铸在 /srv/mgr/manager/ 下(0600、root 所有):
ssh 192.168.1.10 'sudo cat /srv/mgr/manager/roster.key' | sudo tee /etc/grovefs/roster.key >/dev/null && sudo chmod 600 /etc/grovefs/roster.key
# no need to loosen the storage machine's copy to 644 just for scp — sudo cat inside the pipe is enough
# it is a symmetric HMAC key: the client uses it to VERIFY, never to sign, so only this one copy is distributed and the manager's stays secret
```

Mount once by hand (`mount` backgrounds by default, returning only when the mountpoint is ready; on failure the background's error detail is echoed back to the terminal):

```bash
TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
sudo grovefs mount --view share \
     --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token "$TOK" -m /mnt/da
# Neither service coordinate is typed on the client — resolving the roster echoes the pair the manager declared (§4.2);
# 看到 "grovefs: mounted view 'share' at /mnt/da (background pid …)" 即成功;另开窗口:
echo hello-$(date +%s) | sudo tee /mnt/da/probe.txt && sudo ls -l /mnt/da
# 验证完 sudo grovefs umount -m /mnt/da 卸载(后台进程随之退出),下一步换 systemd
```

Per-parameter explanation — `mount`:

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--home` (clients **need not pass it**) | — | A mount lands no data locally, so this parameter is unrelated to both the data plane and the metadata plane — the two coordinates resolve ONLY from `--manager`'s signed roster (§4.2); `--home` is only the default storage root of the node-side verbs (`view create`/`gc`/`import`/`apikey`/`du`) and of the two services themselves — commands that reach the metadata root each name it with `--meta-dir`, `--home` serving only as the fallback |
| `--view` | `share` | The tree-selection label: which authoritative view to mount. Unrelated to Linux usernames (§2); only A/B using the same name shares the same tree. `share` must first be created at the center via `grovefs view --meta-dir /data create --view share` (mounting without creating is refused) |
| `--manager` | `192.168.1.10:50055` | The mount-bootstrap address: the client's **only** hand-typed service coordinate, `mount` resolves the two planes below from its signed roster (§4.2); the hand-typed `--meta-server`/`--data-servers` flags were removed from `mount` |
| `--roster-key-file` | `/etc/grovefs/roster.key` | The key (64-hex) that verifies the roster signature; without it the mount is refused — an unverified roster is "whoever answers wins" |
| data address resolved from the roster | `192.168.1.10:50100` (declared by the manager) | Data-plane address: file bytes go over a custom TCP frame to this `data-server`, never touching the local disk (not NFS). Only a multi-address list in the roster enables cross-machine striping; a single address means no striping |
| meta address resolved from the roster | `192.168.1.10:50110` (declared by the manager) | Metadata-plane address: the directory tree is served by the center `meta-server` — this is the root of "multiple machines sharing one tree"; it also subscribes to invalidation pushes (who wrote what, this machine re-fetches on next access immediately). The roster may carry an ORDERED failover list (§11.8) |
| `--token` | the string in grovefs.env | Fed to both the data-plane `OP_OPEN` and metadata-plane `M_OPEN` handshakes; a mismatch = the mount command reports a permission error directly, fail-fast |
| (optionally `--api-key`) | — | The per-caller credential `<id>:<secret>` presented when the server enabled `--require-api-key` (minted by `grovefs apikey create --for mount`); can also be exported as `GROVEFS_API_KEY` so the secret never enters argv. Missing/invalid/revoked/mcp-scope-only = the mount fails fast and prints the specific refusal reason; a revoked key closes the **in-use** mount (§6) |
| `-m` / `--mountpoint` | `/mnt/da` | Mountpoint. **Note it's an option, must carry `-m`/`--mountpoint`, not written as a trailing positional argument** (measured: writing it bare errors `unexpected argument`). The directory is auto-created if it doesn't exist |
| (no `--ro`) | — | Read-write mode (default) |
| (no `--no-cache`) | — | By default freshness relies on meta-server's real-time invalidation push (§4), no need to pay `--no-cache`'s zero-cache cost; only add it when you want to extremely bypass kernel cache |
| (no `--threads`) | — | Default 4 FUSE worker threads dispatching concurrently, each with its own `/dev/fuse` fd; tune up for IO-heavy loads, `0`=single-thread for A/B baselines only |

After verifying by hand, make it resident with systemd (this case's mountpoint isn't named `/mnt/<view>`, so use a fixed unit rather than §11.5's template instance):

```ini
# /etc/systemd/system/grove-mount.service
[Unit]
Description=GroveFS mount share -> /mnt/da
After=network-online.target
Wants=network-online.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStartPre=-/usr/bin/fusermount -u /mnt/da      # 清掉上次异常退出可能留的死挂载;- 表示失败也继续
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token ${GROVE_TOKEN} -m /mnt/da
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now grove-mount
findmnt -T /mnt/da        # 期望:fstype fuse[.passthrough]，源 grovefs:share
```

> To let **other users also access** the mountpoint, add `--o allow_other` to ExecStart and enable `user_allow_other` in this client's `/etc/fuse.conf` (a FUSE convention limit, unrelated to GroveFS).

### 13.6 Step six: client B (192.168.1.12) read-only mount

Nearly the same command as A, only two changes: `-m /mnt/db` and adding `--ro`:

```bash
sudo mkdir -p /mnt/db
TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
sudo grovefs mount --view share \
     --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token "$TOK" --ro -m /mnt/db
```

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--ro` | (boolean switch) | Read-only mount. A double guard: kernel `MS_RDONLY` + filesystem-side check, all write-intent operations (create/write/mkdir/unlink/rename/chmod/truncate…) uniformly `EROFS`. The data/metadata-plane commands are unchanged, this client just declares it won't issue writes |

The resident unit — on machine B the **full content** is as follows (not an abbreviation, copy as-is; if your machine has `fusermount3` at path `/usr/bin/fusermount`, just swap the command name in ExecStartPre):

```ini
# /etc/systemd/system/grove-mount.service   ← 客户端 B(192.168.1.12)
[Unit]
Description=GroveFS mount share -> /mnt/db (read-only)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStartPre=-/usr/bin/fusermount -u /mnt/db
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token ${GROVE_TOKEN} --ro -m /mnt/db
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-mount
findmnt -T /mnt/db        # 期望:源 grovefs:share,挂载选项列里带 ro
```

**Acceptance (verify both together, this is the case's core selling point)**:

```bash
# A(192.168.1.11):
echo hi-$(date +%s) | sudo tee /mnt/da/from_a.txt
# B(192.168.1.12)——写完立刻,不等轮询:
sudo cat /mnt/db/from_a.txt          # 期望:即时看到同内容(实时失效推送)
sudo touch /mnt/db/x                 # 期望:Read-only file system (EROFS)
```

> **Honest boundary — `--ro` is a client-side declaration, not server-enforced**: `--ro` blocks the normal write path on this machine; but if someone on machine B re-runs `grovefs mount` without `--ro` (they have the token), the server doesn't recognize "this machine should be read-only" — the token is cluster admission, the view is tree selection, neither distinguishes hosts. For **server-side hard read-only**, per 13.7 use a `--view`-bound `--ro` key (the server enforces per op, not relying on the client to carry `--ro` of its own accord).

### 13.7 Optional hardening: server-side hard read-only (view-bound / read-only API key)

Idea: give each of the two mounts a **view-bound key** — A a read-write key, B a read-only key (`--ro`), the center server enforcing per op, so read-only no longer depends on the client carrying `--ro` voluntarily. Mint on the storage machine (`--meta-dir /data` pointing at the same metadata root as meta-server, `--for mount` making it open only the mount plane):

```bash
sudo grovefs apikey --meta-dir /data create --label A-rw --for mount --view share          # A:读写 key
sudo grovefs apikey --meta-dir /data create --label B-ro --for mount --view share --ro     # B:只读 key
sudo grovefs apikey --meta-dir /data list                                                  # 确认两行(view=share,rw/ro)
```

Then add `--require-api-key` to both meta-server and data-server, both clients' ExecStart present their own key via `--api-key`, and restart the services (the two changed lines in full, directly replacing the original ExecStart):

```ini
# 服务端(存储机)两服务都开门:meta-server ... --require-api-key;data-server ... --require-api-key --meta-dir /data

# 客户端 A 的 /etc/systemd/system/grove-mount.service 改成:
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --api-key ${GROVE_A_KEY} -m /mnt/da

# 客户端 B 的改成(--ro key 已由服务端强制,客户端 --ro 可留作双保险):
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --api-key ${GROVE_B_KEY} --ro -m /mnt/db
```

```bash
# 两台客户端各自改完后:
sudo systemctl daemon-reload && sudo systemctl restart grove-mount
```

| Parameter | Meaning |
| --- | --- |
| `apikey create --for mount --view share [--ro]` | Mints a mount credential that can only open the `share` view; `--ro` makes it read-only. **Once a key is bound to a view, using it to open another view is refused with `E_AUTH` at `M_OPEN`** |
| `--ro` (key side) | A read-only key: reads pass, every write-intent op is **dynamically re-checked per call** server-side and returns `E_NOTPERM` (the client sees Operation not permitted; the data-plane content write gets `EROFS`) — not a fault, an auth refusal |
| `--api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) | Presents this key at `M_OPEN`/`OP_OPEN` on mount (env keeps the secret out of argv). A revoke immediately breaks that mount and makes it self-unmount, other keys unaffected |

Verification: even if machine B drops the client `--ro` and remounts, as long as it uses the `--ro` key the write is still refused by the server; after `apikey revoke` on A's key, A's live mount is refused on its first server round-trip (per-op revalidation).

### 13.8 Step seven: case-specific acceptance checklist + rollback

```bash
# ① 存储机:两服务 active 且开机自起
ssh 192.168.1.10 'systemctl is-enabled grove-data-server grove-meta-server && systemctl is-active grove-data-server grove-meta-server'
# ② A 写 → B 秒读(失效推送);B 写 → EROFS/EPERM。见 13.6 验段
# ③ 字节确实落在存储机,不碰客户端盘
ssh 192.168.1.10 'find /data/blocks -type f | head; sqlite3 --version 2>/dev/null || echo "无需 sqlite3,正常"'
#    注:对象按 key 哈希分布在 blocks/ 的 256 个桶里,用 find 看对象;share.db 是 SQLite 库文件,勿在有写活动时直接打开/拷贝
# ④ 中心重启后客户端自愈
ssh 192.168.1.10 'sudo systemctl restart grove-data-server'   # 之后回 A 继续读写,自动重连
# ⑤ 干净卸载与重挂(A 机同理)
ssh 192.168.1.12 'sudo systemctl stop grove-mount && findmnt -T /mnt/db | grep -q grovefs && echo 残留 || echo 已卸载'
# ⑥ Mount bootstrap (required): grove-manager is active, and `grovefs manager status --server 192.168.1.10:50055 --join-key-file /srv/mgr/manager/join.key`
#    lists EVERY member `define` declared (a roster is the declared set, not the healthy subset). A manager outage only blocks NEW mounts; live mounts keep running.
```

All green means the deployment is complete. Rollback is simple too: on both clients `systemctl stop grove-mount` → on the storage machine `systemctl disable --now grove-manager grove-data-server grove-meta-server` (stop the manager first: without it new mounts cannot come up anyway, so stopping it first separates "bootstrap down" from "service down"); whatever is on the disk under `/data`, if not deleted, is a complete backup (§12.3), and to truly start over, just format the data disk (after confirming it's backed up).

Daily maintenance (inspection keywords/backup/upgrade/failure tables) all applies from **§12**, IP mapping: storage machine = 192.168.1.10, write end = 192.168.1.11, read-only end = 192.168.1.12.

### 13.9 Files and commands per machine, in one table (follow it, never flip back)

**Files to create on the storage machine 192.168.1.10 (4 total):**

| File | Which section has the content |
| --- | --- |
| `/etc/grovefs/grovefs.env` | generated by the third command in §13.2 |
| `/etc/systemd/system/grove-data-server.service` | the full first ini block in §13.4 |
| `/etc/systemd/system/grove-meta-server.service` | the full second ini block in §13.4 |
| `/etc/systemd/system/grove-manager.service` | the mount-bootstrap block in §13.3 (required: a mount's coordinates resolve only from it) |

Execution order (storage machine): `mkdir` (§13.2) → start both services by hand to verify (§13.3) → Ctrl-C → write the three units and declare the topology once with `manager define` (§13.3) → `daemon-reload` + `enable --now` (§13.4) → three firewall rules: 50100/50110/50055 (§13.4). Optional hardening adds one step: the view-bound / read-only key, two commands (§13.7).

**Files each of client A 192.168.1.11 / client B 192.168.1.12 must create (3 each, both machines isomorphic):**

| File | Which section has the content |
| --- | --- |
| `/etc/grovefs/grovefs.env` | distributed by scp at the end of §13.2 (no need to rebuild, the same one as the storage machine) |
| `/etc/grovefs/roster.key` | minted by the storage machine's first `manager serve` at `/srv/mgr/manager/roster.key`; scp it down (the §13.5 pre-check block) |
| `/etc/systemd/system/grove-mount.service` | A uses §13.5's full ini; B uses §13.6's full ini |

Execution order (each client): the three-piece pre-check + `mkdir /mnt/xx` (§13.5/13.6) → mount once by hand to verify (§13.5/13.6) → Ctrl-C → write the unit → `daemon-reload` + `enable --now` → confirm with `findmnt`. If you did §13.7: swap ExecStart to the corresponding line in §13.7, then `daemon-reload + restart` again.

### 13.10 This case's parameter cheat sheet (one table for everything)

| Machine | Command | What it does |
| --- | --- | --- |
| storage | `grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token $TOK` | Byte-storage plane: `--listen` binds all interfaces, `--data-dir` base dir (auto-creates `blocks/` under it), `--token` the data-plane passphrase |
| storage | `grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token $TOK` | Metadata plane (default SQLite): `--meta-dir` points at the metadata root (view DBs under its `meta/`), not passing `--meta-store` = SQLite, not passing `--role/--peer` = single node |
| storage | `grovefs view --meta-dir /data create --view share` | Explicitly build the base view: `share` mounted without being created is refused at `M_OPEN`; the DB lands at `/data/meta/share/share.db` |
| client A | `grovefs mount --view share --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token $TOK --run-as-uid 1001 -m /mnt/da` | Read-write mount: `--view` selects the tree, `-m` is required and can't be a positional argument, not writing `--no-cache` keeps freshness via the default push |
| client B | same as A, changed to `--ro --run-as-uid 1002 -m /mnt/db` | Read-only mount: `--ro` blocks the local normal write path; combined with a center `--ro` key for this client = server-side hard read-only |
| storage | `grovefs apikey --meta-dir /data create --for mount --view share [--ro]` | View-bound / read-only key: the key can only open the `share` view (opening others is refused with `E_AUTH` at `M_OPEN`); `--ro` makes each write-intent op refused server-side (`E_NOTPERM`→`EPERM`), `revoke` takes effect immediately |
| storage | `grovefs manager --state-dir /srv/mgr define --meta 192.168.1.10:50110 --data 192.168.1.10:50100` (then `serve --listen 0.0.0.0:50055`) | Mount bootstrap (§4.2): `define` declares the authoritative topology (hot-reloaded by mtime), `serve` mints the join/roster keys and answers lookups — a roster comes back exactly as declared, never narrowed to a healthy subset |
| client A/B | `grovefs mount --view share --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token $TOK -m /mnt/da` | A signed roster replaces the two hand-typed coordinates: one lookup at mount time, the manager proxies no traffic afterwards, so its later outage only makes NEW mounts fail-closed |

### 13.11 Full terminal transcript (real measured output, unchanged verbatim)

Capture conditions: grovefs 0.1.0 @ Ubuntu 20.04, a **one-host four-process equivalent topology** — the manager, the two services and both client mounts all live on one machine and only ever talk over `127.0.0.1`. The ports `15055/15100/15110` in the transcript are the sandbox's real ports, corresponding to the three-machine case's `192.168.1.10:50055/50100/50110`; `/tmp/grove_case/…` corresponds to the storage machine's `/data` and the clients' `/mnt/da|/mnt/db`. Every segment below is real terminal output (incl. Chinese error messages); the token and the apikeys are one-time random values, already voided, and a key's plaintext is written `<id>:<secret>` in the transcript. Rerun script: the repo's `.build-bin/run_case_demo.sh` — the output matches verbatim, only the token, the key ids/secrets and the timestamps take new values each run.

**① Storage machine: declare the topology, start the manager, start both services, create the view explicitly**

```console
$ grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2
ed68c256d28e807312f77515aee6699b
$ grovefs --home /data manager define --meta 127.0.0.1:15110 --data 127.0.0.1:15100   # 拓扑只在这里声明一次
deployment 'default' declared in /tmp/grove_case/gm/manager/deployments.json
  meta (failover order): 127.0.0.1:15110
  data (complete member set): 127.0.0.1:15100
  declaration epoch 1
  next: every address above must be a RUNNING server started with --register-with — a roster ships the full declared set (a stripe's members are not a candidate pool), so one declared-but-down data-server fails mounts instead of being skipped. `manager status` shows who has actually registered.
$ grovefs --home /data manager serve --listen 127.0.0.1:15055   # 前台
manager: join key   /tmp/grove_case/gm/manager/join.key — copy to every node started with --register-with (authorizes a registration; a client never holds it)
manager: roster key /tmp/grove_case/gm/manager/roster.key — copy to every client as `mount --roster-key-file` (signs the answer; symmetric, so keep the manager's copy secret)
manager listening on 127.0.0.1:15055 (host-readable state /tmp/grove_case/gm/manager/deployments.json)
manager: deployment 'default' (epoch 1) — meta [127.0.0.1:15110] data [127.0.0.1:15100]
manager: rosters valid 300s after issue; registrations lapse after 90s of silence; lookups answer from the declaration, live or not
manager: neither key leaves this process — a node proves itself with the JOIN key, a client checks the ROSTER key; heartbeat is ~30s, and a node that cannot reach this service keeps serving its own mounts
$ grovefs data-server --listen 127.0.0.1:15100 --data-dir /tmp/grove_case/data --token "$TOK"   # 前台
data-server listening on 127.0.0.1:15100 (host <本机主机名>, 1 data root(s): /tmp/grove_case/data)   # on three machines use 0.0.0.0:50100; this line also echoes the hostname and the root list — root paths are usually named identically on every node, so read the host first, then the path
$ grovefs meta-server --meta-dir /tmp/grove_case/data --listen 127.0.0.1:15110 --token "$TOK"   # 前台
meta-server listening on 127.0.0.1:15110 (base /tmp/grove_case/data, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view --meta-dir <dir> create --view <name>`); no empty tree is ever auto-created
$ ss -lntp | grep -E "15100|15110|15055"
LISTEN    0         128              127.0.0.1:15100            0.0.0.0:*        users:(("grovefs",pid=3573218,fd=3))
LISTEN    0         128              127.0.0.1:15110            0.0.0.0:*        users:(("grovefs",pid=3573234,fd=3))
LISTEN    0         128              127.0.0.1:15055            0.0.0.0:*        users:(("grovefs",pid=3573207,fd=3))
$ ls /tmp/grove_case/data                        # 两个服务启动时自动把骨架建好了
blocks
meta
$ ls /tmp/grove_case/data/blocks | wc -l         # the 256 buckets are pre-built at startup, not waited for until the first file
256
$ ls /tmp/grove_case/data/blocks | head -3 ; find /tmp/grove_case/data/blocks -type f | wc -l
00
01
02
0                                                # no file written yet, so zero objects in the buckets is correct
$ grovefs view --meta-dir /tmp/grove_case/data create --view share   # views are created explicitly: M_OPEN refuses a name with no tree
created BASE view 'share' at /tmp/grove_case/data/meta/share/share.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
```

**② Client A: read-write mount + write files**

```console
$ grovefs mount --view share --manager 127.0.0.1:15055 --roster-key-file /data/manager/roster.key --token "$TOK" -m /tmp/grove_case/mnt_da   # a remote mount needs no --home
manager 127.0.0.1:15055: deployment 'default' epoch 1 resolved meta [127.0.0.1:15110] data [127.0.0.1:15100] (signature verified, valid for 300s more)
using remote metadata service at 127.0.0.1:15110
using remote data service at 127.0.0.1:15100
mounting view 'share' at /tmp/grove_case/mnt_da (fully remote: no local store)
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB   # the command line gives not a single coordinate: what the manager answers is a SIGNED roster, and a roster whose signature fails is never mounted
$ findmnt -T /tmp/grove_case/mnt_da
TARGET                 SOURCE        FSTYPE OPTIONS
/tmp/grove_case/mnt_da grovefs:share fuse   rw,nosuid,nodev,relatime,user_id=1005,group_id=1005
$ echo hi-1700000000 > /tmp/grove_case/mnt_da/from_a.txt
$ cat /tmp/grove_case/mnt_da/from_a.txt
hi-1700000000
$ mkdir /tmp/grove_case/mnt_da/docs && echo ok > /tmp/grove_case/mnt_da/docs/n1.md
$ ls -lR /tmp/grove_case/mnt_da
/tmp/grove_case/mnt_da:
总用量 5
drwx------ 2 fs fs 4096 11月 22 11:02 docs
-rw------- 1 fs fs   14 11月 22 11:02 from_a.txt

/tmp/grove_case/mnt_da/docs:
总用量 1
-rw------- 1 fs fs 3 11月 22 11:02 n1.md
```

After writing, look at the side effects back on the **storage machine** — two files means exactly two objects (each lands in one of the 256 bucket directories by the FNV-1a hash of its key, the bucket name being two hex digits), and the metadata lives at `meta/<view>/<view>.db` (one directory per physical tree, see §13.1.1):

```console
$ find /tmp/grove_case/data/blocks -type f
/tmp/grove_case/data/blocks/40/9ee71f59-bf58-489a-b957-387f7ac04201
/tmp/grove_case/data/blocks/53/06c14bea-56c7-4f27-a64f-a00fea4ed0f7
$ ls /tmp/grove_case/data/meta
_control
share
```

`share/share.db` (plus its WAL companion files) is this tree's metadata; `_control/` holds the control-plane databases — keys, definitions, the ledger.

**③ Client B: read-only mount, can read, can't write**

```console
$ grovefs mount --view share --manager 127.0.0.1:15055 --roster-key-file /data/manager/roster.key --token "$TOK" --ro -m /tmp/grove_case/mnt_db
manager 127.0.0.1:15055: deployment 'default' epoch 1 resolved meta [127.0.0.1:15110] data [127.0.0.1:15100] (signature verified, valid for 300s more)
using remote metadata service at 127.0.0.1:15110
using remote data service at 127.0.0.1:15100
mounting view 'share' at /tmp/grove_case/mnt_db (fully remote: no local store, read-only)
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
$ findmnt -T /tmp/grove_case/mnt_db                # 注意 OPTIONS 里是 ro
TARGET                 SOURCE        FSTYPE OPTIONS
/tmp/grove_case/mnt_db grovefs:share fuse   ro,nosuid,nodev,relatime,user_id=1005,group_id=1005
$ cat /tmp/grove_case/mnt_db/from_a.txt           # A 刚写的,这边直接能读
hi-1700000000
$ ls /tmp/grove_case/mnt_db
docs
from_a.txt
$ touch /tmp/grove_case/mnt_db/x
touch: 无法创建 '/tmp/grove_case/mnt_db/x': 只读文件系统
$ echo bad > /tmp/grove_case/mnt_db/new.txt
bash: /tmp/grove_case/mnt_db/new.txt: 只读文件系统
$ mkdir /tmp/grove_case/mnt_db/zz
mkdir: 无法创建目录 “/tmp/grove_case/mnt_db/zz”: 只读文件系统
```

**④ Cross-client instant visibility (real-time invalidation push, not polling)**

```console
# 客户端 A 上:
$ echo "round2-$(date +%s)" > /tmp/grove_case/mnt_da/round2.txt
# 客户端 B 上立即 cat,没刷任何东西:
$ cat /tmp/grove_case/mnt_db/round2.txt
round2-1795345375
```

**⑤ Storage machine: view-bound / read-only key hardening + clean wrap-up**

Upgrading read-only from "the client carries `--ro` voluntarily" to "the server enforces per op": mint one key bound to the `share` view for the write end and one for the read end (`--meta-dir` is named exactly like meta-server's, pointing at the same metadata root; the read-only one additionally carries `--ro`). The commands are the same set as §13.7, only pointing the metadata root at this case's `/tmp/grove_case/data`:

```console
$ grovefs apikey --meta-dir /tmp/grove_case/data create --label reader --for mount --view share --ro
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id <id>` reads it back):
  id:    <id>
  owner: <none>
  label: reader
  scope: mount
  view:  share
  rw:    ro (read-only)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   <id>:<secret>

Present it to a `--require-api-key` server as
  grovefs mount --manager ... --roster-key-file ... --api-key '<id>:<secret>' -m /mnt/x
(or export GROVEFS_API_KEY='<id>:<secret>' so it stays out of the command line).
Revoke later with: grovefs apikey --meta-dir /tmp/grove_case/data revoke --id <id>
$ grovefs apikey --meta-dir /tmp/grove_case/data create --label writer --for mount --view share
…  label: writer  scope: mount  view: share  rw: rw (read-write)  key: <id>:<secret>
$ grovefs apikey --meta-dir /tmp/grove_case/data list
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   <id>               1be1…25bb    -                writer           mount   share        rw   2026-11-22T11:02:55.400045375+00:00 -                         never
2   <id>               49bf…91db    -                reader           mount   share        ro   2026-11-22T11:02:55.394175699+00:00 -                         never
```

> Note in this document (not command output): a key bound to one view can only open that view; `--ro` is enforced per op server-side once the server runs with `--require-api-key` — the credential is the per-caller gate. What `list` prints is the head-and-tail fingerprint stored with the key, never the secret (the plaintext is not persisted, see §19).

Clients present the key with `--api-key '<id>:<secret>'` (or `export GROVEFS_API_KEY`) then remount; after B switches to the `--ro` key, even remounting without the client `--ro`, writes are still refused by the server (`E_NOTPERM`→`EPERM`). The server must add `--require-api-key` to meta-server / data-server (see §13.7). Wrap-up unmount (real transcript; `grovefs umount -m` also reaps the backgrounded mount process, so the system `umount` is not needed):

```console
$ grovefs umount -m /tmp/grove_case/mnt_da; grovefs umount -m /tmp/grove_case/mnt_db
unmounted /tmp/grove_case/mnt_da
A umount ok
unmounted /tmp/grove_case/mnt_db
B umount ok
```

> Three systemd-related things not captured in the transcript (boot-time autostart after `enable --now`, auto-respawn on crash, token re-injected from env after reboot) are standard systemd behavior, untested in this sandbox without systemd — on a real machine, run items ①④⑤ of the §13.8 acceptance checklist to verify.

---

## 14. Advanced: customizing the data directory and how clients display

> Answering two frequent questions: ① the server has many disks / a lot of data, how to place the data directory and how existing data gets in; ② for the same batch of data, can different clients see different organizational shapes.

### 14.1 The server's data directory: two independent knobs

| Knob | What it stores | Where it can go |
| --- | --- | --- |
| `data-server --data-dir DIR` | Data base directory: on startup auto-creates `<root>/blocks/` and 256 buckets, bytes stored by key hash in `blocks/<2hex>/<key>`; a comma-separated multi-path list gives multi-disk automatic placement (each whole object picks a disk by key hash; the root list must be fixed, add/remove disks remaps every key) | Any path / any mountpoint, created if it doesn't exist |
| `meta-server --meta-dir DIR` | Authoritative view tree `<meta-dir>/meta/<view>/<view>.db` (the control plane additionally has `meta/_control/`; the SQLite-backend form, on the PG backend both go entirely into `grovefs_control`) | Any path, may be a different disk from the object disk |

Example of putting large data on a separate big disk (on 192.168.1.10 /data is only a small system disk, the object disk is separately mounted at /mnt/bigdisk):

```bash
# /etc/fstab 或手工挂好大盘后,把 unit 的 --data-dir 改掉并声明挂载依赖:
# ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /mnt/bigdisk --token ${GROVE_TOKEN}
# [Unit] 加两行,保证大盘挂载完成才起服务(否则服务会在空盘上开局):
#   Requires=mnt-bigdisk.mount
#   After=mnt-bigdisk.mount
sudo systemctl daemon-reload && sudo systemctl restart grove-data-server
```

**Relocating an existing object directory** (safe, because a key is just a UUID, containing no path information):

```bash
# ① 客户端停写(建议 systemctl stop grove-mount)② 停两服务 ③ 整体 rsync ④ 改 --data-dir ⑤ 拉起
rsync -a /data/blocks/ /mnt/bigdisk/blocks/
```

> **Exception — relocating striped files**: the stripe layout is self-describing in `data_ref` (`s<N>:<stripe>[:<fp>]:<uuid>`) — with `--member-id` configured, which machine chunk i lands on is decided by member **identity**, the list order can be anything, already-written striped files are still found; without it, it's decided by the **order** of the data list resolved from the roster (i.e. the order listed in `manager define --data`, `servers[i%N]`), so if a multi-address list's order changes during migration, already-written striped files lose their chunks. In both modes the **server count** must stay consistent. A single-address `--data-dir` relocation has no such issue.

**Multiple disks / capacity expansion**: two legitimate paths ① one data-server mounts several disks: `--data-dir /data,/mnt/d2,...` a comma-separated list of storage roots, whole objects auto-placed onto one of them by key hash, `df` (statfs) auto-aggregates total capacity — but the root list must be **fixed** for a given store (placement is hash mod number-of-roots, add/remove disks remaps every key and makes old objects unreachable under the new layout), so provision multiple roots at once; ② start several data-servers + client `--data-servers a,b,c` striping (only ≥16MiB is chunked, small files land on one server; only a bandwidth gain when the link is unsaturated, see the honest note in §5.2). You can also merge several disks into one filesystem at the OS layer with LVM/mdadm and mount a single-path `--data-dir`. There is no per-directory quota or storage tiering (unimplemented).

> **Swapping a disk doesn't affect locating; only add/remove/reorder does**: the landing position is recomputed on the spot as a pure function of the key (hash mod number-of-roots), neither the `--data-dir` list nor the object records "which disk number". So a failed disk **swapped in place** by an equal-size clone (same path, same position in `--data-dir`, same root count) still locates data normally — the disk's content and its internal 256-bucket fan-out are all whole-object key-addressed copies, no index to rebuild. Conversely, once you **add, remove, or reorder `--data-dir`**, the modulus changes, every key remaps, old objects addressed under the new layout become unreachable; the placement layer does no remap guard, it only silently ENOENTs (cross-server striping refuses the mount outright and reports `refusing to remap` when the resolved data list's count/order doesn't match). Operationally treat the `--data-dir` list as immutable config: equal-clone in-place swap is fine, in-place expansion/reorder is not, expansion goes via "full reload onto a new fixed layout".

### 14.2 How existing bulk data gets in: two legitimate paths, prefer zero-copy `grovefs import`

**Preferred — register in place without moving** (same filesystem, §3.3): zero byte copy, completes in seconds (only builds links + directory entries, doesn't read content), timestamps/permissions preserved faithfully:

```bash
# 在存储机上(能同时触到 meta 根与 blocks 根的机器)一条命令登记整棵树
grovefs import --meta-dir /data/cluster --view legacy --source /data/legacy --prefix /legacy
# 跨设备的源?import 自动退化为流式拷贝并如实计数(仍可用,只是不再零字节)
```

**Alternative — pour in via the FUSE write path** (when you need to restructure/clean data, PG backend, or want large files to enjoy ≥16MiB cross-server striping):

```bash
# 在任一台已挂载的客户端(或存储机自己也挂一份)分批灌入
rsync -a /srv/legacy/ /mnt/da/imported/           # 目录层级、权限随写自动成型
# 单文件 ≥4MiB 自动分片并发写; the roster-resolved data list being multi-address plus ≥16MiB auto-stripes, import needs no special tuning
# 导完抽验:中心对象目录在增长,视图树里能看到层级
ls /data/blocks | head && find /mnt/da/imported -maxdepth 2 | head
```

The trade-off between the two paths: import is zero-copy but imported objects are **single-file objects** (no striping), and it converges toward online mounts via TTL rather than event push; rsync pouring keeps an extra copy of bytes but newly-written large files can stripe, new files are immediately visible via push. Note (both are implementation facts, not boasting import speed): the throughput ceiling for FUSE writes is the frame link and the center disk; for TB-level bulk data, batch during off-peak; every million files grows `meta/<view>/<view>.db` correspondingly (one node row + directory entry per row), which is normal.

### 14.3 Which mechanisms decide a client's "display method"

| Mechanism | Effect | Fits |
| --- | --- | --- |
| **Different `--view` = different tree** | Two entirely independent hierarchies, mutually invisible; the most thorough way to get different organizational shapes | A/B see fundamentally different things |
| **Same `--view` = one tree** | One copy of bytes, all mountpoints see the exact same authoritative tree (real-time sync); B read-only uses `--ro` or a read-only key (§13) | A/B share the same batch of data |
| **`mount --layout` display lens (§3.1)** | One tree, each mountpoint does its own hide/only/map/link — partial display, path reorganization, bytes untouched, other mounts unaware | Customize the display shape without needing two copies of data |
| **Server-side derived view (§3.2)** | The same lens rules **named and stored centrally on the server** (`meta/views/`), clients just `mount --view <derived name>`, same name same shape across machines, a read-only key can be assigned by name (§13.7); **a derived (virtual) view mount is always read-only** (write via the base name) | Team-shared display contract, no rule-file distribution; distribute a read-only shape |
| **Server-side pure alias (§3.4)** | A public name maps 1:1 to a physical view (`alias <real name>`, no rules): the alias mount is **whole-tree writable**, the real name thereafter **is not a mount entry** (guessing it still can't mount in) | Name governance: public names rotate freely, storage identity not exposed (on leak, `--retire` rotates atomically in one command: old name retired, real name refused for mounting throughout); orthogonal and complementary to shape governance (derived) |
| **Server-side fork view (§3.4.1)** | `view fork` copies a base view's directory entries into an **independent metadata tree** (preserving ino/hardlinks, zero byte copy): structure writable (mkdir/rename/mv/rm), content read-only (`EROFS`); deletion only removes this view's `meta/<name>/<name>.db`, the base view and bytes unaffected | An "organizational sandbox" free to reshape without hurting the base view; content still shares one copy, bytes not copied |
| **Multiple in-tree entries: hardlink + symlink** | One copy of bytes arranged into several organizations in the same tree (by year / by type / shortcut entries) | A "multi-view directory" within one view |

**Hardlink = one copy of bytes, multiple names** (the most useful trick in this case):

```bash
cd /mnt/da
mkdir -p by_year/2026 by_type/docs
ln imported/report.pdf by_year/2026/report.pdf    # 硬链接:两个目录项指同一个对象,字节只存一份
ln -s by_year recent                               # 软链接:目录级快捷入口(只是一个路径字符串)
rm imported/report.pdf                             # 安全:硬链接还在,对象不会被删
ls by_year/2026/ by_type/  recent/                 # 从任一个入口打开都是同一份数据,B 机同样可见
```

GC semantics (source-guaranteed): an object is deleted from `blocks/` only when **the last directory entry referencing it** disappears; refcount read/write completes within the same transaction, concurrent hardlinks cause neither wrong deletion nor leaks. Symlinks are the opposite — they hold no reference, and go dangling when the pointed-to file is really deleted; also **don't point a symlink at another view's path** (under whosever mountpoint, the other tree simply isn't visible).

### 14.4 Existing paths and unimplemented list for "B should only see a portion"

Four landable approaches exist:

1. **`mount --layout` display lens (§3.1), or its server-centrally-defined version — the derived view (§3.2)**: add `only /by_public` or `hide /photos/private` to B's mount (a local rule), or after an admin `grovefs view define`, B directly `mount --view <derived name>` (no rule-file distribution, same name same shape across machines, a read-only key assignable by name) — one authoritative tree, B's mountpoint renders only the permitted shape, **zero byte copy**; but both are display customization **not hard security boundaries** (B without the lens / mounting the base name directly still sees the whole tree, what blocks them is the token / key). The difference is write permission: **a derived (virtual) view mount is always read-only** (the shape is governed server-side, writes can only go via the base name), whereas a client-owned `--layout` lens is still writable (writing real entries directly) — if B needs a writable shape use `--layout`, if you only distribute a read-only shape use a derived view.
2. **Same view + directory convention**: the read-only end B uses symlinks/habitual entries to only enter publishing directories like `by_public/` — simple, but **a convention, not enforced** (B can `cd` anywhere).
3. **Split into two views + pour twice**: `share_pub` holds only public content (at import point `grovefs import --source` at a subdirectory / or rsync with `--include/--exclude` to filter), B mounts this view — **hard isolation, true read-only (with a read-only key)**, the cost is the public portion having an extra copy of bytes (writable shared objects still lack this feature). Choose this for true security isolation.
4. **fork view (§3.4.1)**: `view fork --name L --base R` copies R's directory entries into L's **independent metadata tree** — zero byte copy (objects shared, booked in the cross-view ledger), B freely `mkdir`/`mv`/`rm` to organize its own tree shape, but **content is read-only** (uploading/changing content `EROFS`); deleting L removes only its own metadata, R and the bytes unharmed. Fits "want a copy tree that's freely organized, mutually undisturbed, and doesn't write new content into it".

**Unimplemented, don't expect it in the current version** (honestly noted):

- Subtree mount (`--subpath`/`--prefix`): limiting the mountpoint to a directory inside the view — none (`--layout only/hide` is display pruning, not a path chroot);
- Cross-view **writable** hardlinks / sharing the same object — none, `ln` is in-tree only, writing data across views means two copies of bytes; cross-view byte sharing is only via the **content-read-only** fork view (§3.4.1, structure organizable, uploading/changing content `EROFS`);
- **Continuous automatic sync** of the source directory after import (watcher / periodic rescan) — none, `grovefs import` is a manual idempotent incremental command, rerunning adds new files; rename/replace-write external updates are also followed up by rerunning (§3.3);
- **Remote direct import** — `grovefs import` only takes `--meta-dir`/`--data-dir` (local paths), **not `--meta-server`/`--data-servers` remote addresses**, there's no "pour a directory into a remote cluster from another machine" entry. Reason: import writes the authoritative `meta/<view>/<view>.db` and `blocks/` directly (zero-copy = source and blocks hardlinked on the same disk), and the remote protocol has no corresponding "batch-commit a whole tree" operation. Cross-machine bulk ingest of existing data goes the §14.2 alternative path (an already-mounted point's `rsync -a` pouring via the FUSE write path, large files can still stripe), or run import on the storage machine;
- **splice/zero-copy read-back** — the local `LocalBackend`'s large-block read-back has no zero-copy channel. Reason: the fuser 0.18 currently depended on has only `writev` on the reply side (copying a userspace buffer into `/dev/fuse`), `ReplyData` only takes `&[u8]`, no `splice`/`sendfile`/`vmsplice` entry — to do it would require forking fuser to take over the pipe ourselves. The measured gain ceiling doesn't cover this complexity and risk: warm 64MiB read 1318MB/s vs same-disk native ext4 1402MB/s (a ~6% gap), cold 1GiB read 2.02s ≈ ext4 2.00s (a ~1% gap, disk-speed dominated), and remote/striped backends gain nothing; compounded by page-cache consistency and `--no-cache` strong-consistency mount correctness risks. So not done, kept as negative space.
- per-directory enforced permissions — directory-level permissions unimplemented (`--layout` can do directory-level display pruning, but as noted it doesn't constitute enforced isolation).

**Documentation convention** (README / USAGE / docs.html share one rule): state only the current version's behavior — don't write removed parameters, environment variables or switches, don't do before/after version comparisons or historical baseline numbers, don't write change-narration like "added / now supports / consistent with the old version / fell back to old behavior" (evolution history belongs in git commit records). Real fallback semantics within the current version, and the unimplemented list above, are not in this category and are kept as normal.

---

## 15. Scenario case collection (pick one by purpose, change the addresses and copy as-is)

Earlier sections are organized by **mechanism**, this section by **what you want to do**, giving the shortest executable sequence. Conventions:

- `G=grovefs` (swap for your binary path, e.g. `/usr/local/bin/grovefs`); addresses uniformly written as `10.0.0.10`, ports `50100` (data) / `50110` (metadata) / `5432` (PG) — swap for yours;
- The PG sections were **run command-by-command** on a local PostgreSQL 12 (creating the DB, starting services, mounting, inspection SQL, `pg_dump` + restore to a new DB, `view rm --base`'s transactional ledger teardown + `DROP SCHEMA` + byte reclamation, `refs rebuild`'s two directions (the whole chain run through by `.bneck/pg_view_teardown.sh`'s 44 assertions on a real FUSE mount), and every refusal wording quoted below), and cross-mount `rsync -aH` was also verified for hardlinks and increments between two mounts on the same host; the measured port was 5433, the text writes the standard 5432, the only difference being the port;
- Each scenario ends with an **acceptance command**, get the expected result before moving on, don't pile them all up to check at the end;
- Mechanism detail isn't repeated here, the tables give cross-references.

### 15.0 Scenario navigation

| What I want to do | See this section | Need PG? |
| --- | --- | --- |
| See the effect on one machine in 5 minutes | §15.1 | No |
| One storage machine + several clients (LAN share) | §15.2 | No |
| First verify the PG backend works on a small machine | §15.3 | Yes |
| A production center going straight to PG (systemd + least privilege) | §15.4 | Yes |
| Have a SQLite center, want to switch it to PG (**you'll see an empty tree**, read before acting) | §15.5 | Yes |
| PG daily inspection: which views exist, how many nodes, who is connecting | §15.6 | Yes |
| PG backup / restore / drill | §15.7 | Yes |
| Delete views under PG, clean leftover schemas, clean orphan bytes | §15.8 | Yes |
| Which commands can't be used under PG (and the alternatives) | §15.9 | Yes |
| Zero-copy onboarding of an existing directory | §15.10 | No |
| Read-write / read-only / view-bound privilege split | §15.11 | No |
| Multi-data-server striping expansion | §15.12 | No |
| How to pick among derived view / alias / fork | §15.13 | No |
| Agent (MCP) access and audit | §15.14 | No |
| Slim down long commands: config file + environment variables | §15.15 | No |
| Error symptom table | §15.16 | — |

### 15.1 Scenario: see the effect in 5 minutes (three services on this host + one mount)

```bash
G=grovefs
$G view --meta-dir /data/demo create --view team   # build one physical view tree (/data/demo/meta/team/team.db)
$G --home /data/demo manager define --meta 127.0.0.1:50110 --data 127.0.0.1:50100   # declare the topology (§4.2)
$G --home /data/demo manager serve --listen 127.0.0.1:50055 &
$G data-server --listen 127.0.0.1:50100 --data-dir /data/demo --register-with 127.0.0.1:50055 --join-key-file /data/demo/manager/join.key &
$G meta-server --meta-dir /data/demo --listen 127.0.0.1:50110 --register-with 127.0.0.1:50055 --join-key-file /data/demo/manager/join.key &
$G mount --view team -m /mnt/team \
   --manager 127.0.0.1:50055 --roster-key-file /data/demo/manager/roster.key   # backgrounds by default; add --foreground to debug
mkdir -p /mnt/team/docs && echo hello > /mnt/team/docs/a.txt
cat /mnt/team/docs/a.txt                   # hello
find /data/demo/blocks -type f | head -1   # the bytes really land here: blocks/<2hex>/<uuid>
$G umount -m /mnt/team                     # finish up (don't use the umount command, it won't clean up the background process)
```

The echo of every step comes from a real run on this machine (`.bneck/probe_151.sh`, unedited; only paths and the pid vary with the environment):

```console
created BASE view 'team' at /data/demo/meta/team/team.db
data-server listening on 127.0.0.1:50100 (host lc, 1 data root(s): /data/demo)
meta-server listening on 127.0.0.1:50110 (base /data/demo, role Primary)
manager 127.0.0.1:50055: deployment 'default' epoch 1 resolved meta [127.0.0.1:50110] data [127.0.0.1:50100] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:50110
using remote data service at 127.0.0.1:50100
mounting view 'team' at /mnt/team (fully remote: no local store)
grovefs: mounted view 'team' at /mnt/team (background pid 1162973)
```

Acceptance: `cat` outputs `hello`, at least one object file appears under `blocks/`. This tier enables no token and no API-key gate (both gates are optional server-side settings, §6); the FUSE mountpoint permissions are just OS permissions.

Three services plus one mount = four processes, and that is the **minimal** shape to get started; without manager bootstrap there is no fallback path:

```console
$ grovefs mount --view team -m /mnt/team
error: a mount resolves its cluster coordinates from the deployment's manager: there is no longer a way to name a metadata or data service by hand. Pass --manager HOST:PORT --roster-key-file PATH (or the GROVEFS_MANAGER / GROVEFS_ROSTER_KEY_FILE env knobs) to fetch the deployment's SIGNED roster and dial the meta- and data-servers it declares. Directly naming the addresses was removed on purpose: one --home holding a view's tree AND its bytes would make every client an authoritative store, and hand-typed lists let two mounts of one view dial different (or half a) cluster; a manager declaring the topology once and every client verifying the same signature is what keeps them consistent. Start the services with --register-with the manager, then `grovefs manager define`/`serve` it. --home stays the store root of the node-side verbs (view create, gc, import, apikey, du) and of the servers themselves, never of a mount.
```

(exit code 1, nothing mounted.) A misspelled view name is hard-refused the same way — it never silently grows an empty tree; the server answers with a readable refusal plus one line saying how to create it (§4):

```console
error: view 'nope' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --meta-dir /data/demo create --view nope` (or seed a tree with `grovefs view import`)
```

Mount coordinates have a single source, see `--manager` in §4.2: they are declared once on the server side and a client points at a single bootstrap address.

### 15.2 Scenario: one storage machine + several clients (SQLite center, minimally usable)

Storage machine `10.0.0.10` (data and metadata on the same disk, `blocks/` and `meta/` side by side under `/data/cluster`):

```bash
G=grovefs; TOK=$(head -c 24 /dev/urandom | base64)   # 两端用同一个值
export GROVEFS_TOKEN=$TOK                           # 服务端/客户端都读它,省掉 --token
$G view --meta-dir /data/cluster create --view team   # 1) create the base view first, then start the services
$G --home /data/cluster manager define --meta 10.0.0.10:50110 --data 10.0.0.10:50100   # declare the topology (§4.2)
$G --home /data/cluster manager serve --listen 0.0.0.0:50055 &
$G data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --register-with 10.0.0.10:50055 --join-key-file /data/cluster/manager/join.key &
$G meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --register-with 10.0.0.10:50055 --join-key-file /data/cluster/manager/join.key &
```

Any client (clients **don't install a database, don't land metadata** — both coordinates resolve from the manager):

```bash
export GROVEFS_TOKEN=<同一串>
G=grovefs
$G mount --view team -m /mnt/team \
   --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key        # 读写
$G mount --view team -m /mnt/team_ro \
   --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key --ro   # 本机只读
```

Acceptance: on machine A `echo x > /mnt/team/t.txt`, on machine B `ls /mnt/team_ro` sees it within 2 seconds (TTL default ≤2s, no remount). For **server-side hard read-only** rather than just the local `--ro`, see §15.11.

> **A view name must be created before mounting**: a view name must first be brought into being via `view create` / `view import` before a mount is allowed; a misspelled name gets a readable refusal and won't silently grow an empty tree (this is the only behavior, no switch; details in §4).

### 15.3 Scenario: get the PG backend running first (verifying on one host, one parameter more than §15.2)

```bash
# 0) 中心机上准备角色与库(客户端什么都不用装;已存在则这两条各自跳过)
sudo -u postgres psql -c "CREATE ROLE grove LOGIN PASSWORD 'grovepw';"
sudo -u postgres createdb grovefs
sudo -u postgres psql -d grovefs -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grove;"
#    只需 CREATE(建 schema/表)+CONNECT,不需要 SUPERUSER,不需要预建任何表

# 1) 起服务:meta-server 只多一个 --meta-store
G=grovefs; TOK=s3cr3t-change-me
export GROVEFS_TOKEN=$TOK
export GROVEFS_META_DIR=/data/cluster                      # 省掉两处 --meta-dir
$G data-server --listen 0.0.0.0:50100 --data-dir /data/cluster &
$G meta-server --listen 0.0.0.0:50110 \
   --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs &

# 2) 显式建视图:PG 下就是一个 schema(挂载不会建)
$G view --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs create --view demo

# 3) 客户端:挂载命令与 SQLite 后端一字不差(线协议不变)
$G mount --view demo --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key -m /mnt/demo
mkdir -p /mnt/demo/docs && echo hi > /mnt/demo/docs/a.txt
```

**What exactly got created in PG** (the local measured output is in parentheses):

```bash
sudo -u postgres psql -d grovefs -c '\dn'
#   grovefs_refs        ← 跨视图引用账本(共享表 grovefs_refs.holders)
#   grovefs_v_64656d6f  ← 视图 demo 的 schema
#                        (名字 = grovefs_v_ + 视图名的 UTF-8 字节的十六进制)
printf %s demo | od -An -tx1 | tr -d ' \n'; echo   # 64656d6f,自己算一下对得上
sudo -u postgres psql -d grovefs -c '\dt grovefs_v_64656d6f.*'
#   node / ctrl / locks 三张表(节点、HA 纪元、锁)
```

Acceptance: `grovefs_v_64656d6f` and `grovefs_refs` appear in `\dn`; `cat /mnt/demo/docs/a.txt` outputs `hi`; there's **no** `demo.db` in `ls /data/cluster/meta` (the authoritative tree is in PG, not in a file).

> Three behaviors you must know: ① each view's schema must be **explicitly created** by `view --meta-store postgres://… create` (a mount doesn't create it); running `view create` without `--meta-store` on the PG backend only builds an unused local `<view>.db`; ② a view name over **25 bytes** is refused (PG's 63-character identifier limit, doubled after hex encoding, about 8 CJK characters), the client only sees `Input/output error` + `background mount exited`, the real reason is in the meta-server log: `view name '...' incompatible with PG schema naming (empty, or longer than 25 bytes)`; ③ giving `--meta-store` together with `--role standby`/`--peer`/`--witness` fails immediately on startup (see §15.9).

### 15.4 Scenario: a production center on PG (systemd + least privilege + connection string not on argv)

Center machine (PG and GroveFS on the same or separate machines, clients unaware):

```bash
# 1) 一次性:角色、库、最小权限(角色/库已存在则跳过)
sudo -u postgres psql -c "CREATE ROLE grove LOGIN PASSWORD 'grovepw';"
sudo -u postgres createdb grovefs
sudo -u postgres psql -d grovefs -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grove;"

# 2) 连接串只放 root 可读文件,不进 systemd 单元(argv 在 ps 里全网可见)
sudo install -d -m 700 /etc/grovefs
sudo sh -c 'cat > /etc/grovefs/meta.env' <<'EOF'
GROVEFS_TOKEN=换成你的32字节随机串
GROVEFS_META_DIR=/data/cluster
GROVEFS_META_STORE=postgres://grove:grovepw@10.0.0.9:5432/grovefs
EOF
sudo chmod 600 /etc/grovefs/meta.env
```

`/etc/systemd/system/grovefs-meta.service` only changes the `ExecStart` line (the rest as in §11.4):

```ini
[Service]
EnvironmentFile=/etc/grovefs/meta.env
ExecStart=/usr/local/bin/grovefs meta-server --listen 0.0.0.0:50110 \
          --meta-store ${GROVEFS_META_STORE}
```

> `GROVEFS_META_STORE` has a dual identity: it's both the source of systemd's `${VAR}` expansion and GroveFS's own `--meta-store` env fallback (`view`/`apikey`/`mcp`/`meta-server`/`data-server`/`du` read it when no flag is given, source `resolve_store()`) — giving `${GROVEFS_META_STORE}` explicitly in `ExecStart` and omitting the parameter to go the env fallback take the same value. If your systemd version doesn't expand, write the connection string directly into the unit and `chmod 600` that unit; **don't** stuff it into shell history to save trouble.

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now grovefs-meta grovefs-data
systemctl status grovefs-meta --no-pager | head -5
journalctl -u grovefs-meta -n 20 --no-pager | grep -i "store\|postgres\|listen"
```

Acceptance: `journalctl` shows the listening line with no `connect`/`auth` errors; `sudo -u postgres psql -d grovefs -c '\dn'` gains an extra `grovefs_v_<hex>` after the first client mounts. Backup/HA are handed to PG itself (§15.7, §15.9).

### 15.5 Scenario: have a SQLite center, want to switch it to PG (**read the warning first**)

> **Switching over shows an empty tree.** PG and SQLite are two metadata stores that don't read each other: once `meta-server --meta-store postgres://…` starts, the first time a view is opened it builds an **empty schema** (only the root node). The tree directories under `meta/` and `blocks/` are both still in place, but the new center doesn't recognize them. This isn't a bug, it's a backend boundary; the current version has **no** tool to import SQLite metadata into PG (`view export` only reads from a local `.db`, PG has no corresponding import command; `import` only writes local SQLite).

So there are only three paths, in order of cost:

**Path A: don't switch.** If what you want is "operational unity / high availability", a SQLite center with §5.5's primary/standby already suffices; PG buys you **other people's experience operating a database** and PG's own streaming replication/PITR, not performance (§4.1 has a selection table). For PG on a new project, use it **from day one**, migration cost zero.

**Path B: replay (metadata rebuild, one extra copy of bytes).** The old and new centers coexist, move data via mounts:

```bash
# 旧中心(SQLite)与新中心(PG)同时在线,视图名可以相同
G=grovefs
$G mount --view team -m /mnt/old --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key --token $OLD_TOK
$G mount --view team -m /mnt/new --manager 10.0.0.20:50055 --roster-key-file /etc/grovefs/roster.key --token $NEW_TOK
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 目录项与内容都按新 key 重新登记
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 再跑一遍是增量的(只补新文件)
```

- The object key is a **random UUID** (not a content hash), so the data **lands a new copy on the new center**, provision temporary capacity at old-data-volume × 1;
- Verify after moving: both sides' `find | wc -l` and key files' `sha256sum`; after confirming, retire the old center;
- The bytes in the old `blocks/` have **no reference** on the new center, won't be auto-reclaimed (GC is reference-driven), to reclaim space follow §12.6's trade-offs manually;
- Walk path B fully on a small view first before going to production, don't test on the main view.

**Path C: cold-database surgery (not provided, not committed).** Hand-inject the `node` rows of `meta/<view>/<view>.db` into `grovefs_v_<hex>` — the table structure matches (`id/parent_id/name/is_dir/data_ref/…`), but the `ctrl` epoch, `locks`, sequences, and ledger consistency are all on you to guarantee. No official script, no support if it breaks, **not recommended**.

### 15.6 Scenario: PG daily inspection (use psql, not sqlite3)

```bash
psql "postgres://grove@10.0.0.9:5432/grovefs" -c "..."    # 下文 SQL 都写在这个位置
```

```sql
-- 1) 有哪些视图(一个视图一个 schema,另加共享账本 grovefs_refs)
select nspname from pg_namespace where nspname like 'grovefs_v_%' order by 1;

-- 2) 一条 SQL 看全:每个视图的节点数(实测可用,不需要挨个算 hex)
select n.nspname,
       (xpath('/row/c/text()',
         query_to_xml(format('select count(*) as c from %I.node', n.nspname),
                      false, true, '')))[1]::text::bigint as nodes
from pg_namespace n where n.nspname like 'grovefs_v_%' order by 1;

-- 3) 连接数:活跃视图各自持有 8 写 + 4 读池(共 12 条);空闲超 --engine-idle-secs(默认 300s)且无挂载会话的视图会被回收、连接随之释放
--    → 规划 PG max_connections 按「最近 N 分钟活跃视图数 × 12 + 突发溢出 + 运维连接」留量,不是视图总数;卸载潮后本查询应自己回落
select state, count(*) from pg_stat_activity where datname='grovefs' group by 1 order by 2 desc;

-- 4) 提交/回滚比:40001/40P01 冲突会在客户端透明重试,PG 侧体现为回滚计数
select datname, xact_commit, xact_rollback,
       round(100.0 * xact_rollback / nullif(xact_commit + xact_rollback, 0), 2) as rollback_pct
from pg_stat_database where datname = 'grovefs';

-- 5) 谁在跑长查询(卡住会拖住对应视图的写)
select pid, now() - query_start as dur, state, left(query, 60) as q
from pg_stat_activity where datname='grovefs' and state <> 'idle'
order by dur desc limit 5;

-- 6) 体积 top10(单视图三张表:node/ctrl/locks,大的一定是 node)
select schemaname || '.' || relname as tbl, pg_size_pretty(pg_total_relation_size(relid)) as size
from pg_statio_user_tables order by pg_total_relation_size(relid) desc limit 10;
```

Acceptance: the row count of query 2 equals the current view count, `nodes` is same-order-of-magnitude as the client's `find /mnt/<view> | wc -l` (the client counts the mountpoint itself one extra).

> The difference from SQLite inspection (§12.1): under PG there's **no** `meta/<view>/<view>.db` to query, `grovefs view list` also can't see the trees in PG (§15.9); when `pg_stat_database`'s `rollback_pct` is long-term > 10% and writes slow down, first check whether it's the same directory being high-frequency `rename`/`unlink`'d by multiple clients (row-lock hotspot), rather than tuning GroveFS.

### 15.7 Scenario: PG backup / restore / drill

One complete view = **metadata in PG** + **bytes in `blocks/`**, both must be point-in-time backed up; back up only half and restore yields "names without content" or "objects without names".

```bash
DAY=$(date +%F); BK=/backup/grovefs-$DAY; install -d "$BK"

# 1) 元数据:只导 GroveFS 的 schema(表数 = 视图数 × 3 + 1;
#    实测 1 个视图 + 账本 = 4 张表,约 10KB)
pg_dump -Fc -n 'grovefs_v_*' -n grovefs_refs -f "$BK/meta.dump" grovefs

# 2) 字节:与 1) 同一停写窗口内同步(rsync 有增量,第二次起很快)
rsync -aH --delete-after /data/cluster/blocks/ "$BK/blocks/"

# 3) 恢复到新库(演练不要动生产库)。建库与恢复要 DBA 身份:
#    GroveFS 的运维角色只有 CREATE+CONNECT,没有 CREATEDB
sudo -u postgres createdb grovefs_restore
sudo -u postgres pg_restore -d grovefs_restore "$BK/meta.dump"

# 4) 核对恢复结果(实测:表数 3N+1、节点行数一致、data_ref 与账本都在)
sudo -u postgres psql -d grovefs_restore -c '\dt grovefs_v_*.*'
sudo -u postgres psql -d grovefs_restore -tAc 'select count(*) from grovefs_v_<hex>.node'
sudo -u postgres psql -d grovefs_restore -tAc 'select count(*) from grovefs_refs.holders'

# 5) 演练:临时端口起 meta-server 指向恢复库,用只读挂载验收
G=grovefs
export GROVEFS_TOKEN=$TOK
$G meta-server --meta-dir /tmp/restore-meta --listen 127.0.0.1:50210 \
   --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs_restore &
$G data-server --data-dir /tmp/restore-blocks --listen 127.0.0.1:50200 &   # 先把 $BK/blocks 拷成 /tmp/restore-blocks/blocks
$G mount --view team -m /mnt/restore --manager 127.0.0.1:50055 --roster-key-file ~/roster.key --ro
find /mnt/restore -type f | wc -l        # 与生产侧同量级即可
$G umount -m /mnt/restore
```

- The `pg_dump -n 'grovefs_v_*'` wildcard works on PG ≥ 10 (local PG 12 measured 7 tables); omitting `grovefs_refs` only makes the ledger fall back to the conservative "keep extra bytes" behavior, never a wrong delete;
- **Don't stack GroveFS's `--role` for high availability**: under PG use PG's own streaming replication or PITR (§15.9 has the fail-fast wording);
- After restore, the client's mountpoints must all be unmounted before switching the DB, then remount; don't rename the restore DB to the production name and run live traffic on it.

### 15.8 Scenario: delete a view under PG, clean leftover schemas, reset the ledger

One command does three things: **ledger teardown** (rows in `grovefs_refs.holders` reaching zero are deleted) + **tree deletion** (`DROP SCHEMA … CASCADE`) + **byte reclamation** (`backend.remove` of `blocks/` objects no longer held). 

```bash
G=grovefs
# 1) 先把挂载全部下线(在线客户端遇到 schema 消失会 EIO)
$G umount -m /mnt/demo

# 2) 删整棵基视图:销账与 DROP 在同一事务里,不会出现“树没了账还在”
$G view --meta-dir /data/cluster --meta-store postgres://grove:pw@10.0.0.9:5432/grovefs \
   rm --name demo --base --data-dir /data/cluster
#   removed base view 'demo' from postgres://…: schema dropped with 6 name(s), 4 object(s) no longer held by any view
#   4 object(s) freed from /data/cluster/blocks, 0 not found under that data root (wrong --data-dir? …), 0 could not be freed
#   note: new mounts of 'demo' are now refused — `view create` starts over; the definitions this meta-dir holds were not touched

# 3) 归位账本(只在静止时跑):按存活树重算,删掉没有任何树认领的行
$G refs --meta-dir /data/cluster --meta-store postgres://… rebuild
#   refs ledger rebuilt in postgres://…: 3 tree(s), 12 content key(s), 12 holder unit(s); 2 row(s) corrected, 1 row(s) deleted
```

- **Why it can be atomic**: PG's DDL is transactional, so "decrement one holder per key (DELETE at zero, recording that ref) → `DROP SCHEMA … CASCADE`" can commit in **one transaction**, a crash leaves no "tree gone, ledger still there" intermediate state; compared to the SQLite tier's "first delete `<view>.db`, then `refs rebuild` to recompute the whole ledger", here no after-the-fact rebuild is needed;
- **Bytes reclaimed in this step**: once the `node` rows are deleted, the objects in `blocks/` are orphans; the CLI takes the `DISTINCT data_ref` read before deletion and `backend.remove`s them one by one, **only those zeroed in the ledger** get deleted (still named by an alias/fork/other view counted into `still held by other views`). `--data-dir` points at the machine that really stores `blocks/`, a wrong target only honestly reports `not found`, never falsely claims reclaimed; striped keys (`s<N>:<stripe>[:<fp>]:<uuid>`) are on the data-server, an ordinary local root can't reclaim them, counted into `could not be freed` and skipped;
- **Cases where deletion fails**: the name itself is still definition-governed (first delete the definition with `view rm` WITHOUT `--base`), a definition still resolves through this base, it's still mounted locally — the same guards as SQLite, wording byte-for-byte identical (§3.4.3);
- **A second deletion fails**: reports `no schema grovefs_v_<hex> in the database — 'demo' has no base tree here (a governed name is removed with \`view rm\` WITHOUT --base; …)`, exiting nonzero. Reporting "did nothing" as success would let a typo pass silently;
- **The cost of bypassing the CLI to hand-`DROP SCHEMA … CASCADE`**: the ledger is a global table with no view column, a bare drop only deletes the tree without teardown, leaving one row per object the view ever booked; more importantly **only that schema knows which objects it booked**, the ref list vanishes with the schema — step 3 below can reset the ledger (delete over-counted rows), but **can't reclaim bytes**, those objects have no record to reclaim from then on. The E/H sections of `.bneck/pg_view_teardown.sh` were written for exactly this: recording once before and after the drop, finally asserting "what remains is exactly what it booked" and stating the reason;
- **`refs rebuild`'s direction**: enumerate `grovefs_v_*` schemas → filter out the view names encoded by this scheme via `view_from_schema_name` → per tree `SELECT DISTINCT data_ref` → rewrite `grovefs_refs.holders` by "how many trees still name it". **A single tree read failure aborts the whole thing** (undercounting leads to wrong deletion, overcounting only keeps extra bytes); on a healthy DB it's a no-op (`0 row(s) corrected, 0 row(s) deleted`); it reads trees not the ledger, so it can both delete the over-counted rows left by an out-of-band schema drop and recover lost bookings (a wrongly-deleted `holders` row) from the trees — both directions measured in the E/F sections of `pg_view_teardown.sh`;
- **`pg_drop_views.sh` is an out-of-band inspector**: it does not try to grab what the CLI can do, only used when "you have no grovefs binary at hand", "want to clean leftovers the CLI can't reach (orphan schemas no definition references, invisible to `view list`)", or "want to verify the ledger on a machine that doesn't start a service" (`--list` lists schemas and decoded view names, without `--yes` always a dry run);
- **`gc` still refuses on the PG tier**: it only audits local page files, under PG the tree is a schema and bytes belong to the data-server, no equivalent command (§15.9) — byte reclamation only happens in step 2 above, so "delete the tree first, then think about reclaiming" was always the wrong order.

- **Don't casually `DROP` `grovefs_refs`**: it's the ledger for cross-view deletion safety, losing it only makes the system conservatively keep extra bytes (never wrongly delete objects); to recover from a lost state use step 3's `refs rebuild`. Leftover schemas no definition references and the CLI can't delete — see them clearly first with `bash .bneck/pg_drop_views.sh --list`, then clean with `--yes` (dry-run by default);
- **Confirm after deletion**: that schema disappears from §15.6's query 1, the client can no longer mount that view name; in the ledger `select count(*) from grovefs_refs.holders` reaches zero.

> Suites: `.build-bin/test_pg.sh` (77 assertions, 22 of them section 9's connection-pool floor/ceiling/per-view overrides) and `.build-bin/test_pg_parallel.sh` (41 assertions) both end with `view rm --base` to self-clean, leaving 0 schema / 0 ledger rows in the DB after running; `.bneck/pg_view_teardown.sh` (44 assertions) runs the entire chain above from start to finish on a real FUSE mount. All three "leave no trace when done", TEARDOWN is not a manual step.

### 15.9 Scenario: capability boundaries under a non-SQLite backend (which commands can't be used, alternatives)

The table below gives the capability boundaries under the non-SQLite backend (i.e. PG); every row's "measured wording / behavior" was run on this machine's current build:

> **An ops machine needs only an account key**: the `--meta-store postgres://…` forms in the tables below apply to "run on the store-holding host, or the runner is themselves a DBA". To manage the same metadata from **another machine**, enable `--control-listen` server-side, and on the management machine use `--control-server HOST:PORT --api-key <admin key>`, with `meta-server` doing the same reads/writes on your behalf (§3.5.2).

#### PG tier

| Operation | Under the PG backend | Failure wording / actual behavior | Alternative |
| --- | --- | --- | --- |
| `view create` (without `--meta-store`) | builds no PG tree | only creates an unused local `meta/<view>/<view>.db` | add `--meta-store postgres://…`: `view --meta-store … create --view V` explicitly builds the schema; a mount never builds a tree |
| `view fork` / `view graft` | **usable** | one `INSERT … SELECT` copies all rows of the source schema into a newly created `grovefs_v_<hex>` target (same transaction, id/ino/hardlink/mode/owner/timestamps all preserved), `graft` selects a subset and reissues ids in the same transaction; echoes `forked view 'lab' from base 'alice': 6 entries, 3 shared object(s)`, zero byte copy, the shared ledger gains one holder per named object (proven by the `forksrv` suite on the PG tier) | no alternative needed; to still see a pruned tree use a derived view (§15.13) or an alias (§3.4) |
| `view export` / `view import` | **usable** | export uses a REPEATABLE READ read-only snapshot: writes the schema as host-agnostic JSONL row-for-row identical to the SQLite tier (header + one row per node); import in a single transaction DELETEs → INSERTs row by row → structure/count validation → `setval` raises the sequences → corrects the ledger old→new, rolling back wholly on failure (the newly created schema is `DROP`ped too). An existing target tree needs `--replace`, `--as-fork` writes a `fork <base>` definition, a bad dump leaves no half-product (proven by the `xfer` suite on the PG tier) | whole-DB backup still uses `pg_dump -n grovefs_v_<hex>` (§15.7); the two don't conflict |
| `view list` | usable (add `--meta-store`) | `view --meta-store postgres://… list` reads `grovefs_control.view_defs` + `grovefs_v_*` schemas; without `--meta-store` only shows local files (outputs `no view definitions under …/meta/views`) | same left, or the §15.6 inspection SQL |
| `view rm --base` (delete a whole base view with its bytes) | **usable** (`view --meta-store postgres://… rm --name V --base --data-dir R`) | in one transaction decrements `grovefs_refs.holders` per key (DELETE at zero) then `DROP SCHEMA grovefs_v_<hex> CASCADE`, and after commit takes the `DISTINCT data_ref` read before deletion and `backend.remove`s them one by one. The echo states the two facts on separate lines, not summed into one number: `schema dropped with 6 name(s), 4 object(s) no longer held by any view` / `4 object(s) freed from …/blocks, 0 not found …, 0 could not be freed`. A second deletion reports `no schema grovefs_v_<hex> in the database — 'V' has no base tree here …` and exits nonzero | the same guards as SQLite (governed name / dependent definition / a live mount of that view on this host); `--data-dir` points at the root really storing `blocks/`; §15.8 |
| `import` (zero-copy) | not applicable | creates a local `<view>.db`, metadata doesn't go into PG | mount + `rsync` (same as §15.5 path B) |
| `refs rebuild` | **usable** (`refs --meta-store postgres://… rebuild`) | recomputes the **in-database** shared ledger (not a local `meta/_control/refs.db`): enumerates all `grovefs_v_*` schemas → per tree `SELECT DISTINCT data_ref` → rewrites `grovefs_refs.holders` by "how many trees still name it", deleting rows no tree claims. A no-op on a healthy DB (`0 row(s) corrected, 0 row(s) deleted`); removes over-counted rows when a schema was dropped out-of-band (measured `0 tree(s) … 3 row(s) deleted`), recovers from the trees when `holders` rows were wrongly deleted (measured `2 row(s) corrected`). **A single tree read failure aborts the whole thing** (undercounting leads to wrong deletion) | run only when **quiescent**; normally maintained by server transactions, no periodic run needed. The verbatim recipe is in §15.8 |
| `du` (byte total) | **usable** (`du --meta-store postgres://… --view V --data-servers HOST:PORT,…`) | first checks `information_schema` that `grovefs_v_<hex>` exists, then `SELECT … FROM <schema>.node` to read the tree, bytes batch-statted toward the data-server (the striped storage's address-list length must match the write time, otherwise it refuses rather than zeroing); a nonexistent name gets the same sentence on both tiers `no tree for view 'V' in postgres://… — \`grovefs view create --view V\` makes one …`, never reporting a "misspelled name" as 0 B | same as SQLite (§8.4) |
| `apikey` | usable (add `--meta-store`) | keys and audit go into `grovefs_control.api_keys`/`mcp_calls`, no local `api_keys.db` (proven by the `control` suite) | same as SQLite (§15.11) |
| `mcp` | usable (add `--meta-store`) | all ten governance and read-only probe tools go through `grovefs_control` (`list_view_tree` can also walk a PG tree); `fork_view`/`graft_view` are the same implementation as the CLI, a cross-schema copy on PG | same left; inspection can also use psql (§15.6) |
| HA: `--role standby` / `--peer` / `--witness` | fails immediately on startup | `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG streaming / logical replication / PITR |
| view name > 25 bytes | refused | server log: `incompatible with PG schema naming (empty, or longer than 25 bytes)`; the client only sees `Input/output error` | keep the name within 25 bytes (about 8 CJK chars) |
| connection pool | default + env-tunable | per view default 8 write + 4 read (12 total), tunable via `GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL` (each 0–64); a mount session pins the engine, reclaimed after unmount and `meta-server --engine-idle-secs` (default 300s) of silence, reopened on next access (proven by the `evict` suite) | plan `max_connections` by "**active view count** × (write pool + read pool) + burst overflow"; for write hotspots check lock contention first, don't enlarge the pool |

### 15.10 Scenario: zero-copy ingest of an existing directory (SQLite backend)

```bash
G=grovefs
export GROVEFS_HOME=/data/cluster
$G import --view legacy --source /data/legacy --dry-run          # 先看会进多少,不建库
$G import --view legacy --source /data/legacy                     # 同盘:硬链接,零额外字节
$G import --view legacy --source /data/legacy --prefix /legacy    # 挂到视图内的子目录
$G import --view legacy --source /data/legacy                     # 重跑=增量,只补新文件
```

- Under a meta-server deployment, run it **on the storage machine**: `$G import --meta-dir /data/cluster --view legacy --source /data/legacy`; already-mounted clients converge within ≤2s, no remount needed;
- When the source directory and `blocks/` are on different filesystems it automatically degrades to a streaming copy (`hardlinked` in the output becomes `copied`);
- Later external changes in the source directory (rename/replace) are **not auto-synced**; follow up by rerunning `import` (§14.2); do not use this command on the PG backend (§15.9).

Acceptance: the directory/file count from `$G import … --dry-run` equals the corresponding part of `find /mnt/<view>/<prefix> | wc -l` after the real import; a source file's `ls -i` inode matches the object inode under `blocks/` (hardlink holds).

### 15.11 Scenario: read-write / read-only / view-binding privilege split

```bash
G=grovefs; export GROVEFS_TOKEN=$TOK
# 1) 服务端开门(必须重启才生效)
$G meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key &

# 2) 铸三把 key(回显明文,库里同时落一份可逆副本)
$G apikey --meta-dir /data/cluster create --label alice-rw --for mount --view team
$G apikey --meta-dir /data/cluster create --label bob-ro   --for mount --view team --ro
$G apikey --meta-dir /data/cluster create --label agent    --for mcp

# 3) 客户端:key 走环境变量,不进 argv(ps 可见)
export GROVEFS_API_KEY='<id>:<secret>'
$G mount --view team -m /mnt/team --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key

# 4) 看谁在用、用在哪儿、随时撤销
$G apikey --meta-dir /data/cluster list --view team
$G apikey --meta-dir /data/cluster calls --limit 20
$G apikey --meta-dir /data/cluster revoke --id <id 或其唯一前缀>
```

- The order of the three gates: `--token` (admission) → `--require-api-key` (caller) → the key's `--view`/`--ro` (enforced server-side per op);
- Write requests on a read-only key are refused server-side (client `EPERM`); `flock`/`F_SETLK` also count as write intent, so a read-only role needing a shared lock should use an rw key;
- Revocation is **op-driven**: after revoking, that mount's next server round-trip is sealed, but reads answered entirely by the kernel cache are not interrupted;
- A derived view is already a read-only mount (§15.13), so you don't need a read-only key for shape protection — but **not seeing the full tree** still comes only from key/view binding, not from the lens.

### 15.12 Scenario: multi data-server stripe expansion

```bash
G=grovefs
export GROVEFS_TOKEN=$TOK
# 每台存储机各自起 data-server --data-dir 自己的盘(端口可同),向 manager 注册
for h in 10.0.0.11 10.0.0.12 10.0.0.13; do
  ssh $h "grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/blocks --register-with 10.0.0.10:50055 --join-key-file /etc/grovefs/join.key &"
done
# Declare the whole stripe-member set once on the manager; a client points only at the manager (§4.2)
$G --home /data/cluster manager define --meta 10.0.0.10:50110 --data 10.0.0.11:50100,10.0.0.12:50100,10.0.0.13:50100
$G mount --view big -m /mnt/big \
  --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key
```

- Only files ≥ 16MiB are chunked; small files naturally land only on chunk0; a single already-saturated link does not get faster from striping — you need independent NICs/links to measure any gain;
- The stripe layout is encoded in the object key (`s<server count>:<stripe size>:<uuid>`), so reading an old object still parses by its old layout; **changing the node count only affects new objects**, old data is not auto-redistributed;
- Metadata still exists in a single copy (one meta-server or PG); striping expands only the capacity and bandwidth of **bytes**.

Acceptance: after `dd if=/dev/urandom of=/mnt/big/big.bin bs=1M count=64`, all three storage machines show a visible increment in `du -sh /data/blocks` (a small file appears on only one of them).

### 15.13 Scenario: how to choose among derived view / alias / fork

| Your need | Which to use | One command |
| --- | --- | --- |
| Same tree, different clients see different shapes, **and read-only distribution** | derived view `view define` | `$G view --meta-dir D define --name docs-only --base team --rules docs.rules` |
| Just a public rename of the same tree, **read-write unchanged** | pure alias `view alias` | `$G view --meta-dir D alias --name public --real team` |
| An independent directory tree you can freely `mkdir`/`mv`/`rm`, **content read-only** | fork (**SQLite only**) | `$G view --meta-dir D fork --name lab --base team` |
| The client tunes its own shape ad hoc, and needs to write | `mount --layout F` | `$G mount --view team --layout rules.txt -m /mnt/team --manager MGR:50055 --roster-key-file F` |

```bash
G=grovefs; D=/data/cluster
printf 'only /docs\n' > docs.rules
$G view --meta-dir $D define --name docs-only --base team --rules docs.rules
$G view --meta-dir $D list                      # 定义槽 + 裸基视图都会列出
$G mount --view docs-only -m /mnt/docs --manager 10.0.0.10:50055 --roster-key-file /etc/grovefs/roster.key
```

- Derived-view mounts are always **read-only** (shape governed centrally server-side, writes go through the base name `team`); already-mounted clients hot-swap the shape within ≤2s after the server changes rules, no remount needed;
- An alias is a 1:1 rename, **full-tree read-write**; an alias cannot itself serve as the base of another alias/fork;
- A fork is "touch it and it's a snapshot": names added to the base view later are not injected into an already-created fork; fork/graft **share one contract across both tiers** (SQLite copies the db, PG does cross-schema `INSERT…SELECT`; the `forksrv` suite proves it on both tiers, §15.9); the `EROFS` on content writes is a **client gate**, the hard boundary is still token/key.

### 15.14 Scenario: agent (MCP) access

```bash
G=grovefs; D=/data/cluster
# 1) 一把 mcp scope 的 key(或 both)
$G apikey --meta-dir $D create --label agent --for mcp

# 2) 手动试一次 stdio(客户端通常自己拉起,这步只用于验证)
GROVEFS_MCP_API_KEY='<id>:<secret>' $G mcp --meta-dir $D --require-api-key

# 3) 客户端配置(以 Claude Desktop 的 stdio 为例)
#   command: /usr/local/bin/grovefs
#   args:    ["mcp", "--meta-dir", "/data/cluster"]
#   env:     GROVEFS_MCP_API_KEY=<id>:<secret>
```

- A tool's write operations change the shape seen at the **real mountpoint** (derived views/aliases are shared server-side); preview with `preview_rules` and validate with `validate_definition` before changing (see the `grovefs-mcp` skill);
- Every admission is audited to a specific key: `$G apikey --meta-dir $D calls --limit 20`;
- On the PG backend MCP can only see local SQLite views (§15.9); don't expect it to manage the trees in PG.

### 15.15 Scenario: slimming long commands (config file + environment variables)

```bash
install -d -m 700 ~/.config/grovefs
cat > ~/.config/grovefs/grovefs.conf <<'EOF'
# 只写 GROVEFS_* 键;已存在的同名环境变量优先,flag 优先级最高
GROVEFS_HOME=/data/demo
GROVEFS_TOKEN=s3cr3t
GROVEFS_META_DIR=/data/cluster
GROVEFS_MANAGER=10.0.0.10:50055
GROVEFS_ROSTER_KEY_FILE=/etc/grovefs/roster.key
EOF
chmod 600 ~/.config/grovefs/grovefs.conf

# 之后这就是完整命令:所有旋钮从文件读
G=grovefs
$G mount --view team -m /mnt/team
$G meta-server --listen 0.0.0.0:50110
```

- Precedence is always **flag > live environment variable > config file > built-in default**; a file entry only takes effect when the corresponding env is unset;
- Lookup order: `$GROVEFS_CONFIG` → `~/.config/grovefs/grovefs.conf` (the first that exists wins);
- Parameters that never get a default value: `--view`/`-m` (different on every mount; writing them into the file only bites you);
- `--meta-store` has an env fallback `GROVEFS_META_STORE` (consumed by the control plane/server side/`du`, see §15.4) — on multi-cluster machines this is the global key most likely to keep a stale value; unset it before switching clusters;
- Putting a passphrase in the file is equivalent to putting it in an env var: `chmod 600`, keep it out of git; a background mount re-execs the child process, and both env vars and already-loaded values are inherited.

### 15.16 Scenario: error lookup table

| What you see | Real cause | Where to handle it |
| --- | --- | --- |
| Client `Input/output error` + `background mount exited (exit status: 1)` | The server **cannot resolve** this view inside `M_OPEN` (on PG a name over 25 bytes, a corrupted metadata db, etc.). Note: **a nonexistent name is not in this class** — that gets a readable refusal at the handshake and builds no tree | Look at the last `M_OPEN view='…' resolve failed: …` in the meta-server log; key-class refusals don't go through EIO, they return a readable reason directly |
| `error: view 'x' does not exist on this meta-server (sqlite/postgres): … views are created explicitly …` | The view name has no tree/definition yet, and the server refuses an uncreated view name (it never self-creates under any circumstance) | §4: create before mounting (SQLite `view --meta-dir D create --view x` / `view import`; PG `view --meta-store postgres://… create --view x`) |
| The mount **succeeded**, but the directory is empty | The view does exist, but the tree has no content yet — e.g. a just-`view create`'d empty tree, or an empty tree left over historically | Confirm with `ls meta/`; if created wrong, clear it per §15.8 and redo |
| `no physical base tree … cannot be forked` | Ran `view fork` on the PG backend | §15.13: switch to a derived view/alias |
| `no view definition '<v>' under …/views` | On a PG deployment you omitted `--meta-store` — the governance action landed on local files, and PG has no such definition at all | Add `--meta-store postgres://…` to the original command; a real view deletion is §15.8's `view --meta-store … rm --base` (unbooking + `DROP SCHEMA` in one transaction) |
| `no metadata tree … lives in Postgres` | Ran `view export` under PG | Use `pg_dump -n` (§15.7) |
| `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG and GroveFS native HA cannot be stacked | §15.9: route high availability through PG streaming replication instead |
| `serialization retry exhausted after 12 attempts (view 'x')` | One op hit 12 consecutive collisions without committing (SERIALIZABLE abort, backoff 200µs→25ms exhausted; `SERIALIZATION_MAX_ATTEMPTS`, `src/pg_store.rs`). This line means the retry budget truly ran out, not lock waiting | Investigate the contention surface, not the database: high-frequency `rename`/same-name `create` storms within one directory; this string is measured in the server log by the `pg_parallel` suite |
| `refs … rebuilt: 0 holder entries` | You ran the **local-file** ledger rebuild (without `--meta-store`): it recomputes `meta/_control/refs.db` from the view-tree directories under `<meta-dir>/meta/`, and in a PG deployment those directories don't exist, hence 0 rows | A PG deployment needs `--meta-store`: it reports `refs ledger rebuilt in postgres://…: N tree(s), M content key(s), K holder unit(s); X row(s) corrected, Y row(s) deleted`; the ledger content is in `grovefs_refs.holders` (§15.6/§15.9) |
| A write returns `EPERM` | A read-only key (including the `flock` class) or a derived view | §15.11 / §15.13 |
| Writing fork content returns `EROFS` | The fork's content plane is read-only | Write the base view; keep doing structural rearrangement inside the fork |
| Within 2 seconds after a mount, other clients don't see the change | Not a bug: TTL ≤2s; if it never syncs, check whether both places connect to the **same** meta | §4's "two consistency modes" |
| `connection refused` / handshake timeout | The firewall opened only one of them, or the tokens differ | §11.6, §11.7 acceptance checklists |

### 15.17 Per-backend full tracks (SQLite / PG each: one complete path from deployment to acceptance)

The scenarios above split by "what you want to do"; this section switches viewpoint: **one complete path per database** — deployment → usage → operations → acceptance, copyable start to finish without jumping between §15.2–§15.16. The two tiers differ in only **3 landing points** (native HA, zero-copy directory ingest `grovefs import`, in-place `gc`); every other command keeps the same name and arguments, just swapping `--meta-dir` for `--meta-store <URI>`. The client mount command is byte-for-byte identical across both tiers. Topology follows §13: storage host `192.168.1.10`, clients A/B, PG host `192.168.1.20`; replace IP/paths/passwords with your own.

#### 15.17.1 SQLite (default tier, no database service installed)

**Deployment** (details §15.2, systemd units §11):

```bash
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token "$TOK"        # bytes land in /data/blocks
grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token "$TOK"       # no --meta-store = SQLite
grovefs view --meta-dir /data create --view share                                 # create the view explicitly, or the mount is refused
```

**Usage** (client mount; permission tiers §15.11, the view trio choice §15.13; remote mount needs no `--home`):

```bash
grovefs mount --view share --manager 192.168.1.10:50055 --roster-key-file /etc/grovefs/roster.key --token "$TOK" -m /mnt/da
```

**Operations** (full criteria §12):

```bash
grovefs du   --view share --meta-dir /data --total                       # usage: subtree byte total
grovefs refs --meta-dir /data rebuild                                    # recompute the ledger after an out-of-band rm (run when quiescent)
grovefs gc   --meta-dir /data --data-dir /data --older-than 24h --apply  # reclaim orphan objects in place (SQLite only)
# backup: stop and cold-copy the whole meta/ (one directory per tree + _control/) + rsync -a -H blocks/; native primary/standby HA adds --role/--peer/--witness (§11)
```

**Acceptance**: both ports LISTEN + `findmnt` shows `grovefs:share` + A writes, B reads instantly + B write returns `EROFS` + after restarting data-server the client reconnects on its own.

#### 15.17.2 PostgreSQL tier (metadata into a shared schema, HA/backup delegated to PG)

**Deployment** (verifying on one host §15.3; systemd + least privilege + connection string kept out of argv §15.4):

```bash
# one-time on the PG host: create DB + role + grants (no pre-created tables; the server creates the schema idempotently at first open)
sudo -u postgres createdb grovefs
sudo -u postgres psql -c "CREATE ROLE grove LOGIN PASSWORD 'pw';"
sudo -u postgres psql -c "GRANT CREATE, CONNECT ON DATABASE grovefs TO grove;"
# storage host: data-server is unchanged; meta-server swaps --meta-dir for --meta-store (under PG the server writes no authoritative data locally; mutually exclusive with --role/--peer/--witness)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token "$TOK"
grovefs meta-server --meta-store postgres://grove:pw@192.168.1.20:5432/grovefs --listen 0.0.0.0:50110 --token "$TOK"
grovefs view --meta-store postgres://grove:pw@192.168.1.20:5432/grovefs create --view share   # creates the PG schema explicitly
```

**Usage**: the client mount command is identical to SQLite; governance commands swap `--meta-dir` for `--meta-store` (or set `GROVEFS_META_STORE` once), and keys/definitions/audit all land in `grovefs_control` (§15.4).

**Operations** (patrol §15.6, backup/restore §15.7, drop view/clean schema §15.8, capability boundary §15.9):

```bash
grovefs du   --view share --meta-store postgres://grove:pw@192.168.1.20:5432/grovefs --data-servers 192.168.1.10:50100 --total
grovefs refs --meta-store postgres://grove:pw@192.168.1.20:5432/grovefs rebuild
# reclaim orphans: PG has no in-place form; use remote gc (via the --control-server governance port + an admin key, §15.9)
grovefs gc --control-server 192.168.1.10:50112 --api-key <id:secret> --data-servers 192.168.1.10:50100 --older-than 24h --apply
# backup: pg_dump (one shot covers tree + locks + control plane) + rsync -a -H blocks/; HA/PITR delegated to PG streaming replication/pg_basebackup
```

**Acceptance**: `psql -c '\dn grovefs_v_*'` shows share's hex schema + the client mount command unchanged + after unmount and quiescence the `pg_stat_activity` connections fall back on their own (`--engine-idle-secs` eviction, §15.6).

---

## 16. Internal-mechanism overview and extension development (for readers who change the code)

> The content below is evidenced line by line from **the source and the protocol constants**, not a conceptual primer. The rest of this manual uses only CLI/protocol behavior and does not depend on this section.

### 16.1 Extension points: three traits, one decorator

- **`MetaProvider`** (`src/meta.rs`, 30 methods): the full metadata-layer contract — `lookup`/`getattr`/`readdir`/`create_node`/`rename`/`lock_op`/`fence_handle`/`fetch_fence_epoch`… Four extension points carry default implementations: `readdir_page`/`supports_paged_readdir`/`display_targets`/`flush_listing` (the last being the "if you cache listings yourself, you must be flushable by an invalidation push" half of the contract). Five implementers: `MetaEngine` (local SQLite, one db per view), `PgEngine` (`src/pg_store.rs`, PG, one schema per view), `MetaStore` (enum, picks Sqlite/Pg at runtime, meta-server server side), `RemoteMeta` (client), `LayoutMeta` (the governance decorator). PG is not a new tree-engine semantic: `MetaStore::open_sqlite`/`open_pg` swap the storage kernel behind the same method surface — so every feature automatically runs on both backends, and the acceptance test is rerunning the same assertions with parameters swapped (§10).
- **`StorageBackend`** (`src/backend/mod.rs`, 10 methods): data-object IO — `new_key`/`create`/`read`/`write`/`len`/`fsync`/`truncate`/`remove`/`import_at`/`statvfs`. Implementers: `LocalBackend`/`ShardedBackend`/`PlacementBackend`/`RemoteBackend`. The 256-bucket sharding and striping both live behind this layer; the FUSE layer does not know which disk an object rests on.
- **`LayoutMeta`** (`src/layout.rs`, the governance decorator): wraps any `MetaProvider` to do "source path ↔ display path" translation, materializing no directory entries. `mount --layout` (client side) and the server-side derived view converge on **the same decorator implementation** — the byte-for-byte identical displayed shape is no coincidence, it's the same code.

### 16.2 Wire-protocol quick reference (custom binary frames over TCP)

Fixed-length header + variable-length body, all big-endian; **one connection, one request one response**, concurrency relying on the client connection pool and one server thread per connection — so the synchronous trait can be driven directly by fuser worker threads (`src/remote/proto.rs`'s module comment is the design contract):

| Item | Value |
| --- | --- |
| MAGIC | `0x5948_4653` = ASCII **"YHFS"** |
| MAX_FRAME | `1<<28` = **256 MiB** — over the limit disconnects directly, no splitting |
| Data-plane opcodes | `OP_CREATE=1` … `OP_FENCE=10` (create/read/write/len/truncate/remove/import_at/statvfs/open/fence) |
| Metadata-plane opcodes | `M_OPEN=16` … `M_READDIRP=37` (open/ping/lookup/getattr/…/readdirp paging) |
| Write-intent prefix | The five frames `OP_WRITE/TRUNCATE/REMOVE/IMPORT_AT/FENCE` carry a `[u64 epoch]` prefix in the body, for `--witness`/standby-node gating of stale-node replays (§5.5) |

### 16.3 Minimal path to onboard a new backend

1. Implement the corresponding trait (metadata swaps in `MetaProvider`, bytes swap in `StorageBackend`), matching method semantics against the comments of SQLite's `MetaEngine` / `LocalBackend`; the **PG-style "swap the kernel, keep the method surface"** is the recommended solution — features become fully available automatically;
2. Register the type at the factory: for the data plane refer to `backend::open_local`/`open_local_roots` (`src/backend/mod.rs`), for the metadata plane refer to how `MetaStore::open_sqlite`/`open_pg` (`src/meta/engine.rs`) are wired — one enum variant per backend, the `MetaProvider` method surface untouched; `PgEngine` (`src/pg_store.rs`) is the full worked example of "swap the kernel, keep the method surface": the pool's `checkout`/`requeue` and the `--engine-idle-secs` idle eviction all live inside the implementation, and the trait opens no new entry for them;
3. On the client side, if a new mount type is needed, wire it in at the mount dispatch point in `main.rs`; try to keep protocol opcodes unchanged — the frame layout and opcodes are fixed in `src/remote/proto.rs`, and any change must be upgraded in sync on both sides;
4. Regression: rerun the corresponding suite of `.build-bin/run_all.sh` with parameters swapped, taking the script output as the acceptance criterion (§10). The parameter-swap entry on the backend axis is the store parameter itself — `--meta-dir D` on the SQLite tier, `--meta-store postgres://…` (or env `GROVEFS_META_STORE`) on the PG tier, the same command run once per backend; cross-backend semantic equivalence is additionally backstopped by the 20-row side-by-side matrix of `.bneck/engine_matrix.sh` (§10.1): the same governance sequence runs on both tiers, any cell printing ERROR is a gap, and REFUSED must be one refused by design whose refusal wording and alternative are documented in §15.9.

### 16.4 Architectural trade-offs and development-side boundaries

Why it's designed this way (each point mapping to §4.1, §5, §5.5, §10, §15.9):

- **Single authoritative serialization point**: a view's tree is written by only one meta-server process — eliminating multi-replica divergence, at the cost of that process being the performance and scaling bottleneck. Accepted, until the demand actually appears.
- **Governance in the presentation layer, not the storage layer**: lenses/aliases/forks move no bytes, only changing the directory-tree presentation, so "same data, different shapes" is zero-copy.
- **Token and key layered**: the token manages the cluster admission gate, the key manages per-caller attribution and revocation — different leak-damage-control granularity; merging them would instead force small deployments to take on key operations.
- **async replication RPO≈500ms / `--sync-repl` RPO=0 / `--witness` real fencing**: the consistency tier is explicitly handed to the deployer to choose, at the cost of fencing capability being layered (without a witness, the standby only prevents split-brain but doesn't seal the old primary).
- **Striping at ≥16MiB whole-block round-robin**: sufficient and simple, avoiding metadata explosion; no arbitrary-granularity erasure coding.
- **Advisory locks are lease-based**: no wait timeout, no renewal task, falling back on lease + immediate reclamation on death — simplicity traded for a semantic boundary.
- **Throughput-first durability tiers**: the SQLite write connection's `wal_autocheckpoint=0` + a background checkpoint thread move sync stalls off the write hot path (§10 measured max 125ms→33ms); replication defaults to async (RPO≈500ms), `--sync-repl` explicitly trades for RPO=0 (cost: one control-channel RTT + one `VACUUM INTO` fsync, degrading honestly on timeout rather than misreporting, §5.5) — every tier states its cost.
- **The PG backend doesn't support zero-copy `import` or native HA**: the top-level `import` relies on same-disk hardlinks, and the in-house HA relies on SQLite file-level snapshots; a remote db has no manageable local disk/snapshot path, so it's refused by name — an honest boundary (§15.9), not a bug. `fork`/`graft`/`view export`/`import --as-fork` all work on both tiers (each copying the whole tree in its own storage, evidenced by `forksrv`/`xfer`).

Development reference: `src/` (10 main modules + `remote/` + `backend/`), protocol constants centralized in `src/remote/proto.rs`, telemetry in `src/stats.rs` (report grouping and phase names defer to the code), full-board regression entry `.build-bin/run_all.sh`. This section's content has a layered counterpart in the web docs.html under "§18 Internal mechanism and extension development / §19 Performance-tuning methodology".

---

## Appendix: command quick reference

| Purpose | Command |
|------|------|
| Create a view | `grovefs view --meta-dir D create --view U [--manifest M]` |
| Mount (the ONLY mount form) | `grovefs mount --view U -m MP --manager M:PORT --roster-key-file F [--deployment NAME] [--token T] [--api-key ID:SECRET] [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--foreground\|--log-file F]` (both coordinates resolve from the manager's signed roster; the hand-typed `--meta-server`/`--data-servers` were removed, §4.2) |
| Declare the topology / run the bootstrap service / see who registered | `grovefs manager --state-dir D define --name N --meta M[,M2] --data A[,A2]` ; `grovefs manager --state-dir D serve --listen H:PORT [--roster-ttl S] [--register-ttl S] [--require-registered]` ; `grovefs manager status --server H:PORT [--deployment N] --join-key-file F`; on the node side `meta-server`/`data-server --register-with H:PORT --join-key-file F [--advertise-addr A] [--deployment N] [--register-ttl S]` registers (§4.2) |
| Define a derived view | `grovefs view --meta-dir D define --name V --base B --rules F` (stored at `D/meta/views/V.layout`, `mount --view V` gets the shape; virtual-view mounts are read-only, writes go through the base name `B`) |
| Define a pure alias | `grovefs view --meta-dir D alias --name A --real R` (public name maps 1:1 to the real view: `A` is full-tree writable, `R` can't be mounted directly, §3.4) |
| Create a fork view | `grovefs view --meta-dir D fork --name L --base R` (copies directory entries into an independent `meta/L/L.db`, bytes shared; structure writable, content `EROFS`, §3.4.1) |
| Export/import a single view's metadata | `grovefs view --meta-dir D export --view V --out F [--force]` (host-agnostic JSONL, no bytes) / `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` (atomic replace, bytes copied separately to `blocks/`, §3.4.2) |
| Rotate a public name after a leak | `grovefs view --meta-dir D alias --name A2 --real R --retire A` (write-before-delete, zero window: `A` invalidated, `R` never directly mountable throughout, §3.4) |
| List/delete view definitions | `grovefs view --meta-dir D list` (definition slots + bare base views) / `grovefs view --meta-dir D rm --name V` (derived/alias delete only the definition, leaving data; fork deletes its own `meta/V/V.db` too and rebuilds the ledger) |
| Delete a whole base view (with byte reclamation) | `grovefs view --meta-dir D rm --name V --base [--data-dir R]` (deletes `meta/V/V.db`+WAL/SHM, `refs rebuild` rebuilds the ledger, reclaims bytes no other view still holds; refuses governed names / dependent definitions / live mounts of that view on this host; on the PG tier add `--meta-store postgres://…`, with unbooking + `DROP SCHEMA` in the same transaction, §3.4.3) |
| Investigate/reclaim orphan objects (offline) | `grovefs gc --meta-dir D [--data-dir R] [--apply] [--older-than 24h] [--json]` (scan-only by default; `--apply` requires no live mount on this storage and no grovefs process on this machine, never deletes keys the ledger still holds, refuses PG deployments, §12.6) |
| Import an existing directory | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]` (same-disk hardlink zero-copy, rerun incremental; run on the storage machine, §3.3) |
| Server-side usage statistics | `grovefs du --home H --view V [--data-servers A[,A2]\|--data-dir R] [--path P] [-d N] [-b] [--total]` (batch-fetches and sums object bytes, not per-file stat; byte source `--data-servers` > `--data-dir` > metadata root, refuses outright if the source can't be determined, counting a warning only on a confirmed miss, §12.6) |
| Unmount | `grovefs umount -m MP` |
| Inspect/migrate the metadata layout | `grovefs layout --meta-dir D show` (reports v1 flat or v2, lists the view trees) / `grovefs layout --meta-dir D migrate [--dry-run]` (v1→v2: moves into `meta/<view>/<view>.db` and `meta/_control/`; offline, requires no live mount and no grovefs process on this machine, §12.4) |
| Data service | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| Govern the same metadata from any machine (using an account key instead of the db password) | `grovefs view --control-server H --api-key K create\|define\|alias\|list\|rm` (with `--meta-store` replaced by `--control-server`, same as above) |
| Account key (governance port) | `grovefs --home H apikey create --label L --for admin --owner ACCT` (the first key minted on the disk-holding host) / `apikey --control-server H --api-key K list\|create\|revoke\|rotate\|set-ro\|calls\|update\|purge` |
| Open a remote governance port | `grovefs meta-server --meta-dir D --listen IP:PORT --control-listen IP:PORT` (the second port runs only governance verbs, accepts only admin keys, audits per call) |
| Web console (accounts + base-view delegation) | Install-time seeding: `grovefs meta-server … --control-listen H:P --admin-user A --admin-password-file F` (applies only while the accounts table is empty); start it: `grovefs web --control-server H:P --api-key-file K [--listen 127.0.0.1:8787]`; headless management: `web account create\|list\|passwd\|role\|disable\|enable\|rm` and `web grant add\|rm\|list` (passwords only via `--password-file`/`--password-stdin`, §3.5.3) |
| Metadata service (PostgreSQL backend) | `grovefs meta-server --meta-store postgres://user:pw@host:5432/db --listen IP:PORT` (under PG `--meta-dir` is optional — the server writes no authoritative data locally) |
| Mint an access key | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount\|admin] [--owner ACCT] [--view V] [--ro]` (echoed at creation and readable back later with `apikey secret`; `mount` = opens only mount/data plane, `mcp` = opens only the governance interface, `admin` = opens the governance port and admits the prior two, `--owner` required) |
| List/revoke/audit keys | `grovefs apikey --meta-dir D list [--view V]` (with `--view`, lists only keys that can open V; a pure alias also matches keys bound to the real name) / `revoke --id ID` (exact id or unique prefix; revoking seals the key's **live mounts**) / `secret --id ID` (reads the stored wrapped copy back as the full `<id>:<secret>`; its owner or a superuser gets it, a read through a server is audited while a direct read of the local store is not) / `calls --limit N` (includes rejected mount connections) / `purge --before DUR [--dry-run]` (trims audit rows; SUPERUSER only, no console button) |

