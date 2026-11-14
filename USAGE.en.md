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
| See it work on one machine in 5 minutes (no server process) | `grovefs init` → `mount` → `umount` | §2, §15.1 |
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
| Lock the interface and mount points, revoke per-caller | `apikey create/list/revoke/set-ro/calls` | §3.5.1, §6 |
| Read-only mount / zero-cache strong consistency | `mount --ro` / `--no-cache` | §7 |
| Performance observation and rate limiting (per-stage latency / bandwidth / cache TTL) | `mount --stats*` / `--bw-read|--bw-write` / `--cache-ttl` | §8 |
| Share one view across several machines | `data-server` + `meta-server` | §4, §11 |
| Swap metadata to the PostgreSQL backend | `--meta-store postgres://…` everywhere | §4.1, §6.2, §15.3 |
| Govern the same metadata from any machine (never hand out the DB password) | `meta-server --control-listen` + `view`/`apikey`/`refs --control-server --api-key` | §3.5.2 |
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
| **View** | Each user gets an independent directory tree (with custom nesting), stored in one SQLite DB: `<home>/meta/<view>.db`. Users cannot see each other's files. |
| **Object** | The real bytes of a file. One file = one object file under `blocks/`, named by UUID (or several chunks when striped); objects are auto-distributed over 256 bucket subdirectories by a hash of their key. The view's directory entries point at object keys. |
| **Base dir (base / `--home`)** | **Single-machine mode**: the local root holding `blocks/` (data) + `meta/<view>.db` (views), used by `init` and by mounts that pass no `--meta-server`/`--data-servers`; defaults to `$GROVEFS_HOME` or `~/.grovefs`. **Server/client mode**: clients don't need it (a remote mount writes nothing locally); each store-holding command on the storage machine declares its own root (`meta-server --meta-dir`, `apikey --meta-dir`, `data-server --data-dir`, taking precedence over `--home`; `--home` is the global default root). |

Data and views are **separate by nature**: a view is just "a directory tree + pointers", the bytes always live in `blocks/`.

### 0.1 Terminology map (newcomer quick reference)

| Term in the docs | Meaning |
| --- | --- |
| `--home` | In single-machine mode, the umbrella directory holding `blocks/` (data) and `meta/` (view DBs); server-side commands use their own `--meta-dir`/`--data-dir`, clients don't use it |
| data-server / meta-server | The two central service processes: the former manages bytes (object storage), the latter manages the directory tree (metadata); clients point `--data-servers`/`--meta-server` at them respectively |
| base view / derived view / alias / fork | The four view forms: an independent storage tree (writable) / a read-only display lens defined by rules on the server / a renamed full-tree-writable entry / an independent tree that copies the directory but not the bytes (beginners: see the analogy and comparison table in §3.0 first; operational detail in §3.4–§3.4.1) |
| token / API key | A token is the cluster admission gate (one shared passphrase); an API key is a per-caller, revocable credential that can be bound to a view / marked read-only (§3.5.1) |
| control plane / mount plane | Control plane = `grovefs mcp`'s view-definition management interface; mount plane = the FUSE client's connections to meta/data — the two gates are independent and their scopes never cross (§6) |

> Readers who haven't yet built a "FUSE / mount / object storage" mental model should first read the web manual docs.html's "§0 zero-base primer" (six keywords, what you do and don't need to know, minimum hardware requirements).

### Common parameters: what they govern first, then how to write them

> In the commands below, `--home`, `--meta-dir` and the like are the **parameter forms** (copy them as-is); what each governs is taken from its **actual meaning**:

| What it governs (actual meaning) | Command-line form |
| --- | --- |
| **Local storage root** — the umbrella directory where a single-machine grovefs stores `blocks/` (data) and `meta/` (views) | `--home <dir>` |
| **Server-side metadata root** — on the `meta-server` machine, the umbrella directory holding the authoritative view DBs, view definitions, and API keys | `--meta-dir <dir>` |
| **Where bytes land on disk** — the storage root `data-server` actually writes bytes to (defaults to the metadata root) | `--data-dir <dir>` |
| **The existing directory to be managed** — that batch of data already sitting on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory used to reach this tree | `-m <dir>` (i.e. `--mountpoint`) |
| **Central service address** — the metadata / data service the client connects to (host:port) | `--meta-server <addr>` / `--data-servers <addr>` |
| **View name** — an arbitrary label for a logical tree (unrelated to any Linux username) | `--view <name>` |
| **Cluster admission passphrase** — the shared secret the server sets, presented by the client on mount (one for the data plane, one for the metadata plane) | server `--token <str>` + client `--token <str>` |
| **Revocable access credential** — the server admits per-caller API keys instead (`grovefs apikey create --for mount`); listable, auditable per caller, revocable, and a revoke closes **already-established** mounts; `--view V` **binds a key to a single view** (opening another view is refused at the handshake), `--ro` is read-only enforced server-side (per write-intent op refused) | server `--require-api-key` + client `mount --api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) |
| **Account key (governance credential)** — a key minted with `--for admin --owner <account>` stands in for the DB password: whoever holds it can manage the same metadata from **any** machine that can reach the governance port, with the real reads/writes done on behalf by `meta-server`; one account may hold many keys, each minted and revoked individually | server `--control-listen <addr>` + client `--control-server <addr> --api-key <ID:SECRET>` (or env `GROVEFS_CONTROL_KEY`) |
| **Mount as a given identity** — lock arbitration and audit record ownership under this uid | `--run-as-uid <uid>` |
| **Read-only / zero-cache strong consistency** — mount read-only / disable client caching | `--ro` / `--no-cache` |
| **Fallback switch for directory-listing speed** — turn off READDIRPLUS (which carries attributes alongside the entries) and fall back to plain READDIR + a per-entry `LOOKUP`, for A/B performance troubleshooting only (only mounts whose provider can page inside its own store negotiate READDIRPLUS: local `--home` SQLite/PG, and remotes against a paging-capable `meta-server`; derived/lens views do not negotiate it) | `--no-readdirplus` |
| **Per-stage latency telemetry** — opt-in recording of the time spent in each read/write/metadata stage (aggregate p50/p90/p99/max offline to locate bottlenecks; printed on exit + periodic write to disk + a hidden node `.grove-stats`; not a throughput claim) | `--stats` / `--stats-file <path>` / `--stats-interval <secs>` / `GROVEFS_STATS=1` (see §8.1) |
| **Read-cache TTL tuning** — enlarge the kernel attr/dentry cache TTL (default 2 s) to cut repeated LOOKUP/GETATTR on read-heavy / immutable trees; cross-client freshness still relies on the default invalidation push plus the newer-kernel `AUTO_INVAL_DATA` backstop, `--no-cache` forces it to 0 | `--cache-ttl <secs>` (see §8.2) |
| **Per-mount content bandwidth shaping** — put a GCRA token slot on this mount's content reads and writes (non-overlapping time slots laid out by byte, so the long-run rate equals the cap exactly; overshoot only sleeps, never drops bytes; write-side latency is backpressure), `0` = unlimited | `--bw-read <rate>` / `--bw-write <rate>` (e.g. `50M`; env `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE`; see §8.2) |

### About `--view` and path capacity

- **`--view` is unrelated to Linux usernames** — it is only an **arbitrary label** in the view namespace. The binary never calls `getpwnam`, never checks uid, never requires `useradd`. `alice`, `team-a`, `u42`, `研究组` all work as a view name — as long as it is a legal filename on the target filesystem. It serves exactly two purposes: ① it determines the `<home>/meta/<view>.db` filename; ② it stamps a `grovefs:<view>` label on the FUSE mount for `mount`/`findmnt` to display. **Authentication comes from "can access `--home` / `blocks/`" plus the server's `--token` / `--require-api-key` gates**, not from a Linux user.
- **One tree per user, fully autonomous paths inside the tree** — once mounted, under that mountpoint you can `mkdir`/`touch` **any number** of files and directories, to **any depth**, with any nesting, no different from working in your own home. The granularity of isolation between users is "the whole view tree", not a single path. A manifest is only a convenient entry point for pre-building a skeleton, not a cap on the number of paths; after mounting you can `mkdir -p a/b/c/d/...` at any time.

---

## 1. Environment setup

```bash
# 依赖:只需 Rust 工具链(edition 2021);不需要 libfuse 开发包,也不需要 pkg-config
# (FUSE 客户端是纯 Rust 的 fuser;非 root 挂载时靠发行版的 setuid 助手 fusermount/fusermount3)
# export PKG_CONFIG="$PWD/.build-bin/pkg-config"   # 历史遗留,现行依赖图无人查 pkg-config,删掉也照样构得成
export LC_ALL=C LANG=C
cargo build --release                                       # 产物:target/release/grovefs
```

Executable subcommands: `init` / `mount` / `umount` / `data-server` / `meta-server`.

### 1.1 Command shorthand: env vars and an auto-loaded config file

The long commands you type over and over can be slimmed down wholesale. The precedence for every knob is always:
**command-line argument > current environment variable > config file > built-in default**.

| Knob (env) | Parameter it stands in for | Description |
| --- | --- | --- |
| `GROVEFS_HOME` | `--home` | Local storage root |
| `GROVEFS_META_SERVER` | `mount --meta-server` | Metadata service address (incl. failover list) |
| `GROVEFS_DATA_SERVERS` | `mount --data-servers` | Data service address (incl. stripe list) |
| `GROVEFS_STRIPE_BYTES` | `mount --stripe-bytes` | Stripe chunk size for cross-server striping (default 16 MiB; only takes effect alongside a multi-address `--data-servers` list; an illegal value refuses to start) |
| `GROVEFS_MEMBER_ID` | `data-server --member-id` | This data-server's stable identity id within a striped cluster (a 64-bit integer unique per member; an illegal value refuses to start; if not given, the node participates in stripes in the historical positional mode) |
| `GROVEFS_TOKEN` | `mount --token`, `meta-server/data-server --token` | Cluster admission passphrase |
| `GROVEFS_META_DIR` | `meta-server --meta-dir` | Server-side metadata root (precedence: `--meta-dir` argument > this env > `--home` > `~/.grovefs`) |
| `GROVEFS_META_STORE` | `--meta-store` (`view`/`apikey`/`mcp`/`meta-server`/`data-server`/`du`/`import`) | Non-SQLite backend addressing; when no flag is given this env is the fallback (source `resolve_store()`). `import` only writes a local SQLite tree, so if this env holds `postgres://…`/`fdb://…` it is **honestly refused** |
| `GROVEFS_API_KEY` | `mount --api-key` | env for an existing per-caller credential |
| `GROVEFS_MCP_API_KEY` | MCP `initialize` handshake | Whole-server pre-authentication |
| `GROVEFS_CONTROL_KEY` | `--api-key` (with `--control-server`) | The governance port's admin key (`<id>:<secret>`); if unset it falls back to `GROVEFS_API_KEY`, and either way the secret stays out of argv |
| `GROVEFS_STATS` | `--stats` | `1/true` enables telemetry |
| `GROVEFS_SQLITE_READ_POOL` | — (internal tuning) | Per-view SQLite warm read-connection count, range 0–16; when unset, **local mounts follow `--threads`** (default 4), embedded/`meta-server` default to 1; `0` is fully lazy (open/close per read). Raising it trades read concurrency for about 1.3 extra fds/connections per view (the concurrent-small-file-creation curve in §8 is sized by this: at 16 workers, a read pool of 1 gives ~1.2k, a read pool of 16 gives ~5.5–6.6k files/s) |
| `GROVEFS_PG_WRITE_POOL` | — (internal tuning) | Per-view PG write-connection warm pool, default 8, range 0–64; when the pool is empty a connection is opened ad hoc, the hard ceiling is PG `max_connections` |
| `GROVEFS_PG_READ_POOL` | — (internal tuning) | Per-view PG read-connection warm pool, default 4, range 0–64 |
| `GROVEFS_PG_SYNC_COMMIT` | — (internal tuning) | Session-level `SET synchronous_commit` on PG write connections; default `on` (PG's own default); with `off`/`false`/`0`/`async` a COMMIT return no longer waits for the local WAL to hit disk. On the same test rig (with the write-statement cache already in place) the N=4 write path goes 5,225 → 6,142 ops/s (+18%), N=1/N=8 within run-to-run jitter, at the cost that a crash may lose the last several committed transactions and, with a synchronous standby configured, it also stops waiting for its ack (RPO=0 must keep it `on`) |

On top of the env vars above, grovefs also **auto-loads a `KEY=VALUE` config file** at startup to fill any variable not yet set — so you don't even have to export each time:

```bash
# 查找顺序: $GROVEFS_CONFIG 指定路径 > $HOME/.config/grovefs/grovefs.conf
# (只认 GROVEFS_* 键,其余行提示后忽略)
cat > ~/.config/grovefs/grovefs.conf <<'EOF'
GROVEFS_META_SERVER=192.168.1.10:50110
GROVEFS_DATA_SERVERS=192.168.1.10:50100
GROVEFS_TOKEN=s3cret            # 文件里有口令就 chmod 600(与 env 里放口令同一契约)
EOF
chmod 600 ~/.config/grovefs/grovefs.conf

# 之前: grovefs --home /data/myfs mount --view alice -m /mnt/alice \
#         --meta-server 192.168.1.10:50110 --data-servers 192.168.1.10:50100 --token s3cret
# 之后: 只剩"这次挂哪个视图到哪儿"
grovefs mount --view alice -m /mnt/alice
```

Format details: `#` comments, blank lines, an optional `export ` prefix (the same file can double as a shell script), and single/double-quoted values are all supported; non-`GROVEFS_*` keys and lines missing `=` are ignored after a one-line warning carrying `file:line`; an explicitly specified `$GROVEFS_CONFIG` that can't be read warns and continues (non-fatal). The re-exec child of a background mount inherits the environment, so the shorthand still applies there. `--view`/`-m` are deliberately not included — they differ per mount, and baking them into defaults would be wrong.

Multi-cluster caution: these values are one set of **process-wide global coordinates**; a single command reads one file only and does not auto-match by mount point / view. To switch clusters, pass it per invocation via `GROVEFS_CONFIG=~/grovefs-B.conf grovefs …` (one file per cluster, `chmod 600`) rather than a persistent export — an already-exported variable of the same name **silently overrides** another cluster's config file (the file only fills keys not yet set); the mount banner `using remote metadata service at …` lets you verify the coordinates. The control-plane `--meta-dir` has no env fallback and must be given explicitly per cluster; `GROVEFS_META_STORE` is a global key, and a leftover export would point governance commands at a different PG set, so unset it before switching clusters.

Regression coverage: `bash .build-bin/run_all.sh config` (28 assertions: the precedence chain, parser edge cases, a real FUSE mount driven entirely by the file).

---

## 2. Quick start: single-machine local mode (simplest)

Data and views both live in one local `--home`, no server at all. Good for trying out the features first.

```bash
Y=./target/release/grovefs
BASE=/data/myfs                       # 物理基目录

# 1) 初始化两个用户的视图
$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

# 2) 挂载(默认转后台:挂载点就绪即返回;要前台显示加 --foreground,Ctrl-C 即卸载)
$Y --home $BASE mount --view alice --mountpoint /mnt/alice
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob     # 默认转后台,立即返回

# 3) 像普通目录一样读写
mkdir -p /mnt/alice/projects/2026
echo "年度报告" > /mnt/alice/projects/2026/报告.txt
ls /mnt/bob                                      # bob 看不到 alice 的文件

# 4) 卸载
$Y umount --mountpoint /mnt/alice
fusermount -u /mnt/alice                          # 等价
```

**View isolation**: `/mnt/alice` and `/mnt/bob` are two independent trees, invisible to each other. The hierarchy (nested directories you `mkdir`) is fully custom.

---

## 3. View customization and governance: manifest, lens, derived, alias, fork

`init --manifest` can lay down a directory skeleton in one shot when creating a view. A manifest supports only two directives per line:

```text
# 这是注释,行首 # 会被忽略
mkdir projects
mkdir projects/2026
touch projects/2026/README.txt
```

```bash
$Y --home $BASE init --view alice --manifest ./seed.txt
# 输出:applied seed manifest ./seed.txt
```

Once mounted you see the pre-built hierarchy. Existing mkdir/touch entries are idempotent (re-running does not error).

### 3.0 The four views in one table (beginner edition)

> This section answers just three things: **what each of the four views is, which files/tables each command produces, and how to fill in the parameters**.
> §3.1–§3.4.1 are the operational detail; on first contact, read this section fully, then follow the examples in §3.2/§3.4.

> **Explicit creation (must read)**: the server `meta-server` and a local `--home` mount follow **one and the same rule** — when `meta/<name>.db` (under PG, a `grovefs_v_<hex>` schema) does not exist, it is refused outright with a readable reason, and an empty DB is **never** created at mount time; this rule has no switch.

**First, correct the most common misconception: writing data is not creating a base view.** A `mkdir`/`echo` into a mountpoint only adds nodes to an **already existing** tree; when a local mount finds `meta/<name>.db` missing it **refuses outright**, the server refuses too, and it writes nothing. The only entry points for creating a base view are `init --view` / `view create --view` (the latter first checks that the name is not already taken by a derived/alias/fork), plus `import --view V --source DIR` / `view import` (which build the tree when the target is absent).

#### 1. A one-line analogy

| Form | Analogy | One line to remember it |
| --- | --- | --- |
| **Base view** | The warehouse original | The original that owns the data; content writable, deleting a name reclaims bytes |
| **Derived view** | A read-only filter on the original | Rules stored on the control plane (SQLite local `meta/views/`, PG `grovefs_control`, or FDB cluster `ctl`); mounts read-only; the data is still the base view's copy |
| **Pure alias** | A door plate | Gives the original a public name; what mounts is the original, whole tree writable |
| **fork** | A photocopied catalog | Directory entries copied once, tidiable at will; bytes still shared, content can't be written in |

#### 2. The four forms in one table

| | Base view | Derived view | Pure alias | fork |
| --- | --- | --- | --- | --- |
| **Essence** | A real physical tree | base tree + rules (builds no tree) | name → real view (builds no tree) | An independent physical tree (dir entries copied from the base tree) |
| **Create command** | `init --view V` / `view create --view V` | `view define --name D --base B --rules F` | `view alias --name A --real R` | `view fork --name F --base B` |
| **New files** | `meta/V.db` | `meta/views/D.layout` | `meta/views/A.layout` | `meta/F.db` + `meta/views/F.layout` |
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
| Does a derived view **copy** the base view, just read-only for the user? | **It's not a copy.** A derived view lands only one `.layout` text (`base B` + rules), copying no dir entries, copying no bytes. Each time a directory is read, the server/local computes the display shape **on the fly** on the base tree per the rules. | `view define` builds no `meta/D.db`; the directory has only `meta/views/D.layout`; `view list` prints `view 'D' -> base 'B': 1 hide, 1 only, 1 reloc rule(s) (read-only)` rather than "a tree"; after editing a rule it reshapes via the server within ≤2s. The cost is that the directory can't be changed either and content writes return `EROFS` — because it never had its own dir entries to change. |
| Is an alias just the base view with a different name? | **Yes.** An alias has no metadata of its own; at resolution the name is swapped to the real view `R`, and thereafter reading dirs, making dirs, writing files, deleting files all land on `R`'s **same tree, same bytes**. | An alias occupies only one line `alias R` in `meta/views/A.layout`, with no `meta/A.db`; after `view alias`, a file written into `A/` is immediately visible in `R/` (the same inode). The only difference is **name governance**: while the alias exists `mount --view R` is refused, and one real view can have only one alias at a time (rotate with `--retire`). |
| Is a fork a full metadata copy, where you can change dirs but can't delete the underlying data? | **First half correct; second half needs to separate "delete the name" from "delete the bytes".** A fork copies the base tree's `node` rows (dir entries, incl. inode/hardlink relations), so it has its own complete, modifiable directory tree; content writes return `EROFS`. Deleting a name in the fork removes only the fork's own dir entry, the base tree is untouched; whether bytes are reclaimed depends on the `refs.db` ledger — the object is really deleted only when no view holds it anymore. | `view fork` creates `meta/F.db` and records in `refs.db`; `rm --name F` deletes the fork's own tree + definition, the base tree and still-held bytes are untouched; files added to the base tree later do not automatically appear in the fork. |

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
| Visible shape | delete a row = hide, materialized in `meta/F.db` | computed by rules, no rows landed |
| Base tree add/rename/reattr | **does not follow** (the copy happened in an instant, must re-fork) | **follows immediately** (computed fresh on each readdir) |
| Cost (20,201 entries measured) | `fork` 0.61s, **20,201 rows / 3.5 MB** | `define` 0.00s, **47 bytes** |
| Cost (100,002 entries measured) | `fork` **0.65s**, `meta/F.db` **12 MB** + 13-byte definition | `define` 0.00s, 100 rules **2.9 KB** |
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
| `grovefs view --meta-dir D create --view V [--manifest F]` | creates `D/meta/V.db` (with `node`/`ctrl`/`locks` tables); `--manifest` additionally writes dir/empty-file nodes into the tree | doesn't change other views |
| `grovefs --home D init --view V [--manifest F]` | same as above (equivalent entry; does not check whether the name already has a definition) | same as above |
| `grovefs --home D mount --view V` (local, no definition) | if `D/meta/V.db` is absent it **refuses outright**, telling you to `view create` first. The mount itself writes nothing | doesn't change an existing tree |
| `grovefs view --meta-dir D define --name X --base B --rules F` | writes/overwrites `D/meta/views/X.layout`, body containing a `base B` line, the rest being rules | builds no `X.db`, copies/moves no content |
| `grovefs view --meta-dir D alias --name A --real R [--retire OLD]` | writes/overwrites `D/meta/views/A.layout` (a single line `alias R`); `--retire` also deletes the old alias definition | builds no `A.db`; `R`'s data untouched |
| `grovefs view --meta-dir D fork --name X --base B` | creates `D/meta/X.db` (copying B's `node` rows, preserving inode/hardlink relations); writes `D/meta/views/X.layout` (a single line `fork B`); records the shared bytes in `D/meta/refs.db` | copies no bytes; doesn't change B's tree |
| `grovefs view --meta-dir D rm --name X` | deletes `D/meta/views/X.layout`; if X is a fork, also deletes `D/meta/X.db` (+WAL/SHM) and rebuilds `refs.db` | derived/alias touch no data; fork doesn't touch the base tree or still-held bytes |
| `grovefs view --meta-dir D rm --name V --base` | deletes `D/meta/V.db` (+WAL/SHM), rebuilds `refs.db`, and reclaims objects **no one holds anymore** from `--data-dir` (default `D`) | bytes still held by other views are kept |
| `grovefs mount --view V --layout F` | the rules file itself isn't landed; if a local `meta/V.db` is absent it refuses outright (same as without `--layout`) | doesn't change server state; affects only this mount |
| `grovefs view --meta-dir D export/import --view V` | export: one JSONL file; import: creates `D/meta/V.db` (with `--meta-store fdb://…` the import opens a subspace, §6.3) | doesn't move bytes (bytes must be copied separately into `blocks/` or handed to the data-server's data root) |

A few easily-missed rules:

- **A definition is just a text slot**: `view define/alias/fork` write `meta/views/<name>.layout`; only fork additionally creates a `meta/<name>.db` tree.
- **A definition may precede the base tree**: `view define` allows the base to not be built yet (it warns), but **at mount time** both server and local enforce that the base must already be created, otherwise the derived name is refused mounting. Recommended order remains "`view create` the base view first, then define".
- **Deleting a definition is not hot-effective**: after `view rm --name`, new mounts immediately don't see the definition; already-mounted derived/alias connections keep the old resolution until remount (or until the server cache is cleared by an HA/snapshot event).
- **Deleting a base view has four guards**: the name itself must not still be governed by a definition; no other derived/alias/fork may point at it; it must not still be mounted on this machine; and the tree in PG must be deleted **with the same command** (`view --meta-store postgres://… rm --base`) — the ledger decrement and `DROP SCHEMA` finish in one transaction (§15.8); hand-`DROP SCHEMA` past the CLI leaves ledger rows that forever over-count.

#### 4. What lands on disk: files and tables

After one `view create` + `view define` + `view alias` + `view fork`, the storage root looks roughly like this:

```text
D/
├─ blocks/<2hex>/<key>         # 所有视图共享的文件字节(key 是 UUID,不是内容哈希)
└─ meta/
   ├─ team.db                  # 基视图 team: node + ctrl + locks
   ├─ lab.db                   # fork lab: 自己的 node + ctrl + locks
   ├─ refs.db                  # 跨视图持有账本:谁还在引用哪个 key
   ├─ views/
   │  ├─ docsonly.layout       # base team + only /docs
   │  ├─ public.layout         # alias team
   │  └─ lab.layout            # fork team
   └─ api_keys.db              # 可选:API key/审计(SQLite 控制面)
```

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
| `meta/<view>.db` | a SQLite DB; the `node` table is the directory tree (one row per name), `ctrl` a key-value table (today only the HA `epoch`), `locks` the lock leases | `init`/`view create`/`view fork`/`view import` |
| `meta/views/<name>.layout` | **plain text** definition, not a database; three mutually exclusive forms: `base B`+rules / `alias R` / `fork B` | `view define`/`alias`/`fork`; deleted by `view rm` |
| `meta/refs.db` | the shared-bytes ledger; when a name is deleted it decides "whether any other view still holds it" | auto-booked on every add/delete; `refs rebuild` recomputes |
| `blocks/<2hex>/<key>` | the file's real bytes; all views store only the key | produced on write/import; deleted on reclaim |
| `meta/api_keys.db` | optional control plane: key digests and call audit | the `apikey` command / `mcp --require-api-key` |

A row of the `node` table contains: `id`, `ino`, `parent_id`, `name`, `is_dir`, `data_ref` (the key pointing into blocks), `link_target`, `mode/uid/gid`, `mtime/atime/ctime`, with a uniqueness constraint on `(parent_id, name)`. `ctrl` is a `(k, v)` key-value table holding today only the HA `epoch` (**not an inode counter** — new inodes aren't allocated from a cursor, they equal their own row's `id`); `locks` are POSIX/BSD lock leases. `refs.db` has a single `holders(data_ref TEXT PRIMARY KEY, holders INTEGER)` table recording how many views hold each object. `api_keys.db` (optional) has two tables `api_keys` and `mcp_calls` (credential digests / audit). **The difference between views is not the table structure, but "whether this name has its own `.db`"**.

**The PG backend's (`--meta-store postgres://…`) correspondence**

| SQLite on-disk artifact | PG counterpart |
| --- | --- |
| `meta/<view>.db` | a schema `grovefs_v_<hex(view)>` (with the same-shape `node`/`ctrl`/`locks`) |
| `meta/views/<name>.layout` | a row in the `grovefs_control.view_defs` table |
| `meta/refs.db` | the shared table `grovefs_refs.holders` |
| `meta/api_keys.db` | `grovefs_control.api_keys` + `mcp_calls` |

Under PG, `view define/alias/rm --name` works as usual (writing `view_defs`); `view --meta-store postgres://… create` explicitly builds that view's schema (`grovefs_v_<hex>`); `view --meta-store postgres://… rm --base` decrements the ledger (per-key `grovefs_refs.holders`) and runs `DROP SCHEMA … CASCADE` in one transaction, then reclaims `blocks/` bytes by the `data_ref` read before deletion (§15.8); `view fork` is likewise available on all three tiers (copying the `grovefs_v_<hex>` schema in a single transaction); zero-copy `import` is still based on the local file tree and refuses.

> **PG DB-creation entry point**: `grovefs view --meta-store postgres://… --meta-dir D create --view V` explicitly builds that view's `grovefs_v_<hex>` schema and root row. Mounting does not build a schema — when the schema is absent, `M_OPEN` refuses outright and prompts this command; an existing schema can also be restored from a dump.

#### 5. How to fill in the parameters

| Parameter | Used where | Meaning |
| --- | --- | --- |
| `--meta-dir D` | `view`/`apikey`/`mcp`/`refs`/`import`, and `meta-server` | the metadata root: has `meta/` under it; points to the same directory as `meta-server --meta-dir` |
| `--home D` | grovefs's global parameter (before the subcommand) | the single-machine local root; falls back to it when `--meta-dir` isn't given explicitly |
| `--meta-store postgres://…` | `view`/`apikey`/`mcp`/`meta-server`/`import` | the control plane switches to PG: `view_defs`/`api_keys`/`mcp_calls`; nothing local lands as `.layout`. An `import`'s product can only be a local SQLite tree, so a non-SQLite URI is refused with the alternative path spelled out (in-mount `cp`/`rsync`) |
| `--view V` | `init`/`mount`/`view create` | the view name to build/mount (one safe path element) |
| `--name X` | `view define/fork/alias/rm` | the new definition's name (derived/fork/alias name, or name to delete) |
| `--base B` | `view define/fork` | the referenced **physical base view** name |
| `--real R` | `view alias` | the real physical view name the alias points to |
| `--retire OLD` | `view alias` | atomic rotation: after creating the new alias, delete the same real view's old alias |
| `--rules F` | `view define` | the rules file path (plain text, one verb per line) |
| `--manifest F` | `init`/`view create` | the seed manifest (`mkdir`/`touch` lines), pre-laying the skeleton when building the tree |
| `--data-dir D` | `view rm --base`/`import`/`du` | the root where `blocks/` lives; defaults to the metadata root (same path as `--meta-dir`/`--home`), only needs naming when meta and data are separated; deleting a base view reclaims bytes from here, `du` measures bytes from here |
| `--layout F` | `mount` | this mount's local rules file; affects only this mount, writable; under a local `--home` when combined with a derived name, the explicit file wins, no two-layer lensing |

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
2. It is a single `fork B` line → open `X`'s own `meta/X.db` (the tree copied when the fork was built), unrelated to `B`'s later changes.
3. It is `base B` + rule lines → open `meta/B.db` and apply the following rules as a read-only lens.
4. No `X.layout` → `X` itself is a base view, open `meta/X.db` directly.
5. `meta/X.db` absent → **refuse outright** (local and server consistent), telling you to build the tree first via `view create` / `init` / `view import`; mounting will not create an empty tree.

> The server resolves `<meta-dir>/meta/views/`, the client `--home` resolves `<home>/meta/views/`; both do only a **single hop**: an alias can't point at another alias, and a name that has a definition can't be a fork's base. On a **server** mount it additionally refuses "derived over derived", and a derived mount requires the base to already exist; a local `--home` likewise requires the base tree to be created, else it refuses.

#### 7. The easiest pitfalls

1. **Writing data does not create a base view.** New trees can only be made with `init`/`view create`/`import`; a mount missing the DB refuses outright and creates no empty tree.
2. **A derived view is read-only and is not a permission boundary.** Anyone who can mount the base name still sees the whole tree; use independent views + token/API key for isolation, don't rely on "hiding a few directories".
3. **An alias locks the real name.** While the alias exists, `mount --view R` is refused; new mounts use the alias. After deleting the alias the real name becomes mountable again (new mounts only). One real view can have only one alias at a time, rotate it with `--retire` in one command.
4. **A fork copies only directories, not bytes.** Content writes return `EROFS`; deleting the fork doesn't affect the base tree; files added to the base later don't automatically appear in the fork; all three backends can fork, and it's the same verb in each (the store-holding process does the copy: SQLite copies file rows, PG across schemas, FDB across subspaces, each committing by budget — one transaction if it fits, otherwise automatic batching).
5. **`--layout` and `meta/views/*.layout` are different things.** The former affects only this mount and is writable; the latter is a server-named definition, read-only. When a local run gives both `--view <derived-name>` and `--layout F`, the explicit `--layout` wins and the published definition is bypassed; if the target view was never created, it errors outright. Prefer picking one.
6. **Bytes are truly reclaimed only when the ledger hits zero.** Deleting a derived/alias removes only the definition; deleting a fork removes only its own tree; deleting a name in any view frees the object only once no other view holds it in `refs.db`.
7. **`view define` may precede the base tree.** It warns base tree missing; it's enforced only at mount. `view create` first, then define, saves one round of troubleshooting.
8. **A misspelled view name errors outright, no empty tree grows.** Both local and server refuse uncreated view names — this is the only behavior, no switch.

**Error cross-reference**: `EROFS` = read-only filesystem (derived mounts, fork content writes); `EPERM` = operation not permitted (creating into a synthesized dir, the server's derived-write gate).

#### 8. Walk it through from scratch (copy as-is)

```bash
Y=./target/release/grovefs
D=/data/myfs

# ① 建基视图(真正存数据的那棵)
$Y view --meta-dir $D create --view team
$Y --home $D mount --view team -m /mnt/team &
mkdir -p /mnt/team/docs /mnt/team/raw
echo "hello" > /mnt/team/docs/a.md

# ② 派生:只读,只显示 /docs
cat > docsonly.rules <<'EOF'
only /docs
EOF
$Y view --meta-dir $D define --name docsonly --base team --rules docsonly.rules
$Y --home $D mount --view docsonly -m /mnt/docsonly &
ls /mnt/docsonly                  # docs
echo x > /mnt/docsonly/docs/b.md  # Read-only file system

# ③ fork:独立目录树,结构可改、内容只读
$Y view --meta-dir $D fork --name lab --base team
$Y --home $D mount --view lab -m /mnt/lab &
mkdir /mnt/lab/review                        # 可以
mv /mnt/lab/docs/a.md /mnt/lab/review/a.md    # 可以
echo x > /mnt/lab/new.md                      # Read-only file system

# ④ 别名:公开名,全树可写;此后 team 不能再按真名新挂
$Y view --meta-dir $D alias --name public --real team
$Y --home $D mount --view public -m /mnt/public &
echo hi > /mnt/public/docs/c.md               # 可以,写的就是 team

# ⑤ 看清单与落盘
$Y view --meta-dir $D list
ls $D/meta $D/meta/views
```

`view list`'s output takes this form (order may differ):

```text
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
alias 'public' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
```

> Note: while the alias exists, `base view 'team'` still shows in the listing, but new mounts must use `public`; `team` becomes mountable only after the alias is deleted.

Cleanup order (delete definitions first, then the base tree):

```bash
$Y view --meta-dir $D rm --name docsonly       # 只删定义;team 的数据不动
$Y view --meta-dir $D rm --name lab            # 删 fork 自己的树;team 与共享字节不动
$Y view --meta-dir $D rm --name public         # 删别名;team 恢复可挂(仅新挂载)
$Y view --meta-dir $D rm --name team --base --data-dir $D   # 连树带无人持有的字节一起回收
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
$Y --home $BASE mount --view alice -m /mnt/alice --layout lens.txt
layout overlay lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at /mnt/alice (base $BASE)

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
| `grovefs view --meta-dir D create --view V [--manifest F]` | V = the new **base view** (physical tree) name, F = optional seed manifest | explicitly creates a **physical tree** `D/meta/V.db` (equivalent to `init --view V`); **only a base view's deletion truly reclaims the underlying bytes** (cross-view reference ledger hits zero before physical GC, §5.3); derived views/aliases only reshape/rename, never participate in physical deletion |
| `grovefs view --meta-dir D define --name V --base B --rules F` | D = meta root (same as meta-server's `--meta-dir`), V = derived name, B = physical view name, F = rules file | writes/replaces `D/meta/views/V.layout` (under the PG backend `--meta-store`: edits a `grovefs_control.view_defs` row); `V`'s mount is **read-only** (writes go to `B`); for `meta-server`, established connections **reshape hot** within ≤2s (no remount); a local mount takes effect next mount |
| `grovefs view --meta-dir D list` | — | lists all view definitions: derived views (base/rule counts), pure aliases (→ real view) and forks (→ base view), plus **undefined bare base views** (→ their own `meta/<name>.db`), §3.4/§3.4.1 |
| `grovefs view --meta-dir D rm --name V` | V = derived, alias, or fork name | deletes only the definition, **touches no data**; exception: `rm --name <fork>` also deletes that fork's own `meta/<name>.db` (+WAL/SHM) and runs `refs rebuild` to recompute the ledger (base view and bytes unaffected); after deleting an alias the base name becomes mountable again (new mounts only) |

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

$ grovefs --home $BASE mount --view docsonly -m /mnt/lens &   # 本地：直挂派生名，无 --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ grovefs mount --view docsonly --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/lens_r &   # 远程：同样只给名，M_OPEN 握手带只读标志
$ ls /mnt/lens                       # 只有 docs;基名 team 挂出来仍是全树，且可写
docs
$ echo x > /mnt/lens/docs/new.md     # 经虚拟视图的写一律拒绝
bash: /mnt/lens/docs/new.md: Read-only file system
```

Semantics and guards (verified by the section-9 e2e): define rejects `name == base`, rejects the base itself being a derived view (no nesting), rejects shadowing a physical `<name>.db`, and fail-fasts on illegal rules; **virtual views always mount read-only (hard contract)** — writes belong only to the original (base-name) view, enforced at three layers: `meta-server` carries the derived flag in its `M_OPEN` handshake reply so the client lands a kernel-level read-only mount directly (write/`flock` write-lock returns `EROFS` immediately, zero round-trips); the server adds a separate authoritative gate on write-intent ops against derived names (returns `E_NOTPERM`→`EPERM`, deliberately not `E_ROFS` to avoid being misread as an HA failover signal), so old/spoofed clients can't bypass it; local (`--home`) derived mounts are judged RO at mount time too. **Editing a definition is hot-effective** — `meta-server` checks the definition version before dispatching each op (SQLite reads file mtime, PG reads `xmin`, FDB reads the cluster write-version stamp), so after a replacement an established mount auto-switches to the new shape within the client TTL (≤2s), no remount needed (verified ≤5s assertion, see section 9; the read-only contract is unchanged after reshaping), whereas `view rm` is not hot-effective (established mounts keep the old shape until remount, to avoid yanking a tree in use); multiple clients of the same name are rendered by the server's **same definition**, virtual inodes consistent across machines; under a shared meta-server a derived subscriber registers into both its own-name and base-name buckets and writes are double-broadcast, so base-side writes are immediately visible to derived mounts even without `--no-cache`; to limit a caller by name to reading only this derived view, mint a `--view docsonly --ro` key (writes always go to the base name, derived mounts are read-only anyway). The local mode (`--home`) uses the same mechanism: a mount without `--layout` auto-resolves `<home>/meta/views/<view>.layout` (an explicit `--layout` wins, no two-layer lensing; hot reload is meta-server-only, a local mount resolves once at mount time). Note: the SQLite tier's `grovefs view` operates server-local files and must run on the meta-server machine; under an HA deployment `meta/views/` must be synced to the standby manually along with the control-plane DBs (`api_keys.db`/`refs.db`); the PG/FDB tiers keep definitions in shared storage (`grovefs_control`/cluster `ctl`), governed from any reachable node with the same `--meta-store`, no local files to sync (§6.2/§6.3); it too is **not a security boundary**, mounting the base name directly still shows the whole tree (what can be blocked is the token / key, not the shape).

### 3.3 Zero-copy import of existing data (`grovefs import`): directories stay put, clients get it by connecting

When the server already has a batch of ready data (e.g. `/data/legacy`) that clients should fetch by view, you needn't load it in first: `grovefs import` registers an **existing local directory tree** into a view, bytes stay put — within the same filesystem the object is a hardlink to the source file (zero extra bytes, both paths the same inode, bidirectionally real-time visible), and across devices it auto-degrades to a streaming copy:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs import --view V --source DIR` | V = target view (created if absent), DIR = existing source dir | mirrors the whole tree into the view (default prefix `/<DIR name>`): dirs become nodes, files are hardlinked into `blocks/` and registered; mtime/atime/permissions/owner preserved |
| `grovefs import ... --prefix P` | P = target dir in the view (e.g. `/legacy`) | mirrors under the given prefix, `P` created automatically if absent |
| `grovefs import ... --meta-dir D [--data-dir R]` | D = meta root, R = storage root (default D) | under a meta-server deployment, run on the **storage machine**, writing directly into that deployment's authoritative SQLite tree `meta/V.db` and `R/blocks/`; already-mounted clients converge via TTL (≤2s) |
| `grovefs import ... --meta-store URI` (or `GROVEFS_META_STORE`) | URI = `postgres://…` / `fdb://…` | **refused**: an import can only produce a local SQLite tree, so writing under a PG/FDB deployment would be a shadow tree the server can't read; feed bytes through the mountpoint instead (§14.2), the error text names this alternative path |
| `grovefs import ... --dry-run` | — | reports only how many dirs/files would be imported, **not even creating the view db** |

```bash
$ grovefs --home /data/cluster import --view legacy --source /data/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home /data/cluster import --view legacy --source /data/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
$ grovefs mount --view legacy --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/da &   # 客户端连接即得
$ echo "new" > /mnt/da/legacy/docs/quarter.txt   # 经挂载写 → 源路径同 inode 同步可见
$ cat /data/legacy/docs/quarter.txt
new
$ rm /mnt/da/legacy/photos/2026/y.jpg            # 视图里删名字 → 只摘 blocks/ 链接,源文件毫发无损
```

Key semantics (the `import` suite: 35 assertions; the `IMPORT_BACKEND=fdb` tier: 47): **a re-run is incremental** (existing names skipped, new files registered); **GC removes only the link** (the source path is never deleted by view operations); entries after import have exactly the same rights as native writes (read/write/lock/snapshot/invalidation-push all apply), and can be composed by §3.2 derived views' `map`/`link` into any virtual directory shape — that shape mounts **read-only** (writes go to the imported original view and reach the virtual shape through double-broadcast push); `grovefs import` also refuses a derived name as its target (otherwise it writes a shadow `<view>.db` no one can read). Three implementation facts to note: hardlinking requires the source and `blocks/` to be on the same disk (across disks it auto-emits a copy and counts it honestly); when an external tool updates a source file via **rename/replace write** (vim's default), the view side sees the old inode's last content, so import must be re-run for the new content; symlinks/special files are deliberately skipped. Under a non-SQLite backend (`--meta-store postgres://…` or `--meta-store fdb://…`, incl. the `GROVEFS_META_STORE` fallback) `import` **refuses outright** and gives the alternative path (mount the view, then feed bytes through the mountpoint, §14.2) — it can only produce a local `meta/<view>.db`+`blocks/`, and writing them down would be a shadow tree the server can't see (proven by the `import`/`govcheck` suites; `view fork` is not in this category — it can already copy inside each of the three backends' authoritative stores). The FDB tier not only re-verifies this refusal itself per backend (incl. refusing before `--dry-run`, refusing `GROVEFS_META_STORE` the same, and the refusal landing no shadow db), but also runs the alternative route the refusal names end-to-end: local `import` acts only as staging, what really enters the cluster is `view export` → hand over bytes → `view --meta-store fdb://… import`; after the move, the zero-copy property "source file and view share an inode" no longer holds in either direction (the view reads the deployment's own objects), and the suite makes a negative assertion on this point rather than avoiding it.

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
$ grovefs mount --view teamwork -m /mnt/work --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240
mounting view 'teamwork' at /mnt/work (fully remote: no local store)
$ ls /mnt/work
docs photos
$ echo hello > /mnt/work/docs/2026/new.md   # 别名挂载可写,写入即真实树
$ ls /mnt_docs/docs/2026                    # 同一写经另一路派生名(docsonly)挂载穿透可见
a.md new.md
$ grovefs mount --view team -m /mnt/t --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240
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
$ grovefs mount --view teamwork -m /mnt/leak --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # 拿旧泄露名再挂
$ ls /mnt/leak          # 空:旧名不再解析到真实树(命名空间按需新建,数据不外泄)
$ grovefs mount --view team -m /mnt/probe --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240        # 猜真实名同样被拒(全程治理)
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

Guards (fail-fast at the `alias` command, 26 assertions in the layout suite sections 10/11): refuses `name == real`, refuses shadowing a physical `<name>.db`, refuses the target being a derived view (an alias only maps physical views), refuses an **alias chain** (`--real` pointing at another alias), allows only one outward name per real view at a time (the `--retire` rotation writes-then-deletes within the same command and is not blocked by this constraint); `--retire` additionally refuses pointing at a derived view or a nonexistent alias (it won't mis-delete a shape definition). The local (`--home`) case follows the same contract: a mount first resolves `views/<name>.layout`, and after an alias renames it mounts as usual and prints `alias view 'A' resolves to real view 'R' (full tree, writable)`; directly mounting a governed real name returns `error: view 'R' is governed by an alias — mount the alias name instead`; `grovefs import --view A` refuses an alias as target (an alias is not a storage identity, writing it out would be a shadow db no one can read). Implementation facts: alias resolution is a single hop, re-reading the definition on every `M_OPEN` (SQLite fully scans the `views/` directory — mounting is a rare event, reading a small directory fully is cheaper than maintaining an invalidation protocol; PG/FDB read the same alias table in shared storage); rotation/`rm` affect only **new** mounts (established old-name connections persist until the old connection drops, the same semantics as the read-only gate); a rotated-away old name lands in an on-demand-created empty namespace, never reaching the real tree, so a leak is harmless; the SQLite tier's `meta/views/` are ordinary files, synced to the HA standby manually along with the control-plane DBs (`api_keys.db`/`refs.db`), `grovefs view`/`import` run on the storage machine (operating server-local files); the PG/FDB tiers keep aliases in shared storage (`grovefs_control`/cluster `ctl`), governed from any reachable node with the same `--meta-store`, no local files to sync (§6.2/§6.3). The refusal reason is **carried byte-for-byte into the protocol** (the `M_OPEN` response's data segment carries the server's verbatim text), so a remote refusal to mount the real name echoes exactly like a local one — the two `view 'team' is governed by an alias …` above are real runtime output; equally precise are the refusal reasons for token mismatch, api key missing/invalid/out-of-scope/revoked, and a view-bound key refusing another view (when talking to an **old server** the data segment is empty, and only then does the client fall back to generic wording). **Rotation is name governance (leak damage-control), not a security boundary**: hard isolation still rests on token+key — rotation only stops the old public name from reaching the real tree, it cannot revoke an existing holder's access.

### 3.4.1 fork view: an independent metadata tree, structure writable, content read-only (`grovefs view fork`)

A derived view is a read-only "display lens", an alias is a whole-tree-writable "public name"; between the two there is a real need still missing: **able to tidy the directory tree, but unable to upload content, and deleting your own name never harms the base view**. `grovefs view fork` is exactly this tier — **copying** the base view's dir entries into an independent metadata tree (SQLite: `<meta-dir>/meta/<name>.db`; PG: a new `grovefs_v_<hex>` schema; FDB: a new subspace), preserving inode and hardlinks, copying not a single content byte, sharing them via the cross-view ledger; the definition is only one line `fork <base view>`:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D fork --name V --base B` | V = new fork name, B = local SQLite physical base view name | creates `D/meta/V.db` (copying B's dir entries) and writes `D/meta/views/V.layout` (body only `fork B`); thereafter the two evolve independently, names added to the base view later are not injected into an already-built fork |
| `… fork --name V --base B --empty` (MCP: `fork_view` with `seed:false`) | `--empty` = build only the root, copy no dir entries | still writes a `fork B` definition, mount semantics identical, but the tree has only the root row; then use "graft" (`graft`, next subsection) to load a curated subset on demand — bytes still zero-copied |

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team
forked view 'lab' from base 'team': 42 entries, 37 shared object(s)
note: mounts of 'lab' get an independent tree — structure is writable, content is refused with EROFS (upload bytes belong to base 'team')
note: `grovefs view rm --name lab` deletes this fork's own tree only; shared objects stay while any view (including 'team') still holds them
$ grovefs view --meta-dir $BASE list
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
$ grovefs --home $BASE mount --view lab -m /mnt/lab &
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
$ mkdir /mnt/lab/docs/2026/review      # 组织目录树:放行
$ mv /mnt/lab/docs/2026/a.md /mnt/lab/docs/2026/review/a.md   # 改名:放行(只改 fork 自己的元数据)
$ echo x > /mnt/lab/docs/2026/new.md   # 上传字节:拒绝
bash: /mnt/lab/docs/2026/new.md: Read-only file system
$ grovefs view --meta-dir $BASE rm --name lab
removed fork view 'lab' of base 'team' (its own metadata tree is gone; content bytes stay while any view — including 'team' — still holds a name for them)
```

Semantics and guards: `view fork` refuses `name == base`, refuses a target name that already has a definition or a physical db, refuses a base name that itself has a definition (derived/alias/fork can't be a base), refuses a base name with no physical tree in that store (one sentence across all three tiers: SQLite looks for `meta/<base>.db`, PG for `grovefs_v_<hex>`, FDB for a subspace); `alias` refuses pointing at a fork (a fork is a tree identity, not a public name — mint a key for it with `apikey create --view lab`), `import` refuses a fork target. **The structural face is writable, the content face read-only**: `mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/empty-object `create` are allowed, `write`/`truncate` always `EROFS`; the local (`--home`) and `meta-server` paths judge identically (the latter via the 3rd flag byte in the `M_OPEN` reply). Deletion removes only the fork's own `node` rows: a shared object is judged by the cross-view ledger and reclaimed only after the **last holding view** removes its name; `view rm --name <fork>` deletes the definition + its own `<name>.db` (+WAL/SHM) and runs `refs rebuild` to recompute the ledger, the base view and bytes untouched by a hair. Honest boundary: the content-write gate is a **client gate**, a raw-protocol client bypassing FUSE to connect directly to the data-server can still write bytes — the hard boundary remains the server's token+API key (§6). Visibility: `view list` lists both definition slots (derived/alias/fork) and **bare base views** (physical trees with no definition); `apikey list --view V` lists only keys that can open V (a pure alias also matches a key bound to the real name), and tallies the count of unbound keys. e2e in the `fork` suite (70 assertions, incl. the `--empty` empty seed and `view graft` grafting).

#### Customizing a fork view: seven actions from creation to deletion

A fork can customize only two things: the **tree** (the copied dir entries, tidiable at will) and the **name** (view name, mountpoint, key binding); content bytes are always read-only and shared with the base view. Walk one real tidying through:

| # | Action | Command | Result / criterion |
| --- | --- | --- | --- |
| 1 | Pick the base and name it | `view list` to confirm the base is a **bare base view** (no derived/alias/fork definition) | a fork copies the **physical tree in that store** (SQLite file rows / PG schema / FDB subspace); a derived name, alias, or another fork is refused; the name can't equal the base name nor clash with an existing db |
| 2 | Build the tree | `grovefs view --meta-dir D fork --name F --base B` | 100,002-entry measurement **0.65s**; produces `meta/F.db` (**12 MB**) + `meta/views/F.layout` (13 B, body only `fork B`) |
| 3 | Mount | `grovefs --home D mount --view F -m /mnt/F` | the log shows `fork view 'F' of base 'B': independent tree — structure writable, content read-only (EROFS)` |
| 4 | Tidy the skeleton | `mkdir`/`mv`/`ln -s`/`rm`/`chmod`/`chown`/`touch` (empty file) | changes only `meta/F.db`; `B`'s dir entries and bytes untouched by a hair |
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
grovefs --home D mount --view F -m /mnt/F2 --layout hide-tmp.layout
```

**Can do / can't do (one table to draw the line)**:

| What you want to do | How on a fork | Boundary |
| --- | --- | --- |
| Copy a freely-tidable catalog | `view fork` | copies only dir entries; bytes shared, not copied |
| Hide certain names | `rm` inside the fork | base tree unaffected; but that something was "hidden" is not visible to an audit |
| Let different teams see different shapes | the same fork over multiple mountpoints each with `--layout`; or fork another copy | `--layout` is a client display layer, **not a security boundary** |
| Follow the base tree's additions/renames | **not supported**; re-fork (blue-green rename) | a fork is a **point-in-time snapshot** (§2.4) |
| Upload a new file inside the fork | `touch` empty file allowed; writing content `EROFS` | the content gate is a client gate, the hard boundary remains the server's token/key |
| Base view on PG / FDB | `view fork` still works | PG does a cross-schema `INSERT … SELECT`, FDB copies the subspace; bytes via the shared ledger, the same contract as SQLite (`forksrv` three-tier verified) |
| Treat the fork as a public name/alias | no, `alias` refuses pointing at a fork | a fork is a tree identity; for a public name use `apikey create --view F` |
| Move to another machine | `view export --view F` + `import --as-fork B` | bytes `rsync blocks/` separately (§3.4.2) |
| Delete the fork and also reclaim bytes | `view rm --name F` | only bytes whose ledger hit zero are truly reclaimed; `--base` on a fork is refused |

**Four common pitfalls**: ① `view rm --name F --base` is refused by the "name governed by a definition" guard — a fork is not a base view, deleting it **may only** omit `--base`; ② rebuilding a snapshot loses all fork-side tidying, so for rotation use a new name `F2` and switch the mountpoint, don't `rm` in place and rebuild; ③ every fork is a full catalog (100k entries 12 MB), N perspectives means N × 12 MB, not tens of bytes; ④ don't hand-edit `meta/F.db` — the ledger `refs.db` is updated as a set only at `fork`/`rm`/`import`, a hand-made `data_ref` has no holder record, and both the reclaim decision and `view export`'s reference count would disagree with reality.

**On "the base view's identity": a view's identity is its name** (for SQLite the `meta/<name>.db` filename, for PG the schema name), **there is no separate UUID**. The `fork B` in `views/F.layout` is just a name reference, used for `view list` display, `view rm --base`'s dependency guard, and mount prompts; mounting a fork reads **only its own `meta/F.db`**, never re-checking the base tree at runtime. Hence:

- If the base tree is renamed/moved at the file layer, an already-built fork **mounts as usual, content unchanged** (it's a self-contained snapshot); what changes is that the `fork B` line points at a nonexistent name, and `view list` no longer lists B's tree.
- The supported way to "change the outward name" is **alias rotation** (`view alias --retire <old-name>`): the real name stays, fork/derived/alias proceed as usual; don't hand-`mv meta/<base>.db`.
- The dependency guard works by name: as long as some `fork B`/`base B`/`alias B` still references B, `view rm --name B --base` refuses to delete.
- The cross-view ledger `refs.db` counts **per object** (`holders(data_ref, holders)`) and is independent of view names, so a rename won't mis-count references; if a real drift appears, recompute with `grovefs refs --meta-dir D rebuild`.

#### Selective grafting: install only the part you want, renaming as you go (`graft`)

A full fork is a "whole snapshot"; but when actually delivering a tidied view, the common case is **wanting only a small part of the base view, and renaming it** (`data/4f2c-9a10.fits` → `M31_2025.fits`). Two actions combine, still **copying no bytes**:

1. **Empty fork**: `fork_view` with `seed:false` (CLI form `view fork --empty`) — builds only the root of `meta/F.db`, moving no 100k dir-entry rows.
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
| PG backend | **no** (same reason as fork) | the authoritative tree is in PG, there's no readable `meta/<name>.db` |

The CLI (`grovefs view graft --name F --map PLAN|-`) and MCP (`graft_view`) share one implementation: parameter semantics and refusal wording byte-for-byte identical, only the entrance differs.

### 3.4.2 Single-view metadata export/import: migration across machines and OSes (`grovefs view export / import`)

GroveFS's metadata is **not hardware-bound**: a `node` row stores only `data_ref` (the object key), the object path is a pure function of the key (`blocks/<two hex of FNV1a(key)%256>/<key>`), with no absolute path, device number, hostname, or host inode; GroveFS's own `ino` is in the DB too. So moving a whole DB/directory to another machine works directly — provided **metadata and content bytes go together**. To migrate per view, across machines, selectively, use `view export` to write a tree as host-agnostic JSONL, then `view import` at the target storage root:

| Command | Parameter meaning | Effect |
| --- | --- | --- |
| `grovefs view --meta-dir D export --view V --out F [--force]` | D = source storage root, V = **physical base view or fork** name, F = output file | writes a header (format/version/kind/definition verbatim/node and blob counts) + one node per line (id/inode/name/is_dir/data_ref/link_target/mode/uid/gid/three timestamps); **contains no bytes**; `--force` needed to overwrite an existing F; can export while mounted (a read-only snapshot) |
| `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` | D = target storage root, V = new base view name, F = the file exported above, B = an existing bare base view on the target machine | rebuilds the whole tree in `D/meta/V.db` (preserving id/inode/hardlink/permissions/owner/symlink), then rebuilds the `refs.db` ledger; an existing tree requires `--replace`; `--as-fork B` additionally writes a `fork B` definition (restoring "structure writable, content EROFS") |

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

**The same pair of commands also crosses backends** (SQLite ⇄ FDB, no third tool needed): a JSONL exported from a SQLite tree is exactly the input for importing into a subspace, and vice versa — `view --meta-store fdb://… import` **opens a subspace of that name** in the cluster (the only tree-creation entry besides `view create`), `view --meta-store fdb://… export` writes a subspace as the same JSONL format. Bytes still go through their own object stores (either `rsync -a` the data root, or have the data-server serve the same objects directly):

```bash
S=/data/sqlite-store; C=/data/cluster; CL=fdb:///path/to/fdb.cluster
grovefs view --meta-dir $S export --view team --out /tmp/team.jsonl
rsync -a -H $S/blocks/ $C/blocks/                        # 内容字节另走,元数据已在 JSONL 里
grovefs view --meta-store $CL --meta-dir $C import --view team --in /tmp/team.jsonl
# 回滚方向:同一个集群 dump 再搬回一棵 SQLite 树
grovefs view --meta-store $CL --meta-dir $C export --view team --out /tmp/team2.jsonl
grovefs view --meta-dir $S import --view team_rolledback --in /tmp/team2.jsonl
```

The round trip rewrites no rows: a dump re-exported from the subspace after import is **line-for-line identical** to the source dump (the `diff` criterion), so id/ino/hardlink/mode/owner/the three timestamps/symlink text all carry over as original values rather than being reassigned; at import the two allocator watermarks are raised to the export's maximum (the PG tier's `setval` equivalent), so node ids created after migration are strictly greater than the imported side's max id, never colliding. The shared-bytes ledger (FDB's `refs` subspace in the cluster) is updated in the same transaction with the import/`--replace`, and a failure rolls it all back — a tree is either untouched or wholly the new tree (that's single-transaction semantics; under `--meta-store fdb://…` when the dump doesn't fit one transaction it switches to automatic batching with the root row written last, see §6.3.4). During the migration window the **source store is not written**, the original tree remains, and a rollback is the same pair of commands run in reverse. The PG tier has no such channel: its authoritative tree is in a schema, use `pg_dump`/logical replication (§15.9).

