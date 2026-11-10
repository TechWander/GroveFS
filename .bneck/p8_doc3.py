#!/usr/bin/env python3
# P8-d: docs/fdb-keyspace.md 取证段 v1.4 / v1.5（含 v1.3 补 P5-b/P5-c 双档数字）。
#
# This is the phase log the file's own convention asks for: every implementation
# phase appends its deviations (C-numbered), the evidence that proves each one, and
# the dual-tier assertion counts, so a reader can tell design from delivered fact.
# Numbers below are read off the suite SUMMARY lines, not from memory.
import io

P = '/data/grovefs/grovefs/docs/fdb-keyspace.md'
s = io.open(P, encoding='utf-8').read()
orig = s


def rep(old, new, n=1):
    global s
    assert s.count(old) == n, 'anchor x%d: %r' % (s.count(old), old[:70])
    s = s.replace(old, new, n)


# ---- §4：把"迁移续接"从待交付改成已交付，并给出落点 ----------------------
rep('''  - 迁移续接：外部导入时若携带原 `id`/`ino`，分配器键须 `max(现有)` 续到
    （对位 PG 的 `pg_get_serial_sequence` + `setval`，P8 交付项）。''',
    '''  - 迁移续接：外部导入时若携带原 `id`/`ino`，分配器键须 `max(现有)` 续到
    （对位 PG 的 `pg_store.rs:464` 的 `setval`）——**P8 已交付**：`Tx::raise_to_max`
    （`src/fdb_store.rs:916`）在导入事务内把 `(v,'m','id')` 与 `(v,'m','ino')` 抬到导入行的
    最大值（`src/fdb_store.rs:2980-2981`），判据不是"看起来续上了"，而是**导入后新建节点的 id
    严格大于导入侧最大 id、且全树 id 无重复**（`test_fdb.sh` §12c）。''')

# ---- v1.3 段尾：补 P5-b/P5-c 的双档数字 ---------------------------------
rep('''同阶段核对 P2 两项旧裁决仍成立：① `--meta-store fdb://…` 与 `--role standby`/`--peer`/`--witness`
同时给时 fail-fast，且 FDB 的文案独立（`src/remote/meta/server_ext.rs:586`/`597`，单测覆盖
`FDB + --peer` 与 `FDB + --role standby` 两种给法）；② 派生/别名/只读契约与治理写路径全部走
§3.10 的 I1–I4 论证，未因换地基新增任何例外机制。''',
    '''同阶段核对 P2 两项旧裁决仍成立：① `--meta-store fdb://…` 与 `--role standby`/`--peer`/`--witness`
同时给时 fail-fast，且 FDB 的文案独立（`src/remote/meta/server_ext.rs:586`/`597`，单测覆盖
`FDB + --peer` 与 `FDB + --role standby` 两种给法）；② 派生/别名/只读契约与治理写路径全部走
§3.10 的 I1–I4 论证，未因换地基新增任何例外机制。

**P5-b/P5-c 取证（四套治理套件换后端参数复跑，双档数字）**：套件顶部的 `*_BACKEND=fdb` 把
"挂载够到哪棵树、定义写进哪个控制面、服务监听哪个端口"变成参数，**断言本身不为后端改写**；
部署由 `.build-bin/fdb_scaffold.sh` 供给（`fdb_gate`/`fdb_binary`/`fdb_up`/`mnt_gate`/`kp`/`wk`）。

| 套件 | SQLite 档 | FDB 档 | FDB 档新增的那一条边界 |
|---|---|---|---|
| `test_layout.sh` | 78/0 | 82/0 | 派生名在 `view list` 里可见但**不列举 subspace**（C6 的另一面：定义是文件、树是 subspace） |
| `test_govtime.sh` | 8/0 | 10/0 | 改定义热生效走的是文件 mtime，与集群无关 |
| `test_apikey.sh` | 33/0 | 34/0 | key 与审计落 `meta/api_keys.db`，逐条与 SQLite 档一致 = C6 的直接证据 |
| `test_govcheck.sh` | 30/0 | 29/0 | I1–I4 的 B4 行在此档读作"本地存储 yes / `fdb://` 拒绝并给替代"，是**提供位置的边界**，不是后端违反不变量 |

§3.10 的 I1–I4 对照表即按这四套的 fdb 档逐条复核，未出现需要"放宽不变量"才能通过的项。''')

