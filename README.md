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
- **基目录 `--home`**：本地模式下同时容纳 `blocks/`（数据）与 `meta/`（视图）的物理根；远程挂载不需要它（客户机本地不落盘），中心 `meta-server` 的权威元数据根用 `meta-server --db-path` 指定。

在此之上，`GroveFS` 可把元数据与数据分别服务化（`meta-server` / `data-server`），让**多台客户机经局域网挂载、共享同一份权威视图**——这才是「数据中心存储 + 机房内 LAN 访问」的落地形态，且**不使用 NFS**。

## 核心特性

- **多用户视图隔离**：一用户一棵树，跨用户不可见；完整的 `mkdir`/`mknod`/`rename`/`rmdir` 自定义层级。
- **可插拔存储/元数据后端**（trait 抽象）：本地 `LocalBackend`/`MetaEngine`、远程 `RemoteBackend`/`RemoteMeta`、跨多节点条带 `ShardedBackend`，对上层透明。元数据权威存储亦可切换为 **PostgreSQL**（`meta-server --store postgres://…`）：服务端 schema-per-view、事务化 rename/unlink/lock；此模式下自研 HA 有意关闭，交给 PG 自己的流复制/PITR。
- **视图级 uid 白名单（可选）**：`grove acl --db-path D allow/revoke/list` 管理 `<db-path>/meta/acl.db` 的 `view_acl(view,uid,can_read,can_write)`（`--db-path` 与 `meta-server` 同名同目录，两者读的是同一个文件）。未列入 = 无约束（历史行为不变）；一旦有行 → 读门在 `M_OPEN` 处 `E_AUTH` 拒挂载、写门每次写意图 op 前动态重查并回 `E_NOTPERM`（客户端 FUSE 侧落 `EPERM`；有意区别于 HA 备机 `E_ROFS` 故障转移信号），故 `acl revoke` 对已建立的挂载连接**即时生效**、无需重挂。同一 view 即多人共享同一棵权威树，per-uid 读写分别放行。
- **每客户端显示定制（`mount --layout rules.txt`）**：同一棵权威树、同一份字节，不同挂载点各自套一层「显示透镜」——`hide` 隐藏子树、`only` 白名单、`map` 把条目重排到别的路径（源位让位）、`link` 建两处同时可见的别名。透镜只做 `MetaProvider` 层的路径翻译：真实条目保持真实 inode，数据 IO/锁/GC/失效推送全部直通，不复制目录项、不影响其它挂载的视图。
- **服务端派生视图（`grove view define` + `mount --view <派生名>`）**：想把形状**集中管理在服务端**——`grove view define --name docsonly --base team --rules F` 把规则落进 `<db-path>/meta/views/<name>.layout`（首行 `base <物理视图>`），客户端只 `mount --view docsonly`，**无需任何本地规则文件**。**虚拟视图一律只读挂载**：形状集中治理、只分发不修改，写入只属于原（物理）视图——客户端据 `M_OPEN` 握手标志直接落内核只读（写 `EROFS`），`meta-server` 另对派生名的写意图 op 设权威门禁，本地（`--home`）派生挂载同判。同名派生视图由服务端单例渲染（虚拟 inode 跨机一致），**改定义热生效**（`meta-server` 逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂），可配 `grove acl allow --view <派生名>` 按名授权做分工；本地模式（`--home`）挂载同样解析 `<home>/meta/views/` 下的定义。
- **服务端纯别名（`grove view alias`）**：治理「叫什么」而非「长什么样」——公开名 1:1 映射物理视图（定义仅一行 `alias <真实视图>`），别名挂载**全树可写**，而真实名从此**不是挂载入口**（本地与 `meta-server` 都拒挂被治理的真实名）：对外名字可自由轮换、存储身份不外露，猜出真实名也挂不进来；公开名泄露时 `--retire` 一条命令原子轮换（旧名失效、真实名全程拒挂、ACL 授权沿用）。与派生视图同放 `meta/views/`、同一 `list/rm`；ACL 按真实名判定（一条授权对别名与基名同时生效）；拒绝别名链与派生目标；`import` 拒绝别名目标（别名不是存储身份）。
- **存量数据零拷贝导入（`grove import`）**：服务端已有一批目录数据（如 `/data/legacy`）时，`grove import --view V --source DIR` 把整棵树登记进视图——同文件系统下对象就是源文件的**硬链接**（零额外字节，两个路径读写同一 inode、双向实时可见），跨设备自动退化为流式拷贝；目录层级/时间戳/权限/属主保真，重跑即增量补新文件，删视图里的名字只摘 `blocks/` 链接**绝不伤源数据**。可与派生视图/透镜组合出虚拟目录（`map` 导入路径到任意形状；**派生虚拟视图挂载只读**，写走原视图）。
- **自定义二进制帧 over TCP**（非 NFS/gRPC）：定长头 + 变长体，连接池并发。
- **跨客户端实时失效推送**（默认）：一端写成功，中心经事件总线向同用户其它订阅者广播 `inval_entry`+`inval_inode`，对端下次访问**即时可见**，无需轮询；`--strict` 提供零缓存兜底。
- **分布式 advisory 锁**：`flock` / `fcntl F_SETLK/F_GETLK` 经中心 `locks` 表集中仲裁，跨进程、跨机生效；崩溃即时回收（FUSE `release`）+ 短 lease（默认 15s）+ 续约心跳。
- **高可用主备**：`meta-server --role primary|standby --peer` 心跳/epoch 提升/自动降级；`--sync-repl` 把写 RPO 降到 0；`--witness` 取逗号分隔的 N 个共享 `data-server` 作**独立仲裁者集合**（推荐奇数），**fail-safe、持不重叠的有界多数派租约（需 `N/2+1` 授予；多数活租约即独占证明，写由本地租约校验放行）并对内容数据帧按 token 门控，把双写窗口收敛到租约安全余量内**；宕少数仲裁者仍不冻结写（N=1 退化为单仲裁旧行为）。
- **Token 双面鉴权**、**只读挂载**（`--ro`）、多线程 `fuser 0.18`（`clone_fd` 并发派发）、**分块并行 IO** 与 **跨 server 条带**。
- **元数据读并行 + 目录列举分页 + `FUSE_READDIRPLUS`**：每个视图的 `MetaEngine` 在单条写连接之外再开 4 条附加连接（`READ_POOL_SIZE`），`lookup`/`getattr`/`readdir` 及 nlink 解析都走这条只读池，元数据**读**不再排队等写锁（**写仍串行在同一权威连接上**，单写者语义不动）；服务端 `MetaEngine` 同样吃这条池，故 `meta-server` 的并发 RPC 处理器也不再互等。目录列举新增 `readdir_page(parent, offset, limit)`，SQLite 引擎把它下推成 SQL `LIMIT/OFFSET`，FUSE 回调按 32→1024 几何块流式填回复缓冲，一个内核请求付 `O(缓冲)` 而非 `O(整棵目录)`（改造前每个内核分页都要重扫整棵目录并为每条重算 nlink）；并默认协商 `FUSE_DO_READDIRPLUS`（内核 ≥4.6），目录项连同属性一次带回，`ls -l` 不再退化成逐条 `LOOKUP`+`GETATTR`。同一份 5001 条目录、同一挂载点、逐个新挂载（内核 dentry 缓存为空）实测 `ls -l`：**2.56s**（改造前基线 55.34s；关掉 PLUS 只留分页为 5.22s，三份输出逐字节相同）。分页与 PLUS 只在「能自己分页的存储引擎」上启用——本地 `--home` 的 SQLite、PG 后端（各配一条独立的 4 连接只读池并同样下推 `LIMIT/OFFSET`），以及对接 SQLite/PG `meta-server` 的**远端挂载**：新增 `M_READDIRP` 帧把 `offset/limit` 下推到服务端，客户端每个内核缓冲只取一页并协商 PLUS。同一 5001 条目录经 `meta-server`+`data-server` 实测 `ls -l`：改造前二进制 ~54s → **2.29s**（只留分页、关 PLUS 为 4.22s，两份输出排序后逐字节相同）。唯有派生/透镜视图（`LayoutMeta`）及其对接的老服务端仍走 trait 默认实现（每内核缓冲一次全量列表、不协商 PLUS），详见「诚实边界」。`--no-readdirplus` 保留作 A/B 与兼容排障开关。
- **完整回归**：`run_all.sh` **18 套件全板 GREEN**（新增 `acl` view 白名单 e2e、`layout` 显示透镜与服务端派生视图 e2e、`import` 零拷贝导入 e2e、`pg` PostgreSQL 后端 e2e；`pg` 会在无 PG 主机上自动 SKIP）。

## 架构与原理

三层解耦，全走 trait：

```
┌─ FUSE 挂载进程  (fs_impl.rs: GroveFS，&self 多线程安全)
│    ├── MetaProvider trait   视图树访问
│    │     ├── 本地:  MetaEngine  → <home>/meta/<view>.db
│    │     ├── 远程:  RemoteMeta  → meta-server（中心权威）
│    │     └── 透镜:  LayoutMeta  → mount --layout（每客户端显示定制，装饰以上任一）
│    │           ├── 派生视图: 服务端 LayoutMeta 单例 → mount --view <派生名>（规则由 grove view define 集中定义于 meta/views/）
│    │           └── 纯别名: alias <真实视图> → 公开名 1:1 映射,全树可写,真实名不可直挂（名字治理）
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

> **诚实边界**：上 PG **不会**把元数据写并行化——`meta-server` 作为单一写者,PG 模式下内部仍是单连接串行。PG 的价值在**持久化/复制/运维成熟度**,不在写吞吐扩展;真正的元数据写并发扩展是更大的架构命题,当前未做。默认场景请继续用零依赖的 SQLite。需注意：元数据**读**并行现已覆盖两个引擎——SQLite `MetaEngine` 与 PG `PgEngine` 各持一条 4 连接只读池、目录列举各下推 SQL `LIMIT/OFFSET`；但**写**在两种后端下都仍串行在单一权威连接上（SQLite 的 `conn`、PG 的 `client`），“上 PG 不扩写并行”这一结论不变。

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

```text
客户端 A · uid=1000   mount --view alice ─┐
客户端 B · uid=2000   mount --view alice ─┤ 同一个 --view → 落进同一棵树；
客户端 C · uid=1000   mount --view bob   ─┘ 一个客户端也可同时挂多个 view（多对多）
        │
        ▼ TCP 帧 M_OPEN(view, token, uid)   （客户端从不开 db 文件 / 不持 PG 连接串）
┌─ meta-server（单一权威写者）—— 按序过门，再路由
│  ① token 校验     不匹配 → E_AUTH（集群准入，唯一确定性门；uid 自报属 advisory）
│  ② 读门 view_acl  无行 = 无约束；有任意行 = 白名单，需 can_read，否则 E_AUTH 拒挂载
│  ③ 写门           每个写意图 op 逐次重查 can_write，否则 E_NOTPERM（不缓存 → revoke 即时生效）
│  ④ view 名 → 引擎缓存  首次 M_OPEN 懒开，之后同 view 命中同一实例
└──┬──────────────────────────────────
   ▼ view=alice                        ▼ view=bob
  alice 权威树（A、B 共享，写串行化）    bob 权威树（仅 C，与 alice 互不可见）
   ├─ SQLite（默认）：       一 view = 一个独立 db 文件  <db-path>/meta/alice.db · bob.db
   └─ PostgreSQL（--store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62