e2e: the `fdb` suite section 12 (the full SQLite→subspace→SQLite chain, incl. line-by-line `diff`, watermark raising, guards, source untouched), the `migrate` suite (`MIGRATE_BACKEND=fdb` tier), the `import` suite (`IMPORT_BACKEND=fdb` tier, running the alternative route — apart from this refusal — end-to-end).

**Import atomicity**: a temporary tree is first built at `D/meta/.V.import-<pid>-<time>`, and only after all rows and checks pass and the WAL merges does it `rename` to `D/meta/V.db` — a corrupt file / dangling parent / count mismatch / failed `--replace` never damages the old tree (the old tree is either untouched or wholly the new tree, no intermediate state).

**Guards**: virtual names (alias/derived) own no tree, so `export` refuses outright and prompts exporting their physical base; an existing output file needs `--force`; if the import target already has a definition slot, `view rm` it first; a header format/version mismatch, a root that isn't an `id=1` directory, a dangling parent, or a header count disagreeing with the body are all whole-job refusals; `--as-fork` requires the base view to already exist **on the target machine** and the base name itself to have no definition.

**Migration's topology constraints (data distribution, not hardware binding)**:

- Multi-disk `--data-dir`: an object picks its disk by `FNV1a(key) % number of roots`; on the target the root **count and order must match**; you may also merge into a single root (merging each disk's `blocks/`).
- Cross-server striping (`s<N>`): block i is at `servers[i % N]`. With `--member-id` configured, addressing is by **member identity**: the `--data-servers` order is irrelevant (any order listing the same machines reads/writes correctly), but you must keep **N members with an unchanged set** — swapping out any one machine (even holding the count) is caught by the member-set fingerprint and refused at mount, rather than silently mis-reading someone else's block. The positional mode without `--member-id` addresses by the client list's **index**, so **the order is itself part of the layout**; reordering the list silently mis-routes existing objects (no handshake before reading/writing an existing object can detect a reorder) — this is an inherent hazard of the positional mode; to root it out, give each data-server a unique `--member-id`. **Changing machines / changing IPs is not itself a constraint**: in identity mode, replacing that item in `-o data=` in place (keeping its id) is auto-recognized, metadata unchanged, already-written objects need no `rebal` (the rotating placement `i % N` only decides where newly-written objects land). A count mismatch is refused outright (reporting `laid out across N servers but this mount has M; refusing to remap`).
- HA `epoch` (each view's `ctrl` table): a whole-directory copy carries it over, so the old primary must be stopped after migration; `view export/import` and `view fork` deliberately do not copy `ctrl` or `locks`.
- The PG backend can't use file copying; for whole-DB backup use `pg_dump`/logical replication; but `view export/import` works across all three tiers — a PG tree is in a schema, export goes through a REPEATABLE READ read-only snapshot, import rebuilds in a single transaction (§15.9, proven by the `xfer` suite's PG tier).
- `uid/gid` is numeric metadata (advisory), kept by number; symlink target text is user data, kept verbatim.
- For a whole-directory cold copy, first stop all mounts and the `meta-server`; if you can't stop, use `sqlite3 <db> ".backup"` / `VACUUM INTO`, **don't raw-copy a running DB** (WAL).
- **`st_ino` travels with the DB and is unchanged across machines** (`node.ino` is GroveFS's own numbering), but **`st_dev` is freshly assigned by FUSE on each mount**: a backup/sync tool that dedups by the `(dev, ino)` tuple must rebuild its mapping after a remount or DB move; lookup by ino alone (`find -inum`, `stat -c %i`) is unaffected.
- **How to confirm nothing is missing after copying**: object names are UUIDs, not content hashes, the read side has no verification step, so a missed copy can only be actively compared — the number of keys referenced by the tree (`sqlite3 D/meta/V.db 'SELECT COUNT(DISTINCT data_ref) FROM node WHERE data_ref IS NOT NULL;'`) against the `matched` in `grovefs gc --meta-dir D --data-dir R`'s output; `matched < refs` means there are keys with no object. Use `rsync -a` to preserve mtime (`gc --older-than` relies on it for the new-object grace), and a store that has used `import` must carry `-H` (imported objects and source files are hardlinks).

**Multiple base views**: create any number of base views (`init --view A` / `view create --view A`), each its own `meta/A.db`, trees/bytes/permissions/deletions not affecting each other; `view list` lists all bare base views. `view export/import` works per view, so at migration you may move just one of them, and different base views may bind different keys (`apikey create --view A`).

e2e in the `migrate` suite (74 assertions on the SQLite tier; `MIGRATE_BACKEND=fdb` reruns the same assertions against the cluster, 81). Coverage: export while mounted / host-agnosticism / virtual-name and overwrite guards / import across absolute paths / hardlink and permission fidelity / atomic replace / corrupt-input refusal / `--as-fork` ledger sharing / multi-base-view independence (the FDB tier re-judges "two storage roots" by subspace boundary; `fork`/`--as-fork` share the same contract across all three tiers, see §15.9).

**Scale measured** (single machine, default IO, rerunnable via `bash .build-bin/perf_migrate_scale.sh`): migrating a view of 5052 nodes, 5000 objects — `export` 0.19s producing 1.16 MB JSONL (1.16× the view DB, ~230 B/node); `import` to a new storage path 0.56s (incl. ledger rebuild); after mounting, enumerating 5000 files takes 0.23s, content byte-identical, permissions and owner preserved. Registering the same batch of files into the source view originally (zero-copy import) took 83s — that per-object storage overhead is entirely off the export/import path. **Migration cost is set by the namespace size, not the data volume**; bytes go separately via `rsync -a -H blocks/`.

### 3.4.3 Deleting a whole base view: reclaiming bytes along (`grovefs view rm --base`)

A `view rm` without `--base` deletes only a **definition slot** (derived/alias/fork) and never touches data; to delete a **physical base view** from end to end — its metadata tree gone, the cross-view ledger rebuilt, **content bytes no other view still holds reclaimed from `blocks/`** — use `--base` to say this explicitly (to avoid a mis-deletion):

| Command | Effect |
| --- | --- |
| `grovefs view --meta-dir D rm --name V --base [--data-dir R]` | deletes `D/meta/V.db` (+WAL/SHM) → `refs rebuild` recomputes holders over the surviving trees → for each blob in V's tree **held by no other view** calls `backend.remove`; `--data-dir` points at the root actually storing `blocks/` (default the metadata root `D`, same convention as `import --data-dir`, for meta-server/data-server split deployments); the echo honestly reports the freed/held/not-found/failed counts |

```
$ grovefs view --meta-dir .home rm --name alice --base
removed base view 'alice': 2 node(s) gone, 2 blob reference(s) dropped, 2 object(s) freed from .home/blocks, 0 still held by other views
```

**Four-fold guards** (any hit refuses, the tree untouched by a hair): ① the name is governed by a definition (is a derived/alias/fork definition slot) — first run a `view rm` without `--base` to delete the definition; ② some alias/lens/fork **definition** still resolves through this base (otherwise deleting the base would leave that mount dangling), reports `view 'lens1' is governed by a definition over base 'alice' — remove it first`; ③ the local `/proc/mounts` has a live `grovefs:<V>` mount, reports `view 'alice' is mounted here at …  — unmount it first` (mounts **on other machines** against the same meta-server are invisible to this machine and must be stopped first yourself); ④ **the PG tier goes through the same command** (`view --meta-store postgres://… rm --base`): the base tree is a schema, `remove_view` decrements `grovefs_refs.holders` per key (DELETE at zero) in one transaction, then runs `DROP SCHEMA … CASCADE`, and only after commit reclaims `blocks/` bytes per the returned `unbooked_refs` — there's no SQLite-tier "delete the db then rebuild the ledger" intermediate window. **A hand `DROP SCHEMA` past the CLI does not count as deletion**: the ledger is a global table with no view column, a bare drop deletes only the tree without booking it off; more importantly, which objects that view subscribed to is **known only to that schema**, the ref list vanishes with the schema, and those bytes thereafter have no record to reclaim (proven in section E of §15.8's measurement).

**Freed only when the last namer dies**: a fork's blob refs come from the base view's own keys, the ledger holds them at >1, so when deleting the base the bytes still named by the fork **must not be touched**; bytes are truly reclaimed only once the base and all dependent definitions are gone and the ledger has no holder left. Striped keys `s<N>:<stripe>[:<fp>]:<uuid>` live on the data-server, an ordinary local root can't reclaim them — honestly counted into failed and skipped, never half-deleted. **MCP boundary**: `remove_view` is control-plane only (equivalent to a `view rm` without `--base`), deleting a whole base view goes only through the CLI — the interface never touches data bytes.

e2e in the `viewrm` suite (42 assertions: seeding the tree / mount guard / multi-guard / shared bytes lost from deletion / reclaim after the last namer dies / split root `--data-dir` / re-mount-empty semantics after deletion); the same PG-tier sequence in `.bneck/pg_view_teardown.sh` (44 assertions: real FUSE-mount writes → three-way reconciliation of schema/ledger/bytes → transactional deletion → a second deletion refused → the two directions of `refs rebuild` after an out-of-band drop → residue exactly equal to the unreclaimable part).

### 3.5 Agents customizing data listings: the MCP interface (`grovefs mcp`)

View governance (§3.2/§3.4/§3.4.1) is simultaneously a set of **MCP (Model Context Protocol) tools**. `grovefs mcp` speaks newline-delimited JSON-RPC 2.0 on stdio (implementing the common method surface of the 2025-06-18 protocol: lifecycle `initialize`/`ping`, tools `tools/list`/`tools/call`, resources `resources/list`/`resources/templates/list`/`resources/read`, prompts `prompts/list`/`prompts/get`, completion `completion/complete`, logging `logging/setLevel` and server-side `notifications/message`; client notifications are always silently unanswered), any MCP client (Claude Desktop, an IDE agent, etc.) spawns it as a subprocess, and the agent can then **customize data listings automatically through the interface**: query views, **list the directory a view actually renders** (a derived view via its lens, and preview without landing what a to-be-published rule would grow into), publish a read-only derived lens with inline rule text (the four verbs same as `--layout`, rules ride as text lines, landing no file), rotate the public name, delete a definition, **dry-run a whole definition through all governance guards before publishing (writing nothing)**. It shares the same library functions (`grovefs::mcp`) with the `grovefs view` subcommands — validation, refusal wording, on-disk actions byte-for-byte identical, one contract two entrances, an agent can do nothing the CLI can't and bypass no guard. **Probe is read-only**: `list_view_tree`/`get_view` open the local `<meta-dir>/meta/<view>.db` read-only, render the listing through the same `LayoutMeta` lens a mount uses (no mount, no object bytes read; remote/PG-backend trees are not walked here); the `size` in the reply is a metadata field not a byte count (directory `4096`, file `0`, symlink the target length), for real bytes mount and `stat`. **Control-plane only**: no mount, no data writes. By default it trusts whoever can spawn this process (= already holds the storage root's OS-level permission), cross-network auth is still the server's token / API key's job; when it is fronted to several different callers, `grovefs mcp --require-api-key` **optionally** puts an API-key gate on the interface (see §3.5.1). All echoes below are from real runs:

```
# 1) 存储机:建一个物理视图(智能体稍后要在它的树上定制列表)
$ grovefs --home $HOME_DIR init --view team --manifest team.manifest
initialized BASE view 'team' at /data/xy/grovefs/.demomcp/home/meta/team.db
applied seed manifest .demomcp/team.manifest

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
$ grovefs --home $HOME_DIR mount --view release -m /mnt/release &
derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
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

Ten tools and their parameters: eight governance ones — `list_views` (optional `db_path`), `define_derived_view` (`name`/`base`/`rules`, the latter inline rule text), `alias_view` (`name`/`real`/optional `retire` atomic rotation), `fork_view` (`name`/`base`, optional `seed` — `false` builds only an empty root, leaving "whether to copy dir entries" to the caller; copying the base view's dir entries into an independent metadata tree — structure writable, content EROFS), `graft_view` (`name`/`map` plan text, optional `src` (defaults to the fork definition's base name)/`dest`/`skip_existing` — grafting named entries from another view (a directory source brings its whole subtree) into any position of the target view, renaming line by line, zero byte copy, the whole plan in one transaction), `remove_view` (`name`, deleting a fork also deletes its own tree), `preview_rules` (`rules`, dry-run checking verb syntax without landing), `validate_definition` (`name`/`base`/`rules`, running a whole definition through define's full governance guards before publishing — illegal name, name equals base, shadowing a physical db, overwriting a pure alias, rule syntax — on refusal returning text byte-for-byte identical to a real publish, on acceptance returning a JSON verdict, never landing to disk); two read-only probes — `list_view_tree` (`view` or `base`+`rules` simulation, optional `path`/`depth`/`max`/`db_path`; renders the path listing a mount actually sees, either via a published lens or by simulating a rule set without landing), `get_view` (`name`, returning one definition's raw `definition` text and counts). The same governance is secondarily exposed via MCP's other common method surfaces, reusing the **exact same renderers**, so resources/prompts/completion match tool results byte-for-byte: **resources** `resources/list`/`resources/templates/list`/`resources/read` expose views and the manual as attachable context — `grovefs://guide` returns the sectioned ops manual verbatim, `grovefs://views` the same snapshot as `list_views`, `grovefs://view/<name>/definition` (governance text verbatim) and `/tree` (the listing a mount actually sees, the template carrying `{?path,depth,max}`, synonyms of `list_view_tree`'s parameters — a big tree can attach one subtree at a time, `truncated:true` when bounded off); **prompts** `prompts/list`/`prompts/get` offer two parameterized governance playbooks (`reshape-view` taking `base`/`goal`, `rotate-alias` taking `real`/`new`/`old`); **completion** `completion/complete` is role-aware per `ref` (`reshape-view.base` / `rotate-alias.real` give only physical views, `rotate-alias.new`/`old` give only aliases, the rest give all view names); **logging** after `logging/setLevel` sets a session threshold, the server pushes `notifications/message` by severity (success `info`, failure `warning`) after each dispatched call, `data` carrying the method and the object operated on (e.g. `tools/call list_views ok`). Runtime-facing metadata: each tool carries a `title` and `annotations` (`readOnlyHint` marking `list_views`/`preview_rules`/`validate_definition`/`list_view_tree`/`get_view`, `destructiveHint` only `alias_view`/`remove_view`); the four JSON-returning tools carry an `outputSchema` and results also give `structuredContent`; `initialize`'s `instructions` span seven sections (Discover/Reshape/Curate/Validate before publishing/Golden order/Beyond tools/Mechanics), a client injecting context can follow them. One more protocol detail: negotiated per `2025-06-18`/`2025-03-26`/`2024-11-05`, an unrecognized version returns `2025-06-18`; `arguments` also accepts a JSON string; a missing parameter appends `(tool 'x' requires: …)`, a misspelled tool name appends `did you mean '…'?` and an `available:` list. Semantics: all governance guards apply as usual (derived mounts read-only, the real name refused while an alias exists, `--retire` writes-then-deletes, fork structure writes allowed / content EROFS; hot reshape still only at the `meta-server` entrance, a local mount resolves once at mount). Protocol details: notifications (no id) are never answered; an illegal frame returns `-32700` and continues serving; a missing parameter and an unknown tool return `isError` per MCP convention rather than a protocol error. `db_path` defaults to `grovefs mcp --meta-dir` (falling back to `--home`), also overridable per call. e2e in the `mcp` suite (77 assertions: handshake capability declaration and protocol negotiation / discovery (`instructions` naming all ten tools one by one with sections, `title`+`annotations`+`outputSchema`) / inline-rule reshaping / path probe and shape preview / dry-run validation before publish / alias rotation / empty fork `seed:false` + `graft_view` grafting / graft-guard surface 11 refusal verbatim (self-graft · empty plan · `..` · root path · base view missing `src` · target or source is derived/alias · nameless tree · illegal name · duplicate-name whole-batch rollback · `skip_existing` skipping) / CLI same text / resource enumeration and read (incl. `grovefs://guide`, `grovefs://views`, tree query-parameter drill-down and `max` truncation) / prompt instantiation / role-aware view-name completion per `ref` / `structuredContent` consistent with text / unknown-tool suggestion and missing-parameter prompt / log threshold→notification with `data` carrying the tool name / protocol-error paths). The agent-side wiring manual (in English, for internationalized deployment) is already distilled into the repo as a project skill (`skills/grovefs-mcp/`): `SKILL.md` (how to spawn, the ten-tool parameter table, the golden workflow, an exact refusal-text table per case, hard boundaries), `reference.md` (the full method surface / auth-gate matrix / error codes / on-disk effects / CLI cross-reference), `examples.md` (real echoes), plus two scripts: `scripts/mcp_call.py` (a zero-dependency stdio JSON-RPC client, splitting the reply into stdout/stderr, exit code 0 success / 2 protocol error / 3 tool refusal / 4 transport problem) and `scripts/smoke.sh` (running the full chain on a `mktemp -d` temporary storage root with per-line assertions, never touching real data).

### 3.5.1 Locking the interface and the mountpoints: API-key authentication, scope partitioning and call audit (`grovefs apikey`)

One server often faces **multiple different callers** — an agent runtime fronting one `grovefs mcp` service out, several client machines mounting the same `meta-server` over LAN — when you need revocable credentials and attribution audit. `grovefs apikey` mints API keys (**storing only the SHA-256 digest**, the plaintext printed once at `create`, so even a leaked `api_keys.db` can't be turned into a usable credential), and tags each key with a **scope** (`--for admin` is the credential for the governance port that stands in for the DB password, see §3.5.2) and an **owner** (`--owner <account>`, required for `--for admin`): `--for mcp` (opens only the governance interface), `--for mount` (opens only a `--require-api-key` meta/data server), `--for admin` (opens only the governance port `meta-server --control-listen`, and also admits mount+mcp), `--for any` (**default**, mount+mcp only — `any` never upgrades to `admin`; every key minted before the scope concept was this tier, so old deployments need no change). The two entrances share one registry (`<meta-dir>/meta/api_keys.db`), isolated by scope: a mount credential can't be replayed to the governance API, and an agent key can't open the data plane either. A mount key may also carry **two access-control dimensions**: `--view V` **binds the credential to a single physical view** V — using it to `M_OPEN` another view is refused with `E_AUTH` at the handshake (one without `--view` is a system-level key, able to open any view); `--ro` marks it **read-only** — the server refuses write-intent ops per-op with `E_NOTPERM` on the meta plane and content writes with `EROFS` on the data plane, and hints the client a read-only mount in the `M_OPEN` reply ("read-only" is enforced server-side, not relying on the client's `-o ro` self-discipline); read-only/read-write **can be flipped in place** — `apikey set-ro --id <ID> --rw` flips a read-only key back to read-write, without `--rw` (or with `--ro`) back to read-only, a live mount adopts the new verdict on its next second-level revalidation tick, no remint, no reconnect (the one exception: a mount whose read-only already landed in the kernel at handshake still needs a remount to lift the OS-level cap, see set-ro's paragraph in 3.5.1; and the switch holds only for server-routed mounts, a local `--home` mount has no server credential gate and is unaffected). **Governance-plane gate**: `grovefs mcp --require-api-key` gates all **state-reading/writing methods** accordingly — `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `completion/complete` (`initialize` / `ping` / `prompts/*` / `logging/*` are always open so a client can complete the handshake, fetch static playbooks, and control how verbose the server is). **Mount-plane gate**: `meta-server --require-api-key` gates `M_OPEN`, `data-server --require-api-key` gates `OP_OPEN`, the client presents via `mount --api-key <ID:SECRET>` (if you'd rather not put it on the command line, export `GROVEFS_API_KEY='<ID:SECRET>'`, the secret then stays out of argv); **the enabling order matters**: a server **without** `--require-api-key` does **no** key verification at all (a client presenting a made-up key still mounts, verified `MOUNTED`); with it, it is **fail-closed** — when `api_keys.db` has no usable key **no one (including you) gets in**, so mint the keys first, then restart with the flag; **audit surface**: every `tools/call`, every `resources/read`, and every mount connection (incl. refused ones) appends to the same audit table (time, key, plane method, tool tool, success, detail), read back by `grovefs apikey calls`. A revoke only marks the row revoked (past calls keep attribution), a revoked key authenticates against nothing immediately, **and already-established mounts are closed too** (see below). Storage is one separate local SQLite (`<meta-dir>/meta/api_keys.db`): the control-plane state. The echoes below are from real runs:

```
# 1) 铸造一个 key(明文只打印一次;scope 默认 any)
$ grovefs apikey --meta-dir $HOME_DIR create --label claude-desktop
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    06fc0287c1464bd1b5a8ec1d4c1c9c31
  label: claude-desktop
  scope: any
  key:   06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0451c46889d8e...(64 hex secret)

Present it to a `--require-api-key` server as
  grovefs mount --meta-server ... --data-servers ... --api-key '06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0...(64 hex secret)' -m /mnt/x
(or export GROVEFS_API_KEY='...' so it stays out of the command line).
Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount
credential out of the governance API (and vice versa).
Revoke later with: grovefs apikey --meta-dir $HOME_DIR revoke --id 06fc0287c1464bd1b5a8ec1d4c1c9c31

# 2) 铸造一个只能挂载的 key;scope 写错直接拒绝(打错一个词不会静默放宽权限)
$ grovefs apikey --meta-dir $HOME_DIR create --label laptop-alice --for mount
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646   scope: mount   key: aac1f51a...:a5d5450a...
$ grovefs apikey --meta-dir $HOME_DIR create --label x --for both
error: unknown api key scope 'both' (expected any|mcp|mount|admin)      # exit=1

# 2b) 视图绑定 + 只读的 key(访问控制维度)
$ grovefs apikey --meta-dir $HOME_DIR create --label reader --for mount --view alice --ro
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…(64 hex secret)     # 只能挂 view alice,写一律被服务端拒
$ grovefs mount --view bob -m /mnt/b --meta-server ... --data-servers ... --api-key "$READER_KEY"   # 挂别的视图
error: permission denied: this api key is bound to view 'alice', not 'bob'    # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key
bash: /mnt/a/should_fail: 只读文件系统                                               # 据握手标志落内核只读;服务端另逐写拒 E_NOTPERM/EROFS

# 2c) 原地切换只读/读写(热生效,不重铸不重连)
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id 7c0e --rw
apikey: id='7c0e…' (label 'reader') is now rw (read-write)
  a LIVE mount presenting this key picks up the new verdict on its next op (within the 1s re-check window) — no reconnect;
  only a mount the handshake placed kernel read-only needs a remount to lift the OS-level cap.
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id 7c0e            # 切回只读(默认方向)
apikey: id='7c0e…' (label 'reader') is now ro (read-only)
# 前缀歧义/已撤销/未知 id 与 revoke 同样处理:歧义前缀报错、revoked 行不回活、未知 id 报错(exit=1)。

# 3) 列表带 scope/view/rw 列,绝不落密文;哈希也不落盘(库内只有摘要)
$ grovefs apikey --meta-dir $HOME_DIR list
#   id                                 label            scope   view          rw   created                   last_used                 revoked
1   296ef5839f5146078764ee5d16be6ef7   agent            mcp     -             rw   2026-10-27T06:57:59...      -                         -
2   aac1f51a0d5641ddaa8dbb83dc1cf646   laptop-alice     mount   -             rw   2026-10-27T06:57:59...      2026-10-27T06:58:02...    -
3   06fc0287c1464bd1b5a8ec1d4c1c9c31   claude-desktop   any     <any>         rw   2026-10-27T06:57:59...      -                         -

# 3b) 按视图过滤:只列能开 lab 的 key(纯别名也匹配绑定到真实名的 key),并提示系统级 key 数
$ grovefs apikey --meta-dir $HOME_DIR list --view lab
#   id                                 label            scope   view         rw   created                   last_used                 revoked
1   0cfaffd5fe2e4486af8ed1f282222dec   lab-agent        any     lab          rw   2026-10-27T15:31:33...      -                         -
1 bound key(s) for view 'lab' of 2 total

# 4) 服务端开门时自报门槛;不开门的默认路径行为与从前逐字节一致
$ grovefs meta-server --meta-dir $HOME_DIR --listen 0.0.0.0:50110 --require-api-key
meta-server listening on 0.0.0.0:50110 (base $HOME_DIR, role Primary)
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op
$ grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir $HOME_DIR
data-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live connections on the next op

# 5) 客户端拿各种坏凭据去挂:拒因逐字节的原文回到本地 stderr(退出码 1,不留脏挂载)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ...                      # 不带 key
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "aac1f51a...:deadbeef"
error: authentication failed: invalid api key secret
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "00000000...:whatever"
error: authentication failed: unknown api key id
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "no-colon-here"
error: malformed api key (expected '<id>:<secret>')
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "$MCP_KEY"   # 拿治理 key 来挂
error: authentication failed: this api key is scoped to 'mcp', not 'mount'
#   反方向同样被拒:把 mount key 塞进 MCP initialize 的 apiKey,tools/list 回 -32001,
#   且这条跨入口尝试同样被审计成 scope 拒因。

# 6) 挂载连接与治理调用同表审计(method 分平面、tool 分握手)
$ grovefs apikey --meta-dir $HOME_DIR calls --limit 6
last 6 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-10-27T06:58:02.176356991+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   meta         M_OPEN                 yes  view='alice' uid=1005
2026-10-27T06:58:00.180250185+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   data         OP_OPEN                yes
2026-10-27T06:58:00.169508131+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'
2026-10-27T06:58:00.164775080+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: invalid api key secret

# 7) 撤销一行输出说清两面契约;历史调用仍保留归属
$ grovefs apikey --meta-dir $HOME_DIR revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646
apikey: revoked id='aac1f51a0d5641ddaa8dbb83dc1cf646' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# 8) 封停活挂载:实测撤销后一次写的遭遇(无需运维介入)
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: 权限不够        # EACCES,来自服务端的拒因
$ cat /mnt/a/grovefs-mount.log        # 挂载自己把原因打出来并卸载
using remote metadata service at 10.0.0.5:50110
using remote data service at 10.0.0.5:50100
grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
received termination signal, unmounting cleanly...
unmounted view 'alice' at /mnt/a
$ ls /mnt/a                            # 干净的空目录——不是 "Transport endpoint is not connected"
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

Five subcommands: `create --label L [--for any|mcp|mount|admin] [--owner ACCOUNT] [--view V] [--ro]` (mints and prints `<id>:<secret>` once; `--view`/`--ro` are meaningful only for mount/any scope, `--owner` required for admin), `list [--view V]` (id/owner/label/scope/view/rw/created/last_used/revoked, no secret; with `--view` lists only keys that can open that view, a pure alias also matching a key bound to the real name), `revoke --id ID` (accepts an exact id or a unique prefix), `set-ro --id ID [--rw|--ro]` (flips an active key's read-only flag in place: default and `--ro` set read-only, `--rw` restores read-write; moves only the write permission, not liveness/auth, a server-established mount becomes hot-effective within ≤1s no reconnect — a local `--home` mount has no server credential gate and is unaffected by the switch; a revoked id refuses to flip), `calls --limit N` (default 20, read back in reverse order). An MCP client that can't customize the handshake may have the operator export `GROVEFS_MCP_API_KEY='<id>:<secret>'` to **pre-authenticate the whole service process** (`grovefs mcp` establishes its identity with that key at startup, the client's initialize need not carry one again); the mount-side counterpart env is `GROVEFS_API_KEY` (takes effect when `--api-key` isn't passed, keeping the secret off the command line). The `data-server`'s key-DB path is given by `--meta-dir <metadata root>`, **defaulting to the node's first `--data-dir`** — no extra parameter for a same-machine deployment, but a data-disk/metadata-root split deployment must point it right (pointing at a directory with no key ever minted is fail-closed, admitting none). Honest boundary: this key authenticates **the caller at the entrance** — it lets a caller (or not) issue governance write commands, let it (or not) open a view tree, and attributes each admission to a specific key, but does not change the governance semantics itself; the view-binding/read-only dimensions are enforced server-side; live-connection revalidation is **op-driven** (checked only with traffic), so the revoke's closure boundary is "that mount's first server round-trip after the revoke" — a read answered entirely by the kernel cache needs no round-trip and is thus not interrupted (closed on the next round-trip); `--token` and `--require-api-key` may coexist, both must pass. Without `--require-api-key`, server behavior is exactly as with no gate (only the audit still lands as usual). e2e in the `apikey` suite (33 assertions: mint / list / digest-not-landed / no-key-refused · wrong-secret-refused · right-key-admitted / call attribution / revoke-instant-and-row-kept / short-id-prefix revoke / env pre-auth / `list --view` filter / default-no-gate regression / `set-ro` in-place ro↔rw flip · still-authentic-after-flip; `APIKEY_BACKEND=fdb` reruns the same set against the cluster, 36 — the extra three being precisely "credentials land in the cluster": no `api_keys.db` appears under the local root, a home that never minted a key can still list that key, plaintext not searchable in the local root) and the `mountkey` suite (76 assertions: scope minting and typo refusal / six refusal-reason classes carried into the protocol / admission and env fallback (argv doesn't leak the secret) / connection audit down to a specific key and view / scope two-way isolation / revoke closes live mounts with no dirty endpoint / other keys unaffected / token-and-key double-gate AND / the data plane's independent gate / failover carrying the credential / no-gate default path no regression / view-bound key refusing across views with audit attribution / read-only key's server-enforced write refusal / `set-ro` on the same live mount rw→ro refuses write · ro→rw admits · no reconnect).

### 3.5.2 Governing the same metadata from any machine: the remote control plane (`--control-listen` / `--control-server`)

Governance commands (`view create/define/alias/list/rm`, `fork`/`graft`, `export`/`import`, `apikey`, `refs rebuild`, `gc`) must be able to read and write **the metadata store itself**. On the SQLite backend a tree is `<meta-dir>/meta/<view>.db`, reachable only by a process on the disk-holding host; on the PG backend any machine that can connect to the DB can govern it, but at the cost of handing `postgres://user:pw@…` to every management machine — one shared password that neither says who's editing nor can revoke just one person.

The remote control plane splits these two: `meta-server --control-listen HOST:PORT` opens one more listener that **runs only governance verbs**, and governance commands point at it with `--control-server HOST:PORT --api-key '<id>:<secret>'`, the real operations done on behalf by that already-store-holding process. A management machine has no DB URI, needn't reach `<meta-dir>`, and SQLite and PG take the same road. The credential is an **account key** (§3.5.1's `--for admin --owner <account>`): one account may hold many keys, each minted and revoked individually, each call landing in the audit.

Startup (the disk-holding host; the governance port and the mount port are two ports, each firewallable separately):

```bash
$ grovefs meta-server --meta-dir /srv/gfs --listen 0.0.0.0:50110 --control-listen 0.0.0.0:50111
meta-server listening on 0.0.0.0:50110 (base /srv/gfs, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view create --view <name>`, or `--home <dir> init --view <name>`); no empty tree is ever auto-created
meta-server: CONTROL plane listening on 0.0.0.0:50111 (admin-scope api keys only; every call is audited as method=control — mint with `grovefs apikey create --for admin --owner <account>` on this host)
```

The first admin key is minted on the disk-holding host (`apikey` reads the local DB directly; the governance port won't mint an admin for itself — that would make the gate pointless):

```bash
$ grovefs apikey --meta-dir /srv/gfs create --label ops-alice --for admin --owner alice
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    6e0f9e874f3c4bdca09274f0ac40828b
  owner: alice
  label: ops-alice
  scope: admin
  view:  <any>
  rw:    rw (read-write)
  key:   6e0f9e874f3c4bdca09274f0ac40828b:8516e6eaf85045178490e81d995d92e6227c5bb0204b4b9d845d357c06eb6229

Admin scope also governs a `meta-server --control-listen` from any machine, with
  grovefs view --control-server HOST:PORT --api-key '<id>:<secret>' list
  grovefs apikey --control-server HOST:PORT --api-key '<id>:<secret>' create --label k2 --for mount
```

Any management machine (the secret into env, not into argv):

```bash
$ export GROVEFS_CONTROL_KEY='6e0f9e874f3c4bdca09274f0ac40828b:8516e6eaf85045178490e81d995d92e6227c5bb0204b4b9d845d357c06eb6229'
$ grovefs view --control-server 10.0.0.5:50111 list
no alias/fork/lens definitions in remote control plane at 10.0.0.5:50111 (backend sqlite) — the 1 base view(s) below each map to their own tree
base view 'alice' -> own tree meta/alice.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --control-server 10.0.0.5:50111 --api-key <admin-key> list [--view NAME]` (add --view to see only that view's keys)

$ grovefs view --control-server 10.0.0.5:50111 create --view bob
created BASE view 'bob' at /srv/gfs/meta/bob.db (via 10.0.0.5:50111)
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.

$ grovefs view --control-server 10.0.0.5:50111 rm --name bob --base
removed base view 'bob': 1 node(s) gone, 0 blob reference(s) dropped, 0 still held by other views; 0 object(s) freed from /srv/gfs/blocks, 0 not found under that data root (they live on another store?), 0 could not be freed (striped keys move through a data-server)

$ grovefs refs --control-server 10.0.0.5:50111 rebuild
refs ledger rebuilt at /srv/gfs/meta/refs.db: 0 holder entries across the surviving views
  run it QUIESCED: a booking committed between the server's read pass and write pass would be overwritten with the pre-booking count
```

The audit is the same table as local: the `method` column says `control`, the `tool` column the verb, `detail` carries the account; attempts that fail auth land a row with an empty `key_id`, so "who tried with the wrong credential" is also visible.

```bash
$ grovefs apikey --control-server 10.0.0.5:50111 calls --limit 4
last 4 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-11-11T13:35:53.830004726+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      info                   yes  account=alice
2026-11-11T13:35:51.145091131+00:00 -                                  control      info                   no   authentication failed: this api key is scoped to 'mount', not 'admin'
2026-11-11T13:35:43.825879139+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      apikey.list            yes  account=alice
2026-11-11T13:35:43.820049671+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      refs.rebuild           yes  account=alice
```

Refusal verbatim (exit code 1):

```bash
$ grovefs view --control-server 10.0.0.5:50111 list                     # 不带 key
error: --control-server 10.0.0.5:50111 needs an admin account key: pass --api-key '<id>:<secret>' (or export GROVEFS_CONTROL_KEY). The first one is minted ON THE SERVER HOST with `grovefs apikey --meta-dir <server-meta-dir> create --for admin --owner <account>`.
$ grovefs view --control-server 10.0.0.5:50111 --api-key "$MOUNT_KEY" list
error: authentication failed: this api key is scoped to 'mount', not 'admin'
$ grovefs view --control-server 10.0.0.5:50111 --meta-dir /tmp list      # 本地 flag 与远程并用
error: --control-server 10.0.0.5:50111 cannot be combined with --meta-dir/--meta-store: a remote control plane governs the store ITS server owns, so a local path here would describe a different store. Drop the local flags — the account key alone decides what is governed.
$ grovefs view --control-server 10.0.0.5:50111 scan          # 仍在本地的一档
error: `view scan` is not a governance op: it moves or inspects TREE BYTES or cluster state on this machine, so it cannot run through --control-server 10.0.0.5:50111. Governance over the wire covers view create/define/alias/list/rm (including --base), fork/graft, export/import, apikey and refs; run this one on the server host with --meta-dir/--meta-store.
```

Coverage is split by verb (identical across the three backends, see "the three backends take one road" below):