# ---- v1.4 / v1.5 ---------------------------------------------------------
s = s.rstrip('\n') + '''

### v1.4（P6 台架与 P7 文档口径）

P6 把 `fdb` 套件登记进 `run_all.sh`（**板上只登记这一套**：其余 `*_BACKEND=fdb` 档按 P4/P5 的先例
在板外跑，因为它们要独占同一台机器上的集群与端口，串进板内只会互相撞车）。全板 GREEN（34 套，
含 `fdb` PASS）。P7 把三后端口径写进 `USAGE.md` §6.3/§11.10/§15.9 与 `docs.html` §5.9/§14.x/§16，
`bash skills/grovefs-docs/scripts/doccheck.sh` 取 `EXIT=0`（变更史叙述 0 项、参数差集 0 项、
docs.html 标签闭合 0 错）。

一处**口径事实更正**（不编 C 号，因为它改的是文档而非机制）：P7 首版把 `view export`/`view import`
列进了 FDB 档的"拒绝"清单，而 `src/main.rs:2223`/`2296` 的判定是 `if !store_is_fdb { require_sqlite(..) }`
——这对命令在 `fdb://` 下**本来就可用**。P8 的实现与三套双档取证把它坐实，两份文档同步改写（v1.5）。

### v1.5（P8 迁移与回滚双向）

Spec 的判据是"不新建命令"：搬运走既有的 `view export`/`view import`，回滚是同一对命令反向跑。
两处实现期偏离值得留名，因为都不是设计时能推演出来的：

| # | 修订 | 取证依据 | 影响面 |
|---|---|---|---|
| **C8** | **dump 的便携根名 `/` 不是 FDB 里根键的存储名**：导入必须按 `(ROOT_PARENT, "")` 写根的名字键，并把 `('i',1)` 指向它（`root_entry_name`/`dir_entry_of`），而不是照抄 JSONL 里的 `/` | `src/fdb_store.rs:2718`（`root_entry_name`）与导入循环里 `('i',ino)` 的三分支（目录取 `dir_entry_of`、文件取 `first_holder`）；`test_fdb.sh` §12a 直接断言挂载根 `stat -c %i == 1` | 写错不是"根显示成 `/`"这种外观问题，而是**整棵树挂载后 getattr 全 ENOENT 而所有键都在**——最难归因的一类缺陷。§3.2 的 `'i` 指针语义因此补一条：指针值必须来自键空间自己的存储名，不得来自导出格式 |
| **C9** | **只读路径绝不开出结构**：`view export` 在 fdb 档以 `FdbStore::open(uri, view, read_only = true)` 打开，名字没有 subspace 时按名拒绝（`not a provisioned subspace`）且不落空 dump | `src/main.rs:2252-2265` 的只读探针分支；`test_fdb.sh` §12d、`test_migrate.sh` §8（拒绝后 `meta/views/<name>.layout` 也没写） | 若走普通 `open(...)`，`view create` 作为唯一建树入口（D1/决策表）就被一次导出悄悄破掉；该判据此后列入"任何新增只读 op 的固定检查项" |

另有一处**与键空间无关但必须留档的生命周期事实**（P8 期间修掉的真缺陷）：短命 CLI 进程在
FDB 网络线程尚未起来时就退出，会在 `fdb_shutdown_network` 之前析构顺序不定 → SIGSEGV。
修法是 `src/fdb_store.rs:195/246` 的静态 `NETWORK_GUARD`（`OnceLock<Mutex<Option<NetworkAutoStop>>>`）
+ `src/main.rs:1299` 在 flush 之后显式 `shutdown_network()`；判据是连续 100 次 `--meta-store fdb://…`
一次性命令 0 crash。教训：**"一 view 一 subspace"让每个 CLI 调用都成为一次独立的 FDB 客户端生命周期**，
网络启动/关闭的次序在库形态（服务端常驻）下不成问题，在 CLI 形态下就成了必守约束。

**双档取证（P8 三套，SUMMARY 原文）**：

| 套件 | SQLite 档 | FDB 档 | 迁移链的判据 |
|---|---|---|---|
| `test_fdb.sh` | —（本套件只在 fdb 上跑） | 158/0 | §12：SQLite→subspace→SQLite 全链，正向 `diff` 逐行相同、挂载根 `%i == 1`、12c 水位续接、12d 守卫（`--replace`/ghost/坏格式/悬挂父节点，且失败的 `--replace` 不伤已装树）、12e 回滚同 dump、12f 源存储未被写入 |
| `test_migrate.sh` | 74/0 | 71/0 | 同一套断言换后端：`--as-fork` 在此档按 subspace 边界拒绝（点名 flag、点名缺失、给出 alias/derived 替代、并证明拒绝未开 subspace 也未写定义槽）；一部署一对象存储 ⇒ **零字节搬运**；四个基 subspace 的边界断言 |
| `test_import.sh` | 35/0 | 47/0 | 顶层 `import` 在 `fdb://` 拒绝（含 `--dry-run` 之前即拒、`GROVEFS_META_STORE` 同拒、两处根都未落 db、源检查先于后端检查），并把拒绝点名的三步替代路线端到端跑通：暂存 `import` → `view export` → 交字节 → `view import` → 挂载保真 → 派生视图组合 → re-move 幂等 |

搬运**失去**的东西同样写进判据而不是藏起来：`import` 的零拷贝是"本地存储 + 同盘硬链接"的性质，
跨后端搬进集群后视图读的是部署自己的对象，两侧编辑互不可见——`test_import.sh` fdb 档 §3 对
两个方向各做一条负向断言。`MetaStore::backup_to_bytes` 的第三臂（`src/fdb_store.rs:1462-1467`）
如实报 unsupported 并指向 `fdbbackup`，不复用 PG 文案，`test_fdb.sh` 的 `--role standby` 段实证。

**台架教训（两条，都会让整套件假失败或污染下一套）**：① 双引号字符串里的反引号**会做命令替换**——
`ok` 的一句文案里用双引号包了反引号围起的 `view create`，于是断言执行时真的拉起了 `view`
（vim 只读模式）并阻塞等终端，把 fdb 档挂到超时；文案里的反引号要么转义、要么整句放进单引号
（`test_govcheck.sh:312` 同批修正）。② `set -u` 下
EXIT trap 引用未绑定变量会**中断 trap 本身**，实测泄漏 3 个挂载 + 2 个监听端口给下一个套件；
trap 内一律 `${VAR:-}`（`test_import.sh` 的 `cleanup` 已按此改写并注明后果）。
'''

io.open(P, 'w', encoding='utf-8').write(s)
print('keyspace p8-doc ok: %d -> %d bytes' % (len(orig), len(s)))