数据库里独立存储的单位是「视图」而非「每个客户端」；数据字节走 data-server → 存进其 --root 下的 blocks/<key>，树里只存 data_ref 引用。
```

- **多人共享同一 view**：多台机器、不同 OS 用户，都 `mount --view alice --meta ... --data ...` → 元数据经 `meta-server` 集中（一棵 SQLite/PG 树，串行化写）、内容经 `data-server` 集中（一份 `blocks/`）；`ls` 或 `cat` 立刻跨端可见，实时失效推送保证收敛。若希望**不同 uid 有不同权限**（例如只读旁观者），加白名单：

  ```bash
  # 中心节点上：给 alice 视图开白名单（acl 自带 --db-path，与 meta-server --db-path 同名，指同一个元数据根）
  $Y acl --db-path /data/cluster allow --view alice --uid 1000 --read --write   # rw 协作者
  $Y acl --db-path /data/cluster allow --view alice --uid 2000 --read           # 只读订阅者
  $Y acl --db-path /data/cluster list --view alice                              # 查看已授权的 uid
  $Y acl --db-path /data/cluster revoke --view alice --uid 2000                 # 撤销单条
  ```

  `view_acl(view, uid, can_read, can_write)` 语义：**无行 = 无约束**（任何 uid 挂同一个 view 都放行，向后兼容旧行为）；**有任意行 = 白名单**（该 uid 无匹配行则 `M_OPEN` 直接被拒 `E_AUTH`，挂载 fail-fast；只读授权的 uid 在 op 循环里对写意图 op 逐条拒 `E_NOTPERM`——客户端 FUSE 侧落 `EPERM`，有意区别于 HA 备机的 `E_ROFS` 故障转移信号，以免一次权限拒绝触发伪切换）。写门**每个 op 前动态重查**（不在 `M_OPEN` 缓存），故 `acl revoke`/降级对已建立的连接即时生效。本地模式（无 `--meta`）也走读门校验，写门由 OS 文件权限兜底（view 本地 SQLite 文件属主即最后一道防线）。客户端上报的 uid 默认取 `geteuid()`，也允许 `mount --as-uid N` 显式覆盖，用于共享 `meta-server` 上按角色挂载。

  > **诚实边界**：TCP 客户端自报 uid 属**advisory**（客户端可伪造），真正的准入仍由 `--token` 把关；白名单是给一个已通过 token 的集群内部做**分工/职责**用的，不是防内鬼。本地模式（无 `--meta`）的写门只能靠 OS 权限（同 uid 无法强制隔离）。另两个行为边界：加锁类 op（`flock`/`F_SETLK`，含锁续约）要写中心 `locks` 表、归为写意图，**只读授权对它们一律 `EPERM`**（只读角色需共享锁就得给 `--write`）；读门只在 `M_OPEN` 校验一次，“revoke 即时生效”仅指写门，撤读后已建立连接到重挂才停止可读。

- **元数据后端可切换：SQLite ↔ PostgreSQL**（仅指 `meta-server` 权威存储）：

  ```bash
  # 默认（embedded SQLite，一 view 一 `<db-path>/meta/<view>.db`）
  $Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL：一 view 一个 schema `grovefs_v_<hex(view)>`，事务化 rename/unlink/lock
  $Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 \
       --store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  客户端挂载命令**完全一致**（仍 `--meta host:port --view alice`）——线协议不变。注意：
  - **PG 模式与自研 HA 互斥**：`--store postgres://…` + 任一 `--role standby / --peer / --witness` 在启动时直接 fail-fast（`VACUUM INTO` 快照复制是 SQLite 特性）；PG 上 HA 请交给 PostgreSQL 自己的**流复制/logical replication/PITR**——那本来就是比文件级快照更强的机制。
  - `acl.db` 白名单在 PG 模式下**依然落本地 SQLite**（`<db-path>/meta/acl.db`）：它是控制面准入状态，不是 view 树的一部分，不应跟 view 一起进入 PG 复制域。
  - 建表由服务端在 view 首次 `M_OPEN` 时幂等 `CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS` 完成，不需要预迁移脚本；根节点固定 id=1，`setval(pg_get_serial_sequence(...), MAX(id))` 对齐序列，`ROOT_INO` 语义与 SQLite 完全一致。

  为什么没做 Redis：Redis 的 K/V+TTL 模型与视图树（rename/unlink/hardlink/refcount GC 需事务、锁需要行级仲裁）不同构——把 relational 语义硬塞到 Redis 里等于在应用层重做一个 PG，收益为负。**GroveFS 只加 PG，不加 Redis**。

## 显示定制：同一份数据、每个客户端不同形状（`--layout`）

视图隔离是「不同 view = 不同树」；`--layout` 解决另一半问题：**同一棵存储树，不同客户端挂载时定制显示方式**——只看部分、或把条目重组到自己习惯的路径。规则文件每行一条（`#` 注释，路径为绝对路径，源路径 = 存储树坐标）：

| 规则 | 效果 | 典型用途 |
| --- | --- | --- |
| `hide <src>` | 在源位置隐藏整棵子树 | 屏蔽 private 目录 |
| `only <src>` | 白名单：只显示这些子树（祖先目录自动放行） | 客户端只暴露 /docs |
| `map <src> <dst>` | 重组：源位置消失，条目出现在 `<dst>`（必要时自动合成中间目录） | 把 /data/raw 摆进 /team/raw |
| `link <src> <dst>` | 别名：两处同时可见，同一 inode（不复制字节、不增引用） | 快捷入口 /proj/mylib |

语义要点（均有测试断言）：真实条目保持**真实 inode**，读写/锁/条带/GC 直通数据面；经透镜 `rm` 删的是源条目；`map`/`link` 子树内部仍按源路径接受 `hide`/`only` 过滤；向纯合成的脚手架目录创建返回 `EPERM`，普通创建撞透镜已显示的名字返回 `EEXIST`；symlink 目标文本不改写。`only` 模式下向 pass-through 祖先写入同样返回 `EPERM`（白名单外不可写）。

```bash
$ cat .demo/lens.txt                     # 客户端 C 的透镜
hide /photos/private
map /data/raw /team/raw
$ grove --home .demo/home mount --view alice -m .demo/mnt --layout .demo/lens.txt &
layout overlay .demo/lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at .demo/mnt (base .demo/home)

$ ls .demo/mnt                                   # 该客户端看到：data docs photos team
data  docs  photos  team
$ ls .demo/mnt/photos                             # private 已被隐藏
public
$ echo "hello via lens" > .demo/mnt/team/raw/note.txt   # 写直通源对象，无复制
$ ls .demo/mnt2/data/raw                          # 另一无透镜挂载即时看到
big.bin  note.txt
$ mkdir .demo/mnt/team                            # 'team' 是透镜合成名，已占用
mkdir: 无法创建目录 “.demo/mnt/team”: 文件已存在
```

规则在挂载时一次性解析（非法规则 fail-fast，不降级为「静默全显示」）；透镜是**每挂载实例**的显示层，不落盘、不改变服务端任何状态，因此与其它挂载正交：可叠加在本地 `MetaEngine` 或远程 `RemoteMeta` 之上，也可与 `--ro`、ACL 白名单组合。共享 meta-server 时，挂在合成目录下的条目其失效推送按源路径广播、可能不命中透镜坐标，建议配 `--strict` 关掉属性/条目缓存。

## 服务端派生视图：一份数据、不同 `--view` 名不同形状（`grove view`）

`--layout` 由每个客户端自备规则；当希望把形状**集中管理在服务端**（团队内不同名字共享同一显示契约）时，用派生视图：`grove view define` 把规则存进 meta 根目录的 `<db-path>/meta/views/<name>.layout`（首行 `base <物理视图>`，余行同 `--layout` 四动词），客户端只需 `mount --view <派生名>`，**不需本地规则文件**。派生（虚拟）视图**一律只读挂载**——形状是集中治理的分发视图，所有写入都属于原（基名）视图。同名派生视图在不同机器上由服务端同一份定义渲染（虚拟 inode 跨机一致），且可被 `grove acl allow --view <派生名>` 按名授权，与读写门控组成「形状+权限」分工闭环。本地模式（`--home`）同机制：挂载无 `--layout` 时自动解析 `<home>/meta/views/<view>.layout`。

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grove view --db-path .demo5/home define --name docsonly --base team --rules .demo5/docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grove view --db-path .demo5/home define --name team --base team --rules .demo5/docsonly.rules   # 名与基同名
Error: "derived view name must differ from its base"
$ grove view --db-path .demo5/home list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grove --home .demo5/home mount --view docsonly -m .demo5/mnt &    # 派生名直接挂，无 --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
mounting view 'docsonly' at .demo5/mnt (base .demo5/home, read-only)
$ ls .demo5/mnt                              # 派生名挂载点：只有 docs
docs
$ ls .demo5/mnt_full                         # 基名挂的是同一棵完整树（可写）
docs  photos
$ echo "new entry" > .demo5/mnt_full/docs/2026/b.md   # 基树写入……
$ ls .demo5/mnt/docs/2026                    # ……派生视图短 TTL 后看到（共享 meta-server 时失效推送双播、即时）
a.md  b.md
$ echo "from lens" > .demo5/mnt/docs/2026/c.md        # 经虚拟视图写一律拒绝
bash: .demo5/mnt/docs/2026/c.md: Read-only file system
$ grove view --db-path .demo5/home rm --name docsonly
removed derived view 'docsonly' (underlying data untouched)
```

语义与守卫：define 拒绝 `name == base`、拒绝 base 本身是派生视图（不嵌套透镜）、拒绝遮蔽已有物理 `<name>.db`；非法规则 fail-fast（不会退化成「全显示」）。**虚拟视图一律只读（硬契约）**：写只属于原（基名）视图——`meta-server` 在 `M_OPEN` 握手回包里带派生标志，客户端直接落内核只读挂载（写 `EROFS`、`flock` 写锁同拒），服务端另对派生名的写意图 op 加独立权威门禁（回 `E_NOTPERM`，有意不用 `E_ROFS` 以免被误读为 HA 故障转移信号），旧客户端或改过的客户端都绕不过；本地（`--home`）派生挂载在挂载时同判强制 RO。**改定义热生效、免重挂**：`meta-server` 每次分发 op 前检查定义文件 mtime，`define` 替换后已建立的挂载连接在客户端缓存 TTL（≤2s）内自动切到新形状（换形后只读契约不变）；`view rm` 则**不**热作用（已挂载者保持打开时的形状直到重挂，避免抽掉正在使用的树）。共享 meta-server 下，派生视图的订阅者**同时注册进自身名与 base 名两个桶**，基侧写双播即时失效、对派生挂载即时可见（`layout` 套件第 9 节 e2e 验证：基写→派生立即可见、派生挂载写/删被 `EROFS` 拒且基树无痕、re-define→同连接换形、本地派生挂载同样只读）；`view rm` 只删定义不动数据。`grove view` 与 `grove acl` 一样操作服务端本地文件，需在 meta-server 所在机器执行（或共享同一 `--db-path`）；HA 部署下 `meta/views/` 目录是配置状态、随 `acl.db` 一样手工同步到备机。

热重载实录（同一个已挂载连接，未重挂）：

```bash
$ ls .demo3/mnt                             # docsonly = only /docs，挂载中
docs
$ grove view --db-path .demo3/home define --name docsonly --base team --rules .demo3/broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls .demo3/mnt                             # 同一个挂载，形状自动变宽
docs  photos
```

## 纯别名：名字治理（`grove view alias`）

派生视图治理「长什么样」；**纯别名**治理「叫什么」：名字就是别名，映射值才是真实名。定义同放 `<db-path>/meta/views/<name>.layout`，首行 `alias <真实视图>`、不带任何规则行。与派生视图恰成正反两面：**别名挂载全树可写**（它就是真实视图的原形），而**真实名从此不是挂载入口**——本地与 `meta-server` 都拒挂被治理的真实名，对外名字可以自由轮换，存储身份不随名字外露：

```bash
$ grove view --db-path $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grove mount --view teamwork -m /mnt/work --meta 10.0.0.5:50241 --data 10.0.0.5:50240   # 全树、可写
$ grove mount --view team -m /mnt/t --meta 10.0.0.5:50241 --data 10.0.0.5:50240          # 直挂真实名被拒
Error: Custom { kind: PermissionDenied, error: "meta-server rejected the access token" }
$ grove --home $BASE mount --view team -m /mnt/t                                        # 本地同契约
Error: "view 'team' is governed by an alias — mount the alias name instead (`grove view list` shows it)"
```

公开名泄露后，**一条 `--retire` 命令原子轮换**（先写新名再删旧名，真实名全程被治理、无裸露窗口；ACL 记在真实名上，授权原样沿用）：

```bash
$ grove view --db-path $BASE alias --name teamwork2 --real team --retire teamwork     # 旧公开名已泄露，换成 teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: ACL grants are keyed to 'team', so they carried over untouched — legitimate users just remount under 'teamwork2'
$ grove mount --view teamwork -m /mnt/leak --meta ... --data ...                    # 拿旧泄露名再挂 → 空（不再解析到真实树）
```

守卫与语义：拒绝 `name == real`、遮蔽物理 db、派生目标、别名链；一个真实名同时只允许一个对外名（`--retire` 在同一条命令内先写后删，不受此约束卡住，且拒绝误指向派生视图或不存在的别名）；ACL 在读门之前已按真实名解析（`acl allow --view <真实名>` 一条授权对别名与基名同时生效，也因此**轮换公开名不需重发授权**）；`grove import` 拒绝别名目标（别名不是存储身份）；`view rm` 删别名后真实名对新挂载恢复可挂。轮换/删别名只影响**新**挂载（已建立连接持续到断开），被轮换掉的旧名落到按需新建的空命名空间、触不到真实树——**轮换是名字治理（泄露止损）而非安全边界**，硬隔离仍靠 token+ACL。详见 USAGE §3.4（`layout` 套件第 10/11 节 26 条断言）。

## 存量数据导入：零拷贝直通（`grove import`）

服务端目录里已经有一批数据、想让客户端按视图获取时，不必先「灌」一遍：`grove import` 把**已存在的本地目录树**登记进视图，字节原地不动——

```bash
$ tree .demo4/legacy
.demo4/legacy
|-- docs
|   `-- quarter.txt
`-- photos
    `-- 2026
        `-- y.jpg