| Via the governance port (any machine) | Must be on the disk-holding host (`--meta-dir` / `--meta-store`) |
| --- | --- |
| `view create` (incl. `--manifest` text) / `define` / `alias` / `list` / `rm` (delete a definition, `--base` delete a whole tree) | `view scan` (inventory this machine's `meta/` files or cluster subspaces) |
| `view fork` (whole-tree copy, incl. `--empty`) / `graft` (pick a subset + rename/`--dest`) | `view lease` (the local deployment's writer lease: read out / release / fence) |
| `view export` / `import` (chunked up/down; locally only `--out` and the input file) | `mount` / `umount` (the mountpoint is local) |
| `apikey create` / `list` / `revoke` / `set-ro` / `calls` | `gc --data-dir R` (the SQLite-only form scanning this machine's `blocks/` in place) |
| `refs rebuild` | `du --meta-dir` (local tree files) |
| `gc --data-servers … [--apply]` (the server reads its own tree, bytes enumerated against the data-server) | |

Semantics and boundaries:

- **Same set of guards**: behind the governance port is the same `ControlStore`; refusal reasons for illegal names, existing names, names governed by an alias/lens/fork, etc. are byte-for-byte identical to local, only the executing process changes.
- **Recognizes only admin**: keys of the `mount`/`mcp`/`any` scopes are all refused on that port (`any` never upgrades to admin); there's no anonymous mode, and when `api_keys.db` is empty no one gets in.
- **Revoke is instant**: each call checks and authenticates fresh, after `apikey revoke` the very next verb returns `this api key has been revoked`.
- **No session**: the framed protocol is one-request-one-response, no lease that can be extended; the secret appears only in a frame, the server compares only SHA-256 digests.
- **The port is the boundary**: the governance port opens only to the internal subnet, the mount port still uses `--listen`; a `--control-listen` bind failure makes `meta-server` fail at startup, not come up unhealthy.
- **The three backends take one road**: `fork`/`graft`/`export`/`import`/`gc` are executed by the store-holding `meta-server`, whether the backend is a SQLite file, a PG `grovefs_v_<hex>` schema, or an FDB subspace doesn't change the command shape; under a PG/FDB deployment the server is already the only process that can touch the schema/subspace, and a management machine needs neither a `postgres://` connection string nor a cluster file (so there's no leakage of a shared DB password).
- **A dump carries metadata only**: `export` renders server-side into the `<meta-dir>/meta/.xfer/` staging area and returns **line-aligned chunks** (never splitting a line), `import` pushes chunks back in reverse and installs in a single transaction; a cross-machine import moves only metadata, the bytes themselves must be moved separately — the CLI prints this reminder every time.
- **e2e**: `forksrv` (fork/graft three tiers), `xfer` (export/import three tiers × local/remote), `gcremote` (gc three tiers) each assert line by line "the management machine writes nothing locally + the ledger is exact + consistency across tiers".
- **Defaults unchanged**: a `meta-server` without `--control-listen` is exactly as in the other sections; a governance command without `--control-server` still edits the local / current-URI store directly.

e2e in the `ctlplane` suite (startup banner / the first admin key must be minted on the host / remote create·define·alias·list·rm·refs·apikey full set with no local writes on the management machine / a second home still empty (proving the tree is only on the server) / `--for any`·`--for mount` and a junk key refused / remote revoke-instant / two refusal classes for no-key and local-flag mixing / the `scan`·`lease` two non-governance verbs refused by name / guards still effective after a remote tree delete / the PG tier's same assertions); the remote paths of fork/graft and export/import are covered by the `forksrv` and `xfer` suites respectively (three backends × local/remote).

---

## 4. Production topology: central storage + multiple client LAN mounts

Real scenario: data sits on a central cluster, several client machines mount over LAN, the same user across machines sees **one authoritative view**.
This requires splitting the two services:

```bash
# ===== 中心节点 =====
# 数据服务:把物理对象存到本节点的硬盘目录
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster

# 元数据服务:托管每用户权威视图(多客户机共享同一棵,不再各存各的副本)
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110
#   --meta-dir 就是元数据根:视图库在其 meta/ 下。视图名必须显式创建——先建树再挂:
$Y view --meta-dir /data/cluster create --view alice
#   (不建就挂会被 M_OPEN 拒:view 'alice' does not exist on this meta-server …)

# ===== 每台客户机(远程挂载不需要 --home,本地什么都不落) =====
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

- `--data-servers <host:port>`: file IO goes over the network to the data-server, never touching the client's local disk.
- `--meta-server <host:port>`: the view tree is served by the central meta-server, **multiple clients mounting the same user = sharing one authoritative tree**.
- The client keeps **no view copy locally and writes nothing locally**: a remote mount given `--data-servers`/`--meta-server` **needs no `--home`** (passing one writes nothing) — the authoritative data is on the meta-server, the bytes on the data-server, access control all happens server-side at `M_OPEN`/per-op, the client doesn't consult a local DB.

> **A nonexistent view name is refused outright — this is the only behavior, no switch.**
>
> **Rule**: the name must already exist — a physical tree, or a governance definition (derived/alias/fork) — otherwise `M_OPEN` refuses outright and **writes nothing**. The refusal travels as a **payload-bearing `E_DB`** (not an empty packet), so the client prints the reason rather than a bare `Input/output error`, and the server log has the same sentence (verified verbatim, SQLite):
>
> ```text
> meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition …; no empty tree is ever auto-created
> $ grovefs mount --view typo1 --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/t
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
> **For a new view, always create-then-mount**: SQLite `grovefs view --meta-dir <meta-dir> create --view <name>` (or `--home <meta-dir> init --view`, or `view import` seeding); PG `grovefs view --meta-store postgres://… --meta-dir <meta-dir> create --view <name>` builds the schema explicitly (or restores from `pg_dump`).
>
> **The orthogonal other lock is binding a key to a view**: a key from `apikey create --view team` mounting a different name is refused at the **binding check** (earlier than the existence check), verified `permission denied: this api key is bound to view 'team', not 'ghost3'`, likewise building no tree. Division of labor: the existence check governs "does this name exist", the bound key governs "which tree may this caller open".
>
> Cleaning up a mistakenly created empty tree (first `umount` all mounts of that name): for a bare base view (a physical tree with no definition) use `grovefs view --meta-dir D rm --name <base> --base` in one command — it deletes `meta/<base>.db` (+WAL/SHM), runs `refs rebuild` to recompute the ledger, and reclaims bytes no other view holds (§3.4.3). The manual equivalent: `rm -f meta/<name>.db meta/<name>.db-wal meta/<name>.db-shm` then `grovefs refs --meta-dir D rebuild`; the PG backend is the **same command** with `--meta-store` (`view --meta-store postgres://… rm --name <base> --base`) — the booking-off (per-key decrement of `grovefs_refs.holders`) and `DROP SCHEMA … CASCADE` complete in one transaction, and after commit bytes are reclaimed per the `data_ref` read before deletion (§15.8). A hand `DROP SCHEMA` past the CLI does not count as deletion: the ref list vanishes with the schema, the ledger rows thereafter can be reclaimed by no one (`refs --meta-store postgres://… rebuild` only deletes the over-counted rows, the bytes are already unlocatable).

### 4.1 Does multi-machine sharing really need PG? — getting-started selection

> **Direct answer: no.** The ability to share one tree across machines over LAN comes from "the same `meta-server` + the same `--view`", unrelated to whether the backend stores SQLite or PG. The default embedded SQLite (compiled into the binary as `rusqlite bundled`, zero external dependencies) already suffices for multi-client mounts, write serialization, and real-time invalidation push. PG is only an **optional backend** for meta-server, a pure server-side config (`--meta-store postgres://…` given at meta-server startup, clients have no such option and are unaware). The two are a **pick-one** replacement relation, not stacked: the same authoritative tree lands on one backend only — with SQLite you needn't install PG; after switching to PG the view tree goes only into a PG schema, the server no longer creates/reads/writes any `<meta-dir>/meta/<view>.db` (what stays in the directory is the old pre-switch copy); the control plane follows the backend too — `grovefs view`/`apikey`/`mcp`/`data-server` also accept `--meta-store postgres://…` (or env `GROVEFS_META_STORE`), keys and audit go into `grovefs_control.api_keys`/`mcp_calls`, view definitions into `grovefs_control.view_defs`, and a PG deployment lands no control-plane file locally (proven by the `control` suite). Go to PG only if you want to hand the authoritative tree's persistence/HA/operations **to a mature database**.

| Topology | Backend | Applicable scenario | Command |
| --- | --- | --- | --- |
| Single machine local | SQLite (embedded) | development, testing, single user | `mount --view alice --mountpoint /mnt/alice` |
| **Central node + multi-client LAN** | **SQLite (default)** | **small/medium team sharing, zero external-service dependency** | `meta-server --listen 0.0.0.0:50110` + client `mount --meta-server ip:port --view alice` |
| Central + multi-client LAN | SQLite + HA (`--role`+`--peer`+`--witness`) | auto failover after a single meta-server dies, double-write protection | add `--role primary --peer …` etc. |
| Central + multi-client LAN | **PostgreSQL** (`--meta-store postgres://…`) | want to hand HA/backup/PITR/million-node index tuning **to PG**, and already have PG ops capability | `meta-server --meta-store postgres://user:pw@host:5432/db` (can't add `--peer`/`--witness` at once) |

> **Common misconceptions corrected**:
> - "PG must be installed on both client and server" → wrong, **server-side only** (the meta-server process connects to PG), the client installs only the `grovefs` binary and speaks only GroveFS's TCP framed protocol.
> - "Going to PG gives higher write concurrency" → **partly true**: the default SQLite is still single-writer; but `--meta-store postgres://…` changes writes to a SERIALIZABLE write-connection pool (at most 8 connections, the HopsFS route), concurrent writes on disjoint rows commit in parallel, conflicts transparently retried by SSI (verified by the `pg_ssi`/`pg_parallel` suites). Parallelism is only within a single PG cluster, not partitioning metadata authority across clusters; cross-cluster scaling is still left to PG's own replication/sharding.
> - "You must have PG before sharing a tree" → wrong, the default SQLite supports it, the three suites `metasrv`/`freshness`/`locks` are all pure SQLite.

### Cross-client consistency: two modes

| Mode | Command | Behavior |
|------|------|------|
| **Real-time invalidation push (default, recommended)** | just `--meta-server` | after one client writes successfully, the meta-server broadcasts invalidation (`inval_entry`+`inval_inode`) over an event bus to the same user's other subscribers, the peer's next access is **immediately visible**, no polling. |
| **`--no-cache` strong-consistency backstop** | add `--no-cache` | disables kernel attr/dentry caching and reads directly via `FOPEN_DIRECT_IO`, writes immediately visible. Cost: zero client cache, more metadata/data round-trips. For scenarios with extremely strict freshness demands. |

> **Default is fresh**: the non-`--no-cache` push-invalidation model already guarantees "A writes successfully → B sees it on next access immediately", no polling, no `--no-cache`. Proven end-to-end by `.build-bin/test_freshness.sh` (sequential append of 100 lines, truncate, reverse read all become peer-visible within <500ms). `--no-cache` is **not** the only channel to keep fresh; it merely fully bypasses the kernel cache as a more conservative backstop in the extreme race window where writeback dirty-page flush and remote invalidation interleave.

> **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` are arbitrated centrally via the meta-server's `locks` table (SQLite), **across processes and machines**; after a primary/standby switch the lock state survives via snapshot replication. Lock reclamation has two paths: ① **the lock-holding process crashes** — the kernel closing its fd triggers FUSE `release` (carrying a matching `lock_owner`), the lock row is **deleted immediately**, far less than the lease; ② **the whole mount daemon machine crashes** — no `release`, no renewal, the lock expires and is swept **within a short lease (default 15s)**. A live holder keeps renewing via the mount daemon's **renewal heartbeat** (bumping `expires_at` every lease/3), **never timing out**. Non-cooperative apps (cp/dd) still don't block, consistent with NFS/SMB advisory semantics. Proven by `.build-bin/test_locks.sh` (segment 4 crash-immediate-reclaim / segment 4b renewal across the lease). **Scalability boundary**: lock-waiting (blocking F_SETLK / `flock -w`) polls synchronously in the mount process (every 100ms, up to 10 minutes), occupying a FUSE worker thread; under the default `--threads 4`, if ~4 processes are waiting on locks at the same mountpoint at once, other requests to that mount queue — for lock-heavy scenarios increase `--threads`, event-driven wakeup is on the roadmap.

#### 4.1.1 The three tiers' hard metrics: write parallelism is just the most visible one

| Metric | SQLite (default) | PostgreSQL | FoundationDB (experimental tier) |
| --- | --- | --- | --- |
| Write model | single writer (`BEGIN IMMEDIATE` mutex); reads parallel via WAL | 8 SERIALIZABLE write connections + 4 read-only connections per view, disjoint rows commit in parallel, conflicts transparently retried by SSI | optimistic-concurrency (OCC) commits, conflict retries; but **each view still has one writer**, fenced by a deployment-level lease |
| Write throughput (this repo measured) | **767–1,109 ops/s** (N=1 1,109 / N=4 811 / N=8 767; concurrent writes still single-writer serialized, and dip slightly with mount count) | single-view plateau **~4.6k–5.2k ops/s** (N=4 peak 5,235; N≥8 dips with SSI retries; after the write-statement cache N=1/4/8 are 3,048–3,118 / 4,784–5,202 / 4,518–4,680, §6.2) | **171–498 ops/s** (real four-machine `triple`, PG 1,886–4,798 in the same run; 133–1,601 on a single-machine three-process cluster); the G3 performance gate 4,600 not passed |
| Single-transaction cap | no hard gate (bounded only by memory/disk) | no hard gate | **10 MB**: an over-cap whole-tree write auto-commits in chunks at a 70% budget (with refs ~5,400 rows per batch; the measured boundary of how many rows one transaction writes is in §6.3.4) |
| Availability | native HA: `--role primary/standby` + `--peer` + `--witness`, snapshot forwarding; single writer, switching has a window | left to PG itself (streaming replication / PITR), GroveFS no longer manages it | three replicas + fault domains: `kill -9` loses 0s, whole-machine `virsh destroy` 0–1s, survivors regroup the log set in 4s; `fdbbackup` point-in-time recovery |
| View scale | one `meta/<view>.db` + one engine instance per view, about 10 + (read-pool−1)×~1.3 fds/view (a local mount's read pool = `--threads`, default 4 → about 14; tunable via `GROVEFS_SQLITE_READ_POOL`), under 65536 about four thousand views | 12 connections per view (default 8 write + 4 read, tunable via `GROVEFS_PG_*_POOL`); after `--engine-idle-secs` eviction counted by "active views in the last N minutes" | connection-free model: one database handle + a key prefix per view, designed for ten-thousand-scale hot views |

The conclusion is clear: **the gap between SQLite and PG is the single-writer model itself (one write transaction at a time), measured about 4–6×**; conversely FDB has no write-parallelism advantage either (one writer per view), its value lies elsewhere.

> Historical-account correction: an old version of this section wrote SQLite as "45–58 ops/s, capped by an fsync at ~17.5ms". That fsync was not on the view DB, but on the cross-view ledger: after `ATTACH refs.db`, switching `journal_mode=WAL` reset **that attached DB's own** `synchronous` back to FULL, so every naming operation paid a 17ms-class fsync on `refs.db-wal` (strace: 300 creates → 328 fsyncs, of which 301 landed on `refs.db-wal`). `attach_refs_ledger` now mirrors main's `synchronous` onto the ledger (pinned by the unit test `refs_ledger_mirrors_the_view_synchronous_mode`), single-threaded sequential create went 19.7ms → 2.14ms/op, and the concurrency values above rose accordingly.

Under a remote mount (`meta-server`), one create once required three metadata round-trips: the kernel's `LOOKUP`, `M_CREATE`, and the `M_GETATTR` right after it, only to fill the FUSE reply. Now `M_CREATE` / `M_SYMLINK` inline the just-committed row into the reply (`MetaProvider::create_node_with_attr`; the local engine uses a default implementation that reads one extra row without an extra round-trip), the client no longer adds that `M_GETATTR`: on the same remote mountpoint, 1500 sequential creates, `--stats`' `remeta.rpc` per create **3.003 → 2.003**, **0.457–0.494ms → 0.360–0.407ms/op** (+24%, three re-measures). The attributes come from the just-written row itself, mode/uid/gid/nlink/link-target pinned by `create_and_symlink_replies_carry_the_row_in_one_round_trip` and `.build-bin/probe_create_attr.sh`; when an old server's reply carries no row the client auto-falls back to one `M_GETATTR`, old/new mixed runs unbroken.

#### 4.1.2 What can be replaced is the feature surface, what can't is the form

**On the feature surface SQLite is a superset**: of 22 governance capabilities (see the §10.1 matrix), all but the two FDB-exclusive `view scan`/`view lease` are available on SQLite — `fork`/`graft`, `export`/`import --as-fork`, `refs rebuild`, `rm --base`, remote `gc`, `du`, `apikey`+audit, and even **native HA exists only on SQLite** (PG/FDB both refuse). And SQLite does two things FDB can't: no 10 MB transaction gate, zero-dependency single binary. (That 10 MB on FDB is no longer a reason for "can't move a big tree" — `import`/`fork`/`graft` auto-batch when exceeded, see §6.3.4 — it's just that the same tree spans multiple transactions rather than one.)

**What can't be replaced is these four** (all in "form", not "feature"):

| Dimension | SQLite's real state | Why FDB can't be got around |
| --- | --- | --- |
| Availability to the storage layer | primary/standby + witness + snapshot forwarding: single writer, switching has a window, depends on two external processes and the network | replicas and fault domains are in the storage layer, process/machine failures regrouped by the cluster itself |
| View scale | one file, one engine per view, fd/memory grow linearly, no trick besides "eviction" | connection-free model, ten-thousand-scale hot views is its project threshold |
| Multi-machine shared write | one `--meta-dir` may have only one writer (file lock); putting the `.db` on a shared filesystem doesn't work | multiple deployments can serve **different** views in parallel (leases fenced per deployment), the same view still single-writer |
| Independent metadata scaling | metadata bound to the meta-server machine's disk | the metadata cluster adds nodes independently, decoupled from bytes (data-server/object store) |

#### 4.1.3 Backend-exclusive / semi-exclusive commands

"Exclusive" means only one tier can run it; "semi-exclusive" means all three tiers share the name but differ in landing spot or form. Detail and refusal verbatim in §15.9.

| Command | SQLite | PG | FDB | Note |
| --- | --- | --- | --- | --- |
| `grovefs import --view V --source DIR` | available | refused | refused | the top-level import is "bring an on-disk directory under management", relying on same-disk hardlink zero-copy; a remote DB has no manageable local disk, it refuses and points to `view import` |
| `gc --meta-dir D --data-dir R [--apply]` | available | refused | refused (local form) | offline orphan scanning must see both the DB and the object directory; a remote deployment uses `gc --control-server … --data-servers …` instead |
| `meta-server --role primary/standby --peer … [--witness …]` | available | refused | refused | native HA is SQLite-only; PG uses streaming replication/PITR, FDB uses the cluster itself and `fdbbackup` |
| `view --meta-store fdb://… scan` | refused | refused | **available** | lists which tree subspaces and key counts are in the cluster; can only run on the host holding cluster access (`--control-server` also refuses) |
| `view --meta-store fdb://… lease [--release\|--force]` | refused | refused | **available** | deployment-level writer fencing: list/release/seize; likewise must connect to the cluster directly |
| `view define / alias / list` | definition lands `meta/views/*.layout` | definition lands `grovefs_control.view_defs` | definition lands cluster `ctl` subspace | each tier lands its own control-plane store, same one discipline; FDB see §6.3.5 |
| `apikey … / calls` | `meta/api_keys.db` | `grovefs_control` | cluster `ctl` subspace | capability OK on all three tiers, only the landing spot differs |
| `refs rebuild` / `rm --base` / `du` | `--meta-dir` | `--meta-store postgres://…` | `--meta-store fdb://…` (`du` may also `--meta-server`) | same name different shell; `du` on a nonexistent view name gives the same `no tree for view …` on all three tiers |

#### 4.1.4 How to choose: three lines, don't pick by "does a feature exist"

- **Single center + hundreds of active views + metadata within a few GB** → **SQLite** (default). Least fuss, largest transactions, single binary; add primary/standby HA as needed.
- **Need 8-way parallel writes / hand backup and PITR to the database / already have PG ops** → **PostgreSQL**. The connection wall has been dissolved by `--engine-idle-secs` eviction; write parallelism is its only hard advantage over SQLite (~4.6k vs ~50).
- **Want storage-layer self-healing + ten-thousand-scale hot views + independent metadata scaling** → **FoundationDB** (experimental tier). The cost is the 10 MB transaction sharding (already automatic, see §6.3.4), a production minimum of 5 nodes, the G3 performance gate not passed.

```
你的部署形态？
├─ 单机 / 单中心，活跃视图 < 千级，能接受主备切换 ─────► SQLite（默认）
├─ 要多写并行，或想把 HA / PITR 外包给数据库 ───────────► PostgreSQL
└─ 要存储层自动愈合，且视图上万 / 元数据要独立扩容 ────► FoundationDB（实验档）
```

---

## 5. How to handle data (key focus)

### 5.0 Anatomy of view metadata (one-screen quick reference)

What exactly is stored in one storage root (`<meta-dir>` or a local `--home`), and how a name is parsed when mounted:

```text
<meta-dir>/
├─ blocks/<2hex>/<key>      内容字节,一文件一对象(key=UUID,按哈希扇出 256 桶)
└─ meta/
   ├─ <view>.db             每个物理视图一棵权威树(SQLite WAL)
   │     node(id INTEGER PRIMARY KEY, ino, parent_id, name, is_dir, data_ref,
   │          link_target, mode, uid, gid, mtime, atime, ctime,
   │          UNIQUE(parent_id, name))   ← 树的全部:名字、类型、指向 blocks 的 key、POSIX 属性
   │     ctrl               (k,v) 键值表:今天只有 HA 的 epoch(0/缺行=没启用 HA)
   │     locks              POSIX/BSD 锁表(租约到期自动回收,release 是持有者终止信号)
   ├─ refs.db               跨视图共享对象持有者账本(GC 判「最后一个名字」的唯一依据)
   ├─ views/*.layout        派生/别名/fork 定义(派生=`base <B>` 行+规则行;别名=单行 `alias <R>`;fork=单行 `fork <B>`(三种行可写在文件任意位置,注释/空行跳过))
   └─ api_keys.db           API key SHA-256 摘要 + 调用审计(SQLite 后端的控制面;PG 后端进 grovefs_control)
```

- **Field-by-field glossary**: `node`'s 13 fields (incl. the division between `ino` and `id`, `mode` storing only permission bits, times in seconds), `ctrl.epoch`, `locks`' 9 fields (the lease and `sid` renewal), `refs.holders`' counting semantics, and the read path each of the 4 indexes supports, see the web manual `docs.html` §8.1.1 (with real `sqlite3` query echoes); this section gives only a one-screen reference.
- **The name-resolution chain**: `mount --view X` → `M_OPEN(X)` → the server looks up `meta/views/X.layout`:
  contains a `base <B>` line (derived) → opens the physical tree `meta/<B>.db`, applies the rule lines as a `LayoutMeta` lens, mounts read-only;
  contains an `alias <R>` line → applies no rules, the name lands directly on the real tree `meta/<R>.db`, whole tree writable, the real name refused to mount;
  contains a `fork <B>` line → opens its own `meta/X.db` directly (an independent tree);
  no definition → X is a physical view, opens `meta/X.db` directly. The client needs no rules file locally.
- **Two backends of the same tree**: SQLite (default, one `.db` per view) and PG (`meta-server --meta-store postgres://…`, one schema `grovefs_v_<hex(view)>` per view, the ledger a shared table `grovefs_refs.holders`) are a **pick-one** replacement relation; the control plane follows the backend too: under `--meta-store postgres://…` keys/audit/view definitions all go into the `grovefs_control` schema (`api_keys`/`mcp_calls`/`view_defs`), nothing lands as a local control-plane file; the CLI `view`/`apikey`/`mcp`/`data-server` use `--meta-store` (or env `GROVEFS_META_STORE`) to reach the same system record.
- **The backup closed loop**: `blocks/` + `meta/*.db` + `meta/refs.db` (recomputable with `grovefs refs --meta-dir D rebuild`) + `meta/views/*.layout` + (as needed) `meta/api_keys.db` — under the PG backend one `pg_dump` covers both `grovefs_v_*` and `grovefs_control`, leaving only `blocks/` + env to back up locally — §5.4 gives the full list and both the stop-service and hot-backup approaches.
- **The customization↔storage mapping**: the full cross-reference of the four forms (command→files/tables produced, parameters, common pitfalls) is in §3.0; §5.0's `.layout` is the on-disk form of "how to customize a view" (the rule's four verbs in README "the three view questions" and §3.1/§3.2); fork is the only customization that creates a physical tree (`view fork` copies `node` rows, bytes shared and booked into the ledger).

### 5.1 Physical layout

- **Single object**: `blocks/<2hex>/<key>`, one file = one object file. Objects fan out by key hash automatically into 256 bucket subdirectories (the bucket name is the first two hex of the key hash), one level deep, no deeper tree — the per-directory entry count doesn't grow with the total, at tens of millions of objects each bucket holds only tens of thousands, `ls`/`rsync`/backup tools don't degrade. The bucket name is a pure function of the key, the 256 are pre-built at startup, no mkdir on the hot path.
- **Key naming**: on a view write a UUID is generated as the object key, stored in the view's `data_ref` field.
- **Striping (a big file across multiple data-servers)**: `data_ref` is encoded as `s<N>:<stripe>[:<fp>]:<uuid>` (`<fp>` is the fingerprint of the member-id set, appearing only in identity mode; the positional mode keeps three fields),
  the i-th chunk key = `{uuid}#{i}`, landing at `servers[i % N]` — when `--member-id` is enabled the slot is determined by the **identity** after sorting member ids (the list order is irrelevant), when not enabled it is the **list index**, not the address. **A key without the `s` prefix = a single object** (the storage form when striping is not enabled).

### 5.2 Multi-data-server striping (aggregating capacity/bandwidth)

```bash
# 挂载时给逗号分隔的多个 data-server 地址 → 自动启用确定性条带
$Y mount --view alice \
     --data-servers 10.0.0.5:50100,10.0.0.6:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

- A file is split into chunks only if it's ≥ the chunk size; the chunk size is configured by `--stripe-bytes` (env `GROVEFS_STRIPE_BYTES`), default 16 MiB — it's booked per-object into `data_ref`, so changing the config affects only objects created afterward, old objects remain readable at their own recorded sizes, and a cluster can mix different values. Small files naturally land only in chunk0 (a single node).
- **Value**: spreading a large file over multiple nodes' **aggregate capacity**. Note: on a single already-saturated link (loopback/single NIC) there is no throughput gain, real gains require genuinely independent links.
- A single-address `--data-servers host:port` is striping-not-enabled: the whole object lands on that one machine, the key has no `s` prefix (giving `--stripe-bytes` here is honestly warned as ineffective).
- **Member identity `--member-id` (eliminating the order hazard)**: once each data-server starts with a unique `--member-id N` (or `GROVEFS_MEMBER_ID`), the client's stripe placement is by **sorted member identity** rather than `--data-servers` list order — the list in any order over the same machines reads/writes correctly, machine/IP changes replace in place. Keeping the count but **changing the set** (swapping one machine with a new id) is caught by the member-set fingerprint in `data_ref` and the mount is refused, rather than silently reading the wrong chunk. Either all members configure ids or none do: mixing the two modes in the same key space is refused at mount. Without ids it stays in the historical positional mode by list index, where reordering `--data-servers` silently mis-routes existing objects (see §3.4.2 migration constraints).

### 5.3 Deletion / GC

- After `unlink` (deleting a file), the view drops the reference, the backend best-effort deletes all chunks by key, a striped remove expands to delete all chunks, **idempotent** (leftover chunks can be cleared by GC).
- **The reference ledger `refs` for cross-view shared objects**: one `data_ref` names exactly one underlying object yet may be simultaneously named by multiple physical views (`view fork` and `view export/import --as-fork` copy the same batch of `data_ref`, derived views/aliases share the base view's bytes; whereas `grovefs import` bringing the same source file into two views gives each an **independent key** — both `blocks/` entries are hardlinks to the source file, bytes still the same inode, zero extra usage, but the ledger counts per key separately). A view's internal reference count can't see other views, so taken alone it would mis-delete objects another view still references. Hence the booking lands in `<base>/meta/refs.db` (PG backend a shared table `grovefs_refs.holders`): each view ATTACHes this ledger, the holder count and node add/delete go into the **same IMMEDIATE transaction**, atomically committed across files. The precise physical-deletion rule is "**the last name for this key within the view is removed, and the global ledger has no other view holding it**" — only then `backend.remove`. An out-of-band deletion of a view db (`rm meta/x.db`, a new `--home`) only makes the ledger **over-count**, objects linger and are never mis-deleted; all three tiers have this command, all doing "read the tree, recompute, don't read the ledger": `grovefs refs --meta-dir D rebuild` (rewrites `meta/refs.db`), `refs --meta-store postgres://… rebuild` (rewrites `grovefs_refs.holders`), `refs --meta-store fdb://… rebuild` (rewrites the ledger subspace in the cluster); the direction is always "linger bytes" rather than under-count, see §15.8. A key with no row in the ledger is that view's private (an empty ledger needs no initialization step).

### 5.4 Backup and migration

- **Metadata is not hardware-bound**: object location = `blocks/<two hex of FNV1a(key)%256>/<key>`, derived as a pure function of `data_ref`; a `node` row stores no absolute path/device number/hostname, and `ino` is GroveFS's own numbering. It works directly across machines/OSes, as long as metadata and bytes go together.
- **The list to migrate**: `blocks/` (bytes), `meta/<view>.db` (each view tree), `meta/refs.db` (cross-view ledger, recomputable with `grovefs refs rebuild`), `meta/views/*.layout` (derived/alias/fork definitions), `meta/api_keys.db` (needed only if keys were used). That's the SQLite-backend list; under the PG backend trees, ledger, definitions, keys, audit are all inside the DB and migrate with `pg_dump`/`pg_restore`, locally you need only move `blocks/` + env.
- **Whole-directory cold copy**: after stopping all mounts and the `meta-server`, `rsync -a -H` the whole storage root; **don't raw-copy a running SQLite** (WAL), if you can't stop use `sqlite3 <db> ".backup"` / `VACUUM INTO`.
- **Single-view export/import**: `grovefs view --meta-dir D export --view V --out F` → on the target after copying `blocks/`, `grovefs view --meta-dir D2 import --view V --in F`; preserves id/inode/hardlink/permissions/owner/symlink, atomic replace (a failure doesn't harm the old tree), `--as-fork B` restores fork semantics. Details in §3.4.2.
- **Topology boundary**: multi-disk `--data-dir` count and order must match (an equal-disk clone replacing in place doesn't affect location, only adding/removing/reordering remaps; or merge into a single root); cross-server striping `s<N>:` must keep N members; with `--member-id` the order is irrelevant but the set must not change, without it the order is also part of the layout (§5.2); a whole-directory copy carries the HA `epoch` over, the old primary must be stopped first; the PG backend uses `pg_dump`/logical replication.
- **Multiple base views**: any number of base views each their own `meta/<name>.db`, you may migrate just one, and keys may bind per view.
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
- The client's `--meta-server` supports a failover list: `--meta-server 10.0.0.5:50110,10.0.0.6:50111` — when the primary dies it auto-switches to the standby to continue reads/writes.
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
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token s3cr3t -m /mnt/alice
```

- token mismatch → permission denied directly at connect, no retry.
- empty token (default) = LAN-trusted, no auth, trusted intranet only.

**But `--token` is one shared secret**: any host holding it can mount, the audit can't tell "who mounted", and kicking one caller means rotating the token and cutting everyone. When you need **per-caller revocable credentials**, use the same `grovefs apikey` registry as the mount-plane gate (full mint/scope/audit/revoke usage in §3.5.1):

```bash
# 服务端:把 key 门与 token 叠加(两道都得过),或只上 key 门
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
# 客户端:呈递一个 `--for mount`(或 `--for any`)的 key
$Y mount --view alice --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 \
     --api-key '<id>:<secret>' -m /mnt/alice          # 或 export GROVEFS_API_KEY 不进 argv
```

The two key differences from token: ① **revocable per caller and closes live connections** — the server revalidates once per second (checked only with traffic), a revoked key is refused on that mount's first server round-trip after the revoke (E_AUTH/EACCES carrying the refusal verbatim), the mount prints the reason then **unmounts itself** (leaving no stale endpoint), while other keys' mounts are served as usual; ② **auditable** — every connect (incl. refused) lands in `apikey calls` (method=meta/data, tool=M_OPEN/OP_OPEN, detail records view and uid). Both gates are **entrance gates**; after adding the key's `--view`/`--ro` two access-control dimensions, "who may touch which tree, read-write or read-only" is carried by the credential itself and enforced per-op server-side.

### 6.1 Sharing a view and per-caller differentiated permissions (view-bound / read-only key)

**By default `--view` does not isolate uid**: any uid that does `mount --meta-server <addr> --view alice` lands on alice's same authoritative tree, seeing each other's writes. That is the out-of-the-box "many people share one view" form — metadata central, content central, real-time invalidation push guarantees convergence.

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
   ├─ SQLite（默认）：       一 view = 一个独立 db 文件  <meta-dir>/meta/alice.db · bob.db
   └─ PostgreSQL（--meta-store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62
```

> The independent-storage unit in the database is "the view", not "each client": SQLite one `.db` file per view, PG one schema per view (neither is a single flat table); data bytes go through the data-server → stored into its `blocks/<key>` under `--data-dir`, the tree stores only the `data_ref` reference.

The uid a client reports defaults to `geteuid()`, overridable explicitly with `mount --run-as-uid N` — it is recorded as the entry's owner in lock arbitration and audit only, **advisory** (a TCP client can forge it), and **never an access-control gate**. Determinism rests on `--token`/`--require-api-key` (admission), "who may touch which tree, read-write or read-only" on the key's `--view`/`--ro` binding enforced per-op server-side. Two behavior boundaries you must know: ① **lock-class ops (`flock`/`F_SETLK`, incl. lock renewal) are classified as write-intent** (they write the central `locks` table), so a read-only key (`--ro`) has their requests refused uniformly (`E_NOTPERM`, landing `EPERM` on the client FUSE side, distinct from the HA standby's `E_ROFS` switch signal) — a read-only role that needs a shared lock for read coordination must use a read-write key; ② **revoke is op-driven** (the server revalidates once per second on traffic), a revoked/demoted key is refused on that mount's first server round-trip after the revoke, a read answered entirely by the kernel cache needs no round-trip and is thus not interrupted (closed on the next round-trip).

### 6.2 PostgreSQL metadata backend (optional)

`meta-server` stores the authoritative view tree in embedded SQLite by default (one `<meta-dir>/meta/<view>.db` per view); you can switch it to PostgreSQL:

```bash
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store postgres://user:pw@10.0.0.9:5432/grovefs
```

The client mount command is exactly the same (`--meta-server host:port --view alice`), the wire protocol unchanged. Internally the server maps each view to a `grovefs_v_<hex(view)>` schema (hex guarantees identifier safety, injection-agnostic), the root node fixed at id=1, `ROOT_INO` consistent with SQLite behavior. Table creation is idempotent `CREATE ... IF NOT EXISTS`, no pre-migration script needed. Ops needing multi-statement atomicity (rename/unlink/lock) go through explicit transactions.

**Notes**:
- **Incompatible with the in-house HA**: `--meta-store postgres://…` + any `--role standby` / `--peer` / `--witness` fail-fast at startup (the `VACUUM INTO` snapshot mechanism applies only to SQLite). Leave PG high availability to **PostgreSQL's own streaming replication / logical replication / PITR** — stronger than file snapshots anyway.
- **Deleting a view is one command**: `view --meta-store postgres://… rm --name V --base --data-dir R` decrements `grovefs_refs.holders` per key (DELETE at zero) then `DROP SCHEMA … CASCADE` in **one transaction**, reclaims unheld `blocks/` objects after commit; `refs --meta-store … rebuild` recomputes the whole ledger over the surviving trees when quiesced. PG's DDL is transactional, so this tier has none of the SQLite tier's "delete the db → rebuild the ledger" intermediate state (§15.8).
- **Idle views auto-release connections**: every mounted view keeps a resident engine pool in the meta-server (default 8 write + 4 read connections, tunable via `GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL`). `meta-server --engine-idle-secs N` (default 300, `0` disables) reclaims the engine after the view's last mount session ends and N seconds of silence, the connections close with it; the next access auto-reopens, transparent to clients (proven by the `evict` suite: 6 views 76 connections → 4 after unmount reclaim). So the PG planning formula goes from "total views × 12 ≤ max_connections" to "**active views in the last N minutes** × (write pool+read pool) + burst overflow ≤ max_connections" — the total view count is no longer capped by connection count. A mounted view is never reclaimed (the session count pins the engine, even when no one reads/writes).
- **The write path also caches prepared statements**: the read path caches ≤16 per connection (§8 above), the write path is on par — each write connection caches ≤32 (`WRITE_STMT_CACHE_CAP`, `INSERT`/by-name `lookup`/`DELETE`/`SET ino`/four `setattr`/`rename`/lock ops/three ledger statements). `tokio-postgres`/`postgres` 0.19 has **no** `prepare_cached`, so the cache is hand-built: `WriteConn` holds a `HashMap<String, Statement>`, `WriteTx` shadows `Transaction::query/execute` with inherent methods, so **SQL inside the transaction body, unchanged to a character**, hits the cache; a prepared statement survives `COMMIT`/`ROLLBACK` (measured), only DDL changing the result type triggers `0A000`, when the entry is evicted and one unnamed fallback taken. Measured (`bench_pg_conc.sh`, N∈{1,4,8}, T=8, 6s, default `sync_commit=on`, two rounds): FUSE-rig write throughput **3,048–3,118 / 4,784–5,202 / 4,518–4,680 ops/s**; the cache is built into the code (`WRITE_STMT_CACHE_CAP`, no switch), the control group is the same rig, same op sequence, readings without the cache build **2,556–2,577 / 4,131–4,200 / 3,350–3,497**, single-connection one transaction 577–670µs → 316–330µs, i.e. N=1 +18–21%, N=4 +15–24%, N=8 **+30–36%**, stacking `GROVEFS_PG_SYNC_COMMIT=off` is stably beneficial only at N=4 (5,225 → 6,142, +18%; N=1/N=8 within jitter, see §8); contention cases 180/180 single winner, `test_pg.sh` 24/24.
- **Sharding across multiple PG instances (prototype, not a support surface)**: `--meta-store` accepts comma-separated multiple URIs, `postgres://…/db1,postgres://…/db2`. Each view's `grovefs_v_<hex>` schema lands on **exactly one** instance, the choice a pure function of the view name (FNV-1a hash modulo), so `view create`, any meta-server, and cross-view ops (graft/export/rm by name) **need no registry** to point at the same place; `PgEngine::open` is the sole routing entry, `view list` merges across instances, the control plane (`grovefs_control` definitions/keys/audit) is pinned to the **first** instance. A single URI passes through unchanged, a one-instance deployment is byte-for-byte unchanged.

  Boundaries (deliberately so, don't treat it as one switch short): ① **one shard = one independent "metadata + content" domain** — the `grovefs_refs.holders` cross-view ledger has one copy per database (SQL transactions can't cross DBs), so two instances sharing the same blob batch book separately, unaware the other still holds it, hence each shard must have its own data-server; ② `refs rebuild` therefore **runs per instance** (a multi-URI is explicitly refused with the per-instance command given), `gc` likewise; ③ a graft/fork source and target must be on the same instance (one `INSERT … SELECT` can't cross DBs), different instances give an error naming both landing spots rather than connecting to the wrong server; ④ changing the instance list **remaps** views — resharding is a data move, not a flag change.

  Evidence (`.build-bin/probe_pg_shard.sh`, two DBs `grovefs` + `grovefs_shard2`, 8 views): s1–s8's schemas are on db1/db2 "one and only one" (`db1 + db2 == 1` per view), 4/4 spread; the **same meta-server** serves both sides' views (db2's s7 and db1's s8 each write 200 4KiB files, 2.67–2.87ms/op), row counts land in the routed schema; after emptying all names of the db1 shard, db1's 200 used ledger rows go to zero while db2's ledger totals don't move a character (no cross-shard bleed); `refs --meta-store "a,b" rebuild` is refused and prompts rerunning per instance. The routing function has 6 more unit tests (stability and spread, single-URI passthrough, control-plane first instance, fork/graft same-instance constraint, empty-instance skipping).
- Why not offer Redis: K/V+TTL is not isomorphic to the view tree (transactions/row locks/multi-column indexes are all naturally needed, forcing it would just rebuild a weaker PG on the same instance).
- Scenario-by-scenario runnable commands (single-machine verification / production systemd / inspection / backup-restore / delete-view / capability boundary) in **§15.3–§15.9**.

---

### 6.3 FoundationDB metadata backend (experimental, not a support claim)

> **This tier's status: the feature surface has all passed, the capacity surface has been judged as not passed, so it doesn't enter CANON, doesn't enter the support surface.**
> The supported metadata backends are still embedded SQLite and PostgreSQL (§6.2); metadata-plane write parallelism is only within a single PG cluster, partitioning metadata authority across clusters is not in this round's support surface;
> the account source is the decision gate G3's measurement (2026-11-09, same machine same run same op sequence, RF=3 three replicas): the FoundationDB arm's
> **best of 24 units 1601 ops/s** (`ssd` engine, most units 133–998), taking the disk out of the commit path entirely
> (`memory` engine) still only **1642 ops/s**,
> while the PostgreSQL arm in the same run reached **4100–4900 ops/s** — the threshold is "no worse than the PG plateau ~4.6k ops/s",
> the measured best still 2.9× short, most units 5–15× short. And this machine has no SSD medium (both RAID-card `queue/rotational=1`),
> the planned SATA/NVMe tiers can't be reproduced here. But the attribution data gives a warning: swapping disks alone isn't enough — moving the disk out of the commit path
> still only reaches 1642 ops/s, the remaining ~3× is on the implementation side. Attribution, rerun recipe, and the promotion conditions are all in
> `docs/fdb-keyspace.md` §14 v1.8/§15; an SSD-medium rerun is a necessary precondition, not a sufficient one.
>
> Below is this tier's **current real behavior** (experimental use): `meta-server` can also put the authoritative view tree into FoundationDB —
> one subspace per view (item-by-item design of key encoding and transaction granularity in `docs/fdb-keyspace.md`):

```bash
# 这一臂是 Cargo 可选 feature，默认关闭：不链 libfdb_c 的构建完全不进这段代码
# 客户端库不在系统路径时给一个目录即可（无需装包，见 §11.10 第 1 步）；
# 绑定已预生成（`vendor/foundationdb-sys`），所以构建期也不需 libclang
FDB_CLIENT_LIB_PATH=/path/to/lib cargo build --release --features fdb

$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///etc/foundationdb/fdb.cluster --token $TOK
```

- **Dependency surface**: this arm **depends on no system install package**, for both run and build (referring to the `libfdb_c` and FDB-artifact surface; `mount`'s need for the setuid `fusermount` helper is backend-independent, §1/§11.0). Run: `libfdb_c.so` is a sha256-verified official artifact (§11.10 step 1), at build time `build.rs` writes `$ORIGIN/lib`, `$ORIGIN/../lib` and that directory into the ELF's RUNPATH, so the binary finds it itself, **no `LD_LIBRARY_PATH`, no `ldconfig` needed**; putting `grovefs` and `lib/libfdb_c.so` in the same directory (or a `dist/`-style child one level up) is one self-sufficient deployment. Build: `foundationdb-sys` by default runs bindgen at build time (which would need libclang installed), this repo uses `[patch.crates-io]` to swap it for `vendor/foundationdb-sys` — same code, just with the bindings pre-generated and committed (source, sha256, refresh and rollback steps in `vendor/foundationdb-sys/VENDOR.md`); so `--features fdb` needs only a directory containing `libfdb_c.so`, no package-manager actions.
- **Deployment surface**: the two surfaces above already have a ready physical instance in the repo to verify — `dist/grovefs-fdb-experimental-linux-x86_64/` (main binary + `lib/libfdb_c.so` + official 7.1.67 `fdbserver`/`fdbcli`/`fdbmonitor` + `README.md`/`BUILDINFO.txt`/`SHA256SUMS.txt`, outer `…tar.gz.sha256` fixing the packaged artifact's fingerprint), a target machine needs only glibc >= 2.31, kernel FUSE, and for non-root mounts the distro's own setuid helper `fusermount`/`fusermount3` (with `CAP_SYS_ADMIN`, `fuser` connects `mount(2)` directly, no helper, already measured in a user namespace; the same sentence as §1/§11.0: what's needed is libfuse's userspace tool, not the dev package; this is backend-independent, the SQLite/PG tiers are the same). It is **still an experimental-tier evidence artifact, not a release** (no tag; the binary was rebuilt on a clean worktree, the implementation content corresponds to commit `ed87cbc`, but the G3 performance gate is still not passed), the end-to-end `env -i` proof script is `.bneck/pkg_verify.sh`, item-by-item coordinates in `docs/fdb-keyspace.md` §14 v2.2–v2.9 (v2.9 records the in-package binary's size and fingerprint when switched to the `ed87cbc` version, v2.3 adds the root/`CAP_SYS_ADMIN` cell and the measured "build also needs no `pkg-config`/libfuse dev package").
- **URI form**: `fdb://<cluster file path>` or `fdb://?cluster=<path>`; if both are given empty it falls back to `$GROVEFS_FDB_CLUSTER_FILE`, still empty then it refuses to start (no guessing a default path). The cluster file is FDB's own addressing credential, grovefs doesn't parse its content.
- **Zero client change**: `mount --meta-server host:port --view NAME` unchanged to a character, wire protocol unchanged. Only the meta-server needs to link `libfdb_c`; clients don't install FDB, don't hold the cluster file, don't perceive the backend.
- **Trees are built explicitly**: `grovefs view --meta-store fdb://… create --view NAME` opens a subspace (writing the root row and two allocator keys). `view create --manifest` is **refused** on this tier — a manifest seeds SQLite files; for content, write files directly after mounting.
- **Migration and rollback use the same pair of commands**: `view export` / `view import` work on this tier, producing the **same** host-agnostic JSONL as the SQLite tier — a subspace can be exported, a dump can open a subspace besides `view create`, a dump re-exported from the cluster after import is line-for-line identical to the source dump (id/ino/hardlink/permissions/owner/times all preserved as originals), both allocator watermarks raised to the export's max. Rolling back to SQLite is the same dump imported in reverse, the source store not written during the migration window, the original tree kept. Bytes are not on this channel: they belong to the data-server's data root (§3.4.2 has the whole command sequence).
- **The control plane enters the cluster with the metadata backend**: view definitions/aliases land in the cluster `ctl` subspace's `CtlDefTable` (`view_defs`/`view_def_ver`), API keys and audit land in the same `ctl` subspace's `CtlKeyTable`, isomorphic to `--meta-store postgres://…` moving the control plane into the `grovefs_control` schema (§6.2). So an FDB cluster is the control plane's **sole authority**: every meta-server/MCP/CLI targeting it shares the same definitions and keys, governance commands (`view`/`apikey`/`mcp`) all need `--meta-store fdb://…` (or `GROVEFS_META_STORE`) and **require the cluster to be reachable**, landing no `*.layout`/`api_keys.db` locally. `--meta-dir` on this tier is just a deployment identity (the writer lease fences by it) plus scratch/config, no longer the control-plane record. Hot reload is by the cluster write-version stamp (the counterpart of PG's `xmin`, SQLite's mtime); existing local `.layout` definitions are best-effort uploaded once on the first open via `fdb://` (not overwriting definitions already in the cluster).
- **Encoding account**: intent = RecordLayer-style (hierarchical key space, a record wholly into the value, secondary indexes expressed as independent key ranges); carrier = the Rust `foundationdb` crate's tuple layer + subspace layer. The official RecordLayer is a Python/Java-side legacy binding, this backend **does not use** it, and states only this — must not be written as "used RecordLayer".
- **`--engine-idle-secs` on this tier manages only memory**: FDB has no "12 connections per view" to reclaim (one cluster file shares one handle, socket state is in the client's own network thread), what eviction releases is the in-process engine (number segments, paging cursor, last-touch stamp). A mounted view is still never reclaimed, transparent to clients (§12.1). All three are measured in the `fdb` suite section 13: compressing the threshold to 3s, 8s of silence during a mount ⇒ 0 evictions in the server log; after unmounting the session ⇒ `meta-server: evicted 1 idle engine(s) (>3s idle, no sessions)`; remounting the same view ⇒ reads back the same object (reopen transparent to the client). PG's connection-count planning formula doesn't apply here, capacity depends on the cluster's proxy/GRV and log replica write throughput.
- **High availability and backup belong to the cluster**: FoundationDB ships its own three replicas and coordination layer, snapshots via `fdbbackup`. grovefs's in-house primary/standby (`--role standby` / `--peer` / `--witness`) given together with `--meta-store fdb://…` fail-fast at startup, the wording specifically about FDB (§15.9).
- **The feature surface and the capacity surface have different evidence scopes**: the **feature** criteria have readings in two clusters: the local single-node (replication factor 1) ran all current assertions (incl. the writer-lease section) and the fdb tier of the four governance suites; a real four-machine cross-fault-domain cluster (`triple`, `fault_domains n1..n4`) ran the same assertions all green (measured 2026-11-10 `pass=170 fail=0 SKIP=0`, after that round the suite grew scan / rm-base / lease / backup-restore sections), the four governance suites still have only single-machine readings. The **throughput** numbers at the start of this section have one source only — the decision gate G3's same-run comparison (same machine, same feature-build binary, op sequence copied from the plateau-value source, `configure new triple ssd` three replicas). So these numbers answer "how far the FDB arm is from the PG arm", not extrapolatable deployment capacity. **Replica failure, recovery, and cross-fault-domain topology behavior were measured separately on a real cluster** (2026-11-10, four machines each with its own kernel and its own data disk, `fault_domains n1..n4`): process failure (`pkill -9`) and whole-machine failure (`virsh destroy` one machine) availability losses are 0s and 0–1s respectively, and three survivors regroup the log set and three replicas without the dead machine returning; `fdbbackup`/`fdbrestore` achieve point-in-time recovery — keys written after the snapshot version can only come back via log replay, keys written after the stop version are rolled back. Evidence entries `.bneck/fdb3vm_up.sh`, `fdb3vm_fault.sh`, `fdb3vm_backup.sh`, item-by-item accounts in `docs/fdb-keyspace.md` §14 v2.4. **What's still untested is only "deployment capacity"**: G3 not passed (this machine has no SSD/NVMe medium), the numbers below are same-run comparisons, not deployment capacity — until 2026-11-12 there was a same-account comparison on a real four-machine cluster (next item).
- **Throughput re-measure on a real three-replica cluster (2026-11-12)**: the previous G3 numbers came from a **single-machine three-process** `triple` (faking fault domains with locality keys). This time on a **real four-machine cluster** (the 4 zones brought up by `.bneck/fdb3vm_up.sh`, one VM per zone with its own kernel and its own data disk, `Redundancy mode ssd-2 / triple / Fault Tolerance 1 zones / Replication health Healthy`), a same-run PG/FDB comparison with the same op sequence and the same feature binary (`.bneck/bench_g3.sh` + `CLUSTER3=/data/grovefs/.fdb3vm/cluster3vm`, `N∈{1,4,8}`, `T=8`, 6s, two rounds):

  | N | PG ops/s (two rounds) | FDB ops/s (two rounds) | FDB / PG |
  | --- | --- | --- | --- |
  | 1 | 1,886 / 2,141 | 459 / 171 | ~0.16 |
  | 4 | 4,502 / 4,798 | 498 / 234 | ~0.08 |
  | 8 | 3,872 / 4,583 | 306 / 351 | ~0.08 |

  Verdict `G3_RED` (`fdb/gate=0.11`, `fdb/pg=0.10`) — same conclusion as the single-machine cluster, moving to real three replicas **did not change the order of magnitude**. Two account facts to state: ① **very jittery**: running `.build-bin/bench_fdb3.sh` alone on the same point for 3 rounds, `N=1` gives 425 / 96 / 223, `N=4` gives 141 / 375 / 450, `N=8` gives 422 / 418 / 296 (medians 223 / 375 / 418), i.e. a single-point difference can reach 2–3×, **no single reading can be taken as capacity**, compare over multiple-round medians and report the spread together; ② the FDB arm's actual run time clearly exceeds the box (an 8s box measured 13–19s), showing the commit path was queuing in the slow rounds, not that the driver miscounted. Rerun recipe: `bash .build-bin/bench_fdb3.sh` (built-in preflight, refuses and errors if not `triple` or `Replication health` is unhealthy, so it won't give an incomparable number; `RUNS=n` for multiple rounds, `N_LIST`/`T`/`RUN_SECS` same as the PG rig); the version with the PG comparison arm is `CLUSTER3=… bash .bneck/bench_g3.sh` (both arms interleaved, same session).

#### 6.3.1 Capability boundary: what each of the three layers is responsible for

This tier has three "high-availability"-related layers, with different responsible parties and failure domains, they can't be merged into one sentence "the FDB tier is highly available":

| Layer | Who is responsible | This version's actual behavior | Failure domain / boundary |
| --- | --- | --- | --- |
| Metadata writer | GroveFS meta-server + one `--meta-dir` | **one deployment has only one writer at a time**: at startup it takes a writer-lease row in the cluster (key `(root,"writer")/('lease', <canonical --meta-dir>)`, value `(owner, expires_at_ms)`, owner = `host:pid:nonce`), renewal period `max(TTL/3, 200ms)`; if it can't take it, it waits at most `GROVEFS_FDB_WRITER_WAIT_SECS` (default TTL+2s) then refuses; during running, if renewal finds the row no longer its own, it prints the reason and `exit 1`. TTL default 10s (`GROVEFS_FDB_WRITER_LEASE_SECS`) | one `--meta-dir`. Two writer processes writing the same batch of subtrees is the only form that can't be correct (definitions are already the one cluster-shared copy, the lease forbids two write-authorities, not each holding definitions), that's exactly what the lease forbids |
| FDB cluster | FoundationDB itself | node count, replication factor, fault domains all decided by the cluster's `configure` (`single`/`double`/`triple`/`three_data_hall`…), grovefs neither participates nor rewrites the topology; the startup probe only reads `\xff\xff/status/json` and reports the current topology | the cluster's coordination layer and log replicas. A single-node cluster runs but has no redundancy (when the sole fdbserver dies the mountpoint blocks rather than errors, and continues when the cluster returns) |
| GroveFS native HA | this project's standby/peer/witness | when combined with `--meta-store fdb://…` it **refuses at startup**: it moves SQLite snapshots, uses epoch for primary/standby switching, FDB-tier replicas and recovery belong to the cluster | not applicable |

Two boundaries must be stated separately, they're the easiest to get wrong:

- **Replicas and fault domains are the cluster's capability, not grovefs's**. grovefs replicates nothing; on a `configure new triple` + four-fault-domain cluster, process `kill -9` and whole-machine power loss measured availability losses of 0s and 0–1s, three survivors regroup the log set without waiting for the dead machine back (evidence entries `.bneck/fdb3vm_up.sh` / `fdb3vm_fault.sh` / `fdb3vm_backup.sh`, accounts in `docs/fdb-keyspace.md` §14 v2.4). The same data on a single-node cluster has no such protection.
- **The fencing granularity is "deployment", not view, not cluster**:
  - against "a second writer of the same `--meta-dir`" it is enforced: it is refused **before `bind`** (the port never listens), the refusal wording names the `lease --release --deployment …` exit;
  - against "a still-alive old writer" it is enforced: after `view lease --release --deployment <dir> --force` clears the live row, the old holder prints `meta-server: LOST the FDB writer lease …` on its next renewal (≤ one renewal period, default ~3.3s) and exits with status 1, not continuing to write;
  - against "two different `--meta-dir` serving the same view at once" it is **not enforced**: the lease fences by deployment (`--meta-dir`), by design not across deployments (control-plane definitions/keys are the one cluster-shared copy), so "one view served by one deployment" is an ops convention (§6.3.3);
  - against **clients** it is not enforced: the mountpoint carries no lease or epoch token, fencing only guarantees the old server stops writing, not that an old client won't reconnect to a new server.

#### 6.3.2 Deployment and usage

The minimal usable deployment is "one FDB cluster + one meta-server":

```bash
# 1) 集群先起来，并且自报 available（单机开发集群的最短路径）
fdbcli -C /etc/foundationdb/fdb.cluster --exec 'configure new single ssd'
fdbcli -C /etc/foundationdb/fdb.cluster --exec 'status minimal'   # 期望 The database is available.

# 2) meta-server：启动探针在 bind 之前确认集群可用，再取写者租约
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///etc/foundationdb/fdb.cluster --token $TOK

# 3) 树是显式建的，客户端零变化
$Y view --meta-store fdb:///etc/foundationdb/fdb.cluster create --view alice
$Y --home $BASE mount --meta-server 127.0.0.1:50110 --view alice --mountpoint /mnt/alice
```

After a successful start, stderr has two verifiable lines (§17's keyword table lists them too):

```text
meta-server: fdb cluster up — redundancy single (ssd-2), tolerates 0 zone failure(s), 1 storage server(s), 1 coordinator(s)
meta-server: writer lease HELD for deployment /data/cluster (holder <host>:<pid>:<nonce>, renewed every 3s, expires 10s after the last renewal); …
```

When the cluster is unavailable it **refuses to start** rather than hang waiting: the probe is bounded (`GROVEFS_FDB_OPEN_TIMEOUT_SECS`, default 10s; an unreachable cluster measured returning in 2.07s), the wording carries the cluster's own explanation and names the troubleshooting command:

```text
--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (quorum_not_reachable: Unable to reach a quorum of coordinators.) — …
```

Three environment knobs (process-level, meta-server and `view lease`/`scan` read the same set):

| Variable | Default | Role |
| --- | --- | --- |
| `GROVEFS_FDB_WRITER_LEASE_SECS` | `10` | writer-lease TTL. The ceiling of crash takeover is one TTL; `0` disables the lease (turns off the protection itself, for tests or a certain-single-instance case only) |
| `GROVEFS_FDB_WRITER_WAIT_SECS` | TTL + 2 | the ceiling for waiting at startup on a row still being renewed, refuses past it. Setting it below the TTL would mis-refuse a restart where "a dead process's row hasn't expired yet" |
| `GROVEFS_FDB_OPEN_TIMEOUT_SECS` | `10` | the startup probe's (reading `\xff\xff/status/json`) timeout |

Ops commands:

```bash
# 谁在写、还剩多久：STATE / DEPLOYMENT / HOLDER / EXPIRES
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster lease
# 清掉一个再也不会回来的部署（机器已毁）；活行要 --force 才清
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster \
        lease --release --deployment /data/cluster            # 过期行
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster \
        lease --release --deployment /data/cluster --force    # 活行 = 主动围栏
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster scan   # 租约表是 KIND=lease 的一行，不是 corpse
```

Multi-node and multi-fault-domain: the grovefs side **needs no extra config**, the topology is all done on the cluster side — one fdbserver per machine, each an independent data directory, after `fdbcli --exec 'configure new triple'` use `fault_domains` to assign processes to different fault domains. There is still one meta-server (the same `--meta-dir` may have only one writer); multiple meta-servers hold only under the "each its own `--meta-dir`, each its own views" form, unrelated to the cluster's replica count. Replica count, fault domains, and recovery policy change the cluster, not grovefs's command line.

#### 6.3.3 Known limitations (current version)

- **meta-server is still a single point**: the lease guarantees no dual-write, provides no auto-switch. When the process dies, live mounts get EIO immediately on writes during the service outage (measured 1.5s), and the same mountpoint auto-recovers after a same-address restart; there's no "a second meta-server auto-takeover" mechanism — the lease only guarantees it won't start while the previous is alive. Takeover is by a person (or an orchestrator) restarting on the same `--meta-dir`.
- **Crash takeover waits at most one TTL**: a `kill -9`'d process can't renew itself, a restart waits for the row to expire (default TTL 10s; when the suite compressed the TTL to 2s, four measurements took 1640–1696ms to take over). For faster takeover, reduce the TTL, at the cost that scheduling jitter more readily delays renewal — a lease-losing process exits outright, rather than degrading and continuing to write.
- **A planned restart also waits one TTL**: `meta-server` doesn't catch `SIGTERM`, so after `systemctl restart` the old row still waits to expire before the new process overwrites it (at most one TTL; startup waits `GROVEFS_FDB_WRITER_WAIT_SECS`, default TTL+2, so it won't error, just slow). For zero wait, after the old process has stopped first run `lease --release --deployment <dir> --force` then start.
- **The lease compares expiry with each host's wall clock**: the row stores epoch milliseconds, the process reading it judges by its own clock. Clock drift between hosts near the TTL makes a row expire early or late, deployments must enable NTP.
- **"One view served by one deployment" relies on convention**: the lease fences by deployment, not preventing two different `--meta-dir` from opening the same view and writing each other (they read the cluster's one shared definitions copy, `view list` consistent; the lease doesn't cross deployments).
- **Expired rows are not auto-reclaimed**: a row left by a process reaped via `SIGTERM`/`SIGKILL` (meta-server doesn't catch signals, a normal return runs the release) or by forced fencing stays listed in `view lease` (`STATE=expired`) until a same-deployment restart overwrites it or `--release` clears it; `view scan` counts it as a subspace, the table footer marks it a writer-lease subspace.
- **The startup probe only answers "available/unavailable"**: it doesn't validate schema version, doesn't warm the engine, so the first request may still hit an OCC conflict retry (errno mapping in §12).
- **`view lease` / `view scan` need the cluster reachable**: when unreachable the command fails with the reason, never printing an empty table to pretend "no lease".
- **Clients carry no fencing token**: what's fenced is the server process; an old client reconnecting to a new server works as usual, this is deliberate (the mountpoint shouldn't fail because the server changed hands), but it also means the lease is not an end-to-end consistency barrier.
- **The real shape of a multi-machine deployment (four-machine VM measured)**: four guests each run one `fdbserver` (zone `n1..n4`), the four addresses are **all coordinators at once** (quorum 3). The benefit is any one dying keeps the cluster readable/writable; the cost is **each machine is both coordinator and storage domain** — "losing one" drops a coordinator and a fault domain at once, `status` shows `full_replicas` dropping from 4 to 3 and staying available, rather than dropping one replica (`.bneck/fdb3vm_lease.sh` measured the lease not falsely lost when destroying one while holding it).
- **Admin commands are notably slow when a coordinator is missing**: when coordinators are incomplete, `fdbcli … status json` takes ~5s each time (timeout retries) to return, `meta-server`'s startup probe (`GROVEFS_FDB_OPEN_TIMEOUT_SECS`, default 10s) and `view scan` are both budgeted at this cost; automation scripts should give a single command a ≥15s timeout, or "slow" gets misjudged as "hung".
- **No fdbserver systemd unit on the guests**: the experimental cluster's nodes were brought up manually via `setsid -f fdbserver -p <port> -C <cluster> -d <datadir> -L <logdir>`, a machine restart doesn't bring them back (`systemctl list-units | grep fdb` empty). When the cluster "is there" but nodes didn't return, `status` only reports insufficient replicas without erroring, so after a restart confirm the process and `status minimal` per machine.
- **The capacity surface is still not passed (G3)**: the reason and numbers are at the start of §6.3, this tier is still experimental, not in CANON.

#### 6.3.4 Capacity conversion: how many bytes per row, how many rows one transaction can write

The two numbers can't be mixed, they differ three-fold:

| Account | Each row references a distinct object | No object reference |
| --- | --- | --- |
| **On disk** (the view subspace's key+value summed per key) | **292 B/row** (5 keys/row: `n` name, `d` by id, `h` holders, `r` reverse index, `i` ino pointer; the root row and two allocators counted separately) | **194 B/row** (4 keys/row, no `r`) |
| **One transaction** (FDB's 10 MB transaction state; batched by default, see below) | **≤ 7,700 rows** | **≤ 15,000 rows** |

So the answer to "how many rows of metadata is 10 MB" is **about 34k rows** (each row referencing a distinct object) or **about 52k rows** (no reference), which is the **on-disk** account — use this to compute how big a tree occupies in the cluster. It is **not** how many rows a single `view import`/`fork`/`graft` can move: FDB limits the "transaction state", which besides the key-values written also includes the **ranges that transaction read**, ledger counts and in-memory indexes, measured 2–3× the on-disk bytes. Measured data (every command rerunnable, see the end of this section):

- **On disk**: `t5000` (5,000 rows, one `data_ref` per row) = 25,001 keys / 1,458,714 B ⇒ 292 B/row; `t12000nr` (12,000 rows, no reference) = 48,002 keys / 2,326,965 B ⇒ 194 B/row; the ledger counted separately (the `refs` subspace one row per distinct object, ~55 B, measured 1,001 keys / 55,055 B), so the real occupancy "with reference" is 292 + ~55 ≈ 350 B/row.
- **Single-transaction boundary** (`single ssd` single node, default 10 MB transaction state; batch past it, here the measured boundary of how many rows one transaction writes at a time): `import` with reference **7,700 rows commit, 7,900 rows `2101`**; without reference **15,000 rows commit, 17,000 rows `2101`**; `graft` 6,000 rows (6,000 references) **commits into an empty tree, `2101` into a tree of 10,000 rows** — the same plan's success depends only on how big the target tree is, because `graft`'s same transaction also scans the target tree's entire namespace (id/ino allocator continuation), **the read set is counted into this 10 MB too**.
- **Default automatic batching**: `import`/`fork`/`graft` first price the whole plan by the measured model (`rows×625 B + new-ledger-objects×660 B + rows-read-in-transaction×240 B`), **if it fits it's still one transaction** (a small dump's semantics unchanged); if it doesn't fit it slices by a **70% budget (7,000,000 B)** — with reference **5,447 rows per batch** (7,000,000 / 1,285), without reference 11,200 rows — one commit per batch. Real-cluster measured (single `single ssd`): **20,000-row with-reference `import` 4 transactions, 7.8s** (7,900 rows `2101`); **20,000-row `fork` 4 transactions, 7.2s**; **8,001 entries `graft` into a 20,000-row fork 2 transactions**; **6,002 entries `graft` into a 28,001-row fork 2 transactions** (6,000 rows into a 10,000-row tree gives `2101`); `import --replace` of a 20,000-row tree 4 transactions, 9.6s. Each batch commit prints a line `INFO fdb import/fork/graft: transaction committed … transaction=k of=K`.
- **Batching still holds three things**: ① **the root row written last** — the root row is the sole criterion for "mountable", so a mid-failure leaves "keys no mountpoint can reach" (`view scan` marks `corpse`), not a half-tree a client can read; the error reports "completed X of Y, transaction k/K" and names either a `--replace` rerun or `view rm --name … --base` to clear; `--replace` on a corpse is exactly the **resume entry** (measured: `import` `kill -9`'d at 2/4 transactions, leaving a corpse of 54,472 keys / 10,894 entries, `view scan` marks `corpse`, the same command with `--replace` writes back 20,000 rows in 4 transactions, export line-for-line identical to the source dump, `refs rebuild` `0/0`). ② **the ledger doesn't leak** — `put_row_tx` books once only on "this view's first naming of this object", so batching and cross-batch hardlinks are naturally correct; `--replace`/rerun's unbooking goes through `unbook_refs` (256 references one transaction) then clears the prefix, rather than one transaction touching a million ledger keys (that itself is the same 10 MB wall). ③ **hardlinks across batches preserved** — `graft`'s "source ino → target ino" map is passed as the previous batch's **output** to the next, two names across batches still point at the same inode (measured: 6,002 entries 2 transactions, hardlink pair split across batches, `ino` identical). After five measured rounds `refs rebuild` reports **`0 row(s) corrected, 0 row(s) deleted`**.
- **One entry bigger than a batch can hold**: `graft` recursively splits such a directory into "**write only its own row** (preserving mode/uid/gid/timestamps — otherwise it'd be rebuilt at 0755 and the current euid by `graft_ensure_dir`) + one unit per child", so an 8,000-entry directory can be moved, not refused. If a batched `graft` fails mid-way, **committed batches are kept**, the error directly names that the next command should be a `--skip-existing` resume.

Conversion method and rerun: `view scan` gives each subspace's key count and prefix, `fdbcli -C <cluster> --exec 'getrange "<prefix>" "<prefix>\xff" 200000'`'s line-by-line output restored to byte counts as "1 byte per printable char + 3 chars per `\xNN`"; the recipe for row count and transaction boundary is `view import` of a dump generated by row count (`.fdbtest/gen_dump.py`), binary-searching the line between "commit" and `2101`.

#### 6.3.5 Future evolution directions (not implemented, evaluated in this order)

1. **Takeover automation**: the control plane is in the cluster `ctl` subspace (definitions/keys/audit one shared copy), the lease could move up to view or cluster granularity with a follower — take over on finding a row expired, rather than waiting for a person to restart. Needs to also give clients a fencing token (or use FDB's read version for read consistency), otherwise "old client + new server" is still the weak link.
2. **Capacity promotion**: G3's criteria and rerun recipe in `docs/fdb-keyspace.md` §14 v1.8/§15; the necessary condition is rerunning `.bneck/bench_g3.sh` at RF=3 on SSD/NVMe medium reaching ≥4.6k (SATA) / ≥9k (NVMe) ops/s, and the same-run PG control not below the plateau value. Attribution data shows swapping disks alone isn't enough (`memory` engine 1642 ops/s), the implementation side still has ~3× headroom to address.
3. **Ops closed-loop tightening**: solidify "take a snapshot with `fdbbackup` → restore with `fdbrestore` → verify the mount" into the CLI (currently the `.bneck/fdb_backup_mount.sh` recipe); add explicit reclamation for the lease row (currently relies on a same-deployment restart overwrite or manual `--release`).

---

## 7. Read-only mount

```bash
$Y --home $BASE mount --view alice --mountpoint /mnt/alice_ro --ro
```
Every write operation (create/write/mkdir/unlink/rename/chmod/truncate, etc.) returns `EROFS` (a double guard of kernel `MS_RDONLY` plus a filesystem-side check). Suited to distribution/archive views.

### 7.1 Two boundaries that "look POSIX but aren't" (verified empirically)

- **Permission bits are stored and displayed but not enforced**: `chmod`/`ls -l` faithfully record and echo `node.mode`, but mount admission **does not look at the in-tree mode** — the same uid appending to a `chmod 444` file still returns `rc=0` with the bytes hitting disk (rereading after `echo more >>` includes the new line; measured on a local `--home` mount). The real access control is: server-side token/API key (§6), `--ro` mounts (previous section), mount visibility decided by whether `allow_other` is set, and — in local mode — the OS's file permissions on `blocks/`. **Don't treat `chmod` as an isolation mechanism inside a view**; cross-uid reads being refused is FUSE mount visibility (without `allow_other` other users can't see the mountpoint at all), not the mode taking effect.
- **Sparse holes don't save space**: the backend stores bytes as a full object, so `truncate -s 1M` extending a hole actually occupies the full 1 MiB (`stat` echoes `size=1048576 blocks=2048`, i.e. `st_blocks*512 == st_size`, no hole). Don't expect sparse image files (qcow2, etc.) to use less network/disk; to save space, compress or transpose the format first, then import.

Both are current-version design trade-offs (the object-storage model doesn't maintain extents, and admission doesn't go through the POSIX mode-check path); they're listed as standing observations (§5-1/§5-2) and don't affect data integrity.

---

## 8. Performance knobs

- `--threads N` (default 4): FUSE concurrent-dispatch worker threads, each with its own `/dev/fuse` fd (`clone_fd`, Linux 4.5+). `0` = single-thread mode (usable as an A/B baseline). On kernels below 4.5 it automatically falls back to a single worker thread (no hard failure).
- Large client IO is auto-chunked and parallelized: reads ≥1MiB are sliced into 1MiB chunks and run concurrently, writes ≥4MiB are sharded and run concurrently (no protocol change).
- **Small-file batch creation (group commit + read pool matching worker count)**: `create` calls in the same parent directory are serialized by the kernel's parent-inode lock, so "multithreaded file creation" can only parallelize across **different parent directories** (`cp -r`/`tar -x` unfolding a multi-level subtree is exactly this shape, and it's the only shape with a parallel payoff). Two serialization points are shaved here: ① **group commit** — `CREATE` is first enqueued, and the worker that wins the leader flag commits the whole queue with one `BEGIN IMMEDIATE` (`MetaProvider::create_nodes_batch`, SQLite engine; a lone create becomes its own leader with batch length 1, so single-threaded pays no extra latency), and a duplicate name within a batch only voids that one entry (returning EEXIST itself), not its siblings; ② **read pool as wide as dispatch** — a local mount sizes the SQLite warm-read pool to the `--threads` value (see next item, `GROVEFS_SQLITE_READ_POOL` can still override), otherwise a dozen-plus concurrent readers mostly have to `open` a connection on the spot. Same mountpoint, one parent dir per thread, 128 B files, `--threads 16`, creation rate (files/s, range over multiple reruns): T=1 **1.7k** / T=2 **3.0–3.2k** / T=4 **5.0–5.1k** / T=8 **5.4–5.9k** / T=16 **6.3–7.0k** (another shape — "create only, no write" via `touch` — see the next item; it doesn't pay the object-creation cost on the data plane). Each serialization point can be reproduced and measured: forcing the read pool to one connection (`GROVEFS_SQLITE_READ_POOL=1`) drops the same workload to T=8 **~1.4k** / T=16 **~1.2k**, i.e. every excess concurrent read is opening a connection on the spot; merging two parent dirs into one (back to VFS serialization) leaves each thread at ~1.9k files/s — **that's not the filesystem being slow, it's the VFS's shape**. Count evidence (3200 files, 8 threads, `--stats`): `meta.create` commits only **1.0–1.1k times** (average batch ~3), `meta.setattr` 3200 times but p50 only **50µs** — the kernel `WRITEBACK_CACHE` backfills size and mtime at close, and when both equal the value already in the DB (same length, same epoch second) it neither ftruncates, touches, nor writes to the DB, so none of these 3200 is a write transaction; `write.touch_mtime` p50 = 0 (the new file's creation second is already recorded, the first write in the same second doesn't touch again; mtime has only epoch-**second** precision, rewriting the same value within the second only amplifies). One cost on this path that isn't a serialization point has also been removed: the fixed SQL for CREATE/UNLINK/ledger now lives on a resident connection (`prepare_cached`) instead of parsing each row on the spot — engine-level microbenchmark (same transaction, same batch of statements, N=1200): a single create **83µs/row**, group of 3 **38µs/row**, group of 8 **25µs/row**; the delete side takes the same path: single-threaded `rm` of small files **3.1–3.3k files/s** (`meta.unlink` p50 140µs).
- **Delete group commit**: `unlink`/`rmdir` are enqueued the same way as `create`, and the leader worker deletes the whole queue with one `BEGIN IMMEDIATE` (`MetaProvider::unlink_nodes_batch`, SQLite engine; a lone delete becomes its own leader with batch length 1, so single-threaded pays no extra latency); within a batch, "name doesn't exist" and "directory not empty" only void that one entry, not its siblings; byte reclamation (`backend.remove`) is still done by each worker, so only the commit is merged. Same mountpoint, one parent dir per thread: parallel deletion of 3200 files commits only **1.1–1.8k transactions** (average batch 1.8–2.8), T=8 **6.2–8.6k** files/s, T=16 **11.8–12.2k** files/s, single-threaded unchanged (batch always 1). Engine-level microbenchmark (delete N rows in one transaction, N=2400): batch 1/2/4/8/16 = **53.9 / 30.0 / 18.7 / 13.2 / 10.8 µs/row**.
- **Where one `create` actually goes (SQLite tier, broken down by `--stats`)**: single-machine release, same mountpoint, N=2000: `create` end-to-end **0.177ms/op** (5,660 ops/s, `meta.create` p50 86µs / mean 95µs); under the same conditions `mkdir` **0.162ms** (p50 65µs), `utime` **0.142ms** (`meta.setattr` p50 128µs). **`create` doesn't touch the data plane**: the name lands in the DB and returns; the backend object is opened on first `write`/`truncate` (their respective `O_CREAT`), so "create only, no write" (`touch`, an empty file tree `cp -r`) neither pays an open/close nor occupies a cache fd — `file − mkdir ≈ 15µs` leaves only the one INSERT for data_ref and the ledger, `mkdir − utime ≈ 20µs` is the "INSERT costs more than one UPDATE" part, and the remaining ~100µs is FUSE dispatch plus kernel `LOOKUP` (`mkdir` end-to-end 0.162ms minus its `meta.create` p50 65µs, i.e. the §8.2 floor). The semantic invariants are unchanged: a never-written file's `getattr`/`read` are both 0 bytes (a nonexistent object **is** an empty file), `fsync` succeeds, `unlink` only deletes the name (nothing in the ledger to reclaim), `du` counts 0 bytes, `gc` has no orphan to clean. The failure surface is therefore cleaner too: when `data-server` is unreachable `touch` still succeeds (the name only crosses the metadata plane), and the first op that actually crosses the data plane reports the error — `fsync` gets `EIO` (the `resilience` suite asserts both halves; a write without `fsync` already sits in the page cache, so the application side can't see this failure anyway). The "create only, no write" concurrency curve on the same binary (one parent dir per thread, `--threads 16`, files/s): T=1 **3.3k** / T=2 **4.5k** / T=4 **7.7k** / T=8 **12.2–12.6k** / T=16 **13.3–13.9k**. **SQL statements aren't the big cost; commits and fsync on the volume are**: same volume, same pragma, raw SQLite doing "one INSERT + one ledger INSERT + one COMMIT" (`.build-bin/probe_wal_modes.sh`, N=500 per tier, `/data` is xfs, libsqlite3 3.31.1) — `WAL+NORMAL` per commit **p50 46µs / mean 404µs** (no fsync in the median; the mean and `meta.create`'s occasional 59ms tail spike are both contributed by the periodic checkpoint's fsync, 2,470 commits/s), `synchronous=OFF` **p50 30µs / mean 55µs** (18,159/s), `synchronous=FULL` **p50 16.7ms** (fsync every commit, 53/s), `journal_mode=DELETE+NORMAL` **p50 20.5ms** (37/s); the same code on tmpfs collapses entirely to **~26µs** (37k/s) — fsync is that order-of-magnitude gap. So this tier **provides no durability-lowering knob like `synchronous=OFF`**: it can be 3–7× faster, but the price is "a commit may still roll back after returning" rather than "lose only the most recent few commits", inconsistent with the durability standard of the other tiers in this section. To go faster, change the medium or the backend (PG write parallelism, see §6.2), not the durability. Both scripts are rerunnable: `.build-bin/probe_sqlite_create_breakdown.sh`, `.build-bin/probe_wal_modes.sh`.
- **Metadata read parallelism** (SQLite writes don't parallelize; PG write parallelism, see §6.2): besides the single write connection, each view's `MetaEngine` opens a pool of warm-read connections (**local mount = `--threads` (default 4); library-embedded/meta-server defaults `READ_POOL_SIZE = 1`; `GROVEFS_SQLITE_READ_POOL` overrides, 0–16, opened on demand when the pool is empty and returned after use**), so `lookup`/`getattr`/`readdir` and nlink resolution go through the pool, and concurrent reads on the same view needn't queue behind the write lock; SQLite **writes still serialize on the same authoritative connection** (single-writer semantics unchanged), PG writes take a separate SERIALIZABLE write pool (§6.2). Pool connections read committed data under a WAL read transaction, and their own writes are immediately visible (`cargo test`'s `reads_through_the_pool_observe_committed_writes` pins this). Engines opened on HA standbys/read-only don't pre-open a pool; they establish on first read on demand and return it, equally bounded. The PG engine (`PgEngine`) likewise holds a peer `PG_READ_POOL_SIZE = 4` read-only connection pool (`GROVEFS_PG_READ_POOL`, reads committed data via PG transactions, falling back to the write connection when no read connection is available so failures surface truthfully), and each read connection caches its own prepared statements (≤ 16 per connection: lookup/getattr/two readdir pages/lock probe/epoch read), no longer sending an unnamed statement on every read for PG to re-parse and re-plan — measured on the same mountpoint under `--no-cache`, consecutive `stat` went 0.820ms → 0.528ms/op (−36% latency, +55% throughput, stable across three reruns); the PG write side is separately parallelized by a `PG_WRITE_POOL_SIZE = 8` SERIALIZABLE write pool (§6.2).
- **PG commit durability tier** (`GROVEFS_PG_SYNC_COMMIT=off`, default `on`): the PG write connection runs `SET synchronous_commit = off` at session level, so COMMIT no longer waits for local WAL to hit disk when it returns. Same-rack comparison (`bench_pg_conc.sh`, N∈{1,4,8}, T=8, 6s, write-statement cache already in place): **3,305 / 5,225 / 4,284 → 3,194 / 6,142 / 4,180 ops/s** — the only stable gain is **+18% at N=4 (write pool saturated)**, while N=1/N=8 fall within single-run jitter (−3% / −2%; an earlier round showed the same pattern with N=4 winning +29% alone). The price is that **a crash may lose the most recent few committed transactions**, and with `synchronous_standby_names` configured it also stops waiting for standby acknowledgment — deployments promising RPO=0 must keep the default `on`. SSI isolation, the cross-view ledger, governance invariants, and `--sync-repl` snapshot forwarding are all unaffected (the last is mutually exclusive with the PG backend anyway); under the default tier `test_pg.sh` passes 24/24, and under `GROVEFS_PG_SYNC_COMMIT=off` too, 24/24.
- **Directory listing pagination + READDIRPLUS** (in effect without any switch): `MetaProvider::readdir_page(parent, offset, limit)` is pushed down into a SQL page in both the SQLite and PG engines, and the FUSE callback fills the reply buffer in geometric blocks (READDIRPLUS 32→1024, plain READDIR 128→1024), so one kernel READDIR/READDIRPLUS request pays `O(buffer)` rather than `O(whole directory)`; attributes are carried back once alongside the directory entries (the `FUSE_DO_READDIRPLUS` of kernel ≥4.6, a separate all-or-nothing negotiation group from the capability bits above; unsupported kernels naturally fall back to plain READDIR). Three key implementation points: ① **keyset resumption** — the kernel resumes at "where the reply buffer fills", and the offset doesn't line up with our page boundary, so every 32 rows a cursor `(parent, n) -> name of the (n-1)th row` is recorded: any resumption first locates with `WHERE parent_id=? AND name>? ORDER BY name LIMIT n`, then discards in-memory any prefix shorter than 32 rows; OFFSET is only a fallback when there's no cursor (seekdir, cursor cleared), and any name change (create/unlink/rename/link) immediately clears the cursor, never returning the old order. ② **query plan pinned** — every listing SQL carries an always-true `name>''` (a name can never be empty); its only job is to pin the plan onto `UNIQUE(parent_id,name)`, otherwise SQLite falls back to the `(parent_id,is_dir)` prefix index + a temp B-tree sort, 13–55ms per OFFSET page. ③ **directory nlink via covering index** — `getattr` and READDIRPLUS's `.`/`..` must compute a directory's nlink (`2 + subdirectory count`), done via a pure index scan on `idx_node_parent_kind(parent_id, is_dir)`, 100k sub-items in **0.02ms** (per-row table lookup would be 9.6ms each, and it's computed for every reply). Measured (single-machine release, same machine same disk): a directory of 10,002 entries, `ls -f` base view **131ms**, 100-rule derived **105ms**, `ls -l` **162ms**; a directory of 100,002 entries, `ls -f` base view **1.19s**, fork **1.19s**, 100-rule derived **1.01s** (reread within 2s **0.12s**). A 5001-entry directory `ls -l` local **2.56s** (`--no-readdirplus`, leaving only pagination, 5.22s); remote via `M_READDIRP` same directory **2.29s** (leaving only pagination, 4.22s), output byte-for-byte identical.
- **Scope** (same basis as the previous item): the read-only connection pool and SQL pagination exist on both storage engines — the SQLite `MetaEngine` and the PG `PgEngine` (the former sized per the previous item, the latter 4 read connections each caching its own prepared statements, both tunable via `GROVEFS_SQLITE_READ_POOL` / `GROVEFS_PG_READ_POOL`). Local `--home` (SQLite or PG) gets the full pagination and PLUS; `meta-server`'s concurrent RPC handlers each use their backend's read pool, and the `M_READDIRP` frame pushes pagination down to the server side, so remote clients take one page per buffer and negotiate PLUS (remote measured on the same dataset, 2.29s). The `LayoutMeta` of `--layout`/derived views also overrides `readdir_page`: it **materializes the "rule-pruned shape" per parent directory** (up to 4 directories / 400k rows, TTL 2s), cutting one slice per kernel buffer — so 100k + 100 rules takes 1.01s first, 0.12s reread within 2s; large directories over 400k rows are still computed per buffer (correct, just not cached). Only custom providers that don't override `readdir_page` still go through the trait default (a full listing per buffer, no PLUS negotiated). Metadata **write** parallelism exists only on the PG backend (a SERIALIZABLE write pool within a single cluster, see §6.2); SQLite writes still serialize; multi-authority across nodes isn't done, and **multi-PG-instance sharding** under one authority is only a prototype (`--meta-store a,b` routing by view, boundaries in §6.2). The criteria are pinned by `cargo test`'s `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` and `readdir_page_resumes_at_arbitrary_offsets_and_drops_stale_checkpoints`.
- `--no-readdirplus`: closes the READDIRPLUS negotiation, forcing a fallback to plain READDIR + per-entry `LOOKUP`, for A/B benchmark troubleshooting (this is already the behavior when the kernel doesn't recognize the capability; the switch only forces it).
- Extra fuser options such as `--o allow_other` are passed through to the mount.

### 8.1 Per-stage latency telemetry (opt-in, for bottleneck hunting)

Adding `--stats` at mount time enables **per-stage latency telemetry**: it records the time spent in each stage of a read/write/metadata operation into separate HDR histograms, aggregating p50/p90/p99/max offline to **locate which stage the bottleneck is in** (key resolution vs. data backend vs. remote TCP round-trip vs. stripe fan-out vs. SQLite/PG query vs. SSI retry…), instead of only seeing one total wall clock. This is **a white-box diagnostic facility, not a throughput/scalability claim** — when off, each instrumentation point is skipped by a single relaxed atomic read (no clock, no lock, no allocation), with no measurable effect on the hot path.

```bash
# 卸载时把汇总报告打到 stderr(挂载进程 stderr)
$Y mount --view alice -m /mnt/alice --stats
# 额外周期性 + 退出时原子落盘到文件(CI/长时间采样)
$Y mount --view alice -m /mnt/alice --stats --stats-file /var/log/grovefs.stats --stats-interval 10
# 或不改命令行,用环境开关(GROVEFS_STATS=1)
GROVEFS_STATS=1 $Y mount --view alice -m /mnt/alice
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
| Backend capability layer (server config) | `--data-servers A1,A2[,A3…]` node distribution, ≥`--stripe-bytes` (default 16MiB) auto-striping, `--meta-store postgres://…` | Changes capacity and parallelism (§5.2, §6.2) |
| Structural (code change) | swap the `MetaProvider`/`StorageBackend` implementation (§16.1) | Only reach here when the layers above aren't enough |

**Minimum loop**: ① locate the symptom — mount with `--stats`, run a real workload, read the report: `[io]` for the backend_total/pool_wait share, `[meta]` for each op's p50/p99 and ops/s, `[counters]` for parallel-slice and readdir-prefetch counts; ② judge the bottleneck — backend_total/pool_wait dominating = network RTT or the backend; `meta.create` p50 high and ops/s not rising = SQLite single-writer serialization; a high `dup>0` share = kernel cache not being hit (consider `--cache-ttl`); ③ turn only one knob, restart the mount to take effect (remote mount: systemd `daemon-reload` + restart; local mount: just change env/config); ④ compare two windows, gain claims based on same-machine same-workload measurement — everything given in the manual is a floor line, not a gain number.

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

The full board is **34 stages** in all: `build` → `unit` → **31 suites** → `bench`. **The first 33 stages gate GREEN; `bench` only prints throughput reference numbers and never gates** (for pure functional regression only, use `SKIP_BENCH=1`). `pg`/`pg_parallel`/`control` auto-SKIP on hosts with no reachable PostgreSQL, not lowering GREEN. `fdb` **is not among these 34 stages**: it's an experimental tier (§6.3), run only by name via `bash .build-bin/run_all.sh fdb`, and whatever it prints doesn't affect the default board's color; when run explicitly, a host with no reachable FoundationDB cluster SKIPs the whole suite, and setting `FDB_TEST=1` makes a missing cluster a FAIL (a backend never run must not be counted as passing).

```bash
export LC_ALL=C LANG=C                                 # 仓库根目录下执行(PKG_CONFIG 不必设:无 crate 查 pkg-config,§1)
bash .build-bin/run_all.sh                # 全板 34 段(33 门控 + bench)
                                          #   build unit
                                          #   posix edge e2e remote errors resilience concurrent
                                          #   metasrv ha striping locks freshness disp_fresh
                                          #   layout fork viewrm gc bwlimit migrate import
                                          #   govcheck govtime grovesci mcp apikey mountkey
                                          #   config pg pg_parallel evict control
                                          #   bench(只打吞吐,不门控)
bash .build-bin/run_all.sh fdb            # 实验档 FoundationDB,不在 CANON(§6.3)
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 同上,去掉 bench
bash .build-bin/run_all.sh fork mcp       # 只跑指定套件(build 自动前置)
```

The end-to-end demo script (view isolation + real-time invalidation push) is `.build-bin/test_metasrv.sh`.

### 10.1 Three-engine completeness matrix and cross-machine specials (not in the default board)

The default board's `pg`/`pg_parallel`/`control` can only answer "can this tier mount, read, and write"; they can't answer "is the same task covered on all three backends". The latter is given by the five scripts below, all rerunnable and all cleaning up after themselves:

| Script | What it measures | Latest measurement |
| --- | --- | --- |
| `bash .bneck/engine_matrix.sh` | Runs **the same governance sequence** once each on SQLite/PG/FDB, arranging 22 capabilities side by side into one table: OK=works, REFUSED=by-design unsupported (quotes the refusal wording), ERROR=gap. It runs the **no-FUSE** pure governance plane, so `graft`'s source is a hand-written seed dump (empty `data_ref`, measuring only tree movement); the bytes-and-ledger path belongs to `forksrv`/`xfer` | All 22 rows OK/REFUSED, **no ERROR**; the trailing `post-teardown` check verifies `sqlite tree files 0 / pg schema and ledger back to baseline / fdb tree subspace 0` → `TEARDOWN CLEAN` |
| `bash .bneck/pg_view_teardown.sh` | PG base-view transactional deletion + bidirectional ledger repair, throughout with real FUSE mounts, real data-server, real PG | 44 pass / 0 fail (including `schema dropped with 6 name(s), 4 object(s) no longer held by any view`, `3 row(s) deleted`, `2 row(s) corrected`, second deletion refused) |
| `bash .build-bin/test_pg.sh` / `test_pg_parallel.sh` | PG-tier e2e (concurrency, eviction, control plane); ends with `view rm --base` to self-clean | 24 / 41 assertions all pass; leaves no **this-view** schema or ledger rows after running (on a shared cluster, orphans other suites left behind no longer make this assertion falsely red) |
| `bash .bneck/fdb3vm_lease.sh` | Cross-machine lease on a four-node VM cluster: same `--meta-dir` refused / different dirs parallel / destroy one node while holding the lease / holder vs. observer clock skew of TTL/2 | 59 pass / 0 fail |
| `bash .build-bin/test_fdb.sh` (rerun cross-machine on the guest) | FDB-tier 275 assertions (incl. §10d fork/graft subspace copy, §12 bidirectional migration, §16 lease, §13 eviction semantics) | 275 OK / 0 fail |

> **PG cross-machine deployment has also been measured** (a one-off isolation probe, not in the default board): an independent `initdb`'s PG placed on the HOST (`192.168.122.1:5439`, `listen_addresses` only that subnet, not touching the shared loopback `5433`), guest `gfs1` (`192.168.122.124`) running `meta-server --meta-store postgres://…@192.168.122.1:5439/grovefs` + `data-server` + a client FUSE mount. Evidence: HOST-side `pg_stat_activity` shows 20 resident connections from the guest IP `192.168.122.124` (per view 8 write + 4 read warm pool + control plane/ledger), and the 4 `node` rows (`/`, `hello.txt`, `sub`, `sub/n.txt`) plus 2 `grovefs_refs.holders` holders written by the guest mount all land in the HOST's PG, with write/read/`mkdir` all passing and bytes staying on the guest's `data-server`. So "the metadata DB on another machine" holds for PG, belonging to cross-machine deployment just like FDB's four-node cluster. The probe script, isolated DB, and guest-side artifacts have all been cleaned up; the shared `5433`/`5434` were unaffected.

The matrix's conclusion (also the answer to "is completeness met") — 22 rows, **zero ERROR** across all three tiers:

| Capability | SQLite | PG | FDB |
| --- | --- | --- | --- |
| `view create` (build base tree) | OK | OK | OK |
| `view define` (server-side derived lens) | OK | OK | OK |
| `view alias` (pure alias) | OK | OK | OK |
| `view fork` (independent metadata tree) | OK | OK | OK |
| `view list` (definitions + bare base views) | OK | OK | OK |
| `view export` (metadata dump) | OK | OK | OK |
| `view import` (dump → tree) | OK | OK | OK |
| `view import --as-fork` (dump → fork, keeping fork semantics) | OK | OK | OK |
| `view import` (hand-written seed dump, empty `data_ref`) | OK | OK | OK |
| `grovefs import` (directory → view, zero-copy) | OK | REFUSED | REFUSED |
| `view graft` (pick a subtree onto another tree) | OK | OK | OK |
| `refs rebuild` (cross-view ledger recompute) | OK | OK | OK |
| `gc --data-dir …` (in-place scan of local `blocks/`) | OK | REFUSED | REFUSED |
| `gc --data-servers …` (enumerate orphans via data-server, incl. `--apply`) | OK | OK | OK |
| `du` (byte total, tree from backend, bytes asked of data-server) | OK | OK | OK |
| `du --view <nonexistent name>` (no-tree guard) | REFUSED | REFUSED | REFUSED |
| `apikey list` (credential register) | OK | OK | OK |
| `apikey calls` (MCP call audit) | OK | OK | OK |
| `view scan` (cluster inventory) | REFUSED | REFUSED | OK |
| `view lease` (writer lease row) | REFUSED | REFUSED | OK |
| native HA (`--role standby`) | OK | REFUSED | REFUSED |
| `view rm --base` (tear down the tree with its bytes) | OK | OK | OK |

The two `apikey`/`audit` rows' "OK" are **the same two commands, three destinations**: SQLite writes `<meta-dir>/meta/api_keys.db`, PG writes the `grovefs_control` schema, FDB writes the cluster's `ctl` subspace; the `du`/`dumiss` trees likewise come from these three places (bytes uniformly asked of the data-server).

> **This table answers "can it run", not "how to write it"**: the command shapes for the same task across the three tiers, and the six things only one tier can do, see §4.1.3; why to pick a given tier (write throughput / transaction cap / availability / scale) see §4.1.1.

The remaining REFUSEDs fall into two shapes, both **design boundaries** rather than gaps, each with a "why" and an "alternative" (§15.9 written out row by row):

- **Two verbs that run on the local data root**: `grovefs import` (zero-copy onboarding of a directory) and `gc --data-dir` (in-place scan of `<data-dir>/blocks/`) inherently depend on "tree and bytes on the same machine". In PG/FDB deployments the bytes belong to the data-server, the corresponding form is the remote `gc --data-servers` (OK on all three tiers), and directory onboarding goes through the mountpoint via `rsync`/`cp` or the three steps "local import → `view export` → `view import`".
- **Backend-specific ops planes**: `view scan`/`view lease` inventory the FDB cluster's subspaces and writer leases (SQLite/PG have no such concept), and native HA's snapshot forwarding is specific to the SQLite-file form (PG/FDB each have their own HA toolchain).

`du --view <nonexistent name>`'s REFUSED is not a gap but **an alignment added this round**: previously the same misspelled name — SQLite reported `sqlite open …: No such file or directory`, PG reported `pg query: db error` (`42P01` folded into one sentence), FDB simply returned `0B` — indistinguishable from "a genuinely empty view". Now all three tiers give the same sentence (the matrix's `dumiss` row pins the wording):

```
error: no tree for view 'mxnone' in <store> — `grovefs view create --view mxnone` makes one,
       and `grovefs view list` shows the names that exist
```

What all three tiers share is **control plane + tree + ledger + base-view teardown + fork/graft + export/import + remote gc + du + apikey/audit** — that is, at minimum what a metadata engine must have (the `forksrv`/`xfer`/`gcremote` e2e scripts assert per tier, with `xfer` covering `--as-fork`/`--replace`). How many rows fit in one transaction, how many metadata rows 10 MB equals, see §6.3.4 — that's the "capacity" dimension's basis, orthogonal to the "capability" dimension here.

> The matrix needs `LD_LIBRARY_PATH` to find `libfdb_c` (the script tries `.fdbbin/lib` and `target/fdb/release/lib` itself; if neither is found the whole fdb column reports a library error). Before the matrix runs, confirm there are no leftover grovefs mounts/services on the machine, otherwise the PG column's schema count will include the concurrent run. The script `unset GROVEFS_META_STORE` itself (passing store explicitly per command per arm), so it runs the same from any shell; it also needs PG on 5433 and `fdb:///data/grovefs/.fdb3vm/cluster3vm` (`PGSTORE`/`FDBSTORE` can override). The `/tmp/engmx/*.log` after running are kept for per-cell wording.

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

Data-disk requirement: the filesystem that `data-server --data-dir` (in local mode `--home`) sits on must have real capacity — all file bytes land as objects under its `blocks/`, so **don't** build the data root on tmpfs.

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
| center `10.0.0.5` | `meta-server` | `--meta-dir /data/cluster` (authoritative views at `/data/cluster/meta/<view>.db`), listening `0.0.0.0:50110` |
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
ExecStart=/usr/local/bin/grovefs mount --view %i --foreground --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token ${GROVE_TOKEN} -m /mnt/%i
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

All passing means the deployment is complete. To run the full regression (33 gated stages) see §10 and §12.7.

### 11.8 HA primary/standby topology (modified from §11.4)

Key point: the two meta-servers use **their own separate `--meta-dir`s** (snapshot replication, not a shared disk! It's `blocks/` that's shared, clients' `--data-servers` always points at the same set of data-servers):

```bash
# nodeA(10.0.0.5):primary
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --token $TOK
# nodeB(10.0.0.6):standby(同样写成 systemd unit,只改 --meta-dir/--listen/--role/--peer)
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --token $TOK
# 客户端把故障转移列表写进 --meta-server:
$Y mount --view alice --meta-server 10.0.0.5:50110,10.0.0.6:50111 --data-servers 10.0.0.5:50100 --token $TOK -m /mnt/alice
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
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store postgres://grove:pw@10.0.0.9:5432/grovefs --token $TOK
```

Client commands are unchanged (see §4.1: clients don't install PG, don't hold a connection string, don't sense the backend). Each view's `grovefs_v_<hex(view)>` schema must first be explicitly created via `view --meta-store … create` (a mount only opens an existing schema, never creating it in place), no pre-migration script needed; the control plane follows `--meta-store` into PG too: keys and audit land in `grovefs_control.api_keys`/`mcp_calls`, view definitions in `grovefs_control.view_defs`, and `grovefs view`/`apikey`/`mcp` with the same `--meta-store` (or env `GROVEFS_META_STORE`) connect to the same record, writing no control-plane files locally (the `control` suite proves it section by section). Backup/HA move to PG's own toolchain (one `pg_dump` covers tree + locks + control plane together; `pg_basebackup`/streaming replication/PITR), see §12.3. For the honest boundary on why to/not to go with PG, see §4.1. **For inspection SQL, backup/restore, view deletion, and the PG capability boundaries (incl. measured refusal wording), see §15.6–§15.9.**

### 11.10 FDB experimental-tier rerun recipe (not a deployment commitment; status in §6.3)

> The commands below are **how to bring this tier up** (for experimentation and re-testing), not a recommended deployment. The capacity gate has been judged as not passing, and the supported metadata backends remain SQLite and PostgreSQL (§11.9):

```bash
# 0) 不构建、不装包的跳板:直接用仓库里那份自足投放包(§6.3 「投放面」)
#    dist/grovefs-fdb-experimental-linux-x86_64/:grovefs + lib/libfdb_c.so + fdb/{fdbserver,fdbcli,fdbmonitor}
#    下面 1)–5) 的每一步都能只靠包内二进制完成(包 README 有逐条命令);本配方余下部分走源码树

# 1) 先让集群可达(单节点开发集群可用用户态目录,无需 root/系统包):
bash .build-bin/fdb_env.sh up            # 起 fdbserver、生成集群文件、导出 GROVEFS_FDB_CLUSTER_FILE
fdbcli -C "$GROVEFS_FDB_CLUSTER_FILE" --exec 'status minimal'   # → The database is available.

# 2) 装一个带 feature 的服务端二进制(客户端不需要)。libfdb_c 不必装包:
#    第 1 步那个用户态目录里有 .so,指给构建期即可(绑定已预生成,不需 libclang)
FDB_CLIENT_LIB_PATH=/path/to/lib cargo build --release --features fdb
#    跑起来不再需要 LD_LIBRARY_PATH——RUNPATH 里已写进 $ORIGIN/lib 与该目录

# 3) meta-server 只多一个参数(与 --role standby/--peer/--witness 互斥,同时给会启动即 fail-fast):
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///path/to/fdb.cluster --token $TOK

# 4) 每个要用的视图先开一个 subspace(挂载不建树):
$Y view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster create --view team

# 5) 把一棵已有的 SQLite 树搬进集群(不新建工具;目标名不必先 view create,导入即开 subspace):
$Y view --meta-dir /data/sqlite-store export --view team --out /tmp/team.jsonl
rsync -a -H /data/sqlite-store/blocks/ /data/cluster/blocks/
$Y view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster import --view team --in /tmp/team.jsonl
```

To confirm it's in place, look at the second banner line of the meta-server log: `meta-server: metadata backend foundationdb — one subspace per view in FoundationDB (cluster file …); <base> holds no trees`. Client commands are exactly as in §11.5; the control plane (view definitions/aliases/API keys/audit) is all in the cluster's `ctl` subspace, covered by `fdbbackup` in one go (tree + control plane in the same snapshot), and `--meta-dir` is only the deployment identity plus scratch/config, with no authoritative record to back up separately. The data-server has no parameter change, bytes still stored as `blocks/` objects. For FDB command boundaries see §15.9. Step 4's `view create` and step 5's `view import` are this tier's only two tree-building entry points (a mount builds no tree); running step 5 in reverse is the rollback.

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

# 元数据后端可达性:受支持档只看下面的日志横幅行(它能直接读出树在哪)。
# 元数据面是否在集群上:meta-server 日志的 backend 横幅行(下表)比任何本地目录都可靠
journalctl -u grove-meta-server | grep -E 'metadata backend'
```

Log keyword → meaning → action (strings per the source eprintln):

| Log keyword | Meaning | Action |
| --- | --- | --- |
| `meta sync-repl: ... degraded to async for this op` | `--sync-repl` standby unreachable / ACK timeout, that write's RPO falls back to ~500ms (honest degradation, not a false 0) | Check standby liveness / network; if persistent, either fix the standby or accept async RPO |
| `meta HA: promoted to primary at epoch N` | standby promotion succeeded | Confirm the old primary is stopped / will demote, investigate afterward why the primary was lost |
| `meta HA: demoted to standby (peer epoch N)` / `(a quorum ... holds ... > our ...)` | Found a peer/quorum holding a higher epoch, self-demoted | Normal convergence; if frequent on both sides, check network jitter |
| `meta HA: cannot form lease quorum ... fail-safe self-demote` | witness majority unreachable past the grace (3s), **write freeze** (fail-safe, prefer halting writes to double-writing) | Expected protection, not a fault: restoring arbitrator reachability auto-unfreezes |
| `meta-server: evicted N idle engine(s) (>Xs idle, no sessions)` | idle-engine reclamation (`--engine-idle-secs`, §6.2): N views with no mount session and quiet past the threshold released their engines (PG connections / file handles) | Normal reclamation noise, no action; to keep a view hot, shorten the threshold or confirm sessions are gone; `0` disables eviction (resident model) |
| `meta HA: promotion refused/deferred` | promotion denied (peer holds a higher token or the predecessor lease is still alive) | Normal: never two primaries; if both nodes refuse to promote, check the witness majority |
| `invalidation listener stopped: token rejected` | the client invalidation-push channel was rejected by token (the center token changed) | Restart the affected grove-mount@ instance with the new token |
| `meta-server: metadata backend foundationdb — one subspace per view in FoundationDB (cluster file …); <base> holds no trees` | tree and control plane are both in the cluster, `--meta-dir` holds only logs and scratch (§6.3) | Normal. When hunting "where did my tree go", read this line first: if it says `one db file per view under <base>/meta/`, then `--meta-store` didn't take effect and this server is reading local SQLite trees |
| `meta-server: fdb cluster up — redundancy …, N storage server(s), M coordinator(s)` | the cluster topology read by the startup probe (reads `\xff\xff/status/json`, printed before bind) | Normal. If it disagrees with expectations (a single-node dev cluster showing `single`, `0 zone failure(s)`) the cluster isn't multi-replica configured; read this line before troubleshooting, no need to log into the cluster first |
| `meta-server: writer lease HELD for deployment <dir> (holder …, renewed every Ns, expires Ns after the last renewal); …` | this `--meta-dir`'s writer lease is acquired, renewal cadence and TTL on the same line (§6.3.2) | Normal. A second instance on the same `--meta-dir` is refused after one TTL (§15.9); after `kill -9` a restart waits for the old row to expire before taking over |
| `meta-server: writer lease DISABLED (GROVEFS_FDB_WRITER_LEASE_SECS=0); …` | the lease was deliberately disabled, no longer preventing a second writer in the same dir | Only for testing or a confirmed-single-instance context; remove the variable in production |
| `meta-server: LOST the FDB writer lease for '…' (another server took it over); stopping instead of writing beside it` | renewal found the row belongs to someone else (fenced by `--force`, or taken over after this process paused past its TTL) | The process exiting with 1 is deliberate, not a crash; after confirming no two servers share the directory, restart in place |
| `meta-server: writer lease released for '…'` | on clean shutdown the lease is returned before closing the network | Normal noise, no action |
| `--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (…)` | the startup probe failed within bounds, the server refuses to start rather than hanging | Check the cluster: `fdbcli -C <cluster file> --exec 'status minimal'`; too few coordinators, look at `quorum_not_reachable` |
| `names the FoundationDB backend, but this binary was built without \`--features fdb\`` | the server binary lacks the feature (not linked to `libfdb_c`) yet was given `fdb://…` | Expected refusal, not degradation: rebuild with `--features fdb`, or drop `--meta-store` back to the default SQLite (§6.3) |
| `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness` | trying to serve an FDB deployment with grovefs's own primary/standby | Remove the HA parameters: replication and recovery are the cluster's job (`fdbbackup`), §6.3/§15.9 |
| `fdb:// uri names no cluster file` | the URI has no path and `$GROVEFS_FDB_CLUSTER_FILE` is unset either | Give `fdb://<cluster file>` or `fdb://?cluster=<path>` (§6.3) |
| `meta-server: evicted N idle engine(s) (>Xs idle, no sessions)` (FDB deployment) | what's reclaimed are **in-process** engines (id ranges / cursors / stamps), no connection to close | Normal reclamation noise; on this tier the threshold is just a memory cap (§6.3) |

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
| Data objects | `blocks/` under the data-server `--data-dir` (local mode: `<home>/blocks/`) | A plain directory, back up directly with `rsync -a`/filesystem snapshot; if you ever used `grovefs import` (§3.3), some objects are hardlinks shared with the source directory — use `rsync -a -H` or `cp -a` to preserve the links and avoid doubling content, and the source directory itself should be in the same backup |
| Authoritative views | `<meta-dir>/meta/<view>.db` (incl. WAL) | See the three flows below; **don't bare-copy under write activity** |
| Cross-view ledger | `<meta-dir>/meta/refs.db` | Copy with `meta/`; after loss/replacement recompute from the existing views via `grovefs refs --meta-dir D rebuild` (only ever overestimates, never wrongly deletes) |
| API key registry | SQLite: `<meta-dir>/meta/api_keys.db` (incl. WAL); PG: `grovefs_control.api_keys`/`mcp_calls`; FDB: cluster `ctl` subspace | SQLite as above, cold copy; PG goes into the DB with `pg_dump`; FDB goes into the cluster with `fdbbackup`; none has a separate file to back up |
| View definitions (derived/alias) | SQLite: `<meta-dir>/meta/views/*.layout`; PG: `grovefs_control.view_defs`; FDB: cluster `ctl` subspace (`CtlDefTable`) | SQLite plain text files, must be copied along (HA standbys also need manual sync, §3.2/§3.4); PG replicates with the DB, FDB with `fdbbackup`, neither needs manual sync |
| token | `/etc/grovefs/grovefs.env` | Store separately, encrypted (lost = no client can mount) |
| API key registry and audit | SQLite: `<meta-dir>/meta/api_keys.db` (incl. `-wal`; exists only after `apikey create`) / PG: `grovefs_control.api_keys`+`mcp_calls` (via `pg_dump`) / FDB: cluster `ctl` subspace (via `fdbbackup`) | SQLite **cold-copy** with `meta/` (stop writes first); with `--require-api-key` enabled it is the sole admission basis — losing it = no key authenticates, historical audit gaps. **Never delete `-wal` alone** (it also deletes not-yet-checkpointed key rows and audit rows); in a split-disk deployment `data-server --meta-dir` must point at the metadata root holding this DB (it doesn't copy, only reads that one), a wrong target fails closed; on PG/FDB deployments just give `data-server` the same `--meta-store` URI, no local DB to point at |

**Single-machine / local mode**: first `umount` to stop writes (or stop meta-server), then `cp -a`/`rsync` the whole `--home`. Restore: copy back into place → remount → `ls`/`cat` spot-check. The binary isn't in the backup scope — just reinstall `grovefs`, all data is in `--home`.

**SQLite + in-house HA**: what the standby receives every ~500ms is a full `VACUUM INTO` snapshot (online-backup semantics), but it's **a replica, not an archival backup** (still recommended to take periodic off-site snapshots of either the primary or standby). Node-repair flows:

- Standby broken → after fixing, just `systemctl start` the standby meta-server: it resumes receiving snapshots from the primary and catches up, no manual data injection.
- Primary broken → the standby auto-promotes (see the §12.1 logs); after the old primary is fixed and `start`ed: it finds a higher epoch and **auto-demotes to standby**, never reviving into two primaries (with `--witness` configured, even an old primary restarting with a higher epoch is held at standby by the quorum, `test_ha.sh` section 9 proves it).
- Both halves lost and rebuilding: restore `<meta-dir>/meta/` from the most recent archival snapshot, the precondition for the epoch starting from scratch is **all data-servers also turned over at once** (otherwise leftover `.fence_token` will block writes — this is fail-safe's correct behavior, not corruption).

**PostgreSQL backend**: view-tree backup is entirely handed to the PG toolchain (`pg_dump`/`pg_basebackup` + WAL archiving/PITR); on the GroveFS side only back up `blocks/` + the env file — keys, audit, and view definitions are already in the `grovefs_control` schema, one `pg_dump` covers tree + control plane together (proven by the `control` suite), no local control-plane files to cold-copy. In `--meta-store` mode the in-house snapshot backup API deliberately returns an error (by design). For directly copyable dump/restore/drill commands (incl. DBA database creation and post-restore verification items) see **§15.7**.

**Migrating to a new machine / switching OS**: metadata isn't bound to hardware (stores only object keys, paths are a pure function of keys), a whole-directory cold copy works as-is; for selective per-view migration use `view export/import` (details in §3.4.2). Three topology boundaries: the multi-disk `--data-dir` count/order must match (an equal-size clone swapped in place is fine, add/remove/reorder remaps; or merge into a single root); striping `s<N>:` must keep the N data-servers and their order; a whole-directory copy carries the HA `epoch` over, the old primary must be stopped first. PG backend uses `pg_dump`/logical replication.

### 12.4 Upgrade and rolling changes

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
| No stripe chunks seen for large files | Confirm the file is ≥16MiB and `--data-servers` gave a comma-separated multi-address list | Small files naturally land only in chunk0, by design not a bug |
| Mountpoint disk full | First enlarge the disk holding `blocks/`; afterward use §12.6's three criteria to check "workload volume or unreclaimed bytes" | Capacity governance has no magic: objects only add/delete without compression, long-term rely on `du` slope + physical/logical ratio |

### 12.6 Honest words on capacity and reclamation

- On `unlink` (the last link disappearing) the backend best-effort deletes the object/all shards by key, idempotent. **Pages named by no live view tree and no longer booked in the ledger** (orphans) are audited/reclaimed by the offline command `grovefs gc`: by default it only scans, listing the largest orphans, keys the ledger still holds (run `refs rebuild` first), and skipped non-object items; only `--apply` really deletes, and only pages older than `--older-than` (default `24h`), never deleting keys the ledger still holds, and requiring no live mount on this storage and no grovefs process on this machine (`--apply` must be offline; hardlink `import` inherits the source file's old mtime, so the age gate is a hard constraint, not a mere reference).

  ```bash
  grovefs gc --meta-dir D                        # 只扫描:孤儿 / 账本持有 / 跳过项
  grovefs gc --meta-dir D --data-dir /data      # meta 与 data 分家时指向真正存 blocks/ 的根
  grovefs gc --meta-dir D --apply                # 回收 24h 以上的孤儿(离线)
  grovefs gc --meta-dir D --apply --older-than 0 --json   # 立即回收 + 机器可读
  ```

  Guards: if any view tree can't be opened it aborts wholesale (skipping one tree would misjudge all its objects as orphans); keys the ledger still holds are only reported, not deleted; refused when `--data-dir` has zero intersection with all trees (guards against a misconfigured `--meta-dir`/`--data-dir`, `--force` overrides); PG deployments refuse outright (the tree is a schema, bytes belong to data-server). e2e in the `gc` suite (41 assertions: clean DB / out-of-band tree deletion→held→rebuild→orphan→grace→real delete / non-object not deleted / same-name-on-this-DB mount refused, other DB not blocked / wrong-root foolproof / PG and illegal duration refused / `--json` fields).

  **The form common to all three backend tiers is the remote one**: the server reads its own tree (SQLite/PG/FDB), bytes enumerated toward the data-server — the operator's machine needs neither `meta/` nor `blocks/`, and orphan reclamation on PG/FDB deployments goes through it:

  ```bash
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100,10.0.0.5:50101            # 只扫描
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100 --apply --older-than 24h   # 真删(离线)
  ```

  The same semantics as the local form (`--older-than`/`--force`/`--json`/wrong-root foolproof all judged server-side), but two "absence" facts must be stated rather than assumed: *silence proof only recognizes the server's own sessions* — what `--apply` refuses is "this server still serves a live session" or "these views are still mounted on this machine", returning `gc --apply refused: this deployment is in use — <view> (N live session(s) on this server); …; stop those mounts first (reclaim must run offline, and a mount on another machine is not visible from here, so stop it too)`; it cannot see mounts on another machine, so the second half of that sentence is an operator instruction, not a guarantee. `--data-dir` and `--control-server` are mutually exclusive (the bytes aren't on this machine). e2e in the `gcremote` suite (66 assertions, three tiers: match/orphan count, grace period enforced via the data plane, session silence proof, wrong-root foolproof, `--json` shape and local/remote flag-combination refusals).
- Turn `du` into three decidable criteria: ① **record slope, not size** (append a dated reading the same weekday each week; only a slope that doesn't match business growth is an anomaly); ② **divide by the logical amount** — `blocks/` is physical bytes, what you see inside a mount is logical bytes, only same-order-of-magnitude is normal. Use `grovefs du` for the logical amount (one batch fetch of all object bytes, tens of milliseconds for tens of thousands of files; `find` inside a mount does per-file stat, two orders of magnitude slower):

  ```bash
  grovefs du --home H --view V --total          # 本地 SQLite 部署:一个参数就够
  grovefs du --view V --meta-server M --data-servers A1,A2 --total   # 条带部署:给全 data-server 列表
  find /mnt/da -type f -printf '%s\n' | awk '{t+=$1} END{printf "logical %.2f GiB\n", t/1073741824}'
  ```

  When the byte source can't be determined, `du` refuses to output a number rather than emitting a version of 0: `--data-servers` > `--data-dir` > metadata root; if the tree comes from meta/PG and neither address is given it errors; when the address-list count doesn't match the object `s<N>` layout's number of servers (incl. a tree with both pre- and post-expansion generations) it reports by direction which addresses to give, and a matching count but **reversed order** also errors without a number (otherwise the whole tree would be measured as a pile of 0s). **Only confirmed-missing objects** count into `warning: N of M object(s) are not in the data source and count as 0 bytes`; timeouts, refused connections, read failures all error without a number, because bytes with no answer aren't 0 bytes.

  ③ **a ratio > 1 has only two causes**: something that should have been deleted wasn't (go confirm in the view that the file was truly unlinked), or bytes shared by import/fork are still held by another view (`grovefs view --meta-dir D list` to see who else references it, only physical reclamation when it reaches zero, §5.3).
- The most common third case is actually an accident: **bypassing the mount to `rm meta/<view>.db` directly doesn't trigger a backend delete**, objects stay forever in `blocks/` (the reference ledger only overestimates, never wrongly deletes, so it only takes space, loses no data); the reclaim ledger can be reset with `grovefs refs --meta-dir D rebuild`. After resetting, run `grovefs gc --meta-dir D` again and these pages change from `ledger-held` to orphans, only then does `--apply` reclaim them; without rebuilding, `gc` conservatively never deletes.
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

After any upgrade/config change, run the full board (33 gated stages) on a machine with a build environment, or only the planes you care about:

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
>         │ meta-server :50110   权威树存 /data/meta/*.db  │
>         │          (默认 embedded SQLite3,无需安装任何数据库)    │
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
- The authoritative tree's physical form is just a plain file `/data/meta/share.db` (+ same-named `-wal`/`-shm` companion files, normal for WAL mode);
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
└─ meta/              ← 每个视图一个 SQLite 库
   └─ share.db        ← 视图 share 的权威目录树(目录项、权限、data_ref 指针)
```

"Views (tree directories) separated from objects (bytes)" is GroveFS's fundamental design: the hierarchical tree applications see is stored in `meta/*.db`, the bytes are always in `blocks/`, the two linked by the `data_ref` pointers in directory entries. The token is the cluster admission passphrase: all three machines must carry the same one, and every later `--token` is taken from it.

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
| `--listen` | `0.0.0.0:50100` | Listen address. **The default is `127.0.0.1:50052` (accepts only the local machine)** — to serve the two clients you must explicitly write `0.0.0.0`; the port number itself is your choice, clients just need `--data-servers` pointed correctly (50100/50110 are this document's conventional values) |
| `--data-dir` | `/data` | Data base directory: the service auto-creates `blocks/` and 256 buckets under it, file bytes stored by UUID as key in `blocks/<2hex>/<key>`. **A comma-separated multi-path list is allowed (`/data,/mnt/d2,...`) = automatic placement across several in-machine disks**: each whole object picks one disk by a hash of its key (the client and `data_ref` are entirely unaware, statfs auto-aggregates capacity). But the root list must be **fixed** for a given store — placement is hash mod number-of-roots, neither the list nor the object records a disk ordinal, so a failed disk is **swapped in place** by an equal-size clone (same path, same position, same root count) and still locates; adding/removing disks or reordering `--data-dir` remaps every key and makes old objects unreachable under the new layout (the placement layer doesn't error, it silently ENOENTs); for expansion, provision multiple roots at once, or merge disks at the OS layer with LVM/mdadm and mount a single-path `--data-dir`. The server only knows this layout, a wrong base directory = data "lost" (really just not looked for) |
| `--token` | the string in grovefs.env | The data-plane handshake (`OP_OPEN`) passphrase. A client not carrying the same string → the connection gets `PermissionDenied`, no retry. Not giving it = LAN-trusted, no auth, only for a fully trusted intranet |
| (optionally `--require-api-key`) | — | Turns data-plane admission into **revocable per-caller API keys** (client `mount --api-key`): only accepts live keys minted by `grovefs apikey create --for mount` (or `any`), logs each connect (incl. refusals) to audit, and revalidates live connections each second — a revoke breaks established connections and makes them self-unmount. Given alongside `--token`, both gates must pass; not given = behavior as before (just still auditing). The key-DB path is set by `--meta-dir <metadata root>`, **defaulting to the first `--data-dir`** (in a split-disk deployment you must point it right; pointing at a directory where no key was minted = admits none) |

Per-parameter explanation — `meta-server`:

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--meta-dir` | `/data` | **Metadata root** (subcommand parameter): view DBs land in `<meta-dir>/meta/` (i.e. `/data/meta/share.db`). Takes precedence over the global `--home` — the latter is the default root; when both are given `--meta-dir` wins |
| `--listen` | `0.0.0.0:50110` | As above, the default `127.0.0.1:50053` accepts only the local machine; the metadata plane takes its own port, not crossing the data plane's 50100 |
| `--token` | the same string | The metadata-plane handshake (`M_OPEN`) passphrase, validated independently of the data plane; recommended to use the same one for easier management |
| (optionally `--require-api-key`) | — | The metadata plane admits by the API key in `M_OPEN` (`mount --api-key ID:SECRET`, or client env `GROVEFS_API_KEY`): it reads the same registry (SQLite: `<meta-dir>/meta/api_keys.db`; PG: `grovefs_control.api_keys`) — **the same registry** as `grovefs mcp --require-api-key` / `grovefs apikey --meta-dir`, separated into entrances by scope; a revoke closes established mounts within the first round-trip (the refusal reason is returned with the frame, the mount self-unmounts). Details in §6 / §3.5.1 |
| (not passing `--meta-store`) | — | **This is "the database is sqlite3"** — not passing it means embedded SQLite, one `.db` per view; passing `postgres://…` is what switches to PG (and mutually exclusive with HA, see §11.9) |
| (not passing `--role/--peer/--witness`) | — | Single-authority node form: this case does no primary/standby/quorum, `192.168.1.10` down = metadata plane paused (on-disk data not lost). To add HA, extend per §11.8 |

Verification (a fourth machine or another window on this one):

```bash
ss -lntp | grep -E '50100|50110'      # 两行都在,进程名 grovefs
```

The two foreground windows should each print a banner line — `data-server listening on 0.0.0.0:50100 (host <storage-hostname>, 1 data root(s): /data)` and `meta-server listening on 0.0.0.0:50110 (base /data, role Primary)`; the data line echoes the hostname and root list because nodes often share the same root path names — when reading logs, first confirm the host, then the path. For what the full terminal looks like, see §13.11 record ①.

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
sudo systemctl enable --now grove-data-server grove-meta-server   # enable=开机 自起,now=立刻起

# 建应用要挂的视图:视图只能显式创建,名字不存在就挂会被 M_OPEN 拒
sudo grovefs view --meta-dir /data create --view share
```

**Meaning of every setting in the unit**: `EnvironmentFile` keeps the token out of both the unit file and the ps command line (a passphrase on the command line is visible to any same-machine user via `ps aux`); not writing `Type` means simple — the two servers are foreground blocking processes anyway, systemd just watches process liveness; `Restart=on-failure` respawns on crash (OOM/segfault etc. nonzero exits), watch §12.1's log inspection for repeated respawns; `After=local-fs.target` ensures the service starts only after the disk holding /data is mounted.

Firewall (run on the storage machine, open the two service ports only to the intranet subnet):

```bash
sudo ufw allow from 192.168.1.0/24 to any port 50100 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 50110 proto tcp
# firewalld 等价:
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50100 protocol=tcp accept"
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50110 protocol=tcp accept" && sudo firewall-cmd --reload
```

### 13.5 Step five: client A (192.168.1.11) read-write mount

First the pre-check + mountpoint directory:

```bash
ls -l /dev/fuse && which fusermount fusermount3   # §11.0 三件套
sudo mkdir -p /mnt/da
```

Mount once by hand (`mount` backgrounds by default, returning only when the mountpoint is ready; on failure the background's error detail is echoed back to the terminal):

```bash
TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
sudo grovefs mount --view share \
     --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token "$TOK" -m /mnt/da
# 看到 "grovefs: mounted view 'share' at /mnt/da (background pid …)" 即成功;另开窗口:
echo hello-$(date +%s) | sudo tee /mnt/da/probe.txt && sudo ls -l /mnt/da
# 验证完 sudo grovefs umount -m /mnt/da 卸载(后台进程随之退出),下一步换 systemd
```

Per-parameter explanation — `mount`:

| Parameter | This case's value | Meaning |
| --- | --- | --- |
| `--home` (clients **need not pass it**) | — | A remote mount writes no data locally, this parameter is unrelated to both the data and metadata planes — only local mode (`init`/`mount` without `--meta-server`) uses it to locate the authoritative DB; commands that access the metadata root (`meta-server`/`apikey`) each name it with `--meta-dir`, `--home` being only a fallback |
| `--view` | `share` | The tree-selection label: which authoritative view to mount. Unrelated to Linux usernames (§2); only A/B using the same name shares the same tree. `share` must first be created at the center via `grovefs view --meta-dir /data create --view share` (mounting without creating is refused) |
| `--data-servers` | `192.168.1.10:50100` | Data-plane address: file bytes go over a custom TCP frame to this `data-server`, never touching the local disk (not NFS). Only a comma-separated multi-address list enables cross-machine striping; a single address means no striping |
| `--meta-server` | `192.168.1.10:50110` | Metadata-plane address: the directory tree is served by the center `meta-server` — this is the root of "multiple machines sharing one tree"; it also subscribes to invalidation pushes (who wrote what, this machine re-fetches on next access immediately). A comma-separated list = a failover list (§11.8) |
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
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token ${GROVE_TOKEN} -m /mnt/da
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
     --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token "$TOK" --ro -m /mnt/db
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
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token ${GROVE_TOKEN} --ro -m /mnt/db
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
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --api-key ${GROVE_A_KEY} -m /mnt/da

# 客户端 B 的改成(--ro key 已由服务端强制,客户端 --ro 可留作双保险):
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --api-key ${GROVE_B_KEY} --ro -m /mnt/db
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
```

All green means the deployment is complete. Rollback is simple too: on both clients `systemctl stop grove-mount` → on the storage machine `systemctl disable --now grove-data-server grove-meta-server`; whatever is on the disk under `/data`, if not deleted, is a complete backup (§12.3), and to truly start over, just format the data disk (after confirming it's backed up).

Daily maintenance (inspection keywords/backup/upgrade/failure tables) all applies from **§12**, IP mapping: storage machine = 192.168.1.10, write end = 192.168.1.11, read-only end = 192.168.1.12.

### 13.9 Files and commands per machine, in one table (follow it, never flip back)

**Files to create on the storage machine 192.168.1.10 (3 total):**

| File | Which section has the content |
| --- | --- |
| `/etc/grovefs/grovefs.env` | generated by the third command in §13.2 |
| `/etc/systemd/system/grove-data-server.service` | the full first ini block in §13.4 |
| `/etc/systemd/system/grove-meta-server.service` | the full second ini block in §13.4 |

Execution order (storage machine): `mkdir` (§13.2) → start both services by hand to verify (§13.3) → Ctrl-C → write the two units → `daemon-reload` + `enable --now` (§13.4) → the two firewall rules (§13.4). Optional hardening adds one step: the view-bound / read-only key, two commands (§13.7).

**Files each of client A 192.168.1.11 / client B 192.168.1.12 must create (2 each, both machines isomorphic):**

| File | Which section has the content |
| --- | --- |
| `/etc/grovefs/grovefs.env` | distributed by scp at the end of §13.2 (no need to rebuild, the same one as the storage machine) |
| `/etc/systemd/system/grove-mount.service` | A uses §13.5's full ini; B uses §13.6's full ini |

Execution order (each client): the three-piece pre-check + `mkdir /mnt/xx` (§13.5/13.6) → mount once by hand to verify (§13.5/13.6) → Ctrl-C → write the unit → `daemon-reload` + `enable --now` → confirm with `findmnt`. If you did §13.7: swap ExecStart to the corresponding line in §13.7, then `daemon-reload + restart` again.

### 13.10 This case's parameter cheat sheet (one table for everything)

| Machine | Command | What it does |
| --- | --- | --- |
| storage | `grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token $TOK` | Byte-storage plane: `--listen` binds all interfaces, `--data-dir` base dir (auto-creates `blocks/` under it), `--token` the data-plane passphrase |
| storage | `grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token $TOK` | Metadata plane (default SQLite): `--meta-dir` points at the metadata root (view DBs under its `meta/`), not passing `--meta-store` = SQLite, not passing `--role/--peer` = single node |
| storage | `grovefs view --meta-dir /data create --view share` | Explicitly build the base view: `share` mounted without being created is refused at `M_OPEN`; the DB lands at `/data/meta/share.db` |
| client A | `grovefs mount --view share --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token $TOK --run-as-uid 1001 -m /mnt/da` | Read-write mount: `--view` selects the tree, `-m` is required and can't be a positional argument, not writing `--no-cache` keeps freshness via the default push |
| client B | same as A, changed to `--ro --run-as-uid 1002 -m /mnt/db` | Read-only mount: `--ro` blocks the local normal write path; combined with a center `--ro` key for this client = server-side hard read-only |
| storage | `grovefs apikey --meta-dir /data create --for mount --view share [--ro]` | View-bound / read-only key: the key can only open the `share` view (opening others is refused with `E_AUTH` at `M_OPEN`); `--ro` makes each write-intent op refused server-side (`E_NOTPERM`→`EPERM`), `revoke` immediate |

### 13.11 Full terminal transcript (real measured output, unchanged verbatim)

Capture conditions: grovefs 0.1.0 @ Ubuntu 20.04, a **single-machine three-in-one equivalent topology** — the `127.0.0.1:5010x` in the commands corresponds to the three-machine case's `192.168.1.10:5010x`, `/tmp/grove_case/data` to `/data`, `/tmp/grove_case/mnt_da|mnt_db` to `/mnt/da|/mnt/db`. Every segment is real terminal output (incl. Chinese error messages); the token was a one-time random value already voided. Rerun script: the repo's `.build-bin/run_case_demo.sh`.

**① Storage machine: get the token, start both services in foreground, confirm listening**

```console
$ grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2
690987478d9cb1a4fee062db6a3d04c4
$ ./grovefs data-server --listen 127.0.0.1:50100 --data-dir /tmp/grove_case/data --token "$TOK"
data-server listening on 127.0.0.1:50100 (host <本机主机名>, 1 data root(s): /tmp/grove_case/data)        # 三机上用 0.0.0.0:50100,这行会显示 0.0.0.0 与存储机主机名
$ ./grovefs meta-server --meta-dir /tmp/grove_case/data --listen 127.0.0.1:50110 --token "$TOK"
meta-server listening on 127.0.0.1:50110 (base /tmp/grove_case/data, role Primary)
$ ss -lntp | grep -E "5010|5011"
LISTEN    0         128              127.0.0.1:50110            0.0.0.0:*        users:(("grovefs",pid=1733230,fd=4))
LISTEN    0         128              127.0.0.1:50100            0.0.0.0:*        users:(("grovefs",pid=1733225,fd=3))
$ ls /tmp/grove_case/data                        # 两服务启动时自动把骨架建好
blocks
meta
$ ls /tmp/grove_case/data/blocks | head -3     # 启动时已建好 256 个桶(两位哈希名)
00
01
02
$ find /tmp/grove_case/data/blocks -type f | wc -l
0                                                # 还没写文件,桶里没对象是对的
$ ./grovefs view --meta-dir /tmp/grove_case/data create --view share
created BASE view 'share' at /tmp/grove_case/data/meta/share.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
```

**② Client A: read-write mount + write files**

```console
$ ./grovefs mount --view share --data-servers 127.0.0.1:50100 --meta-server 127.0.0.1:50110 --token "$TOK" -m /tmp/grove_case/mnt_da
using remote metadata service at 127.0.0.1:50110
using remote data service at 127.0.0.1:50100
mounting view 'share' at /tmp/grove_case/mnt_da (fully remote: no local store)   # 远程挂载不需要 --home
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
drwx------ 2 fs fs 4096 10月 17 11:12 docs
-rw------- 1 fs fs   14 10月 17 11:12 from_a.txt

/tmp/grove_case/mnt_da/docs:
总用量 1
-rw------- 1 fs fs 3 10月 17 11:12 n1.md
```

After writing, look at the side effects back on the **storage machine** — two files, exactly two UUID objects, all metadata in share.db (+ WAL companion files):

```console
$ ls /tmp/grove_case/data/blocks
b85d5e05-722d-4980-a0e0-e15b9e094de0
fa513481-830f-4719-b5a7-156e1ace1c96
$ ls /tmp/grove_case/data/meta
share.db  share.db-shm  share.db-wal
```

**③ Client B: read-only mount, can read, can't write**

```console
$ ./grovefs mount --view share --data-servers 127.0.0.1:50100 --meta-server 127.0.0.1:50110 --token "$TOK" --ro -m /tmp/grove_case/mnt_db
using remote metadata service at 127.0.0.1:50110
using remote data service at 127.0.0.1:50100
mounting view 'share' at /tmp/grove_case/mnt_db (fully remote: no local store, read-only)
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
round2-1792235543
```

**⑤ Storage machine: view-bound / read-only key hardening + clean wrap-up**

Upgrading read-only from "the client carries `--ro` voluntarily" to "the server enforces per op": mint each of A and B a key bound to the `share` view (`--meta-dir` named the same as meta-server, pointing at the same metadata root; B's additionally carries `--ro`). The commands are the same set as §13.7, only pointing the metadata root at this case's `/tmp/grove_case/data`:

```bash
# A:读写 key(只能开 share)
./grovefs apikey --meta-dir /tmp/grove_case/data create --label A-rw --for mount --view share
# B:只读 key(只能开 share,且写一律被服务端拒)
./grovefs apikey --meta-dir /tmp/grove_case/data create --label B-ro --for mount --view share --ro
# 确认两行(view=share,一份 rw 一份 ro;secret 只在 create 时打印一次,list 里只有哈希)
./grovefs apikey --meta-dir /tmp/grove_case/data list
# 撤销即时生效:被撤销 key 的存活挂载在下一次服务端往返即被拒并自卸载
./grovefs apikey --meta-dir /tmp/grove_case/data revoke --id <A-rw 的 id>
```

Clients present the key with `--api-key '<id>:<secret>'` (or `export GROVEFS_API_KEY`) then remount; after B switches to the `--ro` key, even remounting without the client `--ro`, writes are still refused by the server (`E_NOTPERM`→`EPERM`). The server must add `--require-api-key` to meta-server / data-server (see §13.7). Wrap-up unmount:

```console
$ fusermount -u /tmp/grove_case/mnt_da; fusermount -u /tmp/grove_case/mnt_db
A umount ok
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
| `meta-server --meta-dir DIR` | Authoritative view tree `<meta-dir>/meta/<view>.db` (the control plane additionally has `api_keys.db`/`refs.db`; the SQLite-backend form, on the PG backend both go entirely into `grovefs_control`) | Any path, may be a different disk from the object disk |

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

> **Exception — relocating striped files**: the stripe layout is self-describing in `data_ref` (`s<N>:<stripe>[:<fp>]:<uuid>`) — with `--member-id` configured, which machine chunk i lands on is decided by member **identity**, the list order can be anything, already-written striped files are still found; without it, it's decided by the **order** of the client `--data-servers` list (`servers[i%N]`), so if a multi-address list's order changes during migration, already-written striped files lose their chunks. In both modes the **server count** must stay consistent. A single-address `--data-dir` relocation has no such issue.

**Multiple disks / capacity expansion**: two legitimate paths ① one data-server mounts several disks: `--data-dir /data,/mnt/d2,...` a comma-separated list of storage roots, whole objects auto-placed onto one of them by key hash, `df` (statfs) auto-aggregates total capacity — but the root list must be **fixed** for a given store (placement is hash mod number-of-roots, add/remove disks remaps every key and makes old objects unreachable under the new layout), so provision multiple roots at once; ② start several data-servers + client `--data-servers a,b,c` striping (only ≥16MiB is chunked, small files land on one server; only a bandwidth gain when the link is unsaturated, see the honest note in §5.2). You can also merge several disks into one filesystem at the OS layer with LVM/mdadm and mount a single-path `--data-dir`. There is no per-directory quota or storage tiering (unimplemented).

> **Swapping a disk doesn't affect locating; only add/remove/reorder does**: the landing position is recomputed on the spot as a pure function of the key (hash mod number-of-roots), neither the `--data-dir` list nor the object records "which disk number". So a failed disk **swapped in place** by an equal-size clone (same path, same position in `--data-dir`, same root count) still locates data normally — the disk's content and its internal 256-bucket fan-out are all whole-object key-addressed copies, no index to rebuild. Conversely, once you **add, remove, or reorder `--data-dir`**, the modulus changes, every key remaps, old objects addressed under the new layout become unreachable; the placement layer does no remap guard, it only silently ENOENTs (cross-server striping refuses the mount outright and reports `refusing to remap` when the `--data-servers` count/order doesn't match). Operationally treat the `--data-dir` list as immutable config: equal-clone in-place swap is fine, in-place expansion/reorder is not, expansion goes via "full reload onto a new fixed layout".

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
# 单文件 ≥4MiB 自动分片并发写;配了多地址 --data-servers 且 ≥16MiB 自动条带,导入无需特殊调优
# 导完抽验:中心对象目录在增长,视图树里能看到层级
ls /data/blocks | head && find /mnt/da/imported -maxdepth 2 | head
```

The trade-off between the two paths: import is zero-copy but imported objects are **single-file objects** (no striping), and it converges toward online mounts via TTL rather than event push; rsync pouring keeps an extra copy of bytes but newly-written large files can stripe, new files are immediately visible via push. Note (both are implementation facts, not boasting import speed): the throughput ceiling for FUSE writes is the frame link and the center disk; for TB-level bulk data, batch during off-peak; every million files grows `meta/<view>.db` correspondingly (one node row + directory entry per row), which is normal.

### 14.3 Which mechanisms decide a client's "display method"

| Mechanism | Effect | Fits |
| --- | --- | --- |
| **Different `--view` = different tree** | Two entirely independent hierarchies, mutually invisible; the most thorough way to get different organizational shapes | A/B see fundamentally different things |
| **Same `--view` = one tree** | One copy of bytes, all mountpoints see the exact same authoritative tree (real-time sync); B read-only uses `--ro` or a read-only key (§13) | A/B share the same batch of data |
| **`mount --layout` display lens (§3.1)** | One tree, each mountpoint does its own hide/only/map/link — partial display, path reorganization, bytes untouched, other mounts unaware | Customize the display shape without needing two copies of data |
| **Server-side derived view (§3.2)** | The same lens rules **named and stored centrally on the server** (`meta/views/`), clients just `mount --view <derived name>`, same name same shape across machines, a read-only key can be assigned by name (§13.7); **a derived (virtual) view mount is always read-only** (write via the base name) | Team-shared display contract, no rule-file distribution; distribute a read-only shape |
| **Server-side pure alias (§3.4)** | A public name maps 1:1 to a physical view (`alias <real name>`, no rules): the alias mount is **whole-tree writable**, the real name thereafter **is not a mount entry** (guessing it still can't mount in) | Name governance: public names rotate freely, storage identity not exposed (on leak, `--retire` rotates atomically in one command: old name retired, real name refused for mounting throughout); orthogonal and complementary to shape governance (derived) |
| **Server-side fork view (§3.4.1)** | `view fork` copies a base view's directory entries into an **independent metadata tree** (preserving ino/hardlinks, zero byte copy): structure writable (mkdir/rename/mv/rm), content read-only (`EROFS`); deletion only removes this view's `meta/<name>.db`, the base view and bytes unaffected | An "organizational sandbox" free to reshape without hurting the base view; content still shares one copy, bytes not copied |
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
- **Remote direct import** — `grovefs import` only takes `--meta-dir`/`--data-dir` (local paths), **not `--meta-server`/`--data-servers` remote addresses**, there's no "pour a directory into a remote cluster from another machine" entry. Reason: import writes the authoritative `meta/<view>.db` and `blocks/` directly (zero-copy = source and blocks hardlinked on the same disk), and the remote protocol has no corresponding "batch-commit a whole tree" operation. Cross-machine bulk ingest of existing data goes the §14.2 alternative path (an already-mounted point's `rsync -a` pouring via the FUSE write path, large files can still stripe), or run import on the storage machine;
- **splice/zero-copy read-back** — the local `LocalBackend`'s large-block read-back has no zero-copy channel. Reason: the fuser 0.18 currently depended on has only `writev` on the reply side (copying a userspace buffer into `/dev/fuse`), `ReplyData` only takes `&[u8]`, no `splice`/`sendfile`/`vmsplice` entry — to do it would require forking fuser to take over the pipe ourselves. The measured gain ceiling doesn't cover this complexity and risk: warm 64MiB read 1318MB/s vs same-disk native ext4 1402MB/s (a ~6% gap), cold 1GiB read 2.02s ≈ ext4 2.00s (a ~1% gap, disk-speed dominated), and remote/striped backends gain nothing; compounded by page-cache consistency and `--no-cache` strong-consistency mount correctness risks. So not done, kept as negative space.
- per-directory enforced permissions — directory-level permissions unimplemented (`--layout` can do directory-level display pruning, but as noted it doesn't constitute enforced isolation).

**Documentation convention** (README / USAGE / docs.html share one rule): state only the current version's behavior — don't write removed parameters, environment variables or switches, don't do before/after version comparisons or historical baseline numbers, don't write change-narration like "added / now supports / consistent with the old version / fell back to old behavior" (evolution history belongs in git commit records). Real fallback semantics within the current version, and the unimplemented list above, are not in this category and are kept as normal.

---

## 15. Scenario case collection (pick one by purpose, change the addresses and copy as-is)

Earlier sections are organized by **mechanism**, this section by **what you want to do**, giving the shortest executable sequence. Conventions:

- `G=grovefs` (swap for your binary path, e.g. `/usr/local/bin/grovefs`); addresses uniformly written as `10.0.0.10`, ports `50100` (data) / `50110` (metadata) / `5432` (PG) — swap for yours;
- The PG sections were **run command-by-command** on a local PostgreSQL 12 (creating the DB, starting services, mounting, inspection SQL, `pg_dump` + restore to a new DB, `view rm --base`'s transactional ledger teardown + `DROP SCHEMA` + byte reclamation, `refs rebuild`'s two directions (the whole chain run through by `.bneck/pg_view_teardown.sh`'s 44 assertions on a real FUSE mount), and every refusal wording quoted below), and cross-mount `rsync -aH` was also verified for hardlinks and increments between two local mounts; the measured port was 5433, the text writes the standard 5432, the only difference being the port;
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

### 15.1 Scenario: see the effect on one machine in 5 minutes (no server process)

```bash
G=grovefs
export GROVEFS_HOME=/data/demo        # 之后所有命令都不用再写 --home
$G init --view team                   # 建一棵物理视图树(meta/team.db)
$G mount --view team -m /mnt/team     # 默认转后台;前台调试加 --foreground
mkdir -p /mnt/team/docs && echo hello > /mnt/team/docs/a.txt
cat /mnt/team/docs/a.txt              # hello
ls /data/demo/blocks | head           # 字节真正落在这里(按 key 命名)
$G umount -m /mnt/team                # 收尾(别用 umount 命令,它不会清理后台进程)
```

Acceptance: `cat` outputs `hello`, at least one object file appears under `blocks/`. Single-machine mode has no token, no network port; the FUSE mountpoint permissions are just OS permissions.

### 15.2 Scenario: one storage machine + several clients (SQLite center, minimally usable)

Storage machine `10.0.0.10` (data and metadata on the same disk, `blocks/` and `meta/` side by side under `/data/cluster`):

```bash
G=grovefs; TOK=$(head -c 24 /dev/urandom | base64)   # 两端用同一个值
export GROVEFS_TOKEN=$TOK                           # 服务端/客户端都读它,省掉 --token
$G init --view team --home /data/cluster            # 1) 先建基视图,再起服务
$G data-server --listen 0.0.0.0:50100 --data-dir /data/cluster &
$G meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 &
```

Any client (clients **don't install a database, don't land metadata**):

```bash
export GROVEFS_TOKEN=<同一串>
G=grovefs
$G mount --view team -m /mnt/team \
   --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110        # 读写
$G mount --view team -m /mnt/team_ro \
   --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 --ro   # 本机只读
```

Acceptance: on machine A `echo x > /mnt/team/t.txt`, on machine B `ls /mnt/team_ro` sees it within 2 seconds (TTL default ≤2s, no remount). For **server-side hard read-only** rather than just the local `--ro`, see §15.11.

> **A view name must be created before mounting**: a view name must first be brought into being via `view create` / `init` / `view import` before a mount is allowed; a misspelled name gets a readable refusal and won't silently grow an empty tree (this is the only behavior, no switch; details in §4).

### 15.3 Scenario: get the PG backend running first (single-machine verification, one parameter more than §15.2)

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
$G mount --view demo --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 -m /mnt/demo
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

> Three behaviors you must know: ① each view's schema must be **explicitly created** by `view --meta-store postgres://… create` (a mount doesn't create it); running `init`/`view create` without `--meta-store` on the PG backend only builds an unused local `<view>.db`; ② a view name over **25 bytes** is refused (PG's 63-character identifier limit, doubled after hex encoding, about 8 CJK characters), the client only sees `Input/output error` + `background mount exited`, the real reason is in the meta-server log: `view name '...' incompatible with PG schema naming (empty, or longer than 25 bytes)`; ③ giving `--meta-store` together with `--role standby`/`--peer`/`--witness` fails immediately on startup (see §15.9).

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

> **Switching over shows an empty tree.** PG and SQLite are two metadata stores that don't read each other: once `meta-server --meta-store postgres://…` starts, the first time a view is opened it builds an **empty schema** (only the root node). `meta/*.db` and `blocks/` are both still in place, but the new center doesn't recognize them. This isn't a bug, it's a backend boundary; the current version has **no** tool to import SQLite metadata into PG (`view export` only reads from a local `.db`, PG has no corresponding import command; `import` only writes local SQLite).

So there are only three paths, in order of cost:

**Path A: don't switch.** If what you want is "operational unity / high availability", a SQLite center with §5.5's primary/standby already suffices; PG buys you **other people's experience operating a database** and PG's own streaming replication/PITR, not performance (§4.1 has a selection table). For PG on a new project, use it **from day one**, migration cost zero.

**Path B: replay (metadata rebuild, one extra copy of bytes).** The old and new centers coexist, move data via mounts:

```bash
# 旧中心(SQLite)与新中心(PG)同时在线,视图名可以相同
G=grovefs
$G mount --view team -m /mnt/old --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 --token $OLD_TOK
$G mount --view team -m /mnt/new --data-servers 10.0.0.20:50100 --meta-server 10.0.0.20:50110 --token $NEW_TOK
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 目录项与内容都按新 key 重新登记
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 再跑一遍是增量的(只补新文件)
```

- The object key is a **random UUID** (not a content hash), so the data **lands a new copy on the new center**, provision temporary capacity at old-data-volume × 1;
- Verify after moving: both sides' `find | wc -l` and key files' `sha256sum`; after confirming, retire the old center;
- The bytes in the old `blocks/` have **no reference** on the new center, won't be auto-reclaimed (GC is reference-driven), to reclaim space follow §12.6's trade-offs manually;
- Walk path B fully on a small view first before going to production, don't test on the main view.

**Path C: cold-database surgery (not provided, not committed).** Hand-inject the `node` rows of `meta/<view>.db` into `grovefs_v_<hex>` — the table structure matches (`id/parent_id/name/is_dir/data_ref/…`), but the `ctrl` epoch, `locks`, sequences, and ledger consistency are all on you to guarantee. No official script, no support if it breaks, **not recommended**.

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

> The difference from SQLite inspection (§12.1): under PG there's **no** `meta/<view>.db` to query, `grovefs view list` also can't see the trees in PG (§15.9); when `pg_stat_database`'s `rollback_pct` is long-term > 10% and writes slow down, first check whether it's the same directory being high-frequency `rename`/`unlink`'d by multiple clients (row-lock hotspot), rather than tuning GroveFS.

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
$G mount --view team -m /mnt/restore --data-servers 127.0.0.1:50200 --meta-server 127.0.0.1:50210 --ro
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
- **`pg_drop_views.sh` has been downgraded to an out-of-band inspector**: it no longer tries to grab what the CLI can do, only used when "you have no grovefs binary at hand", "want to clean leftovers the CLI can't reach (orphan schemas no definition references, invisible to `view list`)", or "want to verify the ledger on a machine that doesn't start a service" (`--list` lists schemas and decoded view names, without `--yes` always a dry run);
- **`gc` still refuses on the PG tier**: it only audits local page files, under PG the tree is a schema and bytes belong to the data-server, no equivalent command (§15.9) — byte reclamation only happens in step 2 above, so "delete the tree first, then think about reclaiming" was always the wrong order.

- **Don't casually `DROP` `grovefs_refs`**: it's the ledger for cross-view deletion safety, losing it only makes the system conservatively keep extra bytes (never wrongly delete objects); to recover from a lost state use step 3's `refs rebuild`. Leftover schemas no definition references and the CLI can't delete — see them clearly first with `bash .bneck/pg_drop_views.sh --list`, then clean with `--yes` (dry-run by default);
- **Confirm after deletion**: that schema disappears from §15.6's query 1, the client can no longer mount that view name; in the ledger `select count(*) from grovefs_refs.holders` reaches zero.

> Suites: `.build-bin/test_pg.sh` (24 assertions) and `.build-bin/test_pg_parallel.sh` (41 assertions) both end with `view rm --base` to self-clean, leaving 0 schema / 0 ledger rows in the DB after running (previously each run leaked a schema); `.bneck/pg_view_teardown.sh` (44 assertions) runs the entire chain above from start to finish on a real FUSE mount. All three "leave no trace when done", TEARDOWN is no longer a manual step.

### 15.9 Scenario: capability boundaries under a non-SQLite backend (which commands can't be used, alternatives)

One table each for PG and FDB; every row's "measured wording / behavior" was run on this machine's current build:

> **An ops machine needs only an account key**: the `--meta-store postgres://…` forms in the tables below apply to "run on the store-holding host, or the runner is themselves a DBA". To manage the same metadata from **another machine**, enable `--control-listen` server-side, and on the management machine use `--control-server HOST:PORT --api-key <admin key>`, with `meta-server` doing the same reads/writes on your behalf (§3.5.2); the FDB tier is the same.

#### PG tier

| Operation | Under the PG backend | Failure wording / actual behavior | Alternative |
| --- | --- | --- | --- |
| `init` / `view create` (without `--meta-store`) | builds no PG tree | only creates an unused local `meta/<view>.db` | add `--meta-store postgres://…`: `view --meta-store … create --view V` explicitly builds the schema; a mount never builds a tree |
| `view fork` / `view graft` | **usable** | one `INSERT … SELECT` copies all rows of the source schema into a newly created `grovefs_v_<hex>` target (same transaction, id/ino/hardlink/mode/owner/timestamps all preserved), `graft` selects a subset and reissues ids in the same transaction; echoes `forked view 'lab' from base 'alice': 6 entries, 3 shared object(s)`, zero byte copy, the shared ledger gains one holder per named object (proven by the `forksrv` suite on the PG tier) | no alternative needed; to still see a pruned tree use a derived view (§15.13) or an alias (§3.4) |
| `view export` / `view import` | **usable** | export uses a REPEATABLE READ read-only snapshot: writes the schema as host-agnostic JSONL row-for-row identical to the SQLite tier (header + one row per node); import in a single transaction DELETEs → INSERTs row by row → structure/count validation → `setval` raises the sequences → corrects the ledger old→new, rolling back wholly on failure (the newly created schema is `DROP`ped too). An existing target tree needs `--replace`, `--as-fork` writes a `fork <base>` definition, a bad dump leaves no half-product (proven by the `xfer` suite on the PG tier) | whole-DB backup still uses `pg_dump -n grovefs_v_<hex>` (§15.7); the two don't conflict |
| `view list` | usable (add `--meta-store`) | `view --meta-store postgres://… list` reads `grovefs_control.view_defs` + `grovefs_v_*` schemas; without `--meta-store` only shows local files (outputs `no view definitions under …/meta/views`) | same left, or the §15.6 inspection SQL |
| `view rm --base` (delete a whole base view with its bytes) | **usable** (`view --meta-store postgres://… rm --name V --base --data-dir R`) | in one transaction decrements `grovefs_refs.holders` per key (DELETE at zero) then `DROP SCHEMA grovefs_v_<hex> CASCADE`, and after commit takes the `DISTINCT data_ref` read before deletion and `backend.remove`s them one by one. The echo states the two facts on separate lines, not summed into one number: `schema dropped with 6 name(s), 4 object(s) no longer held by any view` / `4 object(s) freed from …/blocks, 0 not found …, 0 could not be freed`. A second deletion reports `no schema grovefs_v_<hex> in the database — 'V' has no base tree here …` and exits nonzero | the same guards as SQLite (governed name / dependent definition / live local mount); `--data-dir` points at the root really storing `blocks/`; §15.8 |
| `import` (zero-copy) | not applicable | creates a local `<view>.db`, metadata doesn't go into PG | mount + `rsync` (same as §15.5 path B) |
| `refs rebuild` | **usable** (`refs --meta-store postgres://… rebuild`) | recomputes the **in-database** shared ledger (not a local `meta/refs.db`): enumerates all `grovefs_v_*` schemas → per tree `SELECT DISTINCT data_ref` → rewrites `grovefs_refs.holders` by "how many trees still name it", deleting rows no tree claims. A no-op on a healthy DB (`0 row(s) corrected, 0 row(s) deleted`); removes over-counted rows when a schema was dropped out-of-band (measured `0 tree(s) … 3 row(s) deleted`), recovers from the trees when `holders` rows were wrongly deleted (measured `2 row(s) corrected`). **A single tree read failure aborts the whole thing** (undercounting leads to wrong deletion) | run only when **quiescent**; normally maintained by server transactions, no periodic run needed. The verbatim recipe is in §15.8 |
| `du` (byte total) | **usable** (`du --meta-store postgres://… --view V --data-servers HOST:PORT,…`) | first checks `information_schema` that `grovefs_v_<hex>` exists, then `SELECT … FROM <schema>.node` to read the tree, bytes batch-statted toward the data-server (the striped storage's address-list length must match the write time, otherwise it refuses rather than zeroing); a nonexistent name gets the same sentence across all three tiers `no tree for view 'V' in postgres://… — \`grovefs view create --view V\` makes one …`, never reporting a "misspelled name" as 0 B | same as SQLite (§8.4) |
| `apikey` | usable (add `--meta-store`) | keys and audit go into `grovefs_control.api_keys`/`mcp_calls`, no local `api_keys.db` (proven by the `control` suite) | same as SQLite (§15.11) |
| `mcp` | usable (add `--meta-store`) | all ten governance and read-only probe tools go through `grovefs_control` (`list_view_tree` can also walk a PG tree); `fork_view`/`graft_view` are the same implementation as the CLI, a cross-schema copy on PG | same left; inspection can also use psql (§15.6) |
| HA: `--role standby` / `--peer` / `--witness` | fails immediately on startup | `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG streaming / logical replication / PITR |
| view name > 25 bytes | refused | server log: `incompatible with PG schema naming (empty, or longer than 25 bytes)`; the client only sees `Input/output error` | keep the name within 25 bytes (about 8 CJK chars) |
| connection pool | default + env-tunable | per view default 8 write + 4 read (12 total), tunable via `GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL` (each 0–64); a mount session pins the engine, reclaimed after unmount and `meta-server --engine-idle-secs` (default 300s) of silence, reopened on next access (proven by the `evict` suite) | plan `max_connections` by "**active view count** × (write pool + read pool) + burst overflow"; for write hotspots check lock contention first, don't enlarge the pool |

#### FDB tier

| Operation | Under the FDB backend | Failure wording / actual behavior | Alternative |
| --- | --- | --- | --- |
| `view --meta-store fdb://… create --view V` | **the only way to build a tree** | opens a subspace (writes the root row and two allocator keys); without `--meta-store` only creates a local `meta/V.db` the server can't read | after creating, mount to write content |
| `view create --manifest` | refused (local form) | `` `view create --manifest` seeds a SQLite db file; under `--meta-store fdb://…` mount the view and write the files instead``; **but usable under `--control-server`**: the manifest is sent to the server as text, the server seeding by its own backend | write files directly after mounting, or use the remote governance port (§3.5.2) |
| `view fork` / `view graft` | **usable** | creates a new subspace in the cluster and copies the source subspace wholesale into it (batched if it won't fit one transaction, the root row written last, id/ino/hardlink/mode/owner/timestamps all preserved); echoes `forked view 'lab' from base 'alice': 6 entries, 3 shared object(s)`, zero byte copy, the `refs` ledger books one extra holder per named object; `graft` swaps ids/inos writing a selected subset (also batched by budget, `--skip-existing` resumes, proven by the `forksrv` suite on the FDB tier) | no alternative needed; to just view the same tree under a new name still use an alias/derived view |
| `view export` / `view import` (single-view metadata move) | **usable** (the migration channel across and within backends, §3.4.2) | export writes a subspace as host-agnostic JSONL row-for-row identical to the SQLite tier (header + one row per node, no absolute paths/hostnames/`blocks/` paths); import **opens a subspace under that name** in the cluster (the only tree-building entry besides `view create`), preserving id/ino/hardlink/mode/uid/gid/timestamp/symlink text, updates the shared byte ledger in the same transaction, and raises both allocator watermarks to the export max (matching PG's `setval`); an existing target tree needs `--replace`, a bad dump is wholly refused with no half-product; if the target is a **corpse** (has keys, no root row) it's refused by default with two exits: `view rm --name V --base`, or `--replace`; with `--replace` it **adopts** it — first unsubscribing 256 references in one transaction, then clearing the prefix, then writing batched as usual, exactly the resume path after a batched `import` was interrupted (measured: a 54,472-key / 10,894-entry corpse adopted by `--replace` and rewritten to 20,000 rows, `refs rebuild` reporting `0 corrected / 0 deleted`); export also refuses the same shape (distinguishing a corpse from never-built by the root row, giving different exits for the two); `--as-fork` is usable on this tier (writes a `fork <base>` definition + tree, rolling back the whole subspace if the definition write fails); under `--control-server` it's the same path, only the dump chunks stream across the governance port | bytes go via object storage separately: `rsync -a` the data root, or have the data-server serve the same objects (§14.2) |
| `view rm --base` (delete a whole base view with its bytes) | **usable** (`view --meta-store fdb://… rm --name V --base --data-dir R`) | what's deleted is the **subspace**, not a db file: clears all the view's keys → recomputes the shared `refs` ledger by surviving subtrees → calls the data-server's `remove` for objects no longer held. The echo states the two separately, not summed: `removed base view 'V' from fdb://…: 3505 key(s) gone, 701 node(s), 1 object(s) no longer held by any view` / `0 object(s) freed from …/blocks, 1 not found under that data root (wrong --data-dir? …), 0 could not be freed` / `note: new mounts of 'V' are now refused — \`view create\` starts over; the cluster's view definitions were not touched (\`view scan\` confirms the subspace is gone)`. Deletion **does not touch** the view definitions, aliases, or API keys in the cluster `ctl` | residual cleanup and acceptance: after deletion use `view scan` below to confirm the subspace is gone; `--data-dir` points at the machine really storing `blocks/` (with meta/data split, a wrong target only honestly reports not-found, never falsely claims reclaimed) |
| `view scan` (cluster-side inventory) | **usable** (`view --meta-store fdb://… scan`) | lists the **cluster's physical subspaces**, not the `view list` governance catalog (definitions + base names): `SUBSPACE / KEYS / ENTRIES / KIND / PREFIX`, `KIND` distinguishing `view` (a live tree with a root row), `corpse` (has keys but no root row, can't be mounted), `ledger` (shared ledger), `lease` (writer lease table, belonging to no view); the table footer gives a summary and the PREFIX hex (i.e. the argument to `fdbcli clearrange`) | clean residuals by this table: a `KIND=corpse` or an unclaimed `view` can both be taken by the previous row's `rm --base`; the `ledger` vanishes on the last unheld; a `lease` row is cleared with `view lease --release`, don't delete it as a corpse |
| `view lease` (cluster-side writer lease) | **usable** (`view --meta-store fdb://… lease`) | lists `STATE / DEPLOYMENT / HOLDER / EXPIRES`: `HELD` = a writer still renewing, `expired` = the row is there but the holder no longer renews; `EXPIRES` gives `in/ago Ns`. With `--deployment <that server's --meta-dir>` it pins one deployment; without `--release` it's read-only. The footer explains "one writer per deployment" and how to clean | clean a dead row: `lease --release --deployment <dir>` (an expired row); adding `--force` to clear a live row = a deliberate fence: the old holder on its next renewal prints `LOST the FDB writer lease` and exits with status 1, not continuing to write |
| cluster unavailable at startup | refuses to start (before bind) | `--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (<the cluster's own explanation>) — a meta-server cannot serve a mount from it; check fdbcli -C <cluster file> --exec 'status minimal'`; the probe is bounded (`GROVEFS_FDB_OPEN_TIMEOUT_SECS`, default 10s) | bring the cluster up first and confirm `status minimal` reports available; the probe timeout is tunable (§6.3.2) |
| a second meta-server on the same `--meta-dir` | refuses after waiting `GROVEFS_FDB_WRITER_WAIT_SECS` (default TTL+2s) (before bind) | `deployment '<dir>' already has a live meta-server writer (holder '<host:pid:nonce>', lease expires in Ns): one meta-server per --meta-dir may write this cluster — stop that server, or wait for its lease to expire (10s, GROVEFS_FDB_WRITER_LEASE_SECS), or clear a stale row with … lease --release --deployment … --force` | stop the old instance; on a crash wait the TTL (max 10s) to take over itself; once sure the old instance won't return, fence with `--release --force` (measured in section 16 of the `fdb` suite) |
| `view create` (name taken by a residual) | refused, naming the exit | `a subspace named 'V' already holds keys in fdb://… but has no root row (a corpse — nothing can mount it) — clear it first with \`grovefs view --meta-store fdb://… rm --name V --base\`` | per the hint first `rm --base` the wreckage then build; a healthy subspace (incl. one restored from backup) only reports "already exists" here, not treated as wreckage |
| `import` (zero-copy) | refused | ``import operates on a local SQLite view db; under a FoundationDB deployment (--meta-store fdb://…, or GROVEFS_META_STORE) a base tree is a subspace of the cluster …`` | mount + `rsync`/`cp` via the mountpoint (§14.2); bulk-ingest of an existing directory goes the three-step way: local `import` to stage → `view export` → `view --meta-store fdb://… import` (§3.4.2) |
| `gc --data-dir` (in-place form) | refused (use the remote form) | `` `gc` audits LOCAL page files under <data-dir>/blocks; a FoundationDB deployment's trees are subspaces in the cluster and its bytes are owned by the data servers — reclaim there with the data-server tooling (and back the cluster up with `fdbbackup`), not this command``; **the remote form is common to all three tiers**: `gc --control-server … --data-servers … [--apply]`, the server reading its own tree, bytes enumerated toward the data-server | use the §3.5.2 remote `gc`; cluster backup uses `fdbbackup`, the two divide different labor |
| `refs rebuild` | **usable** (`refs --meta-store fdb://… rebuild`) | recomputes the **in-cluster** shared ledger (not a local `meta/refs.db`): scans all live subspaces, rewrites `refs` by `data_ref → how many views still name it`, and deletes rows no tree claims; a no-op on a healthy cluster (`0 row(s) corrected, 0 row(s) deleted`), a `1 row(s) deleted` is clearing one orphan booking (measured on the FDB tier) | run only when **quiescent** (a new booking between the read and the write would be overwritten by a stale value); normally still maintained by server transactions, no periodic run needed |
| `view list` / `view define` / `view alias` / `mcp` | usable (add `--meta-store fdb://…`), **requires the cluster reachable** | view definitions are in the cluster `ctl` subspace (`CtlDefTable`, what the MCP governance plane reads/writes); every node aimed at the same cluster shares one record; without `--meta-store` only shows local files (outputs `no view definitions …`), the server can't read them | same as PG (§15.11/§15.13/§15.14); definitions are backed with `fdbbackup`, no local control-plane files to copy separately |
| `apikey` / MCP call audit | usable; with `--meta-store fdb://…` **lands in the cluster `ctl` subspace** | without a store it lands in local `meta/api_keys.db`; with a store keys and audit rows are both in `ctl` (`CtlKeyTable`, row-for-row identical to the SQLite tier, proven by the `apikey` suite on the FDB tier), at which point an unreachable cluster makes it unusable — same landing, same discipline as view definitions | same as PG (§15.11) |
| `du` | usable | goes `M_TREE_DUMP` (via meta-server), or `du --meta-store fdb://…` direct to the cluster scanning the subspace in one read-only snapshot; both forms' totals match the bytes seen by the mount (proven by the `fdb` suite). A nonexistent name first checks the root row (`subspace_exists`) then scans, returning the same sentence `no tree for view 'V' in fdb://… — …`, never passing off 0 B as an empty tree | same as SQLite |
| mount (`mount --meta-server`) | usable | refused by name when the view wasn't `view create`d, doesn't self-build an empty tree | create then mount (§4) |
| HA: `--role standby` / `--peer` / `--witness` | fails immediately on startup | `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness: GroveFS-native HA snapshot shipping is SQLite-only.` followed by an FDB-specific advice line | replication and recovery handed to the cluster (`fdbbackup`); a single meta-server instance suffices to serve all subspaces |
| view name > 25 bytes / illegal chars | refused | `view name is N bytes; the backends all cap it at 25 so no view is openable on one engine and unrepresentable on another` / `view name 'X' is not usable as an FDB subspace key` | keep the name within 25 bytes (about 8 CJK chars), same standard as PG |
| a binary without `--features fdb` given `fdb://…` | fails immediately on startup | `… names the FoundationDB backend, but this binary was built without `--features fdb` … an fdb:// uri is never served by SQLite` | use a feature build (§6.3), or drop `--meta-store` |

#### Four FDB-tier ops measurements (backup-restore / whole cluster gone / huge directory / metadata process dies)

Beyond the tables, four things only answerable by a cluster and a live mount all have executable recipes and measured readings (all on a single-node dev cluster, SATA disks; not an availability promise, the R3 experimental tier unchanged):

1. **Backup and restore are one closed loop, but restore requires the target to be empty.** `fdbbackup start/status` takes snapshots, `fdbrestore start -r <cluster>` pours them back; measured 12 steps, all 48 assertions green (`.bneck/fdb_backup_mount.sh`). Three hard constraints: the target cluster must have **no subspace of the same name** (checked range by range, reporting `restore_destination_not_empty`, `-k` doesn't unlock); it only submits the task, needing a backup_agent in the target cluster to actually progress; **`fdbbackup` backs up all keys in the cluster — the tree and the control plane (view definitions/aliases/API keys/audit all in the `ctl` subspace) covered in one snapshot; it does not back up the `blocks/` bytes on the data-server** — so a restorable deployment needs both backed up together: `fdbbackup` + the data root (`--meta-dir` is only deployment identity plus scratch/config, no authoritative record to back up separately). Once restored, mounting to read back content and `view scan` inventorying back the same-named subspace — that's what a closed loop means.

2. **When the metadata cluster vanishes wholesale, the mountpoint doesn't error, it blocks.** `kill -9`'ing the only fdbserver (single-node cluster, no failover to hide behind), within the 8–90 second window: the mountpoint is always in the mount table (reading `/proc/self/mountinfo`, not `findmnt` — the latter itself stats the target and hangs), the FUSE process doesn't die; `cat`/`ls`/`create+write`/a new `dd` on the mountpoint **all park**, not one returns EIO — the client waits for an available version, wrapped outside by the kernel's FUSE wait. Once the cluster returns (measured ~1.6s) parked calls are answered within 5–110ms, **the same mountpoint directly usable, no remount needed**; depending on whether the transaction already expired, the **first** readdir/write after recovery may error once (measured rc=2), **one retry succeeds**. The most important one: files whose writers were told success have not one byte missing after recovery (6/8 confirmed 64 KiB files all present, sha256 unchanged); the data half is independent, 48 assertions in `.bneck/fdb_fault_mount.sh`.

3. **A single directory of twenty thousand entries, listing and reread after deletion align name by name.** Creating 20,000 files one by one via the mountpoint (one transaction each): 153.4s (7.7ms each); `ls -1` 3.8s, `ls -U` 3.7s (~5.2k names/s), `find` 3.7s, `du -s` 13.2s; both listings' row counts **and sets** equal the expected full list name by name (no duplicates, no gaps, page boundaries 0/31/32/33/63/64/65/1023/1024/1025/midpoint/end all resolvable stat by stat); after deleting 500 from the middle the reread is 19,500 entries and the set exactly equals the survivor set, `view scan` reporting 19,501 entries (root + 19,500). The same criteria rerun on 6,000 entries pass. Entry `.bneck/fdb_bigdir_paging.sh` (`N=…` adjustable).

4. **A metadata process dying is a different failure surface: writes immediately report EIO, not park.** `kill -9`'ing **meta-server** (fdbserver still alive), writes on the same mountpoint return `Input/output error` **immediately** (measured returned at 1509ms, the client backoff-gives-up after several tries), no file created; bringing meta-server back at the same address recovers the same mountpoint directly (`ls`/`cat`/new writes all succeed, **files written before the restart keep their bytes intact**), **no remount needed**. The `--ro` in the same section is a **mount policy**, not a view attribute: the same view is writable as usual on another rw mount, the ro side's `create`/`mkdir`/`unlink` all refused with reason `EROFS`. The difference from item 2 is **who waits**: when fdbserver disappears the client parks waiting for an available version (recovery is possible), when meta-server disappears the request fails on the client side with nothing to wait for — so the two can't be written as one sentence. Section 15 of `test_fdb.sh` fixes these three things with 23 assertions (`--ro` / `kill -9` restart recovery / EIO during the interruption).

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

# 2) 铸三把 key(密文只在创建时打印一次)
$G apikey --meta-dir /data/cluster create --label alice-rw --for mount --view team
$G apikey --meta-dir /data/cluster create --label bob-ro   --for mount --view team --ro
$G apikey --meta-dir /data/cluster create --label agent    --for mcp

# 3) 客户端:key 走环境变量,不进 argv(ps 可见)
export GROVEFS_API_KEY='<id>:<secret>'
$G mount --view team -m /mnt/team --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110

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
# 每台存储机各自起 data-server --data-dir 自己的盘(端口可同)
for h in 10.0.0.11 10.0.0.12 10.0.0.13; do
  ssh $h "grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/blocks &"
done
# 客户端一条逗号列表启用确定性条带
$G mount --view big -m /mnt/big \
  --data-servers 10.0.0.11:50100,10.0.0.12:50100,10.0.0.13:50100 \
  --meta-server 10.0.0.10:50110
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
| The client tunes its own shape ad hoc, and needs to write | `mount --layout F` | `$G mount --view team --layout rules.txt -m /mnt/team` |

```bash
G=grovefs; D=/data/cluster
printf 'only /docs\n' > docs.rules
$G view --meta-dir $D define --name docs-only --base team --rules docs.rules
$G view --meta-dir $D list                      # 定义槽 + 裸基视图都会列出
$G mount --view docs-only -m /mnt/docs --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110
```

- Derived-view mounts are always **read-only** (shape governed centrally server-side, writes go through the base name `team`); already-mounted clients hot-swap the shape within ≤2s after the server changes rules, no remount needed;
- An alias is a 1:1 rename, **full-tree read-write**; an alias cannot itself serve as the base of another alias/fork;
- A fork is "touch it and it's a snapshot": names added to the base view later are not injected into an already-created fork; fork/graft **share one contract across all three tiers** (SQLite copies the db, PG does cross-schema `INSERT…SELECT`, FDB copies the subspace — the `forksrv` suite proves it on all three tiers, §15.9); the `EROFS` on content writes is a **client gate**, the hard boundary is still token/key.

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
GROVEFS_META_SERVER=10.0.0.10:50110
GROVEFS_DATA_SERVERS=10.0.0.10:50100
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
| `error: view 'x' does not exist on this meta-server (sqlite/postgres): … views are created explicitly …` | The view name has no tree/definition yet, and the server refuses an uncreated view name (it never self-creates under any circumstance) | §4: create before mounting (SQLite `view create --view x` / `init` / `view import`; PG `view --meta-store postgres://… create --view x`) |
| The mount **succeeded**, but the directory is empty | The view does exist, but the tree has no content yet — e.g. a just-`view create`'d empty tree, or an empty tree left over historically | Confirm with `ls meta/`; if created wrong, clear it per §15.8 and redo |
| `no physical base tree … cannot be forked` | Ran `view fork` on the PG backend | §15.13: switch to a derived view/alias |
| `no view definition '<v>' under …/views` | On a PG deployment you omitted `--meta-store` — the governance action landed on local files, and PG has no such definition at all | Add `--meta-store postgres://…` to the original command; a real view deletion is §15.8's `view --meta-store … rm --base` (unbooking + `DROP SCHEMA` in one transaction) |
| `no metadata tree … lives in Postgres` | Ran `view export` under PG | Use `pg_dump -n` (§15.7) |
| `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG and GroveFS native HA cannot be stacked | §15.9: route high availability through PG streaming replication instead |
| `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness` | The same mutual exclusion, but the FDB tier's remediation wording differs: replication and snapshots are already inside the cluster (`fdbbackup`), native HA has nothing deliverable here | §6.3 / §15.9: drop the HA params and start a single-instance meta-server |
| `... names the FoundationDB backend, but this binary was built without --features fdb (it does not link libfdb_c)` | The default build has no FDB backend. **It does not silently degrade to serving this uri over SQLite** — the server starts with a refusal marker and `serve()` fails directly; `du`/`view create`/MCP entry points error in place too | §6.3: `cargo build --release --features fdb`, or drop `--meta-store` to use the default SQLite |
| `fdb:// uri names no cluster file: give fdb://<path> or set GROVEFS_FDB_CLUSTER_FILE` | The scheme was given but no cluster file, and the env fallback is unset too (coordinators/TLS/machine_class are all in that file, not in the URI) | §6.3: `fdb://<cluster file>` or `fdb://?cluster=<path>`; when both are missing it reads `GROVEFS_FDB_CLUSTER_FILE` |
| `error: fdb open <路径>: No cluster file found in current directory or default location` | The cluster file pointed to by `fdb://` doesn't exist / isn't readable (the latter clause is `libfdb_c`'s own original text; grovefs only prepends `fdb open <路径>:`) | A wrong path is this class: on **the machine running the command** give a cluster file that really exists, or set `GROVEFS_FDB_CLUSTER_FILE` (§6.3) |
| The command **hangs and never returns**: the client only prints `using remote metadata service at …` then nothing, and exiting depends on an external timeout; on the `meta-server` side the banner still prints, the port still listens, but the log adds no more lines | **An unreachable cluster at this tier is not an error, it's a silent wait** — `libfdb_c` keeps retrying per its own policy when coordinators are unreachable, and this backend sets no cap on such waits (`docs/fdb-keyspace.md` §11 lists timeout/retry caps as deferrable). The server does **not probe the cluster at startup**, so "it came up" does not mean "it's connected" | The verdict lives on the cluster side: on the server machine `fdbcli -C <cluster file> --exec "status minimal"`; `The database is unavailable` is this class (given within seconds, unlike grovefs which waits). Once reachability is restored the parked op completes on its own and the mount needs no remount |
| `serialization retry exhausted after 12 attempts (view 'x')` | One op hit 12 consecutive collisions without committing (OCC abort, backoff 200µs→25ms exhausted). **PG and FDB share one driver wording** (`src/pg_store.rs:550`, `src/fdb_store.rs:1523`); there's no lock-wait notion at the FDB tier, so this line means the retry budget truly ran out | Investigate the contention surface, not the cluster: high-frequency `rename`/same-name `create` storms within one directory; on the PG side this string is measured in logs by the `pg_parallel` suite, and the FDB side uses the same constant and same branch |
| `view name is N bytes; the backends all cap it at 25 so no view is openable on one engine and unrepresentable on another` | The view name exceeds 25 bytes. FDB itself doesn't need this cap; the cap is carried over from PG's `schema_for` — to keep views migratable across the three backends (P8) | §15.9: switch to a view name ≤25 bytes |
| `refs … rebuilt: 0 holder entries` | You ran the **local-file** ledger rebuild (without `--meta-store`): it recomputes `meta/refs.db` from `<meta-dir>/meta/*.db`, and in a PG/FDB deployment those files don't exist, hence 0 rows | A backend deployment needs `--meta-store`: PG reports `refs ledger rebuilt in postgres://…: N tree(s) …`, FDB reports the same-shaped cluster ledger; the ledger content is in `grovefs_refs.holders` (§15.6/§15.9) |
| A write returns `EPERM` | A read-only key (including the `flock` class) or a derived view | §15.11 / §15.13 |
| Writing fork content returns `EROFS` | The fork's content plane is read-only | Write the base view; keep doing structural rearrangement inside the fork |
| Within 2 seconds after a mount, other clients don't see the change | Not a bug: TTL ≤2s; if it never syncs, check whether both places connect to the **same** meta | §4's "two consistency modes" |
| `connection refused` / handshake timeout | The firewall opened only one of them, or the tokens differ | §11.6, §11.7 acceptance checklists |

---

## 16. Internal-mechanism overview and extension development (for readers who change the code)

> The content below is evidenced line by line from **the source and the protocol constants**, not a conceptual primer. The rest of this manual uses only CLI/protocol behavior and does not depend on this section.

### 16.1 Extension points: three traits, one decorator

- **`MetaProvider`** (`src/meta.rs`, 22 methods): the full metadata-layer contract — `lookup`/`getattr`/`readdir`/`create_node`/`rename`/`lock_op`/`fence_handle`/`fetch_fence_epoch`… Four extension points carry default implementations: `readdir_page`/`supports_paged_readdir`/`display_targets`/`flush_listing` (the last being the "if you cache listings yourself, you must be flushable by an invalidation push" half of the contract). Six implementers: `MetaEngine` (local SQLite, one db per view), `PgEngine` (`src/pg_store.rs`, PG, one schema per view), `FdbStore` (`src/fdb_store.rs`, FoundationDB, one subspace per view; exists only in builds with feature `fdb`), `MetaStore` (enum, picks Sql/Pg/Fdb at runtime, meta-server server side), `RemoteMeta` (client), `LayoutMeta` (the governance decorator). Neither PG nor FDB is a new tree-engine semantic: `MetaEngine::open_sqlite`/`open_pg` and `MetaStore::open_fdb` swap the storage kernel behind the same method surface — so every feature automatically runs on all three backends, and the acceptance test is rerunning the same assertions with parameters swapped (§10).
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
2. Register the type at the factory: for the data plane refer to `backend::open_local`/`open_local_roots` (`src/backend/mod.rs`), for the metadata plane refer to how `MetaEngine::open_sqlite`/`open_pg` (`src/meta.rs`) and `MetaStore::open_fdb` (`src/meta/engine.rs`) are wired; **the full worked example of onboarding a new metadata backend is `src/fdb_store.rs`** (feature `fdb`: one subspace per view, a SERIALIZABLE transaction body + OCC retry loop, no connection pool hence no `checkout`/`requeue`, and why `drop_cursors` isn't needed here);
3. On the client side, if a new mount type is needed, wire it in at the mount dispatch point in `main.rs`; try to keep protocol opcodes unchanged — the frame layout and opcodes are fixed in `src/remote/proto.rs`, and any change must be upgraded in sync on both sides;
4. Regression: rerun the corresponding suite of `.build-bin/run_all.sh` with parameters swapped, taking the script output as the acceptance criterion (§10). The parameter-swap entry for the FDB tier is each script's top-level `*_BACKEND=fdb` + `.build-bin/fdb_scaffold.sh` (it starts/stops the server and **requires the port to be free before startup** — otherwise `wait_port` adopts an unfamiliar leftover server, making the next suite print red assertions unrelated to the code); semantic equivalence is additionally backstopped by the **three-tier differential comparison** `tests/fdb_tree.rs`: the same op sequence is run against `sqlite`/`postgres`/`foundationdb` respectively, first comparing replies op by op, then comparing that `dump_tree()` structures are isomorphic (row numbers are renumbered by a root traversal, so it compares the tree, not the id-issuance trail); if one tier is unreachable only the remaining tiers are compared, and if fewer than two exist it SKIPs itself rather than reddening the whole board.

### 16.4 Architectural trade-offs and development-side boundaries

Why it's designed this way (each point mapping to §4.1, §5, §5.5, §10, §15.9):

- **Single authoritative serialization point**: a view's tree is written by only one meta-server process — eliminating multi-replica divergence, at the cost of that process being the performance and scaling bottleneck. Accepted, until the demand actually appears.
- **Governance in the presentation layer, not the storage layer**: lenses/aliases/forks move no bytes, only changing the directory-tree presentation, so "same data, different shapes" is zero-copy.
- **Token and key layered**: the token manages the cluster admission gate, the key manages per-caller attribution and revocation — different leak-damage-control granularity; merging them would instead force small deployments to take on key operations.
- **async replication RPO≈500ms / `--sync-repl` RPO=0 / `--witness` real fencing**: the consistency tier is explicitly handed to the deployer to choose, at the cost of fencing capability being layered (without a witness, the standby only prevents split-brain but doesn't seal the old primary).
- **Striping at ≥16MiB whole-block round-robin**: sufficient and simple, avoiding metadata explosion; no arbitrary-granularity erasure coding.
- **Advisory locks are lease-based**: no wait timeout, no renewal task, falling back on lease + immediate reclamation on death — simplicity traded for a semantic boundary.
- **Throughput-first durability tiers**: the SQLite write connection's `wal_autocheckpoint=0` + a background checkpoint thread move sync stalls off the write hot path (§10 measured max 125ms→33ms); replication defaults to async (RPO≈500ms), `--sync-repl` explicitly trades for RPO=0 (cost: one control-channel RTT + one `VACUUM INTO` fsync, degrading honestly on timeout rather than misreporting, §5.5) — every tier states its cost.
- **PG/FDB backends don't support zero-copy `import` or native HA**: the top-level `import` relies on same-disk hardlinks, and the in-house HA relies on SQLite file-level snapshots; a remote db has no manageable local disk/snapshot path, so it's refused by name — an honest boundary (§15.9), not a bug. `fork`/`graft`/`view export`/`import --as-fork` all work on all three tiers (each copying the whole tree in its own storage, evidenced by `forksrv`/`xfer`).

Development reference: `src/` (10 main modules + `remote/` + `backend/`), protocol constants centralized in `src/remote/proto.rs`, telemetry in `src/stats.rs` (report grouping and phase names defer to the code), full-board regression entry `.build-bin/run_all.sh`. This section's content has a layered counterpart in the web docs.html under "§18 Internal mechanism and extension development / §19 Performance-tuning methodology".

---

## Appendix: command quick reference

| Purpose | Command |
|------|------|
| Create a view | `grovefs --home H init --view U [--manifest M]` |
| Local mount | `grovefs --home H mount --view U -m MP [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--foreground\|--log-file F]` |
| Remote mount | `grovefs mount --view U -m MP --data-servers A[,A2] --meta-server M[,M_bak] [--token T] [--api-key ID:SECRET] [--run-as-uid N] [--layout F] [--foreground\|--log-file F]` |
| Define a derived view | `grovefs view --meta-dir D define --name V --base B --rules F` (stored at `D/meta/views/V.layout`, `mount --view V` gets the shape; virtual-view mounts are read-only, writes go through the base name `B`) |
| Define a pure alias | `grovefs view --meta-dir D alias --name A --real R` (public name maps 1:1 to the real view: `A` is full-tree writable, `R` can't be mounted directly, §3.4) |
| Create a fork view | `grovefs view --meta-dir D fork --name L --base R` (copies directory entries into an independent `meta/L.db`, bytes shared; structure writable, content `EROFS`, §3.4.1) |
| Export/import a single view's metadata | `grovefs view --meta-dir D export --view V --out F [--force]` (host-agnostic JSONL, no bytes) / `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` (atomic replace, bytes copied separately to `blocks/`, §3.4.2) |
| Rotate a public name after a leak | `grovefs view --meta-dir D alias --name A2 --real R --retire A` (write-before-delete, zero window: `A` invalidated, `R` never directly mountable throughout, §3.4) |
| List/delete view definitions | `grovefs view --meta-dir D list` (definition slots + bare base views) / `grovefs view --meta-dir D rm --name V` (derived/alias delete only the definition, leaving data; fork deletes its own `meta/V.db` too and rebuilds the ledger) |
| Delete a whole base view (with byte reclamation) | `grovefs view --meta-dir D rm --name V --base [--data-dir R]` (deletes `meta/V.db`+WAL/SHM, `refs rebuild` rebuilds the ledger, reclaims bytes no other view still holds; refuses governed names / dependent definitions / live local mounts; on the PG tier add `--meta-store postgres://…`, with unbooking + `DROP SCHEMA` in the same transaction, §3.4.3) |
| Investigate/reclaim orphan objects (offline) | `grovefs gc --meta-dir D [--data-dir R] [--apply] [--older-than 24h] [--json]` (scan-only by default; `--apply` requires no live mount on this storage and no grovefs process on this machine, never deletes keys the ledger still holds, refuses PG deployments, §12.6) |
| Import an existing directory | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]` (same-disk hardlink zero-copy, rerun incremental; run on the storage machine, §3.3) |
| Server-side usage statistics | `grovefs du --home H --view V [--data-servers A[,A2]\|--data-dir R] [--path P] [-d N] [-b] [--total]` (batch-fetches and sums object bytes, not per-file stat; byte source `--data-servers` > `--data-dir` > metadata root, refuses outright if the source can't be determined, counting a warning only on a confirmed miss, §12.6) |
| Unmount | `grovefs umount -m MP` |
| Data service | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| Govern the same metadata from any machine (using an account key instead of the db password) | `grovefs view --control-server H --api-key K create\|define\|alias\|list\|rm` (with `--meta-store` replaced by `--control-server`, same as above) |
| Account key (governance port) | `grovefs --home H apikey create --label L --for admin --owner ACCT` (the first key minted on the disk-holding host) / `apikey --control-server H --api-key K list\|create\|revoke\|set-ro\|calls` |
| Open a remote governance port | `grovefs meta-server --meta-dir D --listen IP:PORT --control-listen IP:PORT` (the second port runs only governance verbs, accepts only admin keys, audits per call) |
| Metadata service (PostgreSQL backend) | `grovefs meta-server --meta-dir D --listen IP:PORT --meta-store postgres://user:pw@host:5432/db` |
| Mint an access key | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount\|admin] [--owner ACCT] [--view V] [--ro]` (plaintext printed once; `mount` = opens only mount/data plane, `mcp` = opens only the governance interface, `admin` = opens the governance port and admits the prior two, `--owner` required) |
| List/revoke/audit keys | `grovefs apikey --meta-dir D list [--view V]` (with `--view`, lists only keys that can open V; a pure alias also matches keys bound to the real name) / `revoke --id ID` (exact id or unique prefix; revoking seals the key's **live mounts**) / `calls --limit N` (includes rejected mount connections) |

