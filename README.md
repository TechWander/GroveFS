# GroveFS

> **用户视图 FUSE 文件系统** —— 数据集中存放于硬盘，不同用户挂载各自看到**独立的文件树**（支持自定义层级），彻底脱离 NFS。
>
> **A user-view FUSE filesystem** — bytes live centrally on disk while each user mounts an **independent, private directory tree** (with full nesting); no NFS.

[中文](#中文) · [English](#english)

---

<a id="中文"></a>
# 中文

## 是什么

`GroveFS` 把「谁能看到什么（视图）」与「数据实际存在哪（对象）」彻底分开：

- **视图 view**：每个用户一棵独立目录树，存为一个 SQLite 库 `<home>/meta/<view>.db`。不同用户互不可见。
- **数据对象 object**：文件的真实字节，统一存在 `blocks/` 下、以 UUID 命名的扁平对象里；视图目录项只持有对象 key（指针）。
- **基目录 `--home`**：同时容纳 `blocks/`（数据）与 `meta/`（视图）的物理根。

在此之上，`GroveFS` 可把元数据与数据分别服务化（`meta-server` / `data-server`），让**多台客户机经局域网挂载、共享同一份权威视图**——这才是「数据中心存储 + 机房内 LAN 访问」的落地形态，且**不使用 NFS**。

## 核心特性

- **多用户视图隔离**：一用户一棵树，跨用户不可见；完整的 `mkdir`/`mknod`/`rename`/`rmdir` 自定义层级。
- **可插拔存储/元数据后端**（trait 抽象）：本地 `LocalBackend`/`MetaEngine`、远程 `RemoteBackend`/`RemoteMeta`、跨多节点条带 `ShardedBackend`，对上层透明。元数据权威存储亦可切换为 **PostgreSQL**（`meta-server --store postgres://…`）：服务端 schema-per-view、事务化 rename/unlink/lock；此模式下自研 HA 有意关闭，交给 PG 自己的流复制/PITR。
- **视图级 uid 白名单（可选）**：`grove acl allow/revoke/list` 管理 `<home>/meta/acl.db` 的 `view_acl(view,uid,can_read,can_write)`。未列入 = 无约束（历史行为不变）；一旦有行 → 读门在 `M_OPEN` 处 `E_AUTH` 拒挂载、写门每次写意图 op 前动态重查并回 `E_NOTPERM`（客户端 FUSE 侧落 `EPERM`；有意区别于 HA 备机 `E_ROFS` 故障转移信号），故 `acl revoke` 对已建立的挂载连接**即时生效**、无需重挂。同一 view 即多人共享同一棵权威树，per-uid 读写分别放行。
- **自定义二进制帧 over TCP**（非 NFS/gRPC）：定长头 + 变长体，连接池并发。
- **跨客户端实时失效推送**（默认）：一端写成功，中心经事件总线向同用户其它订阅者广播 `inval_entry`+`inval_inode`，对端下次访问**即时可见**，无需轮询；`--strict` 提供零缓存兜底。
- **分布式 advisory 锁**：`flock` / `fcntl F_SETLK/F_GETLK` 经中心 `locks` 表集中仲裁，跨进程、跨机生效；崩溃即时回收（FUSE `release`）+ 短 lease（默认 15s）+ 续约心跳。
- **高可用主备**：`meta-server --role primary|standby --peer` 心跳/epoch 提升/自动降级；`--sync-repl` 把写 RPO 降到 0；`--witness` 取逗号分隔的 N 个共享 `data-server` 作**独立仲裁者集合**（推荐奇数），**fail-safe、持不重叠的有界多数派租约（需 `N/2+1` 授予；多数活租约即独占证明，写由本地租约校验放行）并对内容数据帧按 token 门控，把双写窗口收敛到租约安全余量内**；宕少数仲裁者仍不冻结写（N=1 退化为单仲裁旧行为）。
- **Token 双面鉴权**、**只读挂载**（`--ro`）、多线程 `fuser 0.18`（`clone_fd` 并发派发）、**分块并行 IO** 与 **跨 server 条带**。
- **完整回归**：`run_all.sh` **16 套件全板 GREEN**（新增 `acl` view 白名单 e2e、`pg` PostgreSQL 后端 e2e；`pg` 会在无 PG 主机上自动 SKIP）。

## 架构与原理

三层解耦，全走 trait：

```
┌─ FUSE 挂载进程  (fs_impl.rs: GroveFS，&self 多线程安全)
│    ├── MetaProvider trait   视图树访问
│    │     ├── 本地:  MetaEngine  → <home>/meta/<view>.db
│    │     └── 远程:  RemoteMeta  → meta-server（中心权威）
│    └── StorageBackend trait 数据对象 IO
│          ├── 本地:  LocalBackend  → <home>/blocks/<key>
│          ├── 远程:  RemoteBackend → data-server
│          └── 条带:  ShardedBackend → 多 data-server
└─ 传输：自定义二进制帧 over TCP（定长头 + 变长体），连接池并发
```

- **trait 接缝可插拔**：本地/远程/条带切换只是换实现。
- **中心 MetaServer**：同一用户在多客户机共享**一份权威视图**，不再各存副本导致分叉。
- **事件总线 + Notifier**：写成功后向订阅者推送失效，保证内容新鲜度。
- **fuser 0.18 多线程**：每线程独立 `/dev/fuse` fd（`clone_fd`，Linux 4.5+），请求并发派发不串行。

## 多机共享与存储选型（原理与常见疑问）

> 这一节回答最容易混淆的两个问题：① 「多台服务器经局域网共享同一棵目录树」**要不要 PostgreSQL**？② PG 到底**跑在哪一侧**？

### 一次写请求的完整路径（数据面 / 元数据面各走一半）

GroveFS 把「文件的字节」与「目录树」拆成两条独立通道，这也是它能脱离 NFS 的根本：

```
应用 write()/close()
   │
   ├─ 数据面 ─→ RemoteBackend ─TCP帧→ data-server ─→ blocks/<key>（UUID 命名、扁平存）
   │            （≥4MiB 自动分片并发；大文件可跨多台 data-server 条带）
   │
   └─ 元数据面 ─→ RemoteMeta ─TCP帧→ meta-server ─→ 权威视图树（默认 SQLite，或 PostgreSQL）
                │  （create / rename / setattr / touch_mtime …）
                └─ 写成功后经事件总线向同 view 的其它订阅者推 inval_entry + inval_inode
                                          → 对端下次 getattr/lookup 命中失效、向中心重取 → 即时可见
```

- **谁持有权威**：`meta-server` 是**单一权威串行点**——同一 view 的一棵树只由这一个进程落到存储引擎（SQLite 一 view 一 `.db`，PG 一 view 一 schema），所有客户端经 TCP 帧汇聚到它。这是**有意的设计**（杜绝各存副本导致的视图分叉），不是疏漏。
- **客户端从不直接碰存储**：客户机既不开 `.db` 文件、也不持 PG 连接串，只会说 GroveFS 的线协议。后端是 SQLite 还是 PG，对客户端**完全透明**、命令一字不变。

### 常见疑问

**Q：多台服务器经局域网挂载同一棵目录树，数据库是不是必须上 PG？**
**不需要。**「多机共享一棵树」的能力来自 **`meta-server` + 相同 `--view`**，与底层是 SQLite 还是 PG **无关**。默认的 embedded SQLite（`rusqlite bundled` 编译进二进制、零外部依赖）本来就是为这个拓扑设计的——把 `data-server` + `meta-server` 部署在中心节点，多台客户机 `mount --view alice --meta <中心>:50110 --data <中心>:50100` 即共享同一棵权威树、跨端实时可见。这一整套由 `metasrv` / `freshness` / `locks` 套件在纯 SQLite 后端上证明,**不需要先装 PostgreSQL**。

**Q：那 PG 到底跑在哪一侧？**
**只在服务端**（`meta-server` 那一侧），是它的**可插拔权威存储后端**：

```
客户端(仅 grove 二进制)      服务端                    数据层
mount --view alice   ─TCP→   meta-server   ─postgres://→  PostgreSQL 服务器
  · 无 PG 库/凭据/感知        · --store 是它的启动参数     (与 meta-server 同机或独立 DB 机)
```

`--store postgres://…` 只出现在 `meta-server` 子命令上，客户端没有这个选项、也拿不到连接串。PG 服务器进程物理上跑在中心（可与 meta-server 同机走 loopback，或独立一台 DB 机经局域网连），但**始终在服务端**。

**Q：那什么时候才值得上 PG？**
当你想把权威树的**持久化 / 高可用 / 备份 / 集中运维**交给一个成熟数据库时:PG 自带流复制 / logical replication / PITR、`pg_basebackup`、监控与 DBA 工具链,海量元数据(百万级节点)也可借 PG 的索引/VACUUM 调优。注意**自研 HA(快照复制)与 PG 互斥**(见「共享视图与权限模型」),上了 PG 就用 PG 自己的复制、不再用 GroveFS 的文件级快照。

> **诚实边界**：上 PG **不会**把元数据写并行化——`meta-server` 作为单一写者,PG 模式下内部仍是单连接串行。PG 的价值在**持久化/复制/运维成熟度**,不在写吞吐扩展;真正的元数据写并发扩展是更大的架构命题,当前未做。默认场景请继续用零依赖的 SQLite。

## 构建

**前置**（Linux）：Rust（edition 2021）、`pkg-config`、libfuse 开发环境、`/dev/fuse`。元数据引擎 `rusqlite` 用 `bundled` 特性从源码编译 SQLite，**无系统依赖**。

```bash
cargo build --release          # 产物：target/release/grove
```

> 注：本仓库在**缺少系统 pkg-config** 的环境里开发，构建脚本通过仓库自带的 shim（`.build-bin/pkg-config`）驱动，故跑测试前需：
> ```bash
> export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
> ```
> 若你的环境已有正常 `pkg-config` 与 libfuse，直接 `cargo build --release` 即可。

## 快速上手（单机本地模式）

数据和视图都在同一个 `--home`，无需任何 server：

```bash
Y=./target/release/grove
BASE=/data/myfs

# 1) 初始化两个用户的视图
$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

# 2) 挂载（前台运行，Ctrl-C 即卸载；或加 & 后台）
$Y --home $BASE mount --view alice --mountpoint /mnt/alice
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob   # 另一终端

# 3) 像普通目录一样读写、建自定义层级
mkdir -p /mnt/alice/projects/2026
echo "年度报告" > /mnt/alice/projects/2026/报告.txt
ls /mnt/bob                               # bob 看不到 alice 的文件

# 4) 卸载
$Y umount --mountpoint /mnt/alice
```

用 manifest 预建层级：`init --view alice --manifest seed.txt`（每行 `mkdir <path>` 或 `touch <path>`，`#` 注释，幂等）。

## 关于 `--view` 与路径容量

- **`--view` 是视图命名空间的标签，与 Linux 用户名无关**：任意合法文件名字符串都可以（如 `alice`、`team-a`、`u42`、`研究组`）——它只用来 ① 生成 `<home>/meta/<view>.db`，② 在 FUSE 挂载上打 `grovefs:<view>` 标签。二进制不调 `getpwnam`、不做 uid 校验、不需要 `useradd`。**权限来自「能访问 `--home` 与 `blocks/`」+ 可选 `--token`**，不来自 Linux 用户身份。同 `--home` 下不同 `--view` = 不同视图，跨用户不可见。
- **一个用户一棵树，树内路径完全自主**：挂载后该 mountpoint 下可 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意自定义层级，与在自己 home 里的操作一致；视图隔离的粒度是「整棵树」而非「单一路径」。`--manifest` 只是预建骨架的便捷入口，不是路径数量的上限。

## 共享视图与权限模型

GroveFS 的权限栈只有两层——**令牌 = 集群准入，view 名 = 树选择**。`--view` 字符串本身不是身份：任何 uid 只要挂同一个 `--meta <addr>` + `--view alice` 都会落到 alice 的同一棵权威树上，看到彼此写入。这是有意的，也让「多人共享一个视图」开箱即用：

- **多人共享同一 view**：多台机器、不同 OS 用户，都 `mount --view alice --meta ... --data ...` → 元数据经 `meta-server` 集中（一棵 SQLite/PG 树，串行化写）、内容经 `data-server` 集中（一份 `blocks/`）；`ls` 或 `cat` 立刻跨端可见，实时失效推送保证收敛。若希望**不同 uid 有不同权限**（例如只读旁观者），加白名单：

  ```bash
  # 中心节点上：给 alice 视图开白名单
  $Y --home /data/cluster acl allow --view alice --uid 1000 --read --write   # rw 协作者
  $Y --home /data/cluster acl allow --view alice --uid 2000 --read           # 只读订阅者
  $Y --home /data/cluster acl list --view alice                              # 查看已授权的 uid
  $Y --home /data/cluster acl revoke --view alice --uid 2000                 # 撤销单条
  ```

  `view_acl(view, uid, can_read, can_write)` 语义：**无行 = 无约束**（任何 uid 挂同一个 view 都放行，向后兼容旧行为）；**有任意行 = 白名单**（该 uid 无匹配行则 `M_OPEN` 直接被拒 `E_AUTH`，挂载 fail-fast；只读授权的 uid 在 op 循环里对写意图 op 逐条拒 `E_NOTPERM`——客户端 FUSE 侧落 `EPERM`，有意区别于 HA 备机的 `E_ROFS` 故障转移信号，以免一次权限拒绝触发伪切换）。写门**每个 op 前动态重查**（不在 `M_OPEN` 缓存），故 `acl revoke`/降级对已建立的连接即时生效。本地模式（无 `--meta`）也走读门校验，写门由 OS 文件权限兜底（view 本地 SQLite 文件属主即最后一道防线）。客户端上报的 uid 默认取 `geteuid()`，也允许 `mount --as-uid N` 显式覆盖，用于共享 `meta-server` 上按角色挂载。

  > **诚实边界**：TCP 客户端自报 uid 属**advisory**（客户端可伪造），真正的准入仍由 `--token` 把关；白名单是给一个已通过 token 的集群内部做**分工/职责**用的，不是防内鬼。本地模式（无 `--meta`）的写门只能靠 OS 权限（同 uid 无法强制隔离）。

- **元数据后端可切换：SQLite ↔ PostgreSQL**（仅指 `meta-server` 权威存储）：

  ```bash
  # 默认（embedded SQLite，一 view 一 `<home>/meta/<view>.db`）
  $Y --home /data/cluster meta-server --listen 0.0.0.0:50110

  # PostgreSQL：一 view 一个 schema `grovefs_v_<hex(view)>`，事务化 rename/unlink/lock
  $Y --home /data/cluster meta-server --listen 0.0.0.0:50110 \
       --store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  客户端挂载命令**完全一致**（仍 `--meta host:port --view alice`）——线协议不变。注意：
  - **PG 模式与自研 HA 互斥**：`--store postgres://…` + 任一 `--role standby / --peer / --witness` 在启动时直接 fail-fast（`VACUUM INTO` 快照复制是 SQLite 特性）；PG 上 HA 请交给 PostgreSQL 自己的**流复制/logical replication/PITR**——那本来就是比文件级快照更强的机制。
  - `acl.db` 白名单在 PG 模式下**依然落本地 SQLite**（`<home>/meta/acl.db`）：它是控制面准入状态，不是 view 树的一部分，不应跟 view 一起进入 PG 复制域。
  - 建表由服务端在 view 首次 `M_OPEN` 时幂等 `CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS` 完成，不需要预迁移脚本；根节点固定 id=1，`setval(pg_get_serial_sequence(...), MAX(id))` 对齐序列，`ROOT_INO` 语义与 SQLite 完全一致。

  为什么没做 Redis：Redis 的 K/V+TTL 模型与视图树（rename/unlink/hardlink/refcount GC 需事务、锁需要行级仲裁）不同构——把 relational 语义硬塞到 Redis 里等于在应用层重做一个 PG，收益为负。**GroveFS 只加 PG，不加 Redis**。

## 生产拓扑：中心存储 + 多客户机 LAN 挂载

```bash
# ===== 中心节点 =====
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster/blocks
$Y --home /data/cluster meta-server --listen 0.0.0.0:50110

# ===== 每台客户机（本地 --home 只放临时缓存，不留权威视图副本）=====
$Y --home /local/home mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 /mnt/alice
```

| 选项 | 作用 |
| --- | --- |
| `--data host:port` | 文件 IO 走网络到 `data-server`，不碰客户机本地盘（非 NFS） |
| `--meta host:port` | 视图由中心 `meta-server` 提供 → 多客户机挂同一用户 = 共享同一棵权威树 |
| `--data a,b,c` | 逗号分隔多地址 → 启用跨 server 确定性条带 |
| `--meta 主,备` | 故障转移列表，主挂自动切备 |

## 跨客户端一致性

| 模式 | 命令 | 行为 |
| --- | --- | --- |
| **实时失效推送**（默认，推荐） | 仅 `--meta` | 一客户端写成功后，meta-server 经事件总线向同用户其它订阅者广播失效（`inval_entry`+`inval_inode`），对端下次访问**即时可见**，无需轮询 |
| `--strict` 强一致兜底 | 加 `--strict` | 关闭内核属性/dentry 缓存并 `FOPEN_DIRECT_IO` 直读，写立即可见。代价：零客户端缓存、往返更多 |

**失效须成对推送**：`inval_entry`（目录项）与 `inval_inode`（inode）一起发，否则对端可能拿到陈旧属性。默认推送失效模型已保证「A 写 → B 即时可见」，`--strict` 只是在极端竞争窗口下选择完全旁路内核缓存的更保守兜底。

## 分布式 advisory 锁

`flock` / `fcntl F_SETLK/F_GETLK` 经 meta-server 的 `locks` 表（SQLite）集中仲裁，**跨进程、跨机生效**；主备切换后锁状态随快照复制存活。锁回收两条路径：

1. **持锁进程崩溃**——内核关闭其 fd 触发 FUSE `release`（携带匹配的 `lock_owner`），锁行**即时删除**，远小于 lease。
2. **mount daemon 整机崩溃**——无 `release` 也无续约，锁在一个**短 lease（默认 15s）内**过期被扫。

存活持有者由 mount daemon 的**续约心跳**（每 lease/3 bump `expires_at`）持续续期，**永不超时**。

> **不能靠 pid 判活**：`( flock -x 200; sleep N )` 的 `setlk` pid 是瞬时 helper（加锁即退，锁活在继承的 open-file-description 上），用 pid 存活判定会误杀活锁；`flush` 每次 close 都 fire 且 owner 与 setlk 不一致，不能用来回收。只有 `release`（last-close）是持有者终止的权威信号。非合作应用（cp/dd）仍不阻塞，与 NFS/SMB advisory 语义一致。

## 高可用：meta-server 主备

```bash
# 主：读写，并把快照周期推送给备
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# 备：只读服务，主心跳丢失后自动提升为 primary
$Y --home /data/nodeB meta-server --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主（可选 RPO=0）：加 --sync-repl，写返回前同步等备落盘 ACK
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# 真 fencing（可选）：加 --witness <逗号分隔的主备都可达的 data-server 仲裁者列表，推荐奇数 N>
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
# 客户端故障转移列表
$Y --home /local/home mount --view alice --meta 10.0.0.5:50110,10.0.0.6:50111 --data 10.0.0.5:50100 /mnt/alice
```

- 两节点必须用**各自独立的 `--home`**（各存一份视图副本，节点间靠快照复制）。
- primary 每 ~500ms 推快照 + 心跳 epoch；standby 心跳超时后提升并 epoch+1，旧主恢复发现更高 epoch 自动降级。
- **`--sync-repl`（opt-in）**：写 op 返回前同步推快照并等备 `fsync` + 原子 rename 的 ACK，把 **RPO 从 ~500ms 降到 0**。备不可达/超时时**诚实降级为 async 并打 warn**（不谎报 0）。
- **`--witness`（opt-in，fail-safe fencing）**：用一个或多个主备都能访问的 `data-server` 作**独立仲裁者集合**（逗号分隔，推荐奇数 N；N=1 退化为单仲裁、行为与旧版完全一致）。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予（meta 侧对全列表并发 fan-out 并按多数派判决 `witness_quorum`；两个多数派集合必交于≥一个仲裁者，该仲裁者只把 token 顶高给第一个 claimer，第二个见 `prev==我` 只算续约不算顶高，故同值碰撞天然只有一主）。每个仲裁者维护单调 `fence` token（`OP_FENCE` 的 `fetch_max` CAS 回传**折入前的值 `prev`**）：claim 赢当且仅当 `prev < 我的 epoch`，`prev >= 我` 则被 fence（对端胜出或同值碰撞）——回传 `prev` 正是为了在仲裁者恢复时区分同值碰撞、绝不双主。在任 primary 每 tick 向多数派续一份有界租约（lease，默认 3s；多数报 `prev>我` 则自降级），每个仲裁者拒绝重叠租约——同一时刻至多一份活租约，故多数活租约本身即独占权威的证明；提升方亦非重叠，须待前任租约到期才可抢占（`prev>=candidate` 同值碰撞/对端更高，或前任租约尚活则拒绝/延后，旧主可证明地已停）。**FAIL-SAFE**：配置了 witness 却**组不到租约多数派**连续超过宽限（3s）时，节点放弃写权限——primary 自降级、standby 拒绝提升（**写冻结**），宁牺牲可用也不冒双写。各仲裁者独立持久化 token 到 `<root>/.fence_token`，其重启不清零、不复活过期 primary。**相比单仲裁的可用性红利**：宕掉**少数**仲裁者仍能组多数续租、**不冻结写**；少数仲裁者的 `.fence_token` 盘丢也不复活过期主（多数仍保单调 token）；唯**失去多数**才 fail-safe 冻结。写请求由 primary 的**本地租约有效性**直接放行（不再每写一个仲裁 RTT，仅每续租一次向全列表 fan-out，摊薄到整窗），被 fence 的过期主在租约到点后的下一次复证即拒绝；元数据面复证拦不住内容字节（`write()` 先落 backend、再 debounced touch meta），故客户端把实时 epoch 贴到每个写意图数据帧（create/write/truncate/remove/fsync），作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`，过期主无法绕过元数据面静默写入内容（被封帧向权威重取 epoch 并自动重试一次）；且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化（claim 取独占锁会排在途写排空，而 witness 与存储同节点），故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合（数据面门控仅对 `--witness` 列表内的 data-server 生效，故应把 witness 与客户端实际写入的内容盘同节点部署；列表外的内容盘 token 恒为 0、数据面 gate 不生效，仅剩元数据面 fence 兜底）。两面合起来把双写窗口收敛到**租约安全余量内**（`WITNESS_LEASE_SLACK`=500ms：对端只能在我方租约到点后取 token，我方提前该余量停止信任缓存租约；而非 peer-only 的不封顶）。

## 鉴权 / 只读 / 性能

**Token 双面鉴权**（同时覆盖数据面 `OP_OPEN` 与元数据面 `M_OPEN`）：

```bash
$Y data-server     --listen 0.0.0.0:50100 --root /data/cluster/blocks --token s3cr3t
$Y --home ...      meta-server --listen 0.0.0.0:50110 --token s3cr3t
$Y --home /local/home mount --view alice --data ... --meta ... --token s3cr3t /mnt/alice
```

token 不匹配 → 连接时直接 `PermissionDenied`，不重试。空 token（默认）= LAN-trusted 无鉴权，仅限可信内网。

**只读挂载**：`mount ... --ro`，所有写操作返回 `EROFS`（内核 `MS_RDONLY` + 文件系统侧守卫双保险）。

**性能旋钮**：

| 项 | 说明 |
| --- | --- |
| `--threads N`（默认 4） | FUSE 并发派发工作线程，每线程独立 `/dev/fuse` fd；`0`=单线程（遗留/AB 对照） |
| 分块并行 IO | 客户端读 ≥1MiB 按 1MiB 切片并发；写 ≥4MiB 分片并发 |
| 大文件条带 | 文件 ≥16MiB 才分块；`data_ref` = `s<N>:<stripe>:<uuid>`，chunk i 落 `servers[i%N]`；无前缀 = 旧单对象，向后兼容 |
| touch 去抖 | 同 inode 200ms 窗口去抖 `touch_mtime`，减少元数据 RPC 与 HA 日志量 |

## 备份与迁移

- **数据对象**：`blocks/` 是普通文件目录，可直接 `rsync`/快照备份。
- **视图库**：`<home>/meta/<view>.db`。**勿在有写活动时裸拷 DB**；单机先停写或 checkpoint 后再拷，HA 模式由 meta-server 用备份 API 复制。

## 测试

```bash
export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
cargo test --release                 # 单元 + 集成测试
bash .build-bin/run_all.sh           # 全板 16 套件
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准（纯功能回归）
```

套件：`build`、`unit`、`posix`、`edge`、`e2e`、`remote`、`errors`、`resilience`、`concurrent`、`metasrv`、`ha`（含 `--witness` fencing / fail-safe 场景）、`striping`、`locks`（含崩溃即时回收 / 续约跨过 lease）、`freshness`、`acl`（view 白名单 e2e）、`pg`（PostgreSQL 后端 e2e；无 PG 主机自动 SKIP、不拉低 GREEN）。

## 命令速查

| 目的 | 命令 |
| --- | --- |
| 建视图 | `grove --home H init --view U [--manifest M]` |
| 本地挂载 | `grove --home H mount --view U -m MP [--ro] [--strict] [--threads N] [--as-uid N]` |
| 远程挂载 | `grove --home H mount --view U -m MP --data A[,A2] --meta M[,M_bak] [--token T] [--as-uid N]` |
| 卸载 | `grove umount -m MP` |
| 数据服务 | `grove data-server --listen IP:PORT --root DIR [--token T]` |
| 元数据服务（默认 SQLite） | `grove --home H meta-server --listen IP:PORT [--token T] [--role primary\|standby --peer ADDR [--sync-repl] [--witness DATA_ADDR]]` |
| 元数据服务（PostgreSQL 后端） | `grove --home H meta-server --listen IP:PORT --store postgres://user:pw@host:5432/dbname` |
| 视图白名单授权 | `grove --home H acl allow --view V --uid N [--read] [--write]` |
| 视图白名单撤销 | `grove --home H acl revoke --view V --uid N` |
| 视图白名单查询 | `grove --home H acl list --view V` |

## 诚实标注的边界

以下为本项目遵循「工程诚信」原则明确标注的限制，而非隐藏假设：

- **传输协议**：当前为自定义 TCP 帧，非 gRPC。gRPC（tonic/prost）已做 go/no-go 闸门评估，结论 **GO**（tonic 0.14 免 protoc），但完整集成**有意延后**——现协议已正确且被测。
- **HA RPO**：默认 async RPO ≈ 500ms（`--sync-repl` 可降到 0）。
- **fencing**：`--witness`（逗号分隔的 N 个共享 data-server 作 `prev`-回传 epoch CAS **多数派**仲裁，需 `N/2+1` 授予）已落地并 **fail-safe**，primary 持不重叠的有界多数派租约（多数活租约即独占证明，写由本地租约校验放行）、并对内容数据帧按 token 门控，把双写窗口收敛到**租约安全余量内**（保证收敛单主）。硬边界：严格两节点、**不配第三方仲裁** 时无法防脑裂、窗口不封顶（CAP/共识定理限制，须部署 witness 或第三节点）；**多数派仲裁的可用性红利**：配 `--witness` 后宕**少数**仲裁者仍不冻结写、少数盘丢不复活过期主，唯**失去多数**才 fail-safe 停写（N=1 则退化为单仲裁、witness 本成为单点可用性依赖）。
- **条带**：单条已饱和链路（loopback/单网卡）上**无吞吐增益**，价值在多节点聚合容量；真实多机链路须重测后才可声称收益。
- **验证环境**：全部在 127.0.0.1 回环（模拟 LAN）验证，尚未做真实多机与 fio 级基准。

凡未实测数字处，文档不做收益声称；凡有限制处，均在代码注释与本节明示。

## 许可

MIT。

---

<a id="english"></a>
# English

## What it is

`GroveFS` is a **user-view FUSE filesystem**. It cleanly separates *who sees what* (the view) from *where bytes actually live* (the object):

- **View**: each user gets a private directory tree, stored as one SQLite DB `<home>/meta/<view>.db`. Users cannot see each other's files.
- **Object**: the real bytes of a file, stored flat under `blocks/` as UUID-named objects; the view's directory entries only hold object keys (pointers).
- **Base dir `--home`**: the physical root holding both `blocks/` (data) and `meta/` (views).

On top of that, metadata and data can each be served as a network service (`meta-server` / `data-server`) so **many client machines mount and share one authoritative view over the LAN** — a central-storage + in-rack LAN-access topology, and **no NFS**.

## Key features

- **Per-user view isolation** with full custom nesting (`mkdir`/`mknod`/`rename`/`rmdir`).
- **Pluggable storage/metadata backends** via traits: local, remote, and cross-server striped (`ShardedBackend`). The authoritative metadata store can additionally be swapped to **PostgreSQL** (`meta-server --store postgres://…`): one PG schema per view, transactions around rename/unlink/lock; under PG the in-tree HA (snapshot shipping) is intentionally off — use PG's own streaming replication / PITR instead.
- **Optional per-view uid whitelist**: `grove acl allow/revoke/list` manages `view_acl(view,uid,can_read,can_write)` in `<home>/meta/acl.db`. No rows for a view = unrestricted (back-compat, the legacy suites unchanged); any row = whitelist → `M_OPEN` fails with `E_AUTH` if the uid has no read grant, and every write-intent op is refused with `E_NOTPERM` (surfacing as `EPERM` at the client's FUSE layer, deliberately distinct from an HA standby's `E_ROFS` failover signal) if it lacks a write grant. The write gate is re-evaluated per-op (not cached at open), so `acl revoke` takes effect on already-mounted connections immediately. Multiple clients can therefore share one authoritative tree at differing privilege levels.
- **Custom binary framing over TCP** (not NFS, not gRPC) with a connection pool.
- **Real-time cross-client invalidation push** (default): after a write, the center broadcasts `inval_entry`+`inval_inode` so peers see changes immediately, no polling; `--strict` gives a zero-caching fallback.
- **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` arbitrated centrally, effective across processes and machines; crash reclaim via FUSE `release`, plus a short lease (default 15s) and renewal heartbeats.
- **High availability**: primary/standby `meta-server` with heartbeat/epoch promotion and automatic demotion; `--sync-repl` drops write RPO to 0; `--witness` takes a comma-separated list of shared `data-server` arbiters (odd `N`) and arbitrates by **majority** (`quorum_size = N/2+1`), **fail-safe**, holding a non-overlapping lease at a majority (renewed every tick; each arbiter refuses overlapping leases, so a majority live lease is itself proof of exclusive authority and a write is admitted from a local lease check) and stamping content frames so the arbiters can fence a stale writer, bounding the double-write window to the lease safety margin; losing a minority of arbiters no longer freezes writes (`N=1` degrades to a lone witness).
- **Token authentication**, **read-only mounts** (`--ro`), **multi-threaded `fuser 0.18`** (`clone_fd`), **chunked parallel IO**, and **cross-server striping**.
- **Full regression**: `run_all.sh` — **16 suites, all GREEN** (new `acl` per-view whitelist e2e + `pg` PostgreSQL backend e2e; the PG suite self-skips on hosts without a reachable PostgreSQL, so GREEN is preserved either way).

## Architecture

Three layers, all trait-decoupled:

```
┌─ FUSE mount process  (fs_impl.rs: GroveFS, &self, thread-safe)
│    ├── MetaProvider trait   view-tree access
│    │     ├── local:  MetaEngine  → <home>/meta/<view>.db
│    │     └── remote: RemoteMeta  → meta-server (authoritative)
│    └── StorageBackend trait  object IO
│          ├── local:  LocalBackend  → <home>/blocks/<key>
│          ├── remote: RemoteBackend → data-server
│          └── striped: ShardedBackend → multiple data-servers
└─ Transport: custom binary frames over TCP (fixed header + variable body), pooled
```

- **Pluggable trait seams**: local/remote/striped are interchangeable.
- **Central MetaServer**: one authoritative view per user shared across machines, so no diverging local copies.
- **Event bus + Notifier**: pushes invalidations after writes for content freshness.
- **fuser 0.18 multi-threading**: each worker gets its own `/dev/fuse` fd (`clone_fd`, Linux 4.5+), requests dispatch concurrently.

## Sharing across machines & storage backends (how it works, common questions)

> This section answers the two most-confused questions: (1) does "many servers sharing one directory tree over the LAN" **require PostgreSQL**, and (2) which side does PG actually run on?

### The full path of one write (data plane and metadata plane, half each)

GroveFS splits a file's *bytes* from its *directory tree* into two independent channels — this is precisely why it needs no NFS:

```
app write()/close()
   │
   ├─ data plane ─→ RemoteBackend ─TCP frame→ data-server ─→ blocks/<key>  (UUID-named, flat)
   │                (≥4MiB auto-sharded concurrently; large files can stripe across data-servers)
   │
   └─ metadata plane ─→ RemoteMeta ─TCP frame→ meta-server ─→ authoritative tree (SQLite by default, or PostgreSQL)
                      │  (create / rename / setattr / touch_mtime …)
                      └─ on success, the event bus pushes inval_entry + inval_inode to the view's other subscribers
                                → peer's next getattr/lookup sees the invalidation, re-fetches → visible immediately
```

- **Who holds authority**: `meta-server` is a **single authoritative serialization point** — one view's tree is written to the storage engine by that one process alone (SQLite: one `.db` per view; PG: one schema per view); every client's TCP frames converge on it. This is **deliberate** (it eliminates the view divergence you'd get from per-client copies), not an oversight.
- **Clients never touch storage directly**: a client opens neither the `.db` file nor a PG connection — it only speaks the GroveFS wire protocol. Whether the backend is SQLite or PG is **completely transparent** to clients; the mount command is byte-for-byte identical.

### Common questions

**Q: To mount one directory tree from many servers over the LAN, is PostgreSQL mandatory?**
**No.** The "many machines share one tree" capability comes from **`meta-server` + the same `--view`**, independent of whether the backend is SQLite or PG. The default embedded SQLite (`rusqlite bundled`, compiled into the binary, zero external deps) was built for exactly this topology — deploy `data-server` + `meta-server` on a central node and have clients `mount --view alice --meta <center>:50110 --data <center>:50100`; they share one authoritative tree with real-time cross-client visibility. This whole path is proven on a pure-SQLite backend by the `metasrv` / `freshness` / `locks` suites — **you do not need to install PostgreSQL first**.

**Q: Which side does PG run on?**
**Server-side only** (on the `meta-server`), as its **pluggable authoritative storage backend**:

```
client (just the grove binary)   server-side                 data tier
mount --view alice   ─TCP→        meta-server   ─postgres://→  PostgreSQL server
  · no PG lib / creds / awareness  · --store is ITS flag       (same host as meta-server, or a dedicated DB box)
```

`--store postgres://…` appears only on the `meta-server` subcommand; clients have no such option and never see the connection string. The PostgreSQL server process runs physically in the center (co-located with `meta-server` over loopback, or a dedicated DB machine reached over the LAN) but is **always server-side**.

**Q: So when is PG actually worth it?**
When you want to hand the authoritative tree's **durability / HA / backup / centralized operations** to a mature database: PG brings streaming / logical replication, PITR, `pg_basebackup`, monitoring and DBA tooling, and can tune very large metadata (millions of nodes) with indexes/VACUUM. Note **the in-tree HA (snapshot shipping) is mutually exclusive with PG** (see "Shared views & the permission model") — once on PG, use PG's own replication, not GroveFS's file-level snapshots.

> **Honest boundary**: adding PG does **not** parallelize metadata writes — as a single writer, `meta-server` still serializes internally (one connection) even in PG mode. PG's value is in **durability / replication / operational maturity**, not write-throughput scaling; genuine metadata write-concurrency scaling is a larger architectural problem and is not done here. For default deployments, keep the zero-dependency SQLite.

## Build

**Prerequisites** (Linux): Rust (edition 2021), `pkg-config`, libfuse development environment, `/dev/fuse`. The metadata engine uses `rusqlite` with the `bundled` feature — SQLite is compiled from source, **no system dependency**.

```bash
cargo build --release          # artifact: target/release/grove
```

> Note: this project was developed in an environment **without a system `pkg-config`**; the build scripts drive a repo-shipped shim (`.build-bin/pkg-config`), so before running the tests:
> ```bash
> export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
> ```
> If your machine already has a working `pkg-config` and libfuse, plain `cargo build --release` is enough.

## Quick start (single-node, local)

```bash
Y=./target/release/grove
BASE=/data/myfs

$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

$Y --home $BASE mount --view alice --mountpoint /mnt/alice
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob   # another terminal

mkdir -p /mnt/alice/projects/2026
echo "report" > /mnt/alice/projects/2026/report.txt
ls /mnt/bob                                # bob cannot see alice's files

$Y umount --mountpoint /mnt/alice
```

Seed a hierarchy: `init --view alice --manifest seed.txt` (lines of `mkdir <path>` / `touch <path>`, `#` comments, idempotent).

## About `--view` and per-view path capacity

- **`--view` is a view-namespace label, unrelated to Linux usernames**: any legal filename works (`alice`, `team-a`, `u42`, `research_group`). It is used only to (1) name `<home>/meta/<view>.db` and (2) tag the FUSE mount as `grovefs:<view>`. The binary does not call `getpwnam`, does not verify uid, does not require `useradd`. **Access authority comes from being able to reach `--home` / `blocks/` (or holding `--token`), not from the Linux user**. Under the same `--home`, distinct `--view` values map to distinct, mutually invisible views.
- **One user = one tree, unlimited paths inside**: once mounted you may `mkdir`/`touch` any number of files and directories at any depth under that mountpoint, exactly as in your own home. Per-user isolation applies to the whole tree, not to individual paths. `--manifest` is only a convenience for pre-seeding a skeleton, not a cap on path count or depth.

## Shared views & the permission model

GroveFS has exactly two authorization layers — **token = cluster admission, view name = tree choice**. `--view` is not an identity: any uid that mounts the same `--meta <addr>` with the same `--view alice` lands on alice's single authoritative tree and sees the other writers. That is intentional and makes "one view, many users" work out of the box.

- **Many OS users sharing one view**: point every client at the same `mount --view alice --meta ... --data ...` and both metadata (single SQLite/PG tree, writes serialized through the central engine) and content (single `blocks/`) converge in-place. If you need **differentiated rights per uid** inside that shared tree (e.g. an auditor mount that can list but never mutate), add a whitelist entry on the central node:

  ```bash
  $Y --home /data/cluster acl allow  --view alice --uid 1000 --read --write   # rw peer
  $Y --home /data/cluster acl allow  --view alice --uid 2000 --read          # read-only subscriber
  $Y --home /data/cluster acl list   --view alice                             # show grants
  $Y --home /data/cluster acl revoke --view alice --uid 2000                  # drop one row
  ```

  Semantics: **zero rows for a view = unrestricted** (every uid passes, matches historical behavior); **any row = whitelist** (a uid without a matching row is refused at `M_OPEN` with `E_AUTH` so mount fails fast; a read-only grant sees every write-intent op rejected with `E_NOTPERM` — `EPERM` at the client FUSE layer, deliberately distinct from an HA standby's `E_ROFS` failover signal — while reads continue). The write gate is re-checked before every op rather than cached at open, so `acl revoke` / a rw→ro downgrade takes effect on live connections immediately. In single-node local mounts the same read gate runs, but the write path lands on the underlying SQLite file directly — OS file permissions are the last line of defense there. The uid presented to the server is `geteuid()` by default and can be overridden with `mount --as-uid N` for role-based mounts against a shared `meta-server`.

  > **Honest boundary**: the client-reported uid is **advisory** — a TCP client can forge it. Real admission is still the `--token` gate; the whitelist is for **role separation inside an already-trusted cluster**, not for hostile-tenant isolation.

- **Metadata backend is swappable: SQLite ↔ PostgreSQL** (this is the `meta-server`'s *authoritative* store only; the wire protocol clients see does not change):

  ```bash
  # default: embedded SQLite, one `<home>/meta/<view>.db` per view
  $Y --home /data/cluster meta-server --listen 0.0.0.0:50110

  # PostgreSQL: one `grovefs_v_<hex(view)>` schema per view, tx-guarded rename/unlink/lock
  $Y --home /data/cluster meta-server --listen 0.0.0.0:50110 \
       --store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  Clients mount exactly the same way (`--meta host:port --view alice`). Notes that matter:
  - **PG is mutually exclusive with the in-tree HA flags**: `--store postgres://…` combined with any of `--role standby` / `--peer` / `--witness` fails fast at startup — GroveFS-native HA ships snapshots via SQLite `VACUUM INTO`, which has no PG analogue. On PG, use PostgreSQL's own streaming / logical replication / PITR, which is strictly stronger than the file-snapshot mechanism anyway.
  - `acl.db` remains a **local SQLite file** in PG mode as well: it is control-plane admission state, not part of any view tree, and should not travel with the view into PG's replication domain.
  - Tables are created idempotently on the view's first `M_OPEN` (`CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS`), so no pre-migration script is needed; the root row is pinned at `id=1` and `setval(pg_get_serial_sequence(...), MAX(id))` aligns the sequence, matching SQLite's `ROOT_INO` semantics exactly.

  Why Redis was *not* added: Redis's K/V+TTL model is not isomorphic to a view tree (rename/unlink/hardlink/refcount GC want transactions, lock arbitration wants row-level state). Emulating relational semantics on Redis would rebuild a worse PG at the application layer. **GroveFS ships PG, not Redis.**

## Production topology: central storage + LAN mounts

```bash
# central node
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster/blocks
$Y --home /data/cluster meta-server --listen 0.0.0.0:50110

# each client (local --home holds only a cache; no authoritative view copy)
$Y --home /local/home mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 /mnt/alice
```

| Option | Effect |
| --- | --- |
| `--data host:port` | File IO shipped over the network to `data-server` (no NFS) |
| `--meta host:port` | View served by the central `meta-server` → same user across machines shares one tree |
| `--data a,b,c` | Comma-separated list enables deterministic cross-server striping |
| `--meta primary,standby` | Failover list; falls over automatically when the primary dies |

## Consistency

| Mode | Command | Behavior |
| --- | --- | --- |
| **Real-time invalidation push** (default, recommended) | just `--meta` | After a client's write succeeds, the center broadcasts invalidation (`inval_entry`+`inval_inode`) to that user's other subscribers; peers see it immediately, no polling |
| `--strict` strong-coherence fallback | add `--strict` | Disables kernel attr/dentry cache and reads straight through (`FOPEN_DIRECT_IO`). Trade-off: no client caching, more round-trips |

Invalidations must be sent in pairs (`inval_entry` + `inval_inode`) or peers may hold stale attributes.

## Distributed advisory locks

`flock` / `fcntl` are arbitrated through a central `locks` table (SQLite), effective across processes and machines. Reclaim paths:

1. **Holding process crashes** — the kernel closes its fd, triggering FUSE `release` (carrying the matching `lock_owner`); the lock row is deleted **immediately**, far below the lease.
2. **Whole mount daemon crashes** — no `release`, no renewal; the lock expires within a **short lease (default 15s)**.

Live holders are kept alive by a **renewal heartbeat** (every lease/3), so they **never time out**.

> **Do not judge liveness by pid**: in `( flock -x 200; sleep N )` the `setlk` pid is a transient helper (the lock lives on the inherited open-file-description), so a pid-alive check would wrongly kill live locks; `flush` fires on every close with a mismatched owner and is unusable for reclaim. Only `release` (last-close) is the authoritative signal that a holder terminated.

## High availability

```bash
# primary
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# standby
$Y --home /data/nodeB meta-server --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# primary with RPO=0 synchronous replication
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# true fencing via a majority of independent arbiters (comma-separated list, odd N)
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
```

- The two nodes must use **separate `--home`s** (each keeps its own copy; replication ships whole snapshots).
- `--sync-repl` (opt-in): each write blocks until the standby durably installs the snapshot and ACKs, dropping **RPO from ~500ms to 0**; on ACK timeout it honestly degrades to async with a warn (never falsely reports 0).
- `--witness` (opt-in, **fail-safe fencing**): one or more `data-server`s reachable by both halves (a comma-separated list; an odd count `N` is recommended, and `N=1` behaves exactly as a single witness used to) act as independent arbiters. Authority needs a strict majority `quorum_size(N) = N/2 + 1` to grant: the meta side fans out to the whole list concurrently and applies majority logic (`witness_quorum`). Because two majorities always intersect, only the first claimer can raise a shared arbiter (the later sees `prev == epoch`, a renew, not an advance), so a same-value race yields exactly one primary. Each arbiter keeps a monotonic fence token; `OP_FENCE` folds your epoch via `fetch_max` and returns the **value held before the fold (`prev`)**. A claim wins iff `prev < your epoch`; `prev >= your epoch` means you are fenced — returning `prev` (not the post-fold max) is exactly what resolves same-value collisions so a recovering arbiter never ends up with two primaries. A live primary renews a bounded lease at a majority every tick (self-demotes if a majority reports `prev > mine`); each arbiter refuses overlapping leases — at most one live lease at a time — so a majority live lease is itself proof of exclusive authority, and a write is admitted from a local lease check (one fan-out per renewal, not per write). A would-be promoter may seize a lease only after the incumbent's expires (it refuses if `prev >= candidate`, a tie or peer-ahead, or the incumbent lease is still live). **Fail-safe**: if a configured witness set cannot form a lease quorum beyond a 3s grace, a node gives up write authority (primary self-demotes, standby refuses to promote → writes freeze) rather than risk split-brain. **Availability gain over a lone witness**: losing a *minority* of arbiters neither freezes writes (a quorum is still reachable) nor resurrects a stale primary (a quorum still holds the monotonic token even if a minority of `.fence_token` stores are lost); only losing a *majority* freezes. Each arbiter persists its token at `<root>/.fence_token`, so a restart does not zero it. Beyond the meta-plane proof, the DATA plane is fenced too: the client stamps its live epoch onto every write-intent content frame (create/write/truncate/remove/fsync) and the witness data-server refuses one whose epoch lags its token (`EROFS`), since a `write()` otherwise lands bytes on the backend before the debounced meta touch — a fenced frame re-pulls the epoch from the authority and retries once. The data-server furthermore linearizes the epoch check and the backend write against a promotion (a claim takes an exclusive lock that drains in-flight writes, and the witness co-locates with storage), so a stale write can never land concurrently with the new primary's — content torn-writes are closed on the arbiter, not just bounded (this content gate is effective only on data-servers inside the `--witness` list, so deploy the witness co-located with the storage clients actually write to; a content store outside the set keeps a zero token, its data-plane gate stays inert, and only the meta-plane fence protects it).

## Auth / read-only / performance

**Token auth** covers both the data plane (`OP_OPEN`) and the metadata plane (`M_OPEN`); a mismatch fails at connect with `PermissionDenied`. Empty token (default) = LAN-trusted, no auth — trusted networks only.

**Read-only**: `mount ... --ro` → every mutating op returns `EROFS` (kernel `MS_RDONLY` + filesystem-side guard).

**Performance knobs**: `--threads N` (default 4, per-thread `/dev/fuse` fd), chunked parallel IO (reads ≥1MiB, writes ≥4MiB), large-file striping (≥16MiB), and `touch_mtime` debouncing (200ms window).

## Testing

```bash
export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
cargo test --release
bash .build-bin/run_all.sh                 # 16 suites
SKIP_BENCH=1 bash .build-bin/run_all.sh    # skip throughput bench
```

## Honest limitations

These are explicit, honest boundaries, not hidden assumptions:

- **Transport**: custom TCP frames today, not gRPC. gRPC (tonic/prost) was gate-evaluated as **GO** but full integration is **deliberately deferred**; the current protocol is correct and tested.
- **HA RPO**: ~500ms async by default (0 with `--sync-repl`).
- **Fencing**: `--witness` is implemented and **fail-safe**: a `data-server` list (comma-separated, odd `N`) arbitrates by **majority** (`quorum_size = N/2+1`); a primary holds a non-overlapping lease at a majority (renewed every tick), so a majority live lease is itself proof of exclusive authority and a meta write is admitted from a local lease check (one fan-out per renewal, not per write) AND gates content frames by token at the witness data-servers, bounding the double-write window to the lease safety margin (converging to one primary). The irreducible boundary: a strict two-node pair **without arbiters** cannot prevent split-brain and leaves the window unbounded (CAP/consensus). The **availability gain of a quorum**: with `--witness`, losing a *minority* of arbiters neither freezes writes nor resurrects a stale primary (a quorum still holds the monotonic token); only losing a *majority* fails safe and freezes (`N=1` reverts to a lone witness, which becomes a single availability dependency).
- **Striping**: no throughput gain on a single saturated link (loopback/one NIC); the value is aggregate capacity across nodes — real multi-machine links must be re-benchmarked before claiming gains.
- **Verification environment**: everything validated on 127.0.0.1 (simulated LAN); no real multi-machine or fio-level benchmark yet.

Where numbers are not measured, no benefit is claimed; wherever there is a limitation, it is stated in code comments and here.

## License

MIT.