3 directories, 2 files

$ grove --home .demo4/home import --view legacy --source .demo4/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grove --home .demo4/home import --view legacy --source .demo4/legacy
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grove mount --view legacy -m .demo4/mnt &    # 客户端连接即得这棵树
$ ls .demo4/mnt/legacy
docs  photos
$ stat -c '%y' .demo4/mnt/legacy/docs/quarter.txt   # 时间戳来自源文件本身
2021-05-06 07:08:00.000000000 +0000

$ echo "edited via the mount" > .demo4/mnt/legacy/docs/quarter.txt
$ cat .demo4/legacy/docs/quarter.txt           # 原始源路径同步可见：同一 inode
edited via the mount

$ rm .demo4/mnt/legacy/photos/2026/y.jpg       # 视图里删除一个名字
$ ls .demo4/legacy/photos/2026/                # 用户源数据毫发无损
y.jpg
$ find .demo4/home/blocks -type f -links 2 | head -1   # 对象只是源文件在 blocks/ 的第二个链接名
```

语义要点（`import` 套件 34 条断言）：**同文件系统零拷贝**（对象与源文件共享 inode，`df` 看不出差异），跨设备自动退化为独立拷贝并如实计数；**双向直通**——经挂载写、或外部直接改源文件，另一侧都在客户端 TTL（≤2s）内可见；**GC 只摘链接**——视图里删名字仅 unlink `blocks/` 那条路径，源路径与 inode 不受影响；**幂等增量**——重跑只登记新名字；mtime/atime/权限/属主保真（`--dry-run` 连视图 db 都不创建）。导入后的条目与原生写入的条目完全同权：读写/锁/快照/失效推送全部适用，还能被派生视图 `map`/`link` 组成任意虚拟目录形状——该形状挂载**只读**（写走导入的原视图，推送即时可见；`grove import` 也拒绝向派生名导入，防写出无人可读的影子库）。运行位置与 `grove acl`/`grove view` 相同：在持有存储根（`--home` 或 meta/data server 的 root）的机器上执行。

## 生产拓扑：中心存储 + 多客户机 LAN 挂载

```bash
# ===== 中心节点 =====
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110

# ===== 每台客户机（远程挂载不需要 --home：权威数据、白名单、缓存全在服务端，客户机本地不落盘）=====
$Y mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 -m /mnt/alice
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

> **伸缩性边界**：阻塞等锁在挂载进程里同步轮询（100ms 一次，上限 10 分钟），会占住一个 FUSE worker 线程；默认 `--threads 4` 下同一挂载点 ~4 个进程同时等锁时，其他请求会排队——等锁多的场景调大 `--threads`，事件驱动唤醒在演进计划里。

> **不能靠 pid 判活**：`( flock -x 200; sleep N )` 的 `setlk` pid 是瞬时 helper（加锁即退，锁活在继承的 open-file-description 上），用 pid 存活判定会误杀活锁；`flush` 每次 close 都 fire 且 owner 与 setlk 不一致，不能用来回收。只有 `release`（last-close）是持有者终止的权威信号。非合作应用（cp/dd）仍不阻塞，与 NFS/SMB advisory 语义一致。

## 高可用：meta-server 主备

```bash
# 主：读写，并把快照周期推送给备
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# 备：只读服务，主心跳丢失后自动提升为 primary
$Y meta-server --db-path /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主（可选 RPO=0）：加 --sync-repl，写返回前同步等备落盘 ACK
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# 真 fencing（可选）：加 --witness <逗号分隔的主备都可达的 data-server 仲裁者列表，推荐奇数 N>
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
# 客户端故障转移列表
$Y mount --view alice --meta 10.0.0.5:50110,10.0.0.6:50111 --data 10.0.0.5:50100 -m /mnt/alice
```

- 两节点必须用**各自独立的 `--db-path`**（各存一份视图副本，节点间靠快照复制）。
- primary 每 ~500ms 推快照 + 心跳 epoch；standby 心跳超时后提升并 epoch+1，旧主恢复发现更高 epoch 自动降级。
- **`--sync-repl`（opt-in）**：写 op 返回前同步推快照并等备 `fsync` + 原子 rename 的 ACK，把 **RPO 从 ~500ms 降到 0**。备不可达/超时时**诚实降级为 async 并打 warn**（不谎报 0）。
- **`--witness`（opt-in，fail-safe fencing）**：用一个或多个主备都能访问的 `data-server` 作**独立仲裁者集合**（逗号分隔，推荐奇数 N；N=1 退化为单仲裁、行为与旧版完全一致）。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予（meta 侧对全列表并发 fan-out 并按多数派判决 `witness_quorum`；两个多数派集合必交于≥一个仲裁者，该仲裁者只把 token 顶高给第一个 claimer，第二个见 `prev==我` 只算续约不算顶高，故同值碰撞天然只有一主）。每个仲裁者维护单调 `fence` token（`OP_FENCE` 的 `fetch_max` CAS 回传**折入前的值 `prev`**）：claim 赢当且仅当 `prev < 我的 epoch`，`prev >= 我` 则被 fence（对端胜出或同值碰撞）——回传 `prev` 正是为了在仲裁者恢复时区分同值碰撞、绝不双主。在任 primary 每 tick 向多数派续一份有界租约（lease，默认 3s；多数报 `prev>我` 则自降级），每个仲裁者拒绝重叠租约——同一时刻至多一份活租约，故多数活租约本身即独占权威的证明；提升方亦非重叠，须待前任租约到期才可抢占（`prev>=candidate` 同值碰撞/对端更高，或前任租约尚活则拒绝/延后，旧主可证明地已停）。**FAIL-SAFE**：配置了 witness 却**组不到租约多数派**连续超过宽限（3s）时，节点放弃写权限——primary 自降级、standby 拒绝提升（**写冻结**），宁牺牲可用也不冒双写。各仲裁者独立持久化 token 到 `<root>/.fence_token`，其重启不清零、不复活过期 primary。**相比单仲裁的可用性红利**：宕掉**少数**仲裁者仍能组多数续租、**不冻结写**；少数仲裁者的 `.fence_token` 盘丢也不复活过期主（多数仍保单调 token）；唯**失去多数**才 fail-safe 冻结。写请求由 primary 的**本地租约有效性**直接放行（不再每写一个仲裁 RTT，仅每续租一次向全列表 fan-out，摊薄到整窗），被 fence 的过期主在租约到点后的下一次复证即拒绝；元数据面复证拦不住内容字节（`write()` 先落 backend、再 debounced touch meta），故客户端把实时 epoch 贴到每个写意图数据帧（create/write/truncate/remove/fsync），作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`，过期主无法绕过元数据面静默写入内容（被封帧向权威重取 epoch 并自动重试一次）；且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化（claim 取独占锁会排在途写排空，而 witness 与存储同节点），故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合（数据面门控仅对 `--witness` 列表内的 data-server 生效，故应把 witness 与客户端实际写入的内容盘同节点部署；列表外的内容盘 token 恒为 0、数据面 gate 不生效，仅剩元数据面 fence 兜底）。两面合起来把双写窗口收敛到**租约安全余量内**（`WITNESS_LEASE_SLACK`=500ms：对端只能在我方租约到点后取 token，我方提前该余量停止信任缓存租约；而非 peer-only 的不封顶）。

## 鉴权 / 只读 / 性能

**Token 双面鉴权**（同时覆盖数据面 `OP_OPEN` 与元数据面 `M_OPEN`）：

```bash
$Y data-server     --listen 0.0.0.0:50100 --root /data/cluster --token s3cr3t
$Y meta-server --db-path ... --listen 0.0.0.0:50110 --token s3cr3t
$Y mount --view alice --data ... --meta ... --token s3cr3t -m /mnt/alice
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
| 元数据读连接池 | 每视图 SQLite 引擎 4 条附加连接（`READ_POOL_SIZE`）服务 `lookup`/`getattr`/`readdir`，读不再排写锁；PG 引擎另持一条对等的 4 连接只读池（`PG_READ_POOL_SIZE`）。写仍各走单一权威连接（SQLite `conn` / PG `client`）串行 |
| 目录分页 + READDIRPLUS | `readdir_page` 下推 `LIMIT/OFFSET`，回复缓冲按 32→1024 几何块填充；默认协商 `FUSE_DO_READDIRPLUS`，`--no-readdirplus` 可关。SQLite/PG 本地挂载与对接 SQLite/PG `meta-server` 的远端挂载（`M_READDIRP` 帧下推分页）都吃到两者；只有派生/透镜视图与不含该能力位的老服务端每内核缓冲取一次全量列表、不协商 PLUS |

## 备份与迁移

- **数据对象**：`blocks/` 是普通文件目录，可直接 `rsync`/快照备份。
- **视图库**：`<home>/meta/<view>.db`。**勿在有写活动时裸拷 DB**；单机先停写或 checkpoint 后再拷，HA 模式由 meta-server 用备份 API 复制。

## 部署与维护（手把手）

完整的分阶段教程在 **[USAGE.md](USAGE.md) §11–§14**（网页版同步见 docs.html）：§11 从环境预检 → 构建安装 → 拓扑规划 → token → systemd 常驻（中心双服务 + 客户端模板挂载单元）→ 防火墙 → 8 步验收清单 → HA / PostgreSQL 变体；§12 给日常巡检（日志关键字→处置对照表）、备份恢复（单机 / SQLite+HA / PG 三流程）、滚动升级、故障速查表与变更后回归；§13 是一组**固定 IP 的完整案例实操**（存储机 192.168.1.10 存 /data、SQLite 后端，客户端读写挂 /mnt/da、只读挂 /mnt/db），手把手逐参数解释；§14 进阶回答“数据目录怎么定制/存量大数据怎么导入（首选 `grove import` 零拷贝登记，备选经挂载 rsync 灌入，硬链接多入口、拆 view、未实现清单）/不同客户端想要不同显示方式”。最短生产路径六步：

```bash
# 1) 构建安装（每台节点同一个二进制）
cargo build --release && sudo install -m755 target/release/grove /usr/local/bin/grove
# 2) 中心：生成共享 token 并起两个服务（生产请用 systemd 常驻，USAGE §11.4 有 unit 模板）
TOK=$(openssl rand -hex 16)
grove data-server --listen 0.0.0.0:50100 --root /data/cluster --token $TOK &
grove meta-server --db-path /data/cluster --listen 0.0.0.0:50110 --token $TOK &
# 3) 防火墙只对内网段开 50100/50110
# 4) 客户机：新 view 首挂即自动建权威库，无需预先 init
grove mount --view alice --data 10.0.0.5:50100 --meta 10.0.0.5:50110 --token $TOK -m /mnt/alice
# 5) 验收：双客户端 A 写 B 即时可见 + 跨机 flock + findmnt 显示 grovefs:alice
# 6) 日常：journalctl -u grove-meta-server 盯降级/冻结关键字；冷拷 blocks/ + meta/*.db + acl.db 即完整备份
```

## 测试

```bash
export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
cargo test --release                 # 单元 + 集成测试
bash .build-bin/run_all.sh           # 全板 18 套件
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准（纯功能回归）
```

套件：`build`、`unit`、`posix`、`edge`、`e2e`、`remote`、`errors`、`resilience`、`concurrent`、`metasrv`、`ha`（含 `--witness` fencing / fail-safe 场景）、`striping`、`locks`（含崩溃即时回收 / 续约跨过 lease）、`freshness`、`acl`（view 白名单 e2e）、`layout`（显示透镜 e2e：hide/only/map/link + 直通写删 + 脚手架守卫 + 服务端派生视图：`view define/list/rm` + 跨形状推送 + re-define 热换形免重挂 + 派生挂载只读〔远程/本地〕 + 纯别名：全树可写/真实名拒挂〔远程/本地〕/链与派生目标守卫/删别名恢复基名可挂/原子轮换 `--retire`〔泄露止损：旧名即刻失效、真实名全程拒挂、授权沿用〕，共 78 条断言）、`import`（零拷贝导入 e2e：硬链接直通/mtime 保真/GC 不伤源/增量重跑/派生视图组合〔只读〕）、`pg`（PostgreSQL 后端 e2e；无 PG 主机自动 SKIP、不拉低 GREEN）。

## 命令速查

> 先说人话:下表里的 `--home`、`--db-path` 等是**参数写法**(照抄用),它们各自管的东西如下(以**实际含义**为准):

| 它管什么(实际含义) | 命令行写法 |
| --- | --- |
| **本地存储根目录** — 单机模式下存放 `blocks/`(数据)与 `meta/`(视图)的总目录 | `--home <目录>` |
| **服务端元数据根** — `meta-server` 上放权威视图库、ACL、视图定义的总目录 | `--db-path <目录>` |
| **数据落盘位置** — `data-server` 实际写字节的存储根(默认同元数据根) | `--data-root <目录>` |
| **要被纳管的已有目录** — 本就躺在盘上、想变成一个视图的那批数据 | `--source <目录>` |
| **挂载点** — 本机用来访问这棵树的目录 | `-m <目录>`(即 `--mountpoint`) |
| **中心服务地址** — 客户端连过去的元数据 / 数据服务(主机:端口) | `--meta <地址>` / `--data <地址>` |
| **视图名** — 给一棵逻辑树起的任意标签(与 Linux 用户名无关) | `--view <名>` |
| **接入凭证** — 进入服务端的 token(数据面与元数据面可分别设) | `--token <串>` |
| **以指定身份挂载** — ACL 按此 uid 判定读写 | `--as-uid <uid>` |
| **只读 / 零缓存强一致** — 挂载为只读 / 关闭客户端缓存 | `--ro` / `--strict` |
| **目录列举提速的回退开关** — 关掉 READDIRPLUS（随目录项一并带回属性），退回普通 READDIR + 逐条 `LOOKUP`，仅供 A/B 与兼容排障（provider 自身不能分页的挂载本来就不协商 READDIRPLUS，此开关对它们为空操作） | `--no-readdirplus` |

| 目的 | 命令 |
| --- | --- |
| 建视图 | `grove --home H init --view U [--manifest M]` |
| 本地挂载 | `grove --home H mount --view U -m MP [--ro] [--strict] [--threads N] [--as-uid N] [--layout F] [--no-readdirplus]` |
| 远程挂载 | `grove mount --view U -m MP --data A[,A2] --meta M[,M_bak] [--token T] [--as-uid N] [--layout F]`（不需要 `--home`） |
| 显示规则文件 | 每行 `hide <src>` / `only <src>` / `map <src> <dst>` / `link <src> <dst>`（`#` 注释，绝对路径） |
| 定义派生视图 | `grove view --db-path D define --name V --base B --rules F`（存于 `D/meta/views/V.layout`，客户端 `mount --view V` 即得形状；虚拟视图挂载只读） |
| 定义纯别名 | `grove view --db-path D alias --name A --real R`（公开名 1:1 映射真实视图：`A` 全树可写，`R` 不可直挂） |
| 泄露后轮换公开名 | `grove view --db-path D alias --name A2 --real R --retire A`（先写后删零窗口：`A` 失效、`R` 全程不可直挂、授权不动） |
| 列出/删除视图定义 | `grove view --db-path D list` / `grove view --db-path D rm --name V`（派生/别名同目录同命令；只删定义，不动数据） |
| 导入存量目录 | `grove import --view V --source DIR [--prefix P] [--data-root R] [--dry-run]`（同盘硬链接零拷贝，重跑增量） |
| 卸载 | `grove umount -m MP` |
| 数据服务 | `grove data-server --listen IP:PORT --root DIR [--token T]` |
| 元数据服务（默认 SQLite） | `grove meta-server --db-path D --listen IP:PORT [--token T] [--role primary\|standby --peer ADDR [--sync-repl] [--witness DATA_ADDR]]` |
| 元数据服务（PostgreSQL 后端） | `grove meta-server --db-path D --listen IP:PORT --store postgres://user:pw@host:5432/dbname` |
| 视图白名单授权 | `grove acl --db-path D allow --view V --uid N [--read] [--write]` |
| 视图白名单撤销 | `grove acl --db-path D revoke --view V --uid N` |
| 视图白名单查询 | `grove acl --db-path D list --view V` |

## 诚实标注的边界

以下为本项目遵循「工程诚信」原则明确标注的限制，而非隐藏假设：

- **传输协议**：当前为自定义 TCP 帧，非 gRPC。gRPC（tonic/prost）已做 go/no-go 闸门评估，结论 **GO**（tonic 0.14 免 protoc），但完整集成**有意延后**——现协议已正确且被测。
- **HA RPO**：默认 async RPO ≈ 500ms（`--sync-repl` 可降到 0）。
- **fencing**：`--witness`（逗号分隔的 N 个共享 data-server 作 `prev`-回传 epoch CAS **多数派**仲裁，需 `N/2+1` 授予）已落地并 **fail-safe**，primary 持不重叠的有界多数派租约（多数活租约即独占证明，写由本地租约校验放行）、并对内容数据帧按 token 门控，把双写窗口收敛到**租约安全余量内**（保证收敛单主）。硬边界：严格两节点、**不配第三方仲裁** 时无法防脑裂、窗口不封顶（CAP/共识定理限制，须部署 witness 或第三节点）；**多数派仲裁的可用性红利**：配 `--witness` 后宕**少数**仲裁者仍不冻结写、少数盘丢不复活过期主，唯**失去多数**才 fail-safe 停写（N=1 则退化为单仲裁、witness 本成为单点可用性依赖）。
- **条带**：单条已饱和链路（loopback/单网卡）上**无吞吐增益**，价值在多节点聚合容量；真实多机链路须重测后才可声称收益。
- **显示透镜（`--layout`）**：是**显示层定制而非安全边界**——绕过透镜直接挂同一 view 仍能看到全树，硬隔离请用独立 view + ACL/token；向纯合成目录创建/重命名进入返回 `EPERM`、撞透镜已显示名返回 `EEXIST`、`only` 模式下白名单外不可写；symlink 目标文本不改写；共享 meta-server 下挂合成目录的条目失效推送可能不命中透镜坐标（建议 `--strict`）；规则体目前限同一 view 内部重排，不支持跨 view 引用。
- **服务端派生视图（`grove view`）**：**一律只读挂载**（硬契约）——形状集中治理、只分发不修改，写只属于基名视图；客户端据 `M_OPEN` 握手标志落内核 RO，服务端另对派生名写意图 op 设权威门禁（`E_NOTPERM`），本地派生挂载同判；若需可写的自定义形状，用客户端自备的 `mount --layout` 透镜（§每客户端显示定制，直写真实条目）。其余：同样**不是安全边界**——直挂基名仍见全树（能拦住的是 ACL/token，不是形状）；`define` 替换**热生效**（服务端逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂），`view rm` 不热作用（已挂载者保持旧形状到重挂）；切换形状时旧虚拟 ino 上的合成路径/fd 可能落回 ENOENT，真实条目 inode 稳定不受影响；不嵌套（base 必须是物理视图）、不得与基同名、不得遮蔽物理 db；`meta/views/` 是控制面配置，HA 备机需手工同步（同 `acl.db`）；热重载仅限 `meta-server`，本地（`--home`）挂载仍在挂载时解析一次。
- **服务端纯别名（`grove view alias`）**：与派生视图的正反面——别名挂载全树**可写**、真实名被治理后**不可直挂**（本地与服务端均拒）；单跳解析、每 `M_OPEN` 重扫 `views/`（挂载是罕见事件）；拒绝别名链/派生目标/一真实名多别名；`import` 拒别名目标；删别名仅影响**新**挂载（已建立连接不受影响，同 `view rm` 语义）；`--retire` 原子轮换公开名（先写后删、真实名全程不可直挂、无裸露窗口，旧名落到按需新建的空命名空间而触不到真实树，ACL 记在真实名上授权不动），但轮换/删别名都只影响新挂载；远端拒挂真实名时客户端回显复用 token 拒因文案（`E_AUTH` 共用码，行为正确、提示不精确），本地拒挂文案精确；改名不是安全机制（公开名终究要分发），真实名隐藏属名字治理，硬边界仍是 token+ACL。
- **零拷贝导入（`grove import`）**：硬链接要求源与 `blocks/` 同文件系统（跨设备自动退化为独立拷贝并如实报告）；导入后源文件若被外部工具以 **rename/替换写**方式更新（vim、rsync --inplace 关闭等默认行为），旧 inode 内容冻结在最后一次直通写入上——需对新内容重跑 `import`（已存在的名字会跳过，可用不同 `--prefix` 强制重登记）；symlink/特殊文件有意跳过（不表示）；导入对象为单文件，不走 ≥16MiB 的跨 server 条带（条带仍是经挂载写入新大文件的能力）；meta-server 模式下 import 直写权威 db 文件，需在存储机执行、已挂载客户端靠 TTL（≤2s）收敛而非即时推送；PG 后端不支持（权威树在 Postgres 里，走挂载点 rsync 灌入）；`--source` 建议与存储根同盘并**保留**——删源仅失去「源路径」这一别名，数据不丢，但外部改名/迁移后视图与文件实体的关联就只剩 `blocks/` 链接。
- **元数据读并行 / 目录分页的作用范围**：只读连接池与 `LIMIT/OFFSET` 分页现长在**两个存储引擎**里——SQLite `MetaEngine` 与 PG `PgEngine`（各 4 连接只读池）。本地挂载（`--home` SQLite 或 PG）直接吃到分页与 PLUS；`meta-server` 的并发 RPC 处理器吃各自后端的只读池，且新增的 `M_READDIRP` 帧带 `offset/limit` 把分页下推到服务端，远端客户端每个内核缓冲只取一页并协商 `FUSE_READDIRPLUS`——同一 5001 条目录远端实测由改造前 ~54s 降到 **2.29s**（只留分页、关 PLUS 为 4.22s，两份输出逐字节相同）。仍未吃到这条路径的是**派生/透镜视图**（`LayoutMeta`：其隐藏/映射形状无法表示为单条 SQL 页，走 trait 默认、不协商 PLUS）与对接**不含该能力位的老 `meta-server`** 的挂载（`M_OPEN` ack 缺第二字节则回退全量列表）。元数据**写**并行（多权威）仍未做，两后端各自单写连接串行。开关判据是 `MetaProvider::supports_paged_readdir()`，`cargo test` 里 `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` 钉住该契约。
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
- **Base dir `--home`**: in local mode, the physical root holding both `blocks/` (data) and `meta/` (views). A remote mount needs no local store at all; the central `meta-server` points its authoritative metadata root at `--db-path` instead.

On top of that, metadata and data can each be served as a network service (`meta-server` / `data-server`) so **many client machines mount and share one authoritative view over the LAN** — a central-storage + in-rack LAN-access topology, and **no NFS**.

## Key features

- **Per-user view isolation** with full custom nesting (`mkdir`/`mknod`/`rename`/`rmdir`).
- **Pluggable storage/metadata backends** via traits: local, remote, and cross-server striped (`ShardedBackend`). The authoritative metadata store can additionally be swapped to **PostgreSQL** (`meta-server --store postgres://…`): one PG schema per view, transactions around rename/unlink/lock; under PG the in-tree HA (snapshot shipping) is intentionally off — use PG's own streaming replication / PITR instead.
- **Optional per-view uid whitelist**: `grove acl --db-path D allow/revoke/list` manages `view_acl(view,uid,can_read,can_write)` in `<db-path>/meta/acl.db` (same option name, same directory as `meta-server --db-path`). No rows for a view = unrestricted (back-compat, the legacy suites unchanged); any row = whitelist → `M_OPEN` fails with `E_AUTH` if the uid has no read grant, and every write-intent op is refused with `E_NOTPERM` (surfacing as `EPERM` at the client's FUSE layer, deliberately distinct from an HA standby's `E_ROFS` failover signal) if it lacks a write grant. The write gate is re-evaluated per-op (not cached at open), so `acl revoke` takes effect on already-mounted connections immediately. Multiple clients can therefore share one authoritative tree at differing privilege levels.
- **Per-client presentation overlay (`mount --layout rules.txt`)**: one authoritative tree, one copy of bytes, a different *displayed* shape per mount — `hide` trims a subtree, `only` whitelists, `map` relocates an entry to another path (its source spot vacates), `link` adds an alias visible in both places at once (same inode). The lens is pure `MetaProvider`-layer path translation: real entries keep their real inodes, so data IO, locks, GC and invalidation push go straight through; nothing is copied and other mounts are unaffected.
- **Server-side derived views (`grove view define` + `mount --view <name>`)**: when shapes should be managed **centrally**, `grove view define --name docsonly --base team --rules F` stores the rules in the meta root (`<db-path>/meta/views/<name>.layout`, first line `base <physical view>`) and clients simply `mount --view docsonly` — **no local rules file needed**. Derived (virtual) views mount **READ-ONLY, always**: the shape is centrally governed — distribute it, never modify it — so every write belongs to the original (base) view; a mount of a derived name negotiates the read-only flag at `M_OPEN` and lands on a kernel read-only mount (writes give `EROFS`), with an independent authoritative per-op gate on the `meta-server` and the same check applied to local (`--home`) mounts. One definition renders the same shape for every machine (virtual inodes agree across hosts), **edited definitions hot-apply** (the server checks the definition's mtime per op; live mounts reshape within their ≤2s cache TTL, no remount), and `grove acl allow --view <name>` grants per derived name, closing the shape-plus-privilege loop. A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` when no `--layout` file is given.
- **Server-side pure aliases (`grove view alias`)**: governance of *what a view is called* rather than *what it looks like* — a public name maps 1:1 onto a physical view (the definition is a single `alias <real view>` line, no rules), so an alias mount shows the **FULL tree and stays WRITABLE**, while the **real name stops being a mount entry point** (both `meta-server` and local `--home` mounts of a governed real name are refused). Public names can rotate freely without exposing the storage identity — guessing the real name no longer opens a door, and a leaked public name is retired in **one gapless command** (`alias --retire`: write-before-remove so the real name stays unmountable throughout, and because ACL grants are keyed to the real name they carry over untouched). Aliases share `meta/views/` and `list`/`rm` with derived views; ACLs are checked against the REAL name (resolution precedes the read gate, so one grant covers both doors); alias chains and derived targets are rejected; `grove import` refuses an alias target (an alias is not a storage identity).
- **Zero-copy import of existing data (`grove import`)**: when the server already holds a pile of directory data (say `/data/legacy`), `grove import --view V --source DIR` registers the whole tree into a view without moving bytes — on the same filesystem each object is a **hard link** of the source file (zero extra bytes; both paths read/write one inode, live in both directions), degrading to a streamed copy across devices. Structure/timestamps/mode/owner are preserved, re-running is incremental, and deleting a name in the view only unlinks the `blocks/` path — **the user's source data can never be destroyed by the view**. Composes with derived views/lenses for virtual-directory shapes (`map` the imported paths anywhere; the resulting derived mount is **read-only** — write through the imported base view). `grove import` also refuses a derived target name, which would produce a shadow `<view>.db` nobody can read.
- **Custom binary framing over TCP** (not NFS, not gRPC) with a connection pool.
- **Real-time cross-client invalidation push** (default): after a write, the center broadcasts `inval_entry`+`inval_inode` so peers see changes immediately, no polling; `--strict` gives a zero-caching fallback.
- **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` arbitrated centrally, effective across processes and machines; crash reclaim via FUSE `release`, plus a short lease (default 15s) and renewal heartbeats.
- **High availability**: primary/standby `meta-server` with heartbeat/epoch promotion and automatic demotion; `--sync-repl` drops write RPO to 0; `--witness` takes a comma-separated list of shared `data-server` arbiters (odd `N`) and arbitrates by **majority** (`quorum_size = N/2+1`), **fail-safe**, holding a non-overlapping lease at a majority (renewed every tick; each arbiter refuses overlapping leases, so a majority live lease is itself proof of exclusive authority and a write is admitted from a local lease check) and stamping content frames so the arbiters can fence a stale writer, bounding the double-write window to the lease safety margin; losing a minority of arbiters no longer freezes writes (`N=1` degrades to a lone witness).
- **Token authentication**, **read-only mounts** (`--ro`), **multi-threaded `fuser 0.18`** (`clone_fd`), **chunked parallel IO**, and **cross-server striping**.
- **Parallel metadata reads + paged directory listings + `FUSE_READDIRPLUS`**: alongside its single writer connection, each view's `MetaEngine` keeps 4 extra connections (`READ_POOL_SIZE`) that serve `lookup`/`getattr`/`readdir` and the nlink resolution, so concurrent reads no longer queue behind the write lock (**writes still serialize on that one authoritative connection** — single-writer semantics are untouched); the server-side engine uses the same pool, so concurrent `meta-server` RPC handlers stop waiting on each other too. Directory listing gained `readdir_page(parent, offset, limit)`, which the SQLite engine pushes down as SQL `LIMIT/OFFSET` while the FUSE callbacks fill each reply buffer with geometrically growing 32→1024 chunks, so one kernel request costs `O(buffer)` instead of `O(whole directory)` (previously every kernel page re-scanned the whole directory and re-derived nlink per entry); and `FUSE_DO_READDIRPLUS` (kernel >= 4.6) is negotiated by default, shipping attributes with the entries so `ls -l` stops degrading into N `LOOKUP`+`GETATTR` round-trips. Measured on one 5001-entry directory, same mount point, each pass behind a fresh mount (empty kernel dentry cache), `ls -l`: **2.56s**, against a 55.34s pre-change baseline and 5.22s with `--no-readdirplus` (paging only) — all three listings byte-identical once sorted. Paging and READDIRPLUS are enabled on any provider that pages *inside its own store*: local `--home` SQLite and PostgreSQL (each with its own 4-connection read pool + SQL `LIMIT/OFFSET`), and remote mounts against a SQLite/PG `meta-server` — a new `M_READDIRP` frame pushes `offset/limit` to the server, so a remote client fetches one page per kernel buffer and negotiates READDIRPLUS. Same 5001-entry dataset over `meta-server`+`data-server`: **~54s on the pre-change binary → 2.29s** (4.22s with paging only, PLUS off; listings byte-identical once sorted). Only derived/lens views (`LayoutMeta`) and mounts against a server too old to advertise the paging cap still fall through to one full listing per buffer with no READDIRPLUS. `--no-readdirplus` stays as the A/B and compatibility knob.
- **Full regression**: `run_all.sh` — **18 suites, all GREEN** (new `acl` per-view whitelist e2e + `layout` presentation-overlay e2e incl. server-side derived views and read-only derived mounts, remote & local, and pure aliases — writable alias mounts, real-name mount refusal (remote & local), chain/derived-target guards, rm reopens the real name, and gapless name rotation via `--retire` (a leaked public name is swapped in one step — old name stops resolving, the real name stays unmountable throughout, ACL grants carry over); 78 assertions + `import` zero-copy import e2e + `pg` PostgreSQL backend e2e; the PG suite self-skips on hosts without a reachable PostgreSQL, so GREEN is preserved either way).

## Architecture

Three layers, all trait-decoupled:

```
┌─ FUSE mount process  (fs_impl.rs: GroveFS, &self, thread-safe)
│    ├── MetaProvider trait   view-tree access
│    │     ├── local:  MetaEngine  → <home>/meta/<view>.db
│    │     ├── remote: RemoteMeta  → meta-server (authoritative)
│    │     └── overlay: LayoutMeta → mount --layout (per-client presentation, decorates either)
│    │           ├── derived view: server-side LayoutMeta singleton → mount --view <name> (rules defined once via grove view define, stored in meta/views/)
│    │           └── pure alias: `alias <real view>` → public name maps 1:1 (full tree, writable; real name not mountable — name governance)
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

> **Honest boundary**: adding PG does **not** parallelize metadata *writes* — as a single writer, `meta-server` still serializes every mutation on one authoritative connection (SQLite `conn` / PG `client`) even in PG mode. PG's value is in **durability / replication / operational maturity**, not write-throughput scaling; genuine metadata write-concurrency scaling is a larger architectural problem and is not done here. For default deployments, keep the zero-dependency SQLite. On wording: the shipped metadata *read* parallelism now spans **both** engines — SQLite `MetaEngine` and PostgreSQL `PgEngine` each keep their own 4-connection read pool and page listings with SQL `LIMIT/OFFSET` — so reads fan out on either backend while writes stay serialized.

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

## Parameter meanings at a glance

> Names like `--home` / `--db-path` below are the literal CLI flags (copy them as-is); what each one actually points at:

| What it controls (plain meaning) | Flag |
| --- | --- |
| **Local storage root** — the directory holding `blocks/` (data) and `meta/` (views) in single-node mode | `--home <dir>` |
| **Server metadata root** — the host directory holding the authoritative view DBs, ACL and view definitions for `meta-server` | `--db-path <dir>` |
| **Data landing root** — where `data-server` writes bytes (defaults to the metadata root) | `--data-root <dir>` |
| **Existing directory to take over** — data already on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory you use to reach the tree | `-m <dir>` (`--mountpoint`) |
| **Central service address** — the meta / data endpoints a client connects to (host:port) | `--meta <addr>` / `--data <addr>` |
| **View name** — an arbitrary label for one logical tree (unrelated to Linux usernames) | `--view <name>` |
| **Access credential** — the token to enter the server (data face and meta face can differ) | `--token <str>` |
| **Mount as a given uid** — ACL reads/writes are decided for this uid | `--as-uid <uid>` |
| **Read-only / zero-cache strong consistency** | `--ro` / `--strict` |
| **Directory-listing fast path escape hatch** — turn off READDIRPLUS (attributes shipped with each entry), falling back to plain READDIR + per-entry `LOOKUP`; A/B benchmarks and compatibility debugging only (mounts whose provider cannot page in its own store never negotiate READDIRPLUS, so this flag is a no-op there) | `--no-readdirplus` |

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

```text
Client A (uid=1000)  mount --view alice ─┐
Client B (uid=2000)  mount --view alice ─┤ same --view = one shared tree; one client
Client C (uid=1000)  mount --view bob   ─┘ may mount many views at once (N:M)
        │
        ▼ TCP frame M_OPEN(view, token, uid)   (clients never open .db files or hold PG conn strings)
┌─ meta-server (single authoritative writer) — gates run in order, then routing
│  1) token gate    mismatch -> E_AUTH (cluster admission; the only hard gate; client-reported uid is advisory)
│  2) read gate     view_acl in <db-path>/meta/acl.db: no rows = unrestricted; any row = whitelist needing can_read, else E_AUTH
│  3) write gate    every write-intent op re-checks can_write, else E_NOTPERM (not cached -> revoke is immediate)
│  4) view name -> engine cache   first M_OPEN opens lazily; later mounts of the same view reuse the instance
└──┬──────────────────────────────────
   ▼ view=alice                          ▼ view=bob
  alice's authoritative tree (shared A+B, writes serialized)   bob's tree (C only, invisible to alice)
   ├─ SQLite (default):      one view = one DB file   <db-path>/meta/alice.db · bob.db
   └─ PostgreSQL (--store):  one view = one schema    grovefs_v_616c696365 · grovefs_v_626f62

The unit of isolated storage is the *view*, not the client; content bytes live in the data-server's `<root>/blocks/<key>` (its `--root`), referenced by data_ref inside each tree.
```

- **Many OS users sharing one view**: point every client at the same `mount --view alice --meta ... --data ...` and both metadata (single SQLite/PG tree, writes serialized through the central engine) and content (single `blocks/`) converge in-place. If you need **differentiated rights per uid** inside that shared tree (e.g. an auditor mount that can list but never mutate), add a whitelist entry on the central node:

  ```bash
  $Y acl --db-path /data/cluster allow  --view alice --uid 1000 --read --write   # rw peer
  $Y acl --db-path /data/cluster allow  --view alice --uid 2000 --read          # read-only subscriber
  $Y acl --db-path /data/cluster list   --view alice                             # show grants
  $Y acl --db-path /data/cluster revoke --view alice --uid 2000                  # drop one row
  ```

  Semantics: **zero rows for a view = unrestricted** (every uid passes, matches historical behavior); **any row = whitelist** (a uid without a matching row is refused at `M_OPEN` with `E_AUTH` so mount fails fast; a read-only grant sees every write-intent op rejected with `E_NOTPERM` — `EPERM` at the client FUSE layer, deliberately distinct from an HA standby's `E_ROFS` failover signal — while reads continue). The write gate is re-checked before every op rather than cached at open, so `acl revoke` / a rw→ro downgrade takes effect on live connections immediately. In single-node local mounts the same read gate runs, but the write path lands on the underlying SQLite file directly — OS file permissions are the last line of defense there. The uid presented to the server is `geteuid()` by default and can be overridden with `mount --as-uid N` for role-based mounts against a shared `meta-server`.

  > **Honest boundary**: the client-reported uid is **advisory** — a TCP client can forge it. Real admission is still the `--token` gate; the whitelist is for **role separation inside an already-trusted cluster**, not for hostile-tenant isolation. Two behavioral boundaries to know: lock ops (`flock`/`F_SETLK`, incl. lease renewal) write the central `locks` table and count as write-intent, so a **read-only grant gets `EPERM` even on a shared lock** (grant `--write` if a read role needs locking); and the read gate is checked once at `M_OPEN` — “revoke is immediate” refers to the per-op write gate, while an already-mounted reader keeps reading until remount.

- **Metadata backend is swappable: SQLite ↔ PostgreSQL** (this is the `meta-server`'s *authoritative* store only; the wire protocol clients see does not change):

  ```bash
  # default: embedded SQLite, one `<home>/meta/<view>.db` per view
  $Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL: one `grovefs_v_<hex(view)>` schema per view, tx-guarded rename/unlink/lock
  $Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 \
       --store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  Clients mount exactly the same way (`--meta host:port --view alice`). Notes that matter:
  - **PG is mutually exclusive with the in-tree HA flags**: `--store postgres://…` combined with any of `--role standby` / `--peer` / `--witness` fails fast at startup — GroveFS-native HA ships snapshots via SQLite `VACUUM INTO`, which has no PG analogue. On PG, use PostgreSQL's own streaming / logical replication / PITR, which is strictly stronger than the file-snapshot mechanism anyway.
  - `acl.db` remains a **local SQLite file** in PG mode as well: it is control-plane admission state, not part of any view tree, and should not travel with the view into PG's replication domain.
  - Tables are created idempotently on the view's first `M_OPEN` (`CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS`), so no pre-migration script is needed; the root row is pinned at `id=1` and `setval(pg_get_serial_sequence(...), MAX(id))` aligns the sequence, matching SQLite's `ROOT_INO` semantics exactly.

  Why Redis was *not* added: Redis's K/V+TTL model is not isomorphic to a view tree (rename/unlink/hardlink/refcount GC want transactions, lock arbitration wants row-level state). Emulating relational semantics on Redis would rebuild a worse PG at the application layer. **GroveFS ships PG, not Redis.**

## Per-client presentation: same data, different shapes (`--layout`)

View isolation answers "different views = different trees". `--layout` answers the other half: **one stored tree, customized display per client** — show only part of it, or reorganize entries into paths the client prefers. One rule per line (`#` comments, absolute paths; source paths address the stored tree):

| Rule | Effect | Typical use |
| --- | --- | --- |
| `hide <src>` | hide the whole subtree at its source spot | mask a private dir |
| `only <src>` | whitelist: show just these subtrees (ancestor dirs pass through) | expose only /docs |
| `map <src> <dst>` | relocate: the source spot vacates, the entry shows at `<dst>` (scaffold dirs synthesized as needed) | surface /data/raw as /team/raw |
| `link <src> <dst>` | alias: both spots show it, same inode (no byte copy, no extra refcount) | shortcut /proj/mylib |

Semantics under test: real entries keep their **real inodes** — reads/writes/locks/striping/GC pass through to the data plane; `rm` through the lens deletes the source entry; `hide`/`only` still filter *inside* relocated subtrees by source path; creating inside a purely synthesized scaffold dir gives `EPERM`, a plain create whose name collides with a lens-shown entry gives `EEXIST`; symlink target text is not rewritten. In `only` mode, writing into a pass-through ancestor outside the whitelist is `EPERM` as well.

```bash
$ cat .demo/lens.txt                     # client C's lens
hide /photos/private
map /data/raw /team/raw
$ grove --home .demo/home mount --view alice -m .demo/mnt --layout .demo/lens.txt &
layout overlay .demo/lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at .demo/mnt (base .demo/home)

$ ls .demo/mnt                                   # this client sees: data docs photos team
data  docs  photos  team
$ ls .demo/mnt/photos                             # private is gone
public
$ echo "hello via lens" > .demo/mnt/team/raw/note.txt   # write-through, no copy
$ ls .demo/mnt2/data/raw                          # a plain (unlensed) mount sees it at once
big.bin  note.txt
$ mkdir .demo/mnt/team                            # 'team' is lens scaffolding — name taken
mkdir: cannot create directory '.demo/mnt/team': File exists
```

Rules are parsed once at mount time (an invalid file fails fast rather than silently showing everything). The lens is a **per-mount-instance** presentation layer — nothing is persisted, no server state changes — so it composes with local `MetaEngine` or remote `RemoteMeta`, with `--ro`, and with the ACL whitelist. Against a shared `meta-server`, invalidations for entries living under synthesized dirs are broadcast by *source* path and may miss lens coordinates; add `--strict` (zero attribute/entry cache) in that setup.

## Server-side derived views: one copy of data, a shape per `--view` name (`grove view`)

`--layout` is configured per client. When shapes should be **managed centrally** — so that a team shares one display contract under a name — use derived views: `grove view define` stores the rules in the meta root (`<db-path>/meta/views/<name>.layout`, first line `base <physical view>`, remaining lines are the same four verbs as `--layout`), and clients simply `mount --view <name>` with **no local rules file**. Derived (virtual) views mount **read-only, always** — the shape is a centrally governed distribution, every write belongs to the original (base) view. One definition renders the same shape on every machine (a server-side `LayoutMeta` singleton per derived name, so virtual inodes agree across hosts), and `grove acl allow --view <name>` can grant access per derived name, closing the shape-plus-privilege loop. A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` automatically when no `--layout` file is passed.

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grove view --db-path .demo5/home define --name docsonly --base team --rules .demo5/docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grove view --db-path .demo5/home define --name team --base team --rules .demo5/docsonly.rules   # name == base
Error: "derived view name must differ from its base"
$ grove view --db-path .demo5/home list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grove --home .demo5/home mount --view docsonly -m .demo5/mnt &    # mount by name, no --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
mounting view 'docsonly' at .demo5/mnt (base .demo5/home, read-only)
$ ls .demo5/mnt                              # the derived-name mount: just docs
docs
$ ls .demo5/mnt_full                         # the base name mounts the same, full tree (writable)
docs  photos
$ echo "new entry" > .demo5/mnt_full/docs/2026/b.md   # a write on the base tree…
$ ls .demo5/mnt/docs/2026                    # …shows in the derived view after the short TTL (immediate via a shared meta-server: pushes are double-broadcast)
a.md  b.md
$ echo "from lens" > .demo5/mnt/docs/2026/c.md        # writes through a virtual view are refused, always
bash: .demo5/mnt/docs/2026/c.md: Read-only file system
$ grove view --db-path .demo5/home rm --name docsonly
removed derived view 'docsonly' (underlying data untouched)
```

Guards and semantics: `define` refuses `name == base`, refuses a base that is itself a derived view (no nested lenses) and refuses to shadow an existing physical `<name>.db`; invalid rules fail fast (never degrading into "show everything"). **Virtual views are read-only by hard contract**: writes belong to the original (base) view only. The `meta-server` ships a derived-name flag in the `M_OPEN` ack, so the client lands a kernel read-only mount immediately (every mutation — and `flock` write locks — give `EROFS` with zero round-trips); the server additionally keeps an **independent authoritative per-op gate** on derived names (answering `E_NOTPERM`, deliberately *not* `E_ROFS`, which clients read as an HA failover signal), so old or crafted clients cannot bypass it; a local (`--home`) mount of a derived name enforces the same RO at mount time. **Edits hot-apply, no remount**: the `meta-server` checks the definition file's mtime before dispatching every op, so a replaced definition reaches established mounts within the client's cache TTL (≤2s) — and the read-only contract survives the reshape; a `view rm` deliberately does **not** hot-apply (live mounts keep the shape they opened with until remount — nobody has the tree yanked out from under them). Behind a shared `meta-server`, a derived view's subscribers register in **both its own bucket and the base name's bucket** and every write is broadcast to both, so base writes land in derived mounts even without `--strict` (verified by section 9 of the `layout` e2e: base writes visible in the derived mount, derived mounts refuse writes/deletes with `EROFS` and leave the base tree untouched while the base name stays writable on the same server, a re-define reshapes a live mount, and local derived mounts are read-only too). `view rm` deletes only the definition, never data. Like `grove acl`, `grove view` operates on server-local files — run it where the `meta-server` lives (or share the same `--db-path`); under HA, `meta/views/` is control-plane config and must be synced to the standby by hand, exactly like `acl.db`.

Hot reload on one established connection (no remount anywhere in this transcript):

```bash
$ ls .demo3/mnt                             # docsonly = only /docs, mounted
docs
$ grove view --db-path .demo3/home define --name docsonly --base team --rules .demo3/broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls .demo3/mnt                             # same mount, shape widened by itself
docs  photos
```

## Pure aliases: name governance (`grove view alias`)

Derived views govern *what a view looks like*; **pure aliases** govern *what it is called* — the name is the alias, the mapping is the entry. The definition shares `<db-path>/meta/views/<name>.layout`, first line `alias <real view>`, no rule lines. The mirror image of a derived view: an **alias mount shows the FULL tree and is WRITABLE** (it simply is the real view under a public name), while the **real name stops being a mount entry point** — `meta-server` and local (`--home`) mounts alike refuse a governed real name, so public names rotate freely and the storage identity stays unexposed (guessing the real name no longer opens a door):

```bash
$ grove view --db-path $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grove mount --view teamwork -m /mnt/work --meta 10.0.0.5:50241 --data 10.0.0.5:50240   # full tree, writable
$ grove mount --view team -m /mnt/t --meta 10.0.0.5:50241 --data 10.0.0.5:50240          # the governed real name: refused
Error: Custom { kind: PermissionDenied, error: "meta-server rejected the access token" }
$ grove --home $BASE mount --view team -m /mnt/t                                        # local mount: same contract
Error: "view 'team' is governed by an alias — mount the alias name instead (`grove view list` shows it)"
```

When a public name leaks, rotate it in **one gapless command** with `--retire` (write the new alias before removing the old, so the real name stays governed throughout — there is no window where it becomes mountable by itself; ACL grants are keyed to the real name, so they carry over untouched):

```bash
$ grove view --db-path $BASE alias --name teamwork2 --real team --retire teamwork   # old public name leaked, swap to teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: ACL grants are keyed to 'team', so they carried over untouched — legitimate users just remount under 'teamwork2'
$ grove mount --view teamwork -m /mnt/leak --meta ... --data ...                     # old leaked name remounts as an empty view (never the real tree)
```

Guards and semantics: `alias` rejects `name == real`, shadowing a physical `<name>.db`, derived-view targets, and alias chains (resolution is single-hop by design); one real view may carry at most one public name (`--retire` rotates within one command — write-before-remove, so it is never blocked by this invariant — and refuses a `--retire` pointing at a derived view or a non-existent alias, so a shape definition can never be dropped by accident). ACL grants are checked against the REAL name (the alias swap happens before the read gate at `M_OPEN`, so `grove acl allow --view <real>` authorizes both doors — and rotating the public name needs NO re-grant). `grove import` refuses an alias target — an alias is not a storage identity, and writing a separate `<alias>.db` would create a shadow tree nothing reads. `view rm` on an alias reopens the real name for NEW mounts (established connections are untouched); rotation/rm only affect NEW mounts, and a retired name resolves to a fresh empty namespace that never reaches the real tree. Note the honest boundary: hiding/rotating the real name is name governance (leak hygiene), not a security mechanism — public names must be distributed to be used and rotation cannot revoke an existing token-holder's access, so real isolation stays token + ACL. (Verified by sections 10/11 of the `layout` e2e suite — 26 alias assertions; see USAGE §3.4 for full transcripts.)

## Importing existing data: zero-copy passthrough (`grove import`)

When the server already holds a pile of data under some directory and clients should get it through a view, there is no need to “stream it in” first: `grove import` registers an **existing local directory tree** into a view with the bytes staying exactly where they are —

```bash
$ tree .demo4/legacy
.demo4/legacy
|-- docs
|   `-- quarter.txt
`-- photos
    `-- 2026
        `-- y.jpg

3 directories, 2 files

$ grove --home .demo4/home import --view legacy --source .demo4/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grove --home .demo4/home import --view legacy --source .demo4/legacy
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grove mount --view legacy -m .demo4/mnt &    # a client mount now sees the tree
$ ls .demo4/mnt/legacy
docs  photos
$ stat -c '%y' .demo4/mnt/legacy/docs/quarter.txt   # the file's own clock, not import time
2021-05-06 07:08:00.000000000 +0000

$ echo "edited via the mount" > .demo4/mnt/legacy/docs/quarter.txt
$ cat .demo4/legacy/docs/quarter.txt           # the ORIGINAL source path, same inode
edited via the mount

$ rm .demo4/mnt/legacy/photos/2026/y.jpg       # delete a name inside the view
$ ls .demo4/legacy/photos/2026/                # the user's dataset is untouched
y.jpg
$ find .demo4/home/blocks -type f -links 2 | head -1   # the object was just a second link
```

Semantics (34 assertions in the `import` suite): **zero copies on one filesystem** — object and source share the inode (nothing shows up in `df`), with an honest streamed-copy fallback across devices; **live passthrough both ways** — an edit through the mount or an external rewrite of the source file shows up on the other side within the client’s ≤2s TTL; **GC only unlinks** — removing the name in the view deletes only the `blocks/` path, never the source; **idempotent and incremental** — re-running registers just the new names; mtime/atime/mode/owner preserved (`--dry-run` does not even create the view DB). Imported entries are then first-class: reads/writes, locks, snapshots and invalidation push all apply, and derived views/lenses can reshape them into virtual directories with `map`/`link` — that virtual shape then mounts **read-only** (writes go to the imported base view, visible through the shape via the double-broadcast pushes), and `grove import` refuses a derived target name outright, since it would write a shadow `<view>.db` no mount ever reads. Like `grove acl`/`grove view`, run it on the machine holding the store (the `--home`, or the meta/data servers’ root).

## Production topology: central storage + LAN mounts

```bash
# central node
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110

# each client (a remote mount needs no --home: nothing authoritative lives on the client; no local writes at all)
$Y mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 -m /mnt/alice
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

> **Scaling boundary**: a blocking lock wait polls synchronously inside the mount process (100ms cadence, capped at 10 minutes) and occupies one FUSE worker thread; with the default `--threads 4`, ~4 concurrent waiters on the same mount start queuing other requests — raise `--threads` for lock-heavy workloads; event-driven wake-up is on the roadmap.

> **Do not judge liveness by pid**: in `( flock -x 200; sleep N )` the `setlk` pid is a transient helper (the lock lives on the inherited open-file-description), so a pid-alive check would wrongly kill live locks; `flush` fires on every close with a mismatched owner and is unusable for reclaim. Only `release` (last-close) is the authoritative signal that a holder terminated.

## High availability

```bash
# primary
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# standby
$Y meta-server --db-path /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# primary with RPO=0 synchronous replication
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# true fencing via a majority of independent arbiters (comma-separated list, odd N)
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
```

- The two nodes must use **separate `--db-path`s** (each keeps its own copy; replication ships whole snapshots).
- `--sync-repl` (opt-in): each write blocks until the standby durably installs the snapshot and ACKs, dropping **RPO from ~500ms to 0**; on ACK timeout it honestly degrades to async with a warn (never falsely reports 0).
- `--witness` (opt-in, **fail-safe fencing**): one or more `data-server`s reachable by both halves (a comma-separated list; an odd count `N` is recommended, and `N=1` behaves exactly as a single witness used to) act as independent arbiters. Authority needs a strict majority `quorum_size(N) = N/2 + 1` to grant: the meta side fans out to the whole list concurrently and applies majority logic (`witness_quorum`). Because two majorities always intersect, only the first claimer can raise a shared arbiter (the later sees `prev == epoch`, a renew, not an advance), so a same-value race yields exactly one primary. Each arbiter keeps a monotonic fence token; `OP_FENCE` folds your epoch via `fetch_max` and returns the **value held before the fold (`prev`)**. A claim wins iff `prev < your epoch`; `prev >= your epoch` means you are fenced — returning `prev` (not the post-fold max) is exactly what resolves same-value collisions so a recovering arbiter never ends up with two primaries. A live primary renews a bounded lease at a majority every tick (self-demotes if a majority reports `prev > mine`); each arbiter refuses overlapping leases — at most one live lease at a time — so a majority live lease is itself proof of exclusive authority, and a write is admitted from a local lease check (one fan-out per renewal, not per write). A would-be promoter may seize a lease only after the incumbent's expires (it refuses if `prev >= candidate`, a tie or peer-ahead, or the incumbent lease is still live). **Fail-safe**: if a configured witness set cannot form a lease quorum beyond a 3s grace, a node gives up write authority (primary self-demotes, standby refuses to promote → writes freeze) rather than risk split-brain. **Availability gain over a lone witness**: losing a *minority* of arbiters neither freezes writes (a quorum is still reachable) nor resurrects a stale primary (a quorum still holds the monotonic token even if a minority of `.fence_token` stores are lost); only losing a *majority* freezes. Each arbiter persists its token at `<root>/.fence_token`, so a restart does not zero it. Beyond the meta-plane proof, the DATA plane is fenced too: the client stamps its live epoch onto every write-intent content frame (create/write/truncate/remove/fsync) and the witness data-server refuses one whose epoch lags its token (`EROFS`), since a `write()` otherwise lands bytes on the backend before the debounced meta touch — a fenced frame re-pulls the epoch from the authority and retries once. The data-server furthermore linearizes the epoch check and the backend write against a promotion (a claim takes an exclusive lock that drains in-flight writes, and the witness co-locates with storage), so a stale write can never land concurrently with the new primary's — content torn-writes are closed on the arbiter, not just bounded (this content gate is effective only on data-servers inside the `--witness` list, so deploy the witness co-located with the storage clients actually write to; a content store outside the set keeps a zero token, its data-plane gate stays inert, and only the meta-plane fence protects it).

## Auth / read-only / performance

**Token auth** covers both the data plane (`OP_OPEN`) and the metadata plane (`M_OPEN`); a mismatch fails at connect with `PermissionDenied`. Empty token (default) = LAN-trusted, no auth — trusted networks only.

**Read-only**: `mount ... --ro` → every mutating op returns `EROFS` (kernel `MS_RDONLY` + filesystem-side guard).

**Performance knobs**: `--threads N` (default 4, per-thread `/dev/fuse` fd), chunked parallel IO (reads ≥1MiB, writes ≥4MiB), large-file striping (≥16MiB), `touch_mtime` debouncing (200ms window), a 4-connection read pool per view engine on both backends (`READ_POOL_SIZE` / `PG_READ_POOL_SIZE`) that unblocks concurrent metadata reads while writes stay serialized, SQL-paged directory listings (`readdir_page`, `LIMIT/OFFSET` filled into the reply buffer in 32→1024 geometric chunks), and `FUSE_READDIRPLUS` negotiation (on by default wherever the provider pages in its own store — SQLite/PG local, and remote against a capable `meta-server` via `M_READDIRP`; `--no-readdirplus` to fall back to plain READDIR + per-entry LOOKUP).

## Deployment & operations (hands-on)

The full step-by-step guide lives in **[USAGE.md](USAGE.md) §11–§14** (mirrored in docs.html): §11 walks through environment preflight → build & install → topology planning → shared token → systemd units (two central services + a templated client mount unit) → firewall → an 8-step acceptance checklist → HA / PostgreSQL variants; §12 covers routine inspection (log-keyword → action table), backup/restore (single-node / SQLite+HA / PG), rolling upgrades, a troubleshooting table and post-change regression; §13 is a **fixed-IP worked example** (storage server 192.168.1.10 with data in /data on the SQLite backend, one read-write client at /mnt/da and one read-only client at /mnt/db) explaining every flag hands-on; §14 answers “custom data directories, bulk import of existing data (zero-copy `grove import` first, streamed rsync through a mount as the alternative; hardlink aliasing, view splitting, and an explicit not-implemented list), and per-client display layouts”. The shortest production path in six steps:

```bash
# 1) build & install the same binary on every node
cargo build --release && sudo install -m755 target/release/grove /usr/local/bin/grove
# 2) center: shared token + the two services (use systemd in production; units in USAGE §11.4)
TOK=$(openssl rand -hex 16)
grove data-server --listen 0.0.0.0:50100 --root /data/cluster --token $TOK &
grove meta-server --db-path /data/cluster --listen 0.0.0.0:50110 --token $TOK &
# 3) open 50100/50110 to the internal subnet only
# 4) client: a brand-new view self-provisions its authoritative DB on first mount — no pre-`init`
grove mount --view alice --data 10.0.0.5:50100 --meta 10.0.0.5:50110 --token $TOK -m /mnt/alice
# 5) accept: A-write/B-read instantly visible across two clients + cross-host flock + findmnt shows grovefs:alice
# 6) operate: watch journalctl -u grove-meta-server for degrade/freeze keywords; a cold copy of blocks/ + meta/*.db + acl.db is a complete backup
```

## Testing

```bash
export PKG_CONFIG=/path/to/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
cargo test --release
bash .build-bin/run_all.sh                 # 18 suites
SKIP_BENCH=1 bash .build-bin/run_all.sh    # skip throughput bench
```

## Honest limitations

These are explicit, honest boundaries, not hidden assumptions:

- **Transport**: custom TCP frames today, not gRPC. gRPC (tonic/prost) was gate-evaluated as **GO** but full integration is **deliberately deferred**; the current protocol is correct and tested.
- **HA RPO**: ~500ms async by default (0 with `--sync-repl`).
- **Fencing**: `--witness` is implemented and **fail-safe**: a `data-server` list (comma-separated, odd `N`) arbitrates by **majority** (`quorum_size = N/2+1`); a primary holds a non-overlapping lease at a majority (renewed every tick), so a majority live lease is itself proof of exclusive authority and a meta write is admitted from a local lease check (one fan-out per renewal, not per write) AND gates content frames by token at the witness data-servers, bounding the double-write window to the lease safety margin (converging to one primary). The irreducible boundary: a strict two-node pair **without arbiters** cannot prevent split-brain and leaves the window unbounded (CAP/consensus). The **availability gain of a quorum**: with `--witness`, losing a *minority* of arbiters neither freezes writes nor resurrects a stale primary (a quorum still holds the monotonic token); only losing a *majority* fails safe and freezes (`N=1` reverts to a lone witness, which becomes a single availability dependency).
- **Striping**: no throughput gain on a single saturated link (loopback/one NIC); the value is aggregate capacity across nodes — real multi-machine links must be re-benchmarked before claiming gains.
- **Presentation overlay (`--layout`)**: it is a **display customization, not a security boundary** — mounting the same view without the lens still shows the full tree; use separate views + ACL/token for hard isolation. Creating/renaming into a purely synthesized dir gives `EPERM`, a name colliding with a lens-shown entry gives `EEXIST`, and `only` mode refuses writes outside the whitelist; symlink target text is not rewritten; with a shared `meta-server`, invalidations for entries under synthesized dirs may miss lens coordinates (use `--strict`); rules relocate entries within one view only — cross-view references are not supported.
- **Server-side derived views (`grove view`)**: mounts of a derived name are **read-only by hard contract** — the shape is centrally governed, distributed but never modified; every write belongs to the base view. Enforcement is three-layered (kernel RO negotiated at `M_OPEN` → `EROFS` at the client; an authoritative per-op gate on the meta-server answering `E_NOTPERM`; the same check at local `--home` mount time). If you need a *writable* custom shape, use the client-side `mount --layout` lens (§ Per-client presentation overlay), which writes through to the real entries. Otherwise still **not a security boundary** — mounting the base name directly still shows the full tree (ACL/token gate access, not shapes); definition **edits hot-apply** (the server checks the file's mtime per op; live mounts switch shape within their ≤2s cache TTL, no remount), while `view rm` does not hot-apply (established mounts keep their shape until remount); during a shape swap, paths/fds on old virtual inodes may resolve to ENOENT — real entries keep stable inodes; no nesting (the base must be a physical view), no `name == base`, no shadowing a physical db; `meta/views/` is control-plane config that an HA standby must sync by hand (like `acl.db`); hot reload is `meta-server`-only — a local (`--home`) mount still resolves its definition once at mount time.
- **Server-side pure aliases (`grove view alias`)**: the mirror image of derived views — an alias mount shows the full tree and is WRITABLE, while a governed real name is NOT mountable by its own name (refused locally and by the `meta-server`). Resolution is single-hop, re-scanned per `M_OPEN` (mounts are rare); alias chains, derived targets and one-real-name-with-two-public-aliases are rejected at the command; `import` refuses alias targets; removing an alias affects only NEW mounts (established connections keep flowing, like `view rm`); `--retire` rotates a leaked public name in one gapless step (write-before-remove so the real name is never momentarily mountable; the retired name resolves to a fresh empty namespace that never reaches the real tree, and ACL grants keyed to the real name carry over) — but rotation, like `rm`, only affects NEW mounts and cannot revoke an existing token-holder, so it is leak hygiene, not a security boundary. One honest wording wart: a remote refusal surfaces with the shared `E_AUTH` text ("meta-server rejected the access token") rather than an alias-specific cause — behavior correct, message imprecise; the local refusal message names the alias contract exactly. And the honest boundary: name hiding is governance, not security — the public name must be distributed anyway; token + ACL remain the hard gates.
- **Zero-copy import (`grove import`)**: hard links require source and `blocks/` to share a filesystem (cross-device falls back to an independent copy, reported honestly); if an external tool later updates the source file via **rename/replace-write** (the default of vim, rsync without `--inplace`, …) the imported inode keeps the bytes from the last passthrough write — re-run `import` for new content (existing names are skipped; use a different `--prefix` to force a re-registration); symlinks/special files are deliberately skipped (not representable); imported objects are single files and do not participate in the ≥16MiB cross-server striping (striping remains a property of fresh writes through a mount); in a `meta-server` deployment import writes the authoritative DB file directly, so run it on the storage host and expect live mounts to converge by TTL (≤2s) rather than push; the PostgreSQL backend is not supported (its authoritative tree lives in Postgres — stream data in through a mounted path); keep `--source` on the store’s filesystem and **around** — deleting the source only drops that alias name (data survives in `blocks/`), but after external renames the `blocks/` link is the only remaining path to the bytes.
- **Scope of the metadata read pool and directory paging**: both now live in **two storage engines** — SQLite `MetaEngine` and PostgreSQL `PgEngine` (each a 4-connection read pool + SQL `LIMIT/OFFSET` paging). Local mounts (`--home` SQLite or PG) get paging and READDIRPLUS directly; `meta-server` handlers use their backend's read pool, and the new `M_READDIRP` frame carries `offset/limit` so a remote client fetches one page per kernel buffer and negotiates `FUSE_READDIRPLUS` — the same 5001-entry directory over `meta-server`+`data-server` drops from ~54s pre-change to **2.29s** (4.22s paging-only, listings byte-identical). What still falls through to one full listing per buffer with no READDIRPLUS: **derived/lens views** (`LayoutMeta` — its hidden/mapped shape can't be one SQL page) and mounts against a **server too old to advertise the paging cap** (a short `M_OPEN` ack with no second byte). Metadata *write* concurrency (multi-writer) remains not done — both backends serialize on a single writer connection. The gate is `MetaProvider::supports_paged_readdir()`, pinned by the `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` test.
- **Verification environment**: everything validated on 127.0.0.1 (simulated LAN); no real multi-machine or fio-level benchmark yet.

Where numbers are not measured, no benefit is claimed; wherever there is a limitation, it is stated in code comments and here.

## License

MIT.
