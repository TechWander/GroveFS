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
- **数据对象 object**：文件的真实字节，存在 `blocks/` 下的对象文件里、以 UUID 命名；对象按 key 的哈希自动扇出到 `blocks/` 内 **256 个桶目录**（单目录不随总量增长，千万级对象下每台目录只有数万条目，备份/遍历工具不退化）；视图目录项只持有对象 key（指针）。
- **基目录 `--home`**：本地模式下同时容纳 `blocks/`（数据）与 `meta/`（视图）的物理根；远程挂载不需要它（客户机本地不落盘），中心 `meta-server` 的权威元数据根用 `meta-server --meta-dir` 指定。

在此之上，`GroveFS` 可把元数据与数据分别服务化（`meta-server` / `data-server`），让**多台客户机经局域网挂载、共享同一份权威视图**——这才是「数据中心存储 + 机房内 LAN 访问」的落地形态，且**不使用 NFS**。

## 核心特性

- **多用户视图隔离**：一用户一棵树，跨用户不可见；完整的 `mkdir`/`mknod`/`rename`/`rmdir` 自定义层级。
- **可插拔存储/元数据后端**（trait 抽象）：本地 `LocalBackend`/`MetaEngine`、远程 `RemoteBackend`/`RemoteMeta`、跨多节点条带 `ShardedBackend`，对上层透明。元数据权威存储亦可切换为 **PostgreSQL**（`meta-server --meta-store postgres://…`）：服务端 schema-per-view、事务化 rename/unlink/lock；此模式下自研 HA 有意关闭，交给 PG 自己的流复制/PITR。
- **视图绑定 / 只读的 API key（推荐访问控制）**：`grovefs apikey create --for mount --view V [--ro]` 铸出的凭据自带「**能开哪棵树、读写还是只读**」两个维度——绑定 `--view V` 的 key 只能 `M_OPEN` 视图 V（开别的视图在握手处即 `E_AUTH` 拒绝），`--ro` 的 key 被服务端**逐写意图 op 拒 `E_NOTPERM`**（meta 面）与 `EROFS`（data 面内容写），并在 `M_OPEN` 回包里提示客户端只读挂载，故「只读」不依赖客户端自觉 `-o ro`。只读↔读写还可 `apikey set-ro --id X [--rw|--ro]` **原地互切**——服务端在同一 ≤1s 复验 tick 上重读凭据行当前 `ro` 值，活挂载免重铸、免重连即按新值放行/拒写。这两维都在服务端硬强制、与客户端自报 uid 无关（key 才是真正的门）。系统级 key（省略 `--view`）仍可开任意视图。
- **跨视图共享对象的删除安全（引用账本 `refs`）**：一个 `data_ref` 命名**唯一**一个底层对象，却可能被多个物理视图同时挂着名字（`grovefs import` 同一源文件进两个视图解析成同一 key、派生视图/别名共享基视图字节）。视图内的 `node.data_ref` 引用计数看不见别的视图，单看它会误删他视图仍在引用的对象。故记账落在 `<base>/meta/refs.db`（PG 后端落共享表 `grovefs_refs.holders`）：每个视图 ATTACH 该账本，把 holder 计数与节点增删纳入**同一个 IMMEDIATE 事务**，跨文件原子提交。物理删除的准确规则是「**该视图内此 key 的最后一个名字被摘除、且全局账本再无其它视图持有**」——只有此时才 `backend.remove`。视图 db 被 out-of-band 删除（`rm meta/x.db`、新 `--home`）只会让账本**高估**、对象多留不误删；`grovefs refs --meta-dir D rebuild` 按现存视图树重算 holder 归位。**只有基视图（`grovefs view create` 显式创建的物理树）的删除才真正回收底层字节**，派生视图/别名挂载只读、其删除从不物理 GC。删基视图需显式声明 `grovefs view rm --name V --base`（删 `meta/V.db`+WAL/SHM、`refs rebuild` 重建账本、回收到零其他视图持有的字节；PG 部署与活挂载拒绝，字节走 `--data-dir`）。账本里没有行的键即该视图私有（空账本不需要任何初始化步骤）。账本归位后仍残留、或崩溃在「对象已建、node 行未提交」之间留下的无人命名页，用离线 `grovefs gc --meta-dir D [--data-dir R] [--apply]` 审计/回收：默认只扫描，`--apply` 只删超过 `--older-than`（默认 24h）的页、永不删账本仍持有的键，并要求本存储无活挂载、本机无 grovefs 进程（PG 部署直接拒绝）。
- **每客户端显示定制（`mount --layout rules.txt`）**：同一棵权威树、同一份字节，不同挂载点各自套一层「显示透镜」——`hide` 隐藏子树、`only` 白名单、`map` 把条目重排到别的路径（源位让位）、`link` 建两处同时可见的别名。透镜只做 `MetaProvider` 层的路径翻译：真实条目保持真实 inode，数据 IO/锁/GC/失效推送全部直通，不复制目录项、不影响其它挂载的视图。
- **服务端派生视图（`grovefs view define` + `mount --view <派生名>`）**：想把形状**集中管理在服务端**——`grovefs view define --name docsonly --base team --rules F` 把规则落进 `<meta-dir>/meta/views/<name>.layout`（首行 `base <物理视图>`；PG 后端下同一条命令 `--meta-store postgres://…` 写进 `grovefs_control.view_defs` 表），客户端只 `mount --view docsonly`，**无需任何本地规则文件**。**虚拟视图一律只读挂载**：形状集中治理、只分发不修改，写入只属于原（物理）视图——客户端据 `M_OPEN` 握手标志直接落内核只读（写 `EROFS`），`meta-server` 另对派生名的写意图 op 设权威门禁，本地（`--home`）派生挂载同判。同名派生视图由服务端单例渲染（虚拟 inode 跨机一致），**改定义热生效**（`meta-server` 逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂）；本地模式（`--home`）挂载同样解析 `<home>/meta/views/` 下的定义。
- **服务端纯别名（`grovefs view alias`）**：治理「叫什么」而非「长什么样」——公开名 1:1 映射物理视图（定义仅一行 `alias <真实视图>`），别名挂载**全树可写**，而真实名从此**不是挂载入口**（本地与 `meta-server` 都拒挂被治理的真实名）：对外名字可自由轮换、存储身份不外露，猜出真实名也挂不进来；公开名泄露时 `--retire` 一条命令原子轮换（旧名失效、真实名全程拒挂）。与派生视图同放 `meta/views/`、同一 `list/rm`；视图绑定 key 记在真实名，对别名与基名同时生效；拒绝别名链与派生目标；`import` 拒绝别名目标（别名不是存储身份）。
- **服务端 fork 视图（`grovefs view fork`）**：基视图与派生透镜之间的「可组织、不可上传」中间态——`grovefs view fork --name lab --base team` 把基视图的目录项**复制**成一份独立元数据树（`meta/lab.db`，保留 inode/硬链接），内容字节不复制、经跨视图账本共享；定义仅一行 `fork <基视图>`。fork 挂载后 **结构可写**（`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod` 全放行）、**内容写一律 `EROFS`**（`write`/`truncate`；上传字节属于基视图），删除只摘 fork 自己的元数据、基视图分毫不动，共享对象只在**最后一个持有视图**摘名后才回收。本地（`--home`）与 `meta-server` 两条路径同判（后者经 `M_OPEN` 回包第 3 个标志字节）；同名定义槽与派生/别名共用 `view list`/`view rm`，删 fork 会一并删掉它自己的树并重建账本。基必须是**本地 SQLite 物理视图**（有定义的名字不能当基；PG 后端暂不支持 fork），`alias` 拒绝指向 fork（fork 是树身份，不是公开名——给它发 key 用 `apikey create --view lab`）。fork 支持**策展**：`--empty` 只建根、不复制任何条目，再用 `view graft` 把别处视图里选中的条目接进来——计划一行一条（`<src>` 原样落 `--dest`，或 `<src>TAB<dst>` 同时改名/换目录），`--map -` 直接读 stdin 便于外部程序生成；整份计划**一个事务**（要么全落、要么全不落）、字节零复制（条目进 fork，对象仍经跨视图账本共享），同名默认拒绝、`--skip-existing` 改为跳过。
- **存量数据零拷贝导入（`grovefs import`）**：服务端已有一批目录数据（如 `/data/legacy`）时，`grovefs import --view V --source DIR` 把整棵树登记进视图——同文件系统下对象就是源文件的**硬链接**（零额外字节，两个路径读写同一 inode、双向实时可见），跨设备自动退化为流式拷贝；目录层级/时间戳/权限/属主保真，重跑即增量补新文件，删视图里的名字只摘 `blocks/` 链接**绝不伤源数据**。可与派生视图/透镜组合出虚拟目录（`map` 导入路径到任意形状；**派生虚拟视图挂载只读**，写走原视图）。
- **自定义二进制帧 over TCP**（非 NFS/gRPC）：定长头 + 变长体，连接池并发。
- **跨客户端实时失效推送**（默认）：一端写成功，中心经事件总线向同用户其它订阅者广播 `inval_entry`+`inval_inode`，对端下次访问**即时可见**，无需轮询；`--no-cache` 提供零缓存兜底。
- **分布式 advisory 锁**：`flock` / `fcntl F_SETLK/F_GETLK` 经中心 `locks` 表集中仲裁，跨进程、跨机生效；崩溃即时回收（FUSE `release`）+ 短 lease（默认 15s）+ 续约心跳。
- **高可用主备**：`meta-server --role primary|standby --peer` 心跳/epoch 提升/自动降级；`--sync-repl` 把写 RPO 降到 0；`--witness` 取逗号分隔的 N 个共享 `data-server` 作**独立仲裁者集合**（推荐奇数），**fail-safe、持不重叠的有界多数派租约（需 `N/2+1` 授予；多数活租约即独占证明，写由本地租约校验放行）并对内容数据帧按 token 门控，把双写窗口收敛到租约安全余量内**；宕少数仲裁者仍不冻结写（N=1 即单仲裁者）。
- **Token 双面鉴权**（集群共享口令）、**可撤销的 per-caller API key 门**（`meta-server`/`data-server --require-api-key` + `mount --api-key`，key 可 `--view` 绑定单一视图、`--ro` 服务端强制只读）、**只读挂载**（`--ro`）、多线程 `fuser 0.18`（`clone_fd` 并发派发）、**分块并行 IO** 与 **跨 server 条带**。
- **显式视图名（唯一规则）**：视图只能由 `init --view` / `view create` / `view import` / `import --view` 显式建出来（SQLite `meta/<view>.db`，PG `grovefs_v_<hex>` schema）；`M_OPEN` 与本地挂载一律先探测存在性，未知名字**直接拒绝**并给出可读原因（客户端直接打印，服务端日志同一句），**不落盘**，开 `--require-api-key` 时还进 `apikey calls` 审计。派生/别名/fork 等治理定义照常可挂；拼错名字不会长出一棵空树（USAGE §4）。
- **智能体 MCP 接口（`grovefs mcp`）**：把上述视图治理（派生视图/纯别名/fork 的 `define/alias/fork/graft/list/rm`）以 MCP（Model Context Protocol）暴露给 AI 智能体：规则以**内联文本**经 `tools/call` 提交，智能体无需碰任何文件即可自动定制数据列表的形状（含 `list_view_tree` 只读探查真实目录清单 / 不落盘预览未发布规则的形状、`get_view` 读回单个定义、`validate_definition` 发布前把整副定义走完整套治理守卫而不写盘）；同一套治理再以 2025-06-18 协议的其余常用方法面二次暴露——资源（`resources/list`/`resources/templates/list`/`resources/read`，把视图作为可附着上下文）、提示（`prompts/list`/`prompts/get`，参数化治理剧本）、补全（`completion/complete`，用真实视图名前缀补全）、日志（`logging/setLevel` → 服务端 `notifications/message`），且复用完全相同的渲染器（故资源/提示/补全与工具结果逐字节一致）；与 CLI 共用同一套校验与错误文案（一个契约、两个入口），控制面 only——不挂载、不触数据；`grovefs mcp --require-api-key` 可选地为接口上 API key 门，同一份 `grovefs apikey` 注册表也可经 `meta-server`/`data-server --require-api-key` 门住挂载与数据入口（key 带 scope：`--for mcp|mount|any`），并记录每次调用/连接的审计日志；撤销可逐调用方生效并封停**已建立**的挂载（详见 USAGE §3.5.1/§6）。详见「智能体定制数据列表」一节与 `USAGE.md` §3.5。
- **元数据读并行 + 目录列举分页 + `FUSE_READDIRPLUS`**：每个视图的 `MetaEngine` 在单条写连接之外再开 4 条附加连接（`READ_POOL_SIZE`），`lookup`/`getattr`/`readdir` 及 nlink 解析都走这条只读池，元数据**读**不必排队等写锁（SQLite 的**写仍串行在同一权威连接上**，单写者语义不动；PG 后端写另走 SERIALIZABLE 写池并行，见下文 PG 诚实边界）；服务端 `MetaEngine` 同样吃这条池，故 `meta-server` 的并发 RPC 处理器互不等待。目录列举走 `readdir_page(parent, offset, limit)`，SQLite 引擎把它下推成 SQL `LIMIT/OFFSET`，FUSE 回调按 32→1024 几何块流式填回复缓冲，一个内核请求付 `O(缓冲)` 而非 `O(整棵目录)`；并默认协商 `FUSE_DO_READDIRPLUS`（内核 ≥4.6），目录项连同属性一次带回，`ls -l` 不必逐条 `LOOKUP`+`GETATTR`。同一份 5001 条目录、同一挂载点、逐个新挂载（内核 dentry 缓存为空）实测 `ls -l`：**2.56s**（`--no-readdirplus` 只留分页为 5.22s，两份输出逐字节相同）。分页与 PLUS 只在「能自己分页的存储引擎」上启用——本地 `--home` 的 SQLite、PG 后端（各配一条独立的 4 连接只读池并同样下推 `LIMIT/OFFSET`），以及对接 SQLite/PG `meta-server` 的**远端挂载**：`M_READDIRP` 帧把 `offset/limit` 下推到服务端，客户端每个内核缓冲只取一页并协商 PLUS。同一 5001 条目录经 `meta-server`+`data-server` 实测 `ls -l`：**2.29s**（`--no-readdirplus` 只留分页为 4.22s，两份输出排序后逐字节相同）。唯有派生/透镜视图（`LayoutMeta`）仍走 trait 默认实现（每内核缓冲一次全量列表、不协商 PLUS），详见「诚实边界」。`--no-readdirplus` 保留作 A/B 排障开关。
- **分阶段延迟遥测（`--stats`，opt-in、默认关闭零成本）**：给一次读/写/元数据 op 内部各阶段（端到端 `op_total`、key 解析、后端 IO、分片扇出、元数据 DB、远程 RPC）打时间戳，用 hdrhistogram 汇成 p50/p90/p99/max 离线定位瓶颈。**默认关闭时每个埋点仅一次 relaxed 原子读即短路**（连 `Instant::now()` 都不调用），读写热路径不受影响；仅显式开启才采样，直方图按 worker 线程分片不串行化到全局锁。开关 `--stats` / `--stats-file <路径>`（周期覆盖写）/ `--stats-interval <秒>` / `GROVEFS_STATS=1`；退出时经 `stderr` 打印报告，只读控制节点 `<mnt>/.grove-stats` 仅在开启时出现（保留 inode、不落盘，治理不变量不受影响）。是排障工具而非吞吐测量，详见 `USAGE.md` §8.1。
- **挂载级内容带宽整形（`--bw-read` / `--bw-write`）**：给一次挂载的内容读、写各上一道**客户端 GCRA 令牌槽**（按字节铺开互不重叠的时间槽，长跑速率精确等于上限），按实际交付字节计费——读在回复前、写在落盘后，超额的 op 只睡到自己的时间槽，慢的是速度而不是丢数据；写侧延迟天然形成背压。`0`/缺省 = 不限，热路径零成本（一次 `rate == 0` 分支）；`--stats` 的 `bytes read`/`bytes written` 计数可直接核对实际吞吐。**诚实边界**：这是自限速而非隔离——绕过 FUSE 的原始协议客户端、或同一视图的多个挂载各自拿满额度；服务端也无法把内容寻址的字节归到某棵视图上。回归 `bwlimit` 套件钉住速率下限、env 回退、`0`=不限与非法速率拒绝。
- **完整回归**：`run_all.sh` **全板 34 段（33 门控 + bench）GREEN**（`layout` 显示透镜与服务端派生视图 e2e、`fork` 独立元数据树 fork 视图 e2e、`viewrm` 删整个基视图与零持有字节回收 e2e（四重守卫/最后命名者死才释放/分家根 `--data-dir`）、`gc` 离线孤儿对象审计与回收 e2e（账本持有不删/grace 年龄门/本库活挂载拒 apply 而他库同名视图不拦/零交集数据根防呆）、`bwlimit` 每挂载内容读/写带宽整形 e2e（GCRA 令牌槽 24MiB@8MiB/s 读写下限断言/超额睡眠不丢字节/env `GROVEFS_BW_READ` 回退/`0`=不限/非法速率拒绝/只读挂载下 `--bw-write` 空转）、`migrate` 视图元数据导出/导入与跨机器迁移 e2e、`import` 零拷贝导入 e2e、`mcp` 智能体治理接口 e2e、`apikey` MCP 接口 API key 认证与调用审计 e2e、`mountkey` 可撤销挂载凭据（scope 分区/拒因入协议/撤销封停活挂载/视图绑定与只读 key/`set-ro` 就地翻转写裁决热生效）e2e、`config` 配置文件与 env 回退 e2e、`pg` PostgreSQL 后端 e2e、`control` 控制面统一 e2e（视图定义/key 随 `--meta-store` 进 `grovefs_control`、本地不落控制面文件、PG meta-server 从 PG 解派生挂载/热换形/key 门控）；`pg`、`control` 会在无 PG 主机上自动 SKIP）。

## 架构与原理

三层解耦，全走 trait：

```
┌─ FUSE 挂载进程  (fs_impl.rs: GroveFS，&self 多线程安全)
│    ├── MetaProvider trait   视图树访问
│    │     ├── 本地:  MetaEngine  → <home>/meta/<view>.db
│    │     ├── 远程:  RemoteMeta  → meta-server（中心权威）
│    │     └── 透镜:  LayoutMeta  → mount --layout（每客户端显示定制，装饰以上任一）
│    │           ├── 派生视图: 服务端 LayoutMeta 单例 → mount --view <派生名>（ 规则由 grovefs view define 集中定义于 meta/views/）
│    │           ├── fork 视图: fork <基视图> → 独立 meta/<名>.db（结构可写、内容 EROFS，字节共享）
│    │           └── 纯别名: alias <真实视图> → 公开名 1:1 映射,全树可写,真实名 不可直挂（名字治理）
│    └── StorageBackend trait 数据对象 IO
│          ├── 本地:  LocalBackend  → <home>/blocks/<2hex>/<key>
│          ├── 远程:  RemoteBackend → data-server
│          └── 条带:  ShardedBackend → 多 data-server
└─ 传输：自定义二进制帧 over TCP（定长头 + 变长体），连接池并发
```

- **trait 接缝可插拔**：本地/远程/条带切换只是换实现；接口面、线协议常量与新后端接入路径速览见 USAGE §16（网页版 docs.html §18/§19 有分层版）。
- **中心 MetaServer**：同一用户在多客户机共享**一份权威视图**，不再各存副本导致分叉。
- **事件总线 + Notifier**：写成功后向订阅者推送失效，保证内容新鲜度。
- **fuser 0.18 多线程**：每线程独立 `/dev/fuse` fd（`clone_fd`，Linux 4.5+），请求并发派发不串行。

## 视图三问：怎么定制、元数据存哪、权限怎么管

> 三大主题的速览地图；每小节末尾链向本文与 USAGE 的详节。

### 一、如何定制视图：规则透镜，只改呈现不搬字节

定制的唯一素材是**基视图的真实显示路径**（先查出来再写规则，不要照源目录结构猜——`import` 会把源目录整体挂进 `/seeds` 之类的前缀下）。查询途径：挂载后 `find /mnt/x`，或经 MCP `list_view_tree`（不挂载、只读元数据渲染；CLI 面没有独立的列树子命令）。规则文本一行一个动词（`#` 注释）：

```text
hide <src>          剪掉一棵子树
only <src>          白名单：只服务这些子树（含抵达它们的祖先目录）
map  <src> <dst>    搬走：条目从 <src> 消失、出现在 <dst>（<dst> 父目录自动合成）
link <src> <dst>    镜像：<src> 保留、<dst> 再挂一个名字（同一 inode，多视图呈现）
```

四个挂载入口共用同一副 `LayoutMeta` 透镜，只差规则从哪来：

| 入口 | 规则来源 | 生效范围 | 写语义 |
|---|---|---|---|
| `mount --layout F` | 客户端本地文件 | 只影响这个挂载 | 全树可写，写落真实条目 |
| `grovefs view define --name D --base B --rules F` | 服务端控制面（SQLite：`meta/views/D.layout`；PG：`grovefs_control.view_defs`） | 所有挂 `--view D` 的客户端 | 派生挂载只读，写回基视图 |
| `grovefs view fork --name X --base B` | 独立元数据树 `meta/X.db` | 单独一棵可组织的树 | 结构可写、内容 `EROFS`、字节共享 |
| `grovefs view fork --name X --base B --empty` + `grovefs view graft --name X --map P` | 空树 + 接枝计划 | 策展后的 fork | 只落选中的条目，字节仍共享 |
| `grovefs view alias --name P --real R` | `alias <R>` 一行 | 公开名 1:1 治理 | 全树可写，真实名拒挂 |

定制出的形状**可以和基视图完全不一样**：`<src>` 端被基树钉死，`<dst>` 端随你发明；想让自定义树成为视图全貌，配一条 `only <新树根>` 即可。透镜做不到的只有一件事——凭空造内容：渲染出的每个条目都是基树某条目的换名/镜像/裁剪加合成目录，字节永远不复制。热生效：经 `meta-server` 的挂载在 ≤2s 内自动换形（服务端逐 op 复查定义 mtime），本地 `--home` 裸挂载在挂载时解析一次、改了要重挂。

> 详节：README「显示定制 / 服务端派生视图 / 纯别名 / fork 视图」四节、USAGE §3.1–§3.4.1、MCP 工作流见 README「智能体定制数据列表」与 `skills/grovefs-mcp/`。

### 二、视图元数据如何存储：一 view 一引擎，控制面跟随后端

```text
<meta-dir>/meta/
├── <view>.db            每物理视图一棵权威树（SQLite WAL）：node 表（id/ino, parent_id, name,
│                        is_dir, data_ref, link_target, mode/uid/gid, mtime/atime/ctime,
│                        UNIQUE(parent_id,name) + ino/parent/data_ref 索引）+ ctrl + locks（POSIX 锁租约）
├── refs.db              跨视图共享对象持有者账本（GC 依它判「最后一个名字」；派生/别名/fork 共享基字节）
├── views/*.layout       派生/别名/fork 的定义(派生=首行 `base <B>` + 规则行;别名=单行 `alias <R>`;fork=单行 `fork <B>`)
└── api_keys.db          API key 摘要 + 调用审计（SQLite 后端的控制面；PG 后端进 grovefs_control）
数据字节：blocks/<2hex>/<key>（key=UUID，按哈希扇出 256 桶，单目录条目不随总量增长）
PG 后端（meta-server --meta-store postgres://…）：一 view 一 schema `grovefs_v_<hex(view)>`，同构 node/ctrl/locks
  + 共享表 grovefs_refs.holders；控制面另进共享 schema grovefs_control（view_defs / api_keys / mcp_calls 三表）——
  换 PG 后视图树与控制面都不落本地文件；`grovefs view`/`apikey`/`mcp`/`data-server` 加 `--meta-store postgres://…`
  （或 env `GROVEFS_META_STORE`）指向同一 URI，与 meta-server 读写同一份系统记录。
```

写路径上「树」与「字节」各走一半：元数据 op 落 `node` 行、内容写落 `blocks/`，`data_ref` 是两者唯一的连接点（条带对象编码为 `s<N>:<stripe_bytes>:<uuid>`）。备份要带的清单、GC 的精确规则与跨机迁移路径都在详节。

> 详节：USAGE §5.1（物理布局）/ §5.3（删除与 refs 账本）/ §5.4（备份迁移清单）、README「多机共享与存储选型」（SQLite↔PG 二选一）、USAGE §3.4.2（`view export/import` 跨机搬一棵树）。

### 三、权限如何管理：两层栈 + 一把可撤销的钥匙

准入栈只有两层——**token = 集群准入，view 名 = 树选择**；`--view` 字符串不是身份，任何 uid 挂同一个 `--view alice` 就落进 alice 的同一棵树。需要**逐调用方差异化权限**时上第三层：可撤销 API key（`--require-api-key` 开门，服务端逐 op 硬强制）：

```text
M_OPEN / OP_OPEN（TCP 帧握手）按序过门：
① token          不匹配 → E_AUTH（唯一确定性门；客户端自报 uid 属 advisory）
② api-key 门     scope 分区（--for mcp|mount|any）；撤销即拒；key 若 --view V 绑定：开其它视图握手处即 E_AUTH
③ ro 写门        key 若 --ro：meta 面逐写意图 op E_NOTPERM、data 面内容写 EROFS、加锁类 op EPERM；
                 M_OPEN 回包提示只读挂载——不依赖客户端 -o ro
④ view 名 → 引擎缓存  打开已存在的树，之后同 view 命中同一实例
治理面（grovefs mcp）：同一份 key 注册表（SQLite：api_keys.db；PG：grovefs_control.api_keys）经 scope=mcp 开门，tools/call 与 resources/read 全部落审计；
撤销后活挂载 ≤1s 封停（op 驱动逐秒复证），历史调用仍保留归属。
```

诚实边界：显示治理（派生/别名/fork）**不是安全边界**——它只改呈现的名字树，可见即可读、可读即可写回（fork 除外）；真正的门是 token 与可撤销 key。key 只存 SHA-256 摘要、明文打印一次；控制面（key/审计/视图定义）**跟随元数据后端**——SQLite 部署落 `api_keys.db` + `views/*.layout`，PG 部署全部进 `grovefs_control` schema，同一份状态两种形态、绝不双写。

> 详节：README「共享视图与权限模型」「给入口上把锁：API key 认证…」、USAGE §6（含 §6.1 视图绑定/只读 key 实测）、§3.5.1（十步 key 演练）。

## 多机共享与存储选型（原理与常见疑问）

> 这一节回答最容易混淆的两个问题：① 「多台服务器经局域网共享同一棵目录树」**要不要 PostgreSQL**？② PG 到底**跑在哪一侧**？

### 一次写请求的完整路径（数据面 / 元数据面各走一半）

GroveFS 把「文件的字节」与「目录树」拆成两条独立通道，这也是它能脱离 NFS 的根本：

```
应用 write()/close()
   │
   ├─ 数据面 ─→ RemoteBackend ─TCP帧→ data-server ─→ blocks/<2hex>/<key>（UUID 命名、256 桶自动扇出）
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
**不需要。**「多机共享一棵树」的能力来自 **`meta-server` + 相同 `--view`**，与底层是 SQLite 还是 PG **无关**。默认的 embedded SQLite（`rusqlite bundled` 编译进二进制、零外部依赖）本来就是为这个拓扑设计的——把 `data-server` + `meta-server` 部署在中心节点，多台客户机 `mount --view alice --meta-server <中心>:50110 --data-servers <中心>:50100` 即共享同一棵权威树、跨端实时可见。这一整套由 `metasrv` / `freshness` / `locks` 套件在纯 SQLite 后端上证明,**不需要先装 PostgreSQL**。两种后端是**二选一**的替代关系、不是叠加部署：同一棵权威树只落在其中一种后端上——用 SQLite 就不需要装 PG，换 PG 后视图树只进 PG schema、服务端不再创建/读写任何 `<meta-dir>/meta/<view>.db`；控制面同样跟随后端——`grovefs view`/`apikey`/`mcp`/`data-server` 也接 `--meta-store postgres://…`（或 env `GROVEFS_META_STORE`），key 与审计进 `grovefs_control.api_keys`/`mcp_calls`、视图定义进 `grovefs_control.view_defs`，PG 部署下本地不落任何控制面文件（`control` 套件逐节实证）。

**Q：那 PG 到底跑在哪一侧？**
**只在服务端**（`meta-server` 那一侧），是它的**可插拔权威存储后端**：

```
客户端(仅 grovefs 二进制)      服务端                    数据层
mount --view alice   ─TCP→   meta-server   ─postgres://→  PostgreSQL 服务器
  · 无 PG 库/凭据/感知        · --meta-store 是它的启动参数     (与 meta-server 同机或独立 DB 机)
```

`--meta-store postgres://…` 只出现在 `meta-server` 子命令上，客户端没有这个选项、也拿不到连接串。PG 服务器进程物理上跑在中心（可与 meta-server 同机走 loopback，或独立一台 DB 机经局域网连），但**始终在服务端**。

**Q：那什么时候才值得上 PG？**
当你想把权威树的**持久化 / 高可用 / 备份 / 集中运维**交给一个成熟数据库时:PG 自带流复制 / logical replication / PITR、`pg_basebackup`、监控与 DBA 工具链,海量元数据(百万级节点)也可借 PG 的索引/VACUUM 调优。注意**自研 HA(快照复制)与 PG 互斥**(见「共享视图与权限模型」),上了 PG 就用 PG 自己的复制、不再用 GroveFS 的文件级快照。选型是**二选一**而非叠加：后端在 `meta-server` 启动时经 `--meta-store` 定死，同一棵权威树不会同时落 SQLite 又落 PG；中途换库用 `view export/import`（目前只面向本地 SQLite 树）或 `pg_dump` 单向搬移，不支持双写。

> **诚实边界**：写模型在 SQLite 与 PostgreSQL 下**不同**——默认 SQLite 后端 `meta-server` 对同一 view 仍是**单写者**（一条权威连接 `conn` + 互斥，写串行）；换 `--meta-store postgres://…` 后写路径放宽为**有界 SERIALIZABLE 写连接池**（`PG_WRITE_POOL_SIZE = 8`，HopsFS 路线，事务基座交给 PostgreSQL 自带 SSI）——不相交 `(parent_id, name)` 行上的并发写真正并行提交，读写重叠的一例由 SSI 中止（`SQLSTATE 40001/40P01`）并**透明重试**（200µs–25ms 退避、≤12 次、绝不外泄给 POSIX 调用方），由 `pg_ssi`（Rust 5 项）与 `pg_parallel`（bash 三挂载并发）套件证明、无 PG 主机自动 SKIP。这把“单一权威写者”放宽为“单一权威调度（serially equivalent）”，治理不变量 I1–I4 在任意 SSI 接受的调度下逐条不变。元数据**读**并行则覆盖两个引擎——SQLite `MetaEngine` 与 PG `PgEngine` 各持一条 4 连接只读池、目录列举各下推 SQL `LIMIT/OFFSET`。仍未做的是**跨集群/跨 server 的元数据写扩展**（把权威分区、Spanner 式外部一致性）——PG 写并行只在单集群内。默认场景请继续用零依赖的 SQLite；PG 的增量价值是**写并行 + 持久化/复制/运维成熟度**。

## 构建

**前置**（Linux）：只需 Rust 工具链（edition 2021）。**不需要 `pkg-config`，也不需要 libfuse 开发包**——FUSE 客户端是纯 Rust 的 `fuser`（其 `libfuse*` feature 未启用），实测 `PKG_CONFIG=/nonexistent cargo build --release --features fdb` 全量构建通过，产物 `DT_NEEDED` 里没有任何 fuse 库（只有 `libfdb_c` + glibc 五件）。元数据引擎 `rusqlite` 用 `bundled` 特性从源码编译 SQLite，同样**无系统依赖**。挂载路径要的是内核 `/dev/fuse`，以及**非 root 时**发行版自带的 setuid 助手 `fusermount`/`fusermount3`（有 `CAP_SYS_ADMIN` 则直连 `mount(2)`）——那是用户态工具，不是开发包。

```bash
cargo build --release          # 产物：target/release/grovefs
```

> 注：仓库里还留着 `.build-bin/pkg-config` 这个 shim，套件脚本也仍在 `export PKG_CONFIG=…`——那是
> **历史遗留**（早期 `fuser` 走 libfuse 后端时才需要），现行依赖图里没有任何 crate 查询 `pkg-config`，
> 所以它**可留可删、不影响构建**。新环境直接：
> ```bash
> cargo build --release && export LC_ALL=C LANG=C
> ```

## 快速上手（单机本地模式）

数据和视图都在同一个 `--home`，无需任何 server：

```bash
Y=./target/release/grovefs
BASE=/data/myfs

# 1) 初始化两个用户的视图
$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

# 2) 挂载（默认转后台——挂载点就绪即返回，只打一行摘要；前台附着用 `--foreground`）
$Y --home $BASE mount --view alice --mountpoint /mnt/alice
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob

# 3) 像普通目录一样读写、建自定义层级
mkdir -p /mnt/alice/projects/2026
echo "年度报告" > /mnt/alice/projects/2026/报告.txt
ls /mnt/bob                               # bob 看不到 alice 的文件

# 4) 卸载
$Y umount --mountpoint /mnt/alice
```

用 manifest 预建层级：`init --view alice --manifest seed.txt`（每行 `mkdir <path>` 或 `touch <path>`，`#` 注释，幂等）。

## 命令瘦身：环境变量与自动加载的配置文件

重复敲的长命令可以瘦身。每个旋钮的取值优先级恒为：**命令行参数 > 当前环境变量 > 配置文件 > 内置默认值**。grovefs 启动时自动加载一个 `KEY=VALUE` 文件填入尚未设置的 `GROVEFS_*` 环境变量，查找顺序：`$GROVEFS_CONFIG` 指定路径 > `~/.config/grovefs/grovefs.conf`：

```bash
# ~/.config/grovefs/grovefs.conf（有口令就 chmod 600，与 env 里放口令同一契约）
GROVEFS_META_SERVER=192.168.1.10:50110
GROVEFS_DATA_SERVERS=192.168.1.10:50100
GROVEFS_TOKEN=s3cret

# 之前：grovefs --home /data/myfs mount --view alice -m /mnt/alice --meta-server … --data-servers … --token …
# 之后：只剩“这次挂哪个视图到哪儿”
grovefs mount --view alice -m /mnt/alice
```

可被 env/文件顶替的旋钮：`GROVEFS_HOME`（`--home`）、`GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS` / `GROVEFS_TOKEN`（`mount --meta-server/--data-servers/--token`，服务端 `--token` 同源）、`GROVEFS_META_DIR`（`meta-server --meta-dir`）、`GROVEFS_API_KEY` / `GROVEFS_MCP_API_KEY`（早已存在的凭据 env，现在也能写进文件）、`GROVEFS_STATS`。语法支持 `#` 注释、`export ` 前缀、引号包裹值；非 `GROVEFS_*` 键与坏行警告后忽略。`--view`/`-m` 刻意不收录——每次挂载都不同，写进默认值是错的；后台挂载的 re-exec 子进程继承环境，简写同样生效。回归：`config` 套件（28 条断言：优先级链/解析边界/纯由文件驱动的真实 FUSE 挂载）。

## 关于 `--view` 与路径容量

- **`--view` 是视图命名空间的标签，与 Linux 用户名无关**：任意合法文件名字符串都可以（如 `alice`、`team-a`、`u42`、`研究组`）——它只用来 ① 生成 `<home>/meta/<view>.db`，② 在 FUSE 挂载上打 `grovefs:<view>` 标签。二进制不调 `getpwnam`、不做 uid 校验、不需要 `useradd`。**权限来自「能访问 `--home` 与 `blocks/`」+ 服务端准入（`--token`，或可撤销的 `--require-api-key` + `mount --api-key`）**，不来自 Linux 用户身份。同 `--home` 下不同 `--view` = 不同视图，跨用户不可见。
- **一个用户一棵树，树内路径完全自主**：挂载后该 mountpoint 下可 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意自定义层级，与在自己 home 里的操作一致；视图隔离的粒度是「整棵树」而非「单一路径」。`--manifest` 只是预建骨架的便捷入口，不是路径数量的上限。

## 共享视图与权限模型

GroveFS 的权限栈只有两层——**令牌 = 集群准入，view 名 = 树选择**。`--view` 字符串本身不是身份：任何 uid 只要挂同一个 `--meta-server <addr>` + `--view alice` 都会落到 alice 的同一棵权威树上，看到彼此写入。这是有意的，也让「多人共享一个视图」开箱即用：

```text
客户端 A · uid=1000   mount --view alice ─┐
客户端 B · uid=2000   mount --view alice ─┤ 同一个 --view → 落进同一棵树；
客户端 C · uid=1000   mount --view bob   ─┘ 一个客户端也可同时挂多个 view（多对多）
        │
        ▼ TCP 帧 M_OPEN(view, token, uid)   （客户端从不开 db 文件 / 不持 PG 连接串）
┌─ meta-server（单一权威写者）—— 按序过门，再路由
│  ① token 校验     不匹配 → E_AUTH（集群准入，唯一确定性门；uid 自报属 advisory）
│  ② api-key 门     --require-api-key 时逐调用方鉴 scope/撤销；key 若 --view 绑定：开其它视图同处 M_OPEN 即 E_AUTH 拒
│  ③ ro 写门        key 若 --ro：每个写意图 op 直接 E_NOTPERM（data 面内容写 EROFS），M_OPEN 回包告知只读挂载
│  ④ view 名 → 引擎缓存  首次 M_OPEN 打开已存在的树，之后同 view 命中同一实例
└──┬──────────────────────────────────
   ▼ view=alice                        ▼ view=bob
  alice 权威树（A、B 共享，写串行化）    bob 权威树（仅 C，与 alice 互不可见）
   ├─ SQLite（默认）：       一 view = 一个独立 db 文件  <meta-dir>/meta/alice.db · bob.db
   └─ PostgreSQL（--meta-store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62

数据库里独立存储的单位是「视图」而非「每个客户端」；数据字节走 data-server → 存进其 --data-dir 下的 blocks/<2hex>/<key>（桶由 key 哈希决定），树里只存 data_ref 引用。
```

- **多人共享同一 view**：多台机器、不同 OS 用户，都 `mount --view alice --meta-server ... --data-servers ...` → 元数据经 `meta-server` 集中（一棵 SQLite/PG 树，串行化写）、内容经 `data-server` 集中（一份 `blocks/`）；`ls` 或 `cat` 立刻跨端可见，实时失效推送保证收敛。若希望**不同调用方有不同权限**（例如只读旁观者、或某客户端只许碰某一棵树），用视图绑定 / 只读 API key（服务端需 `--require-api-key`）：

  ```bash
  # 中心节点上：给每个调用方铸一把绑定 alice 的 key（apikey 自带 --meta-dir，与 meta-server 同名，指同一个元数据根）
  $Y apikey --meta-dir /data/cluster create --label rw-peer --for mount --view alice        # rw 协作者
  $Y apikey --meta-dir /data/cluster create --label reader  --for mount --view alice --ro   # 只读订阅者
  $Y apikey --meta-dir /data/cluster list                                                   # 查看已铸造的 key（含 view/rw 列）
$Y apikey --meta-dir /data/cluster set-ro --id <ID> --rw                          # 只读→读写原地热切换（不带 --rw 切回只读；活挂载 ≤1s 生效免重连）
  $Y apikey --meta-dir /data/cluster revoke --id <ID>                                       # 撤销单条（对已建立挂载 ≤1s 内封停）
  ```

  `--view alice` 把 key 绑定到单一物理视图 alice：拿它去 `M_OPEN` 别的视图会在握手处直接 `E_AUTH` 拒（视图隔离由凭据本身承载，无需任何按 uid 的表）。`--ro` 的 key 被服务端逐写意图 op 拒 `E_NOTPERM`（meta 面）/`EROFS`（data 面内容写），并在 `M_OPEN` 回包里提示只读挂载——只读由服务端强制，不依赖客户端 `-o ro`。撤销只是把行标记 revoked（历史调用仍留归属），且能打到活连接（服务端逐秒复证、op 驱动）。省略 `--view` 则是系统级 key，可开任意视图。

  > **诚实边界**：`--api-key` 认证的是**入口的调用方**（key 可撤销、可归属），客户端自报 uid 属 advisory、不参与准入。key 只存 SHA-256 摘要、明文打印一次；绑定/只读两维都在服务端硬强制。加锁类 op（`flock`/`F_SETLK`，含锁续约）归为写意图，`--ro` key 对它们一律 `EPERM`（只读角色需共享锁就不能用 `--ro`）。不开 `--require-api-key` 时，服务端行为与不设门一致（仅照常落审计）。详见「可撤销凭据」一节与 USAGE §3.5.1/§6。

- **元数据后端可切换：SQLite ↔ PostgreSQL**（仅指 `meta-server` 权威存储）：

  ```bash
  # 默认（embedded SQLite，一 view 一 `<meta-dir>/meta/<view>.db`）
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL：一 view 一个 schema `grovefs_v_<hex(view)>`，事务化 rename/unlink/lock
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
       --meta-store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  客户端挂载命令**完全一致**（仍 `--meta-server host:port --view alice`）——线协议不变。注意：
  - **PG 模式与自研 HA 互斥**：`--meta-store postgres://…` + 任一 `--role standby / --peer / --witness` 在启动时直接 fail-fast（`VACUUM INTO` 快照复制是 SQLite 特性）；PG 上 HA 请交给 PostgreSQL 自己的**流复制/logical replication/PITR**——那本来就是比文件级快照更强的机制。
  - **控制面随元数据后端走**：SQLite 部署下 key/审计落 `<meta-dir>/meta/api_keys.db`、视图定义落 `meta/views/*.layout`；PG 部署下两者进共享 schema `grovefs_control`（`api_keys`/`mcp_calls`/`view_defs`）——`grovefs view`/`apikey`/`mcp`/`data-server` 加 `--meta-store postgres://…`（或 env `GROVEFS_META_STORE`）指向与 meta-server 同一 URI 即可，一次 `pg_dump` 同时覆盖视图树（`grovefs_v_*`）、锁与控制面，无需另备文件。
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
$ grovefs --home .demo/home mount --view alice -m .demo/mnt --layout .demo/lens.txt &
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

规则在挂载时一次性解析（非法规则 fail-fast，不降级为「静默全显示」）；透镜是**每挂载实例**的显示层，不落盘、不改变服务端任何状态，因此与其它挂载正交：可叠加在本地 `MetaEngine` 或远程 `RemoteMeta` 之上，也可与 `--ro`、视图绑定 key 组合。共享 meta-server 时，挂在合成目录下的条目其失效推送按源路径广播、可能不命中透镜坐标，建议配 `--no-cache` 关掉属性/条目缓存。

## 服务端派生视图：一份数据、不同 `--view` 名不同形状（`grovefs view`）

`--layout` 由每个客户端自备规则；当希望把形状**集中管理在服务端**（团队内不同名字共享同一显示契约）时，用派生视图：`grovefs view define` 把规则存进 meta 根目录的 `<meta-dir>/meta/views/<name>.layout`（首行 `base <物理视图>`，余行同 `--layout` 四动词；SQLite 后端落此文件，`view … --meta-store postgres://…` 时写进 PG 的 `grovefs_control.view_defs` 表），客户端只需 `mount --view <派生名>`，**不需本地规则文件**。派生（虚拟）视图**一律只读挂载**——形状是集中治理的分发视图，所有写入都属于原（基名）视图。同名派生视图在不同机器上由服务端同一份定义渲染（虚拟 inode 跨机一致）；访问控制仍由视图绑定 / 只读 API key 承担（形状与权限两面正交）。本地模式（`--home`）同机制：挂载无 `--layout` 时自动解析 `<home>/meta/views/<view>.layout`。

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir .demo5/home define --name docsonly --base team --rules .demo5/docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir .demo5/home define --name team --base team --rules .demo5/docsonly.rules   # 名与基同名
error: derived view name must differ from its base
$ grovefs view --meta-dir .demo5/home list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs --home .demo5/home mount --view docsonly -m .demo5/mnt &    # 派生名直接挂，无 --layout
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
$ grovefs view --meta-dir .demo5/home rm --name docsonly
removed derived view 'docsonly' (underlying data untouched)
```

语义与守卫：define 拒绝 `name == base`、拒绝 base 本身是派生视图（不嵌套透镜）、拒绝遮蔽已有物理 `<name>.db`；非法规则 fail-fast（不会退化成「全显示」）。**虚拟视图一律只读（硬契约）**：写只属于原（基名）视图——`meta-server` 在 `M_OPEN` 握手回包里带派生标志，客户端直接落内核只读挂载（写 `EROFS`、`flock` 写锁同拒），服务端另对派生名的写意图 op 加独立权威门禁（回 `E_NOTPERM`，有意不用 `E_ROFS` 以免被误读为 HA 故障转移信号），旧客户端或改过的客户端都绕不过；本地（`--home`）派生挂载在挂载时同判强制 RO。**改定义热生效、免重挂**：`meta-server` 每次分发 op 前检查定义文件 mtime，`define` 替换后已建立的挂载连接在客户端缓存 TTL（≤2s）内自动切到新形状（换形后只读契约不变）；`view rm` 则**不**热作用（已挂载者保持打开时的形状直到重挂，避免抽掉正在使用的树）。共享 meta-server 下，派生视图的订阅者**同时注册进自身名与 base 名两个桶**，基侧写双播即时失效、对派生挂载即时可见（`layout` 套件第 9 节 e2e 验证：基写→派生立即可见、派生挂载写/删被 `EROFS` 拒且基树无痕、re-define→同连接换形、本地派生挂载同样只读）；`view rm` 只删定义不动数据。`grovefs view` 与 `grovefs apikey` 一样操作服务端本地文件，需在 meta-server 所在机器执行（或共享同一 `--meta-dir`）；HA 部署下 `meta/views/` 目录是配置状态、随 `api_keys.db` 一样手工同步到备机。

热重载实录（同一个已挂载连接，未重挂）：

```bash
$ ls .demo3/mnt                             # docsonly = only /docs，挂载中
docs
$ grovefs view --meta-dir .demo3/home define --name docsonly --base team --rules .demo3/broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls .demo3/mnt                             # 同一个挂载，形状自动变宽
docs  photos
```

## 纯别名：名字治理（`grovefs view alias`）

派生视图治理「长什么样」；**纯别名**治理「叫什么」：名字就是别名，映射值才是真实名。定义同放 `<meta-dir>/meta/views/<name>.layout`（PG 后端：`grovefs_control.view_defs` 表行），首行 `alias <真实视图>`、不带任何规则行。与派生视图恰成正反两面：**别名挂载全树可写**（它就是真实视图的原形），而**真实名从此不是挂载入口**——本地与 `meta-server` 都拒挂被治理的真实名，对外名字可以自由轮换，存储身份不随名字外露：

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs mount --view teamwork -m /mnt/work --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # 全树、可写
$ grovefs mount --view team -m /mnt/t --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240          # 直挂真实名被拒
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
$ grovefs --home $BASE mount --view team -m /mnt/t                                        # 本地同契约
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

公开名泄露后，**一条 `--retire` 命令原子轮换**（先写新名再删旧名，真实名全程被治理、无裸露窗口；视图绑定 key 记在真实名上，对别名与基名同时生效，故轮换公开名不需重铸 key）：

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork     # 旧公开名已泄露，换成 teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
$ grovefs mount --view teamwork -m /mnt/leak --meta-server ... --data-servers ...                    # 拿旧泄露名再挂 → 空（不再解析到真实树）
```

守卫与语义：拒绝 `name == real`、遮蔽物理 db、派生目标、别名链；一个真实名同时只允许一个对外名（`--retire` 在同一条命令内先写后删，不受此约束卡住，且拒绝误指向派生视图或不存在的别名）；视图绑定 key 记在真实名上，对别名与基名同时生效（也因此**轮换公开名不需重铸 key**）；`grovefs import` 拒绝别名目标（别名不是存储身份）；`view rm` 删别名后真实名对新挂载恢复可挂。轮换/删别名只影响**新**挂载（已建立连接持续到断开），被轮换掉的旧名落到按需新建的空命名空间、触不到真实树——**轮换是名字治理（泄露止损）而非安全边界**，硬隔离仍靠 token+API key。详见 USAGE §3.4（`layout` 套件第 10/11 节 26 条断言）。

## fork 视图：可组织但不可上传的独立元数据树（`grovefs view fork`）

派生视图是只读的「显示透镜」，别名是全树可写的「公开名」；两者之间还缺一档真实需求：**能整理目录树、但不能上传内容，且删掉自己的名字绝不伤基视图**。`grovefs view fork` 就是这一档——它把基视图的目录项**复制**成一份独立元数据树（`<meta-dir>/meta/<name>.db`，保留 inode 与硬链接），内容字节一个不拷、经跨视图账本共享；定义仅一行 `fork <基视图>`：

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team
forked view 'lab' from base 'team': 42 entries, 37 shared object(s)
note: 'lab' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch 'team'
note: content bytes are SHARED with 'team' (cross-view ledger): the object is freed only when no view holds a name for it anymore
$ grovefs view --meta-dir $BASE list            # 一条命令看清所有视图：定义槽 + 未加定义的裸基视图
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
$ grovefs mount --view lab -m /mnt/lab --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # 本地 --home 同判
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
$ echo hello > /mnt/lab/notes.md           # 内容写 → EROFS（上传字节属于基视图）
$ mkdir /mnt/lab/sorted && mv /mnt/lab/docs/a.md /mnt/lab/sorted/   # 结构写 → 放行
$ rm /mnt/lab/photos/old.jpg               # 只摘 lab 自己的名字，基视图仍见
$ grovefs apikey --meta-dir $BASE list --view lab    # 按「能开哪个视图」过滤 key
... 1 bound key(s) for view 'lab' of 2 total
  note: 1 unbound key(s) admit every view too — plain `apikey list` shows them
$ grovefs view --meta-dir $BASE rm --name lab
removed fork view 'lab' of base 'team' (its own metadata tree is gone; content bytes stay while any view — including 'team' — still holds a name for them)
```

语义与边界：

- **创建即快照**：只复制目录项（保留 `id`/`ino` 与硬链接关系）、不复制 `ctrl`/`locks`；源树必须存在且非空。复制后两边各自演化，基视图后续新增的名字**不会**注入已建 fork（各自一棵树）。
- **策展：空 fork + 接枝**：`fork --empty` 只留根行（即 MCP `fork_view` 的 `seed:false`），把「先复制全量再删」换成「只挑要的」；`grovefs view graft --name LAB [--src SRC] [--map PLAN|-] [--dest DIR] [--skip-existing]` 把 `--src`（缺省取 fork 的定义基名；**基视图必须显式 `--src`**——没有可推断的来处）里的条目落到 fork 的任意目录：`docs<TAB>tutorial/docs` 既换目录也改名（目标基名即新名），目录源**带整棵子树**，缺失目录自动创建；整份计划**单事务**、字节零复制，只有拥有自己树的视图（fork）能接收。
- **结构可写、内容只读**：`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/`touch`（空对象）全放行；`write`/`truncate` 在 FUSE 层回 `EROFS`（本地与远端同判，远端靠 `M_OPEN` 回包第 3 个标志字节）。**诚实边界**：这是客户端门，绕过 FUSE 直连 data-server 的 raw 协议客户端可写字节；硬边界仍是服务端 token + API key（与派生只读、`-o ro` 同一层级）。
- **删除隔离 + 跨视图账本**：fork 里删名字只删自己 `node` 行；共享对象只有在「本视图无同名、且账本显示无其它视图持有」时才真正 `backend.remove`。`view rm --name <fork>` 会删掉它自己的 `meta/<name>.db`（+ WAL/SHM）并 `refs rebuild` 重算账本，基视图与其余视图的字节不受影响。
- **守卫**：`name != base`；目标名已有定义或已有物理 db 拒绝；基名必须无定义（透镜/别名/fork 不能当基）且存在本地 `meta/<base>.db`（PG 后端暂不支持 fork——权威树不在文件里）；`alias` 拒绝指向 fork（fork 是树身份，不是公开名，给它发 key 用 `apikey create --view lab`）；`import` 拒绝 fork 目标。
- **可见性**：`view list` 同时列出定义槽（派生/别名/fork）与**裸基视图**（无定义的物理树）；`apikey list --view V` 只列能开 V 的 key（纯别名也匹配绑定到真实名的 key），并汇总未绑定（系统级）key 数量。
- **e2e**：`fork` 套件 70 条断言——创建/守卫/`--empty` 空种子 + `view graft` 接枝（stdin 计划/改名/目录子树/同名整批拒绝/`--skip-existing`）/`view list`/`apikey list --view`/本地与 `meta-server` 双路径挂载（结构写放行、内容 EROFS、截断拒绝）/删除隔离与账本/`view rm` 删树重建账本。

空 fork 与接枝的实测（`--map -` 走 stdin，整份计划一个事务）：

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team --empty
forked view 'lab' from base 'team' as an EMPTY tree: root only, nothing copied
note: place the subset you want with `grovefs view graft --name lab --src team --map plan.tsv` (or the `graft_view` tool, same implementation) — a directory source brings its subtree; each destination basename is the rename
note: 'lab' has the same fork semantics — structure writable, content writes (write/truncate) refused with EROFS, bytes shared through the cross-view ledger
$ printf 'docs\ttutorial/docs\nreadme.md\tindex.md\n' | grovefs view --meta-dir $BASE graft --name lab --src team --map - --dest staging
grafted 5 entries (3 directories) from 'team' into 'lab' at /staging: 2 shared object(s), 0 skipped
note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
$ grovefs view --meta-dir $BASE graft --name lab --src team --map plan.tsv --dest staging   # 同名再跑 → 整份拒绝
error: 'docs' already exists in the destination — rename it in the plan or pass --skip-existing
$ grovefs view --meta-dir $BASE graft --name team --map plan.tsv   # 基视图不接受接枝
error: 'team' is a base view — pass --src to name the view its entries come from
```

## 智能体定制数据列表：MCP 接口（`grovefs mcp`）

视图治理同时是一套 **MCP（Model Context Protocol）工具**：`grovefs mcp` 在 stdio 上说换行分隔的 JSON-RPC 2.0（实现 2025-06-18 协议的常用方法面：生命周期 `initialize` / `ping`、工具 `tools/list` / `tools/call`、资源 `resources/list` / `resources/templates/list` / `resources/read`、提示 `prompts/list` / `prompts/get`、补全 `completion/complete`、日志 `logging/setLevel` 与服务端 `notifications/message`；客户端通知一律静默不作答），任何智能体运行时（Claude Desktop、IDE agent 等）把它当子进程拉起，即可**自动定制数据列表的形状**——查视图、**列出某视图真实呈现的目录清单**（派生视图经其透镜渲染、还能不落盘地预览一副待发布规则会长成啥样）、用内联规则文本发布只读派生透镜、轮换公开名、删定义；规则以文本随行（`hide`/`only`/`map`/`link`，同 `--layout` 四动词），智能体不需要碰任何文件。下列回显全部取自 `mcp` 套件的真实运行：

```bash
$ grovefs mcp --meta-dir /data/xy/grovefs/.demomcp/home      # 由 MCP 客户端作为子进程拉起，stdin/stdout 即协议通道
# 智能体侧 tools/call 的入参与回显（工具结果文本原样呈现）：
[list_views]   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{
  "db_path": "/data/xy/grovefs/.demomcp/home",
  "defined": [],
  "physical": [
    "team"
  ]
}
[preview_rules] "rules": "bog /x"                          # 先发后落：非法动词在落地前被抓住
bad layout rules: line 1: unknown layout verb 'bog'
[define_derived_view] {"name":"release","base":"team","rules":"# public release: no private photos\nhide /photos/private"}
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
# 用户侧直接挂智能体定的名（无 --layout、无本地文件）：
$ grovefs --home $HOME_DIR mount --view release -m .demomcp/mnt &
derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ ls .demomcp/mnt
docs  photos
[list_view_tree] {"view":"team"}                       # 先摸清真实路径（读本地元数据，不挂载、不碰数据字节）：智能体据此写规则而非猜
{"kind":"physical","rendered_from":"team","dir_count":4,"file_count":3,"truncated":false,"entries":[
  {"path":"/docs","kind":"dir"},{"path":"/docs/2026","kind":"dir"},{"path":"/docs/2026/b.md","kind":"file"},
  {"path":"/docs/a.md","kind":"file"},{"path":"/photos","kind":"dir"},{"path":"/photos/private","kind":"dir"},
  {"path":"/photos/private/x.jpg","kind":"file"}]}
[list_view_tree] {"view":"release"}                    # 经已发布透镜渲染：显示路径为 /docs /docs/2026 /docs/2026/b.md /docs/a.md /photos（/photos/private 已被规则剪除，与挂载所见逐径一致）
[list_view_tree] {"base":"team","rules":"only /docs"}  # 不落盘预览一副待发布规则的形状（不写任何 .layout）：仅 /docs 子树呈现
[validate_definition] {"name":"release","base":"team","rules":"hide /photos/private"}  # 发布前干跑：把 name+base+rules 走完整套治理守卫，不写盘，回显确切判定
{"accepted":true,"dry_run":true,"kind":"derived","hide":1,"only":0,"reloc":0,"would_replace_existing_definition":false,"base_view_db_present":true,"note":""}
[get_view] {"name":"release"}                          # 读回单个透镜原始规则，供"读—改—写"迭代
{"kind":"derived","base":"team","hide":1,"only":0,"reloc":0,"definition":"base team\nhide /photos/private","mount":"read-only (virtual shape; writes go to the base view)"}
[list_views] 再次                       # 结构化 JSON：name/kind/real_view/规则计数，智能体可直接行动
view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)   # CLI 对照：`grovefs view list` 逐字节同文案
[define_derived_view] {"name":"team","base":"team",...}                       # 名与基同名
isError → derived view name must differ from its base          # 与 CLI 逐字节同文案
```

语义与边界：十个工具（治理八个 `list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`，只读探查两个 `list_view_tree` / `get_view`）与 `grovefs view` 子命令共用**同一套库函数**（`grovefs::mcp`）——校验、拒绝文案、落盘动作逐字节相同，一个契约、两个入口，智能体做不出 CLI 做不出的事，也绕不过任何守卫（派生只读、fork 结构与内容分治、别名拒挂真实名与 fork、`--retire` 先写后删等全部适用；热生效同样只在 `meta-server` 入口，本地挂载挂载时解析一次）。`fork_view` 在同一套守备下把物理基视图复制成独立元数据树（`list_views` 以 `kind:"fork"` 报出，`get_view` 返回 `own_tree`，`remove_view` 连树一并删除并重建账本）；`fork_view` 的 `seed:false` 只建空树、`graft_view` 再按计划接枝（一行 `<src>` 或 `<src>TAB<dst>`，整份一事务、字节零复制、同名默认拒绝或 `--skip-existing`），与 `grovefs view fork --empty` / `view graft` 是同一实现。`list_view_tree`/`get_view` 是**纯元数据只读探查**：以只读方式打开本地 `<meta-dir>/meta/<view>.db`、经与挂载同一副 `LayoutMeta` 透镜渲染目录清单（不挂载、不读对象字节），让智能体拿真实显示路径写规则、并在提交前预览效果；远端/PG 后端的树不在此遍历（需本地 SQLite 元数据根）；回包里的 `size` 是元数据字段而非字节数（目录 `4096`、文件 `0`、软链为 target 长度），需真实字节请挂载后 `stat`。`validate_definition` 是**发布前干跑**：把一副 `name`+`base`+`rules` 走完 `define_derived_view` 的全部治理守卫（非法名、名与基同名、遮蔽物理 db、覆盖纯别名、规则语法），拒绝时回与真实发布**逐字节相同**的文案、接受时回 JSON 判定，且**绝不写盘**——比只校验动词语法的 `preview_rules` 更进一步，让坏定义永远落不了地。同一治理再以 2025-06-18 协议的其余常用方法面二次暴露——资源 `resources/list`/`resources/templates/list`/`resources/read`（把视图与手册作为可附着上下文：`grovefs://guide` 原样回分节操作手册、`grovefs://views` 回与 `list_views` 相同的快照、`grovefs://view/<name>/definition` 与 `/tree`，树模板带查询参数 `{?path,depth,max}`，与 `list_view_tree` 的参数同义——大树可以一次只附一个子树，被界剪断时仍报 `truncated:true`）、提示 `prompts/list`/`prompts/get`（两副参数化剧本：`reshape-view` 取 `base`/`goal`，`rotate-alias` 取 `real`/`new`/`old`）、补全 `completion/complete`（按 `ref` 做角色感知：`reshape-view.base` / `rotate-alias.real` 只给物理视图，`rotate-alias.new`/`old` 只给别名，其余给全部视图名）、日志 `logging/setLevel` → 服务端 `notifications/message`（阈值以上按成功 `info`/失败 `warning` 推送，`data` 带方法与被操作对象，如 `tools/call list_views ok`），且复用**完全相同的渲染器**（故资源/提示/补全与工具结果逐字节一致）。面向智能体运行时的元数据：每个工具带 `title` 与 `annotations`（`readOnlyHint` 标出五个纯读工具 `list_views`/`preview_rules`/`validate_definition`/`list_view_tree`/`get_view`，`destructiveHint` 只给 `alias_view`/`remove_view`，客户端据此决定能否自动放行），四个回 JSON 的工具带 `outputSchema` 且结果同时给 `structuredContent`（客户端不必再剥 `content[0].text`）；`initialize` 回的分节 `instructions`（Discover/Reshape/Curate/Validate before publishing/Golden order/Beyond tools/Mechanics）是智能体动手前唯一的路线图，客户端注入上下文即可照做。协议健壮性：`protocolVersion` 按 `2025-06-18`/`2025-03-26`/`2024-11-05` 协商，客户端报不认识的版本时回 `2025-06-18`，不原样复读；`arguments` 既收对象也收 JSON 字符串（部分客户端会多序列化一层）；缺参错误附 `(tool 'x' requires: …)`、工具名打错附 `did you mean '…'?` 与 `available:` 全清单，智能体重试一次即可自纠。治理失败按 MCP 惯例回在结果内（`isError: true` + 原文），协议层错误才走 JSON-RPC error；通知（无 id）一律不作答；非法帧回 `-32700` 后继续服务。**控制面 only**：不挂载、不写数据。默认信任能启动这个进程的调用方（与本地挂载同等的操作系统级边界），跨网鉴权仍由服务端 token/API key 负责；当它被 front 给多个不同调用方时，`grovefs mcp --require-api-key` 打开**可选的 API key 门**（见下小节）——同一套凭据家族现在经 scope 也延伸到挂载/数据入口。接入：客户端配置里把 `grovefs mcp --meta-dir D` 登记为 stdio 服务器即可（`db_path` 也可逐调用覆盖）。e2e 见 `mcp` 套件（77 条断言：握手能力面声明与协议协商/发现（`instructions` 逐个点名十工具且分节、`title`+`annotations`+`outputSchema`）/内联规则换形/路径探查与形状预览/发布前干跑校验/别名轮换/fork 生命周期（含空种子与接枝）/CLI 同文案/`structuredContent` 与文本一致/资源枚举与读取（含 `grovefs://guide`、`grovefs://views`、树查询参数下钻与 `max` 截断）/提示实例化/按 `ref` 角色感知的视图名补全/未知工具建议与缺参提示/日志阈值→通知且 `data` 带工具名/协议错误路径）与 `apikey` 套件（33 条断言），USAGE §3.5。智能体侧的接线手册（英文，便于国际化部署）随仓库一起分发在 `skills/grovefs-mcp/`（`SKILL.md` + `reference.md` + `examples.md`，附零依赖 stdio 客户端 `scripts/mcp_call.py` 与临时存储根上的接线自检 `scripts/smoke.sh`）。

### 给入口上把锁：API key 认证、scope 分区、视图绑定 / 只读与调用审计（`grovefs apikey`）

同一个服务常常要面对**多个不同调用方**——智能体把一个 `grovefs mcp` front 出去，多台客户机经 LAN 挂同一个 `meta-server`——此时需要可撤销的凭据与归属审计。`grovefs apikey` 铸造 API key（**只存 SHA-256 摘要**，明文仅在 `create` 时打印一次，故 `api_keys.db` 泄露也换不出可用凭据），并给每个 key 打 **scope**：`--for mcp`（只开治理接口）、`--for mount`（只开 `--require-api-key` 的 meta/data 服务端）、`--for any`（**默认**，两者皆可；scope 出现前铸造的旧 key 全是这一档，故老部署无需改动）。挂载 key 还可带**两个访问控制维度**：`--view V` 把凭据**绑定到单一物理视图** V——用它 `M_OPEN` 别的视图在握手处即被 `E_AUTH` 拒（系统级 key 省略 `--view` 则可开任意视图）；`--ro` 标为**只读**——服务端在 meta 面对其**逐写意图 op 拒 `E_NOTPERM`**、data 面对内容写拒 `EROFS`，并在 `M_OPEN` 回包提示客户端只读挂载（「只读」由服务端强制，不靠客户端自觉 `-o ro`）。两处入口共用同一份注册表（`<meta-dir>/meta/api_keys.db`），靠 scope 隔离：挂载凭据重放不到治理 API，智能体 key 也开不了数据面。**治理面**：`grovefs mcp --require-api-key` 对所有**读写治理状态的方法**设门——`tools/list`、`tools/call`、`resources/list`、`resources/templates/list`、`resources/read`、`completion/complete`（`initialize` / `ping` / `prompts/*` / `logging/*` 始终开放，好让客户端完成握手并出示 key）。**挂载面**：`meta-server --require-api-key` 卡 `M_OPEN`、`data-server --require-api-key` 卡 `OP_OPEN`（`--meta-dir` 指到元数据根，默认取首个 `--data-dir`），客户端以 `mount --api-key <ID:SECRET>` 呈现（或导出 `GROVEFS_API_KEY` 使密钥不进 argv）。**审计面**：每一次 `tools/call`、`resources/read` 与每一次挂载连接（含被拒的）都追加进同一张表（时间、key、平面 method、工具 tool、成败、详情），`grovefs apikey calls` 回读。撤销只是把行标记为 revoked（历史调用仍保留归属），撤销后的 key 立即鉴不上，**且已建立的挂载也被封停**。存储是一处本地 SQLite（`<meta-dir>/meta/api_keys.db`），控制面状态。下列回显取自真实运行：

```
$ grovefs apikey --meta-dir <home> create --label laptop-alice --for mount   # 铸造并一次性打印明文 <id>:<secret>
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  key:   aac1f51a0d5641ddaa8dbb83dc1cf646:a5d5450a7cf4...（64 hex 密文）

$ grovefs apikey --meta-dir <home> create --label reader --for mount --view alice --ro   # 视图绑定 + 只读
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…            # 这把 key 只能挂 view alice，且写操作一律被服务端拒

# 用绑定/只读 key 挂别的视图 / 写数据都被拒（无需任何按 uid 的授权表）：
$ grovefs mount --view bob -m /mnt/b --meta-server ... --data-servers ... --api-key "$READER_KEY"
error: permission denied: this api key is bound to view 'alice', not 'bob'   # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key
bash: /mnt/a/should_fail: 只读文件系统                                               # 客户端据握手标志落内核只读；服务端另逐写拒 E_NOTPERM/EROFS

$ grovefs apikey --meta-dir <home> create --label x --for both               # 打错一个词不静默放宽权限
error: unknown api key scope 'both' (expected any|mcp|mount)

$ grovefs meta-server --meta-dir <home> --listen 0.0.0.0:50110 --require-api-key
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op

# 拒因逐字节随握手回传（不只是一个错误码）：
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ...                      # 不带 key
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "$MCP_KEY"   # 拿治理 key 来挂
error: authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> calls --limit 6        # 挂载连接与治理调用同表审计
last 6 call(s), newest first:
ts   key_id   method   tool   ok   detail
...  aac1f51a0d5641ddaa8dbb83dc1cf646  meta  M_OPEN   yes  view='alice' uid=1005
...  aac1f51a0d5641ddaa8dbb83dc1cf646  data  OP_OPEN  yes
...  -                                 meta  M_OPEN   no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646   # 撤销（行保留供历史归属）
apikey: revoked id='aac1f51a0d5641ddaa8dbb83dc1cf646' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# 封停活挂载实测（无需运维介入）：
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: 权限不够                                  # EACCES，携服务端拒因
$ cat /mnt/a/grovefs-mount.log
  grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
  received termination signal, unmounting cleanly...
  unmounted view 'alice' at /mnt/a                              # 然后 ls /mnt/a 是干净目录，不是 stale endpoint

# 治理面门的形状：不带 apiKey 的 initialize 照常应答，随后 tools/list 被拒
$ grovefs mcp --meta-dir <home> --require-api-key
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize 照常应答（门只卡读写治理状态的方法）
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}
# 携带 "apiKey":"<id>:<secret>" 后该会话鉴权通过，工具面放行：
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)

$ grovefs apikey --meta-dir <home> list                            # id/label/scope/view/rw/created/last_used/revoked，绝不含密文
```

无法自定义握手的 MCP 客户端，可由运维方导出 `GROVEFS_MCP_API_KEY='<id>:<secret>'` **预鉴权整个服务进程**（挂载侧对应的是 `GROVEFS_API_KEY`）。诚实边界：这把 key 认证的是**入口的调用方**——它让某个调用方能（或不能）发治理写命令、能（或不能）开某棵视图树，并把每次准入归咎到具体 key，但不改变治理语义本身；视图绑定/只读（`--view`/`--ro`）维度由服务端强制，且只读↔读写可 `set-ro` 原地翻转、在同一 ≤1s 复验 tick 热生效（免重铸免重连）；活连接复证是 **op 驱动**的（有流量才查），故封停边界是“撤销后的首个服务端往返”——完全由内核缓存回答的读不需要往返，因此不会被打断（下一次往返即封）；`--token` 与 `--require-api-key` 可同时存在，两道都得过。不开 `--require-api-key` 时，服务行为与不设门完全一致（只是照样落审计）。e2e 见 `apikey` 套件（33 条断言：铸造/列表/哈希不落盘/无 key 拒·错 secret 拒·对 key 放行/调用归属/撤销即失效且行保留/短 id 前缀撤销/env 预鉴权/`list --view` 过滤/默认不设门回归）与 `mountkey` 套件（76 条断言：scope 铸造与 typo 拒绝/六类拒因入协议/准入与 env 回退（argv 不泄密）/连接审计到具体 key 与视图/scope 双向隔离/撤销封停活挂载且无脏 endpoint/其他 key 不受波及/token 与 key 双门 AND/数据面独立门/故障转移携带凭据/不设门默认路径无回归/视图绑定 key 跨视图拒挂与审计归属/只读 key 服务端强制写拒/`set-ro` 在同一活挂载 rw→ro 拒写·ro→rw 放行·免重连）。

## 存量数据导入：零拷贝直通（`grovefs import`）

服务端目录里已经有一批数据、想让客户端按视图获取时，不必先「灌」一遍：`grovefs import` 把**已存在的本地目录树**登记进视图，字节原地不动——

```bash
$ tree .demo4/legacy
.demo4/legacy
|-- docs
|   `-- quarter.txt
`-- photos
    `-- 2026
        `-- y.jpg

3 directories, 2 files

$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grovefs mount --view legacy -m .demo4/mnt &    # 客户端连接即得这棵树
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

语义要点（`import` 套件 34 条断言）：**同文件系统零拷贝**（对象与源文件共享 inode，`df` 看不出差异），跨设备自动退化为独立拷贝并如实计数；**双向直通**——经挂载写、或外部直接改源文件，另一侧都在客户端 TTL（≤2s）内可见；**GC 只摘链接**——视图里删名字仅 unlink `blocks/` 那条路径，源路径与 inode 不受影响；**幂等增量**——重跑只登记新名字；mtime/atime/权限/属主保真（`--dry-run` 连视图 db 都不创建）。导入后的条目与原生写入的条目完全同权：读写/锁/快照/失效推送全部适用，还能被派生视图 `map`/`link` 组成任意虚拟目录形状——该形状挂载**只读**（写走导入的原视图，推送即时可见；`grovefs import` 也拒绝向派生名导入，防写出无人可读的影子库）。运行位置与 `grovefs view` 相同：在持有存储根（`--home` 或 meta/data server 的 root）的机器上执行。

## 生产拓扑：中心存储 + 多客户机 LAN 挂载

```bash
# ===== 中心节点 =====
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

# ===== 每台客户机（远程挂载不需要 --home：权威数据、白名单、缓存全在服务端，客户机本地不落盘）=====
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

| 选项 | 作用 |
| --- | --- |
| `--data-servers host:port` | 文件 IO 走网络到 `data-server`，不碰客户机本地盘（非 NFS） |
| `--meta-server host:port` | 视图由中心 `meta-server` 提供 → 多客户机挂同一用户 = 共享同一棵权威树 |
| `--data-servers a,b,c` | 逗号分隔多地址 → 启用跨 server 确定性条带 |
| `--data-dir /d1,/d2` | data-server 的逗号分隔多存储根 → 机内多盘自动放置（整对象按 key 哈希选定一条盘；落盘位置是 key 哈希 mod 根数现场重算，列表与对象都不记录盘序号，故等盘克隆**原位顶替**照常定位；但增删盘或调换 `--data-dir` 顺序会重映射所有 key 使旧对象不可达，扩容宜一次性配齐多根或用 LVM/mdadm 合盘） |
| `--meta-server 主,备` | 故障转移列表，主挂自动切备 |

## 跨客户端一致性

| 模式 | 命令 | 行为 |
| --- | --- | --- |
| **实时失效推送**（默认，推荐） | 仅 `--meta-server` | 一客户端写成功后，meta-server 经事件总线向同用户其它订阅者广播失效（`inval_entry`+`inval_inode`），对端下次访问**即时可见**，无需轮询 |
| `--no-cache` 强一致兜底 | 加 `--no-cache` | 关闭内核属性/dentry 缓存并 `FOPEN_DIRECT_IO` 直读，写立即可见。代价：零客户端缓存、往返更多 |

**失效须成对推送**：`inval_entry`（目录项）与 `inval_inode`（inode）一起发，否则对端可能拿到陈旧属性。默认推送失效模型已保证「A 写 → B 即时可见」，`--no-cache` 只是在极端竞争窗口下选择完全旁路内核缓存的更保守兜底。

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
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# 备：只读服务，主心跳丢失后自动提升为 primary
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主（可选 RPO=0）：加 --sync-repl，写返回前同步等备落盘 ACK
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# 真 fencing（可选）：加 --witness <逗号分隔的主备都可达的 data-server 仲裁者列表，推荐奇数 N>
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
# 客户端故障转移列表
$Y mount --view alice --meta-server 10.0.0.5:50110,10.0.0.6:50111 --data-servers 10.0.0.5:50100 -m /mnt/alice
```

- 两节点必须用**各自独立的 `--meta-dir`**（各存一份视图副本，节点间靠快照复制）。
- primary 每 ~500ms 推快照 + 心跳 epoch；standby 心跳超时后提升并 epoch+1，旧主恢复发现更高 epoch 自动降级。
- **`--sync-repl`（opt-in）**：写 op 返回前同步推快照并等备 `fsync` + 原子 rename 的 ACK，把 **RPO 从 ~500ms 降到 0**。备不可达/超时时**诚实降级为 async 并打 warn**（不谎报 0）。
- **`--witness`（opt-in，fail-safe fencing）**：用一个或多个主备都能访问的 `data-server` 作**独立仲裁者集合**（逗号分隔，推荐奇数 N；N=1 就是单仲裁者（多数派为 1））。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予（meta 侧对全列表并发 fan-out 并按多数派判决 `witness_quorum`；两个多数派集合必交于≥一个仲裁者，该仲裁者只把 token 顶高给第一个 claimer，第二个见 `prev==我` 只算续约不算顶高，故同值碰撞天然只有一主）。每个仲裁者维护单调 `fence` token（`OP_FENCE` 的 `fetch_max` CAS 回传**折入前的值 `prev`**）：claim 赢当且仅当 `prev < 我的 epoch`，`prev >= 我` 则被 fence（对端胜出或同值碰撞）——回传 `prev` 正是为了在仲裁者恢复时区分同值碰撞、绝不双主。在任 primary 每 tick 向多数派续一份有界租约（lease，默认 3s；多数报 `prev>我` 则自降级），每个仲裁者拒绝重叠租约——同一时刻至多一份活租约，故多数活租约本身即独占权威的证明；提升方亦非重叠，须待前任租约到期才可抢占（`prev>=candidate` 同值碰撞/对端更高，或前任租约尚活则拒绝/延后，旧主可证明地已停）。**FAIL-SAFE**：配置了 witness 却**组不到租约多数派**连续超过宽限（3s）时，节点放弃写权限——primary 自降级、standby 拒绝提升（**写冻结**），宁牺牲可用也不冒双写。各仲裁者独立持久化 token 到 `<root>/.fence_token`，其重启不清零、不复活过期 primary。**相比单仲裁的可用性红利**：宕掉**少数**仲裁者仍能组多数续租、**不冻结写**；少数仲裁者的 `.fence_token` 盘丢也不复活过期主（多数仍保单调 token）；唯**失去多数**才 fail-safe 冻结。写请求由 primary 的**本地租约有效性**直接放行（不再每写一个仲裁 RTT，仅每续租一次向全列表 fan-out，摊薄到整窗），被 fence 的过期主在租约到点后的下一次复证即拒绝；元数据面复证拦不住内容字节（`write()` 先落 backend、再 debounced touch meta），故客户端把实时 epoch 贴到每个写意图数据帧（create/write/truncate/remove/fsync），作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`，过期主无法绕过元数据面静默写入内容（被封帧向权威重取 epoch 并自动重试一次）；且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化（claim 取独占锁会排在途写排空，而 witness 与存储同节点），故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合（数据面门控仅对 `--witness` 列表内的 data-server 生效，故应把 witness 与客户端实际写入的内容盘同节点部署；列表外的内容盘 token 恒为 0、数据面 gate 不生效，仅剩元数据面 fence 兜底）。两面合起来把双写窗口收敛到**租约安全余量内**（`WITNESS_LEASE_SLACK`=500ms：对端只能在我方租约到点后取 token，我方提前该余量停止信任缓存租约；而非 peer-only 的不封顶）。

## 鉴权 / 只读 / 性能

**Token 双面鉴权**（同时覆盖数据面 `OP_OPEN` 与元数据面 `M_OPEN`）：

```bash
$Y data-server     --listen 0.0.0.0:50100 --data-dir /data/cluster --token s3cr3t
$Y meta-server --meta-dir ... --listen 0.0.0.0:50110 --token s3cr3t
$Y mount --view alice --data-servers ... --meta-server ... --token s3cr3t -m /mnt/alice
```

token 不匹配 → 连接时直接 `PermissionDenied`，不重试。空 token（默认）= LAN-trusted 无鉴权，仅限可信内网。

**可撤销的 per-caller 凭据**（同一入口，但逐调用方可审计、可踢人）：token 是一份共享密钥——拿到它的主机都能挂，也分不出「是谁挂的」，要踢一个调用方只能轮换 token 并断全部人。改用 `grovefs apikey` 铸造的 key 做门（`--for mount`，只开挂载/数据入口）：

```bash
$Y apikey --meta-dir /data/cluster create --label client-a --for mount      # 明文只打印一次，只存 SHA-256
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
$Y mount --view alice --data-servers ... --meta-server ... --api-key '<id>:<secret>' -m /mnt/alice   # 或 export GROVEFS_API_KEY 使密钥不进 argv
$Y apikey --meta-dir /data/cluster calls --limit 20                          # 每次挂载连接（含被拒）都归属到具体 key
$Y apikey --meta-dir /data/cluster revoke --id <id>                          # 封停该 key 的**活**挂载（秒级）
```

与 token 的两处关键差异：① 拒因逐字节随握手回客户端（缺失/未知 id/密文不符/scope 不符/已撤销各自成文），且服务端**每秒复证活连接**——撤销后该挂载在首个服务端往返上被打断、打印服务端原文并干净自卸载（不留下 `Transport endpoint is not connected`），其他 key 的挂载不受影响；② 连接与治理调用同表可审计。`--token` 与 `--require-api-key` 可同时开（两道都得过）；不开时行为与不设门完全一致（仍落审计）。详见 `USAGE.md` §3.5.1/§6。

**只读挂载**：`mount ... --ro`，所有写操作返回 `EROFS`（内核 `MS_RDONLY` + 文件系统侧守卫双保险）。

**性能旋钮**：

| 项 | 说明 |
| --- | --- |
| `--threads N`（默认 4） | FUSE 并发派发工作线程，每线程独立 `/dev/fuse` fd；`0`=单线程（遗留/AB 对照） |
| 分块并行 IO | 客户端读 ≥1MiB 按 1MiB 切片并发；写 ≥4MiB 分片并发 |
| 大文件条带 | 文件 ≥16MiB 才分块；`data_ref` = `s<N>:<stripe>:<uuid>`，chunk i 落 `servers[i%N]`；无前缀 = 单对象（未启用条带的存储形态） |
| touch 去抖 | 同 inode 200ms 窗口去抖 `touch_mtime`，减少元数据 RPC 与 HA 日志量 |
| 元数据读连接池 | 每视图 SQLite 引擎 4 条附加连接（`READ_POOL_SIZE`）服务 `lookup`/`getattr`/`readdir`，读不必排写锁；PG 引擎另持一条对等的 4 连接只读池（`PG_READ_POOL_SIZE`）。写仍各走单一权威连接（SQLite `conn` / PG `client`）串行 |
| 目录分页 + READDIRPLUS | `readdir_page` 下推 `LIMIT/OFFSET`，回复缓冲按 32→1024 几何块填充；默认协商 `FUSE_DO_READDIRPLUS`，`--no-readdirplus` 可关。SQLite/PG 本地挂载与对接 SQLite/PG `meta-server` 的远端挂载（`M_READDIRP` 帧下推分页）都吃到两者；只有派生/透镜视图与不含该能力位的老服务端每内核缓冲取一次全量列表、不协商 PLUS |
| 分阶段延迟遥测 `--stats` | 把读/写/元数据 op 各阶段延迟汇成分位数离线排瓶颈；**默认关闭、零成本**（埋点仅一次原子读短路），仅 `--stats` 才采样，直方图按线程分片；退出 `stderr` + 可选周期文件 + 只读节点 `.grove-stats`。详见 §8.1（USAGE.md） |
| 后台 WAL checkpoint | 每个可写视图引擎一条后台 `wal_checkpoint(PASSIVE)` 线程（每 1s，~30s 一次 TRUNCATE）+ 写连接 `wal_autocheckpoint=0`，把提交里的同步 checkpoint 停顿移出写热路径；经 `Arc<StopSignal>` 随引擎 `Drop` 关停，只读/备机不启动。热读路径另 `prepare_cached` 复用语句。实测同 3000 文件创建 `meta.create` total 0.38s、max 33ms |
| 读缓存 TTL `--cache-ttl` | 把内核 attr/dentry 缓存 TTL 从默认 2s 放大，削减读多/不可变树的重复 LOOKUP/GETATTR；跨端新鲜度仍靠默认 `notify` 失效推送，新内核（≥3.15）协商 `AUTO_INVAL_DATA`（size/mtime 变则丢陈旧页）保证安全，`--no-cache` 恒钉 0。与 `max_write`/`readahead` 按 `FUSE_MAX_MAX_PAGES` 自动截断、`PARALLEL_DIROPS`/`WRITEBACK` 能力协商同属“新内核快、老内核降级”（详见 §8.2 USAGE.md） |
| 内容带宽整形 `--bw-read` / `--bw-write` | 给本挂载的内容读、写各上一道令牌槽（GCRA 排程：按字节铺开互不重叠的时间槽，长跑速率精确等于上限）：24MiB@8MiB/s 约 3 秒读完，单个大于 1 秒额度的 op 最多提前 1 秒放行；超额只**延迟**回复、绝不丢字节，写侧延迟即背压。`0`=不限（默认）；env `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE` 回退，非法速率在解析处即拒。**诚实边界**：客户端自限速，不是隔离——绕开 FUSE 直连 data-server、或多挂载同视图各拿满额度；元数据 op 不整形（详见 §8.2 USAGE.md） |

## 备份与迁移

元数据**不绑定任何硬件**：`node` 行只存 `data_ref`（对象 key），对象的物理位置由 key 纯函数推出——`blocks/<FNV1a(key)%256 两 hex>/<key>`，没有绝对路径、设备号、主机名或宿主 inode。GroveFS 自己的 `ino` 也在 DB 里，与宿主文件系统无关。所以换机器、换系统、换目录都能直接用，只要把**元数据**和**内容字节**两半一起带走：

| 要迁的东西 | 位置 | 说明 |
| --- | --- | --- |
| 内容字节 | `<home>/blocks/`（或 `data-server --data-dir` 下的 `blocks/`） | 普通文件，`rsync -a -H`/快照直接拷；桶布局由 key 纯函数决定，拷贝后无需重建索引 |
| 视图元数据 | `<home>/meta/<view>.db` | 每个视图一棵独立 SQLite 树；**勿在有写活动时裸拷**（WAL 可能未合并） |
| 跨视图账本 | `<home>/meta/refs.db` | 记录每个对象被几个视图持有；导入/冷拷后可用 `grovefs refs --meta-dir D rebuild` 重算 |
| 视图定义 | `<home>/meta/views/*.layout` | 派生/别名/fork 的纯文本定义，无主机路径，随控制面一起拷（PG 后端：`grovefs_control.view_defs`，随 `pg_dump` 走） |
| API key / 审计 | `<home>/meta/api_keys.db` | 用了 `apikey` 才需要；密钥只存 SHA-256（PG 后端：`grovefs_control.api_keys`/`mcp_calls`，随 `pg_dump` 走） |

**两种迁移方式**：

1. **冷拷贝整目录（最快，适合同架构整体搬家）**：停掉所有挂载和 `meta-server`/`data-server`，`rsync -a -H` 整个存储根（`blocks/` + `meta/`）到新机。运行中的 SQLite 不能裸拷；不能停机时用 `sqlite3 <db> ".backup /tmp/x.db"`（或 `VACUUM INTO`）做一致性快照。
2. **单视图导出/导入（可跨机器、可选择性迁移）**：`view export` 把一棵视图树写成宿主无关的 JSONL（表头 + 每行一个 node，含 id/inode/名字/时间/`data_ref`；**不含内容字节**），`view import` 在目标存储根重建 `<view>.db` 并重算账本。导入是原子的：先在旁边建好临时树，全部校验通过才 `rename` 到位——坏文件或失败的 `--replace` 不会破坏旧树。

```bash
# 源机：导出一棵视图（挂载中也可，读到的是快照）
grovefs view --meta-dir /data/src export --view team --out /tmp/team.jsonl
# 把内容字节拷到目标机（只拷 blocks/ 即可；元数据走上面的 JSONL）
rsync -a -H /data/src/blocks/ /data/dst/blocks/
# 目标机：导入成新的基视图
grovefs view --meta-dir /data/dst import --view team --in /tmp/team.jsonl
grovefs --home /data/dst mount --view team -m /mnt/team   # 树、硬链接、权限、属主、符号链接原样
```

`view import` 的守卫：目标已有树必须显式 `--replace`；目标名已有定义槽（派生/别名/fork）先 `view rm`；表头格式/版本、根节点、悬挂父节点、表头计数与正文不符都会整单拒绝。要恢复 fork 语义（结构可写、内容只读）加 `--as-fork <基视图>`，导入后会写入 `fork <基视图>` 定义并跨视图共享字节。

**规模实测**（单机、默认 IO，可用 `bash .build-bin/perf_migrate_scale.sh` 复跑）：迁移一棵 5052 节点、含 5000 个对象的视图，`export` 0.19s 产出 1.16 MB JSONL（视图库文件的 1.16 倍，约 230 B/节点），`import` 到另一个绝对路径的存储根 0.56s（含跨视图账本重建），挂载后 0.23s 枚举完 5000 个文件且内容逐字节一致、权限与属主保留。作为对比，当初把这一批文件注册进源视图（零拷贝导入，逐对象工作）花了 83s——**迁移代价由命名空间大小决定，不由数据量决定**，字节那一半另走 `rsync -a -H blocks/`。

**迁移时唯一需要注意的“拓扑”约束**（不是硬件绑定，是数据分布）：

- **多盘 `--data-dir` 分盘**：对象按 `FNV1a(key) % root 数量` 选盘，目标机 root 的**数量与顺序必须一致**；想合并成单盘也可以（把各盘 `blocks/` 合并即可，单 root 下所有 key 都命中同一棵）。
- **跨 server 条带对象**（`data_ref = s<N>:…`）：第 i 块在 `servers[i % N]`——**是列表下标，不是地址**。目标仍需 N 个 data-server，但两种错配后果不同：**数量不符会被明确拒绝**（不会静默读到错的 chunk）；**顺序错位检不出来**（守卫只比 N），接手那台缺它现在被派到的 `{uuid}#{i}`，症状是那批文件读失败。由此得到一个常见运维结论：**换机器 / 换 IP 本身不是约束**——只要在 `-o data=` 里**原位替换**那一项就能自动识别，元数据不用改、已写对象不需 `rebal`（轮转放置 `i % N` 只决定新写入对象落哪台）。
- **HA 的 `epoch`** 存在每个视图 `ctrl` 表里；整目录拷贝会把它带过去，迁移后**旧主必须停止**（或用新的存储根重新初始化），否则旧主仍在线可能造成双主。`view export/import` 与 `view fork` 有意不复制 `ctrl`，即新树从干净的 epoch 开始。
- **PostgreSQL 后端**不能文件拷贝：用 `pg_dump`/逻辑复制迁库；`view export/import` 目前面向本地 SQLite 树。
- `uid/gid` 是数值元数据（advisory），换系统后按数字保留；symlink 的目标文本属于用户数据，原样保留，不会因为换机器被改写。
- **`st_ino` 随库走，`st_dev` 不随**：`node.ino` 是 GroveFS 自己的编号（存在 DB 里，硬链接共享它，`fork` 与 `export/import` 连号原样复制），迁机器、换目录都不变；但 `st_dev` 是 FUSE 每次挂载新分配的，所以按 `(dev, ino)` 二元组去重的备份/同步工具在重挂或迁库后要重建映射。
- **拷完怎么确认没漏**：对象名是 UUID（**不是内容哈希，也不按内容去重**），读侧没有校验环节，漏拷只能主动比——树上引用的键数对 `grovefs gc --meta-dir D --data-dir R` 输出里的 `matched`，`matched < refs` 就是有键没有对象（漏拷或 `--data-dir` 列表不对的典型症状）；`rsync -a` 保 mtime（`gc --older-than` 靠它判宽限），用过 `import` 的存储必带 `-H`。

**多个基视图**：可以建任意多个基视图，每个一棵自己的 `meta/<name>.db`，树、字节引用、权限和删除都各自独立（`grovefs view create --view A` / `init --view A`，`view list` 会把未定义的裸基视图全部列出）。`view export/import` 也是**按视图**进行的，迁移时可以只搬其中一棵；不同基视图可以绑定不同的 API key（`apikey create --view A`），互不影响。

## 部署与维护（手把手）

完整的分阶段教程在 **[USAGE.md](USAGE.md) §11–§14**（网页版 [docs.html](docs.html) 同步覆盖，并按读者阶段重排为「零基础→专家」四档：新增零基础预备知识、内部机制与扩展开发、性能调优方法论三章，支持自动/浅色/深色阅读主题）：§11 从环境预检 → 构建安装 → 拓扑规划 → token → systemd 常驻（中心双服务 + 客户端模板挂载单元）→ 防火墙 → 8 步验收清单 → HA / PostgreSQL 变体；§12 给日常巡检（日志关键字→处置对照表）、备份恢复（单机 / SQLite+HA / PG 三流程）、滚动升级、故障速查表与变更后回归；§13 是一组**固定 IP 的完整案例实操**（存储机 192.168.1.10 存 /data、SQLite 后端，客户端读写挂 /mnt/da、只读挂 /mnt/db），手把手逐参数解释；§14 进阶回答“数据目录怎么定制/存量大数据怎么导入（首选 `grovefs import` 零拷贝登记，备选经挂载 rsync 灌入，硬链接多入口、拆 view、未实现清单）/不同客户端想要不同显示方式”。最短生产路径六步：

```bash
# 1) 构建安装（每台节点同一个二进制）
cargo build --release && sudo install -m755 target/release/grovefs /usr/local/bin/grovefs
# 2) 中心：生成共享 token 并起两个服务（生产请用 systemd 常驻，USAGE §11.4 有 unit 模板）
TOK=$(openssl rand -hex 16)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token $TOK &
grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token $TOK &
# 3) 防火墙只对内网段开 50100/50110
# 4) 显式建视图（挂载从不建树），然后在客户机挂载
grovefs view --meta-dir /data/cluster create --view alice
grovefs mount --view alice --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token $TOK -m /mnt/alice
# 5) 验收：双客户端 A 写 B 即时可见 + 跨机 flock + findmnt 显示 grovefs:alice
# 6) 日常：journalctl -u grove-meta-server 盯降级/冻结关键字；冷拷 blocks/ + meta/*.db 即完整备份
```

## 测试

```bash
export LC_ALL=C LANG=C             # PKG_CONFIG 不必设：没有 crate 查 pkg-config（见「构建」一节）
cargo test --release                 # 单元 + 集成测试
bash .build-bin/run_all.sh           # 全板 34 段（33 门控 + bench：build/unit + 31 个功能套件）
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准（纯功能回归）
# 扇出规模化基准（opt-in，默认 #[ignore] 不拖慢 CI）：批量建对象 + 随机读，
# 断言 256 桶全用上·单目录条目数受控(≈N/256)·路径 O(1)。默认 30 万对象，秒级完成；
# 拉到千万级验证 S1 主张：GROVEFS_FANOUT_SCALE=10000000 cargo test --release -- --ignored --nocapture
```

套件：`build`、`unit`、`posix`、`edge`、`e2e`、`remote`、`errors`、`resilience`、`concurrent`、`metasrv`、`ha`（含 `--witness` fencing / fail-safe 场景）、`striping`、`locks`（含崩溃即时回收 / 续约跨过 lease）、`freshness`、`layout`（显示透镜 e2e：hide/only/map/link + 直通写删 + 脚手架守卫 + 服务端派生视图：`view define/list/rm` + 跨形状推送 + re-define 热换形免重挂 + 派生挂载只读〔远程/本地〕 + 纯别名：全树可写/真实名拒挂〔远程/本地〕/链与派生目标守卫/删别名恢复基名可挂/原子轮换 `--retire`〔泄露止损：旧名即刻失效、真实名全程拒挂〕，共 78 条断言）、`fork`（独立元数据树 fork 视图 e2e：创建/守卫/`view list` 裸基视图/`apikey list --view` 过滤/本地与 meta-server 双路径挂载〔结构写放行、内容 EROFS、截断拒绝〕/删除隔离与账本/`view rm` 删树重建账本（含 `--empty` 空种子与 `view graft` 接枝），共 70 条断言）、`migrate`（视图元数据迁移 e2e：`view export` 把单视图目录项导出为宿主无关的 JSONL〔无绝对路径/主机名/blocks 路径〕、`view import` 导入另一存储路径后 id/inode/硬链接/权限/属主/符号链接全部保真，账本重建、导入原子化〔失败的 `--replace` 不伤旧树〕、`--as-fork` 恢复 fork 语义并跨视图共享字节、多个基视图互不干扰，共 74 条断言）、`import`（零拷贝导入 e2e：硬链接直通/mtime 保真/GC 不伤源/增量重跑/派生视图组合〔只读〕）、`mcp`（智能体治理接口 e2e：握手与工具发现/内联规则定制数据列表/路径探查与形状预览〔物理全树、经透镜、不落盘模拟〕/发布前干跑校验〔治理守卫全跑·不写盘〕/别名无空轮换/`fork_view` 建独立树与空种子/`graft_view` 接枝/与 CLI 逐字节同文案/通知不作答与非法帧恢复，共 65 条断言）、`apikey`（MCP 接口 API key 认证与调用审计 e2e：铸造·列表·哈希不落盘·无 key 拒·对 key 放行·调用归属·撤销即失效且行保留·短 id 前缀撤销·env 预鉴权·`list --view` 过滤·默认不设门回归·`set-ro` 原地 ro↔rw 翻转·翻转后仍鉴权，共 33 条断言）、`mountkey`（可撤销的挂载凭据 e2e：scope 铸造与 typo 拒绝·六类拒因入协议·准入与 env 回退（argv 不泄密）·连接审计到具体 key 与视图·scope 双向隔离·撤销封停活挂载且无脏 endpoint·其他 key 不受波及·token 与 key 双门 AND·数据面独立门·故障转移携带凭据·不设门默认路径无回归·视图绑定 key 跨视图拒挂与审计归属·只读 key 服务端强制写拒·`set-ro` 同一活挂载 rw→ro 拒写·ro→rw 放行·免重连，共 76 条断言）、`config`（命令瘦身 e2e：自动加载的 `KEY=VALUE` 配置文件与 env 回退——flag > env > 文件 > 默认值优先级链（GROVEFS_HOME/META/DATA/TOKEN/DB_PATH）、解析边界（注释/`export` 前缀/引号值/非 `GROVEFS_*` 键警告后忽略/坏行定位警告/读不到的显式 `$GROVEFS_CONFIG` 不致命）、以及一台命令行不带任何坐标、纯由文件驱动的带 token 门真实 FUSE 挂载，共 28 条断言）、`pg`（PostgreSQL 后端 e2e；无 PG 主机自动 SKIP、不拉低 GREEN）。

## 命令速查

> 先说人话:下表里的 `--home`、`--meta-dir` 等是**参数写法**(照抄用),它们各自管的东西如下(以**实际含义**为准):

| 它管什么(实际含义) | 命令行写法 |
| --- | --- |
| **本地存储根目录** — 单机模式下存放 `blocks/`(数据)与 `meta/`(视图)的总目录 | `--home <目录>` |
| **服务端元数据根** — `meta-server` 上放权威视图库、视图定义、API key 的总目录 | `--meta-dir <目录>` |
| **数据落盘位置** — `data-server` 实际写字节的存储根(默认同元数据根) | `--data-dir <目录>` |
| **要被纳管的已有目录** — 本就躺在盘上、想变成一个视图的那批数据 | `--source <目录>` |
| **挂载点** — 本机用来访问这棵树的目录 | `-m <目录>`(即 `--mountpoint`) |
| **中心服务地址** — 客户端连过去的元数据 / 数据服务(主机:端口) | `--meta-server <地址>` / `--data-servers <地址>` |
| **视图名** — 给一棵逻辑树起的任意标签(与 Linux 用户名无关) | `--view <名>` |
| **接入凭证** — 进入服务端的 token(数据面与元数据面可分别设) | `--token <串>` |
| **以指定身份挂载** — 锁仲裁与审计按此 uid 记属主（advisory，可被 `--run-as-uid` 覆盖） | `--run-as-uid <uid>` |
| **只读 / 零缓存强一致** — 挂载为只读 / 关闭客户端缓存 | `--ro` / `--no-cache` |
| **目录列举提速的回退开关** — 关掉 READDIRPLUS（随目录项一并带回属性），退回普通 READDIR + 逐条 `LOOKUP`，仅供 A/B 与兼容排障（provider 自身不能分页的挂载本来就不协商 READDIRPLUS，此开关对它们为空操作） | `--no-readdirplus` |

| 目的 | 命令 |
| --- | --- |
| 建视图 | `grovefs --home H init --view U [--manifest M]` |
| 本地挂载 | `grovefs --home H mount --view U -m MP [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--no-readdirplus] [--foreground|--log-file F]`（默认转后台） |
| 远程挂载 | `grovefs mount --view U -m MP --data-servers A[,A2] --meta-server M[,M_bak] [--token T] [--api-key ID:SECRET] [--run-as-uid N] [--layout F] [--foreground|--log-file F]`（不需要 `--home`） |
| 显示规则文件 | 每行 `hide <src>` / `only <src>` / `map <src> <dst>` / `link <src> <dst>`（`#` 注释，绝对路径） |
| 定义派生视图 | `grovefs view --meta-dir D define --name V --base B --rules F`（存于 `D/meta/views/V.layout`，客户端 `mount --view V` 即得形状；虚拟视图挂载只读） |
| 定义纯别名 | `grovefs view --meta-dir D alias --name A --real R`（公开名 1:1 映射真实视图：`A` 全树可写，`R` 不可直挂） |
| 泄露后轮换公开名 | `grovefs view --meta-dir D alias --name A2 --real R --retire A`（先写后删零窗口：`A` 失效、`R` 全程不可直挂、授权不动） |
| 创建 fork 视图 | `grovefs view --meta-dir D fork --name L --base R`（复制目录项成独立 `meta/L.db`，字节共享；结构可写、内容 `EROFS`；`--empty` 只建根，供接枝策展） |
| 接枝进 fork | `grovefs view --meta-dir D graft --name L [--src S] --map PLAN\|- [--dest P] [--skip-existing]`（计划一行一条 `<src>[TAB<dst>]`，整份一事务、字节共享；目录源带整棵子树，目标基名即改名） |
| 导出/导入单视图元数据 | `grovefs view --meta-dir D export --view V --out F [--force]`（宿主无关 JSONL，不含字节）/ `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]`（原子替换；字节另拷 `blocks/`） |
| 按视图查 key | `grovefs apikey --meta-dir D list --view L`（只列能开 L 的 key；纯别名也匹配绑定到真实名的 key） |
| 列出/删除视图定义 | `grovefs view --meta-dir D list`（定义槽 + 裸基视图）/ `grovefs view --meta-dir D rm --name V`（派生/别名只删定义不动数据；fork 连自己的 `meta/V.db` 一并删除并重建账本） |
| 智能体接口（MCP） | `grovefs mcp --meta-dir D [--meta-store postgres://…]`（stdio JSON-RPC：治理 `list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`，只读探查 `list_view_tree` / `get_view`；规则内联文本， 与 `grovefs view` 同校验同文案；同一治理另经 `resources/*`/`prompts/*`/`completion/complete`/`logging/setLevel` 常用方法面暴露） |
| API key 与调用审计 | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount]` / `list` / `revoke --id X` / `set-ro --id X [--rw|--ro]`（活跃 key 只读↔读写原地翻转，活挂载 ≤1s 热生效免重连） / `calls [--limit N]`（key 只存 SHA-256、明文打印一次、`list` 带 scope 列；配合 `grovefs mcp --meta-dir D --require-api-key` 门住读写治理状态的方法，或 `meta-server`/`data-server --require-api-key` 门住挂载入口（客户端 `mount --api-key`/env `GROVEFS_API_KEY`）；每次调用与每次挂载连接都落审计（SQLite：`meta/api_keys.db`；PG：`grovefs_control.mcp_calls`），撤销封停活挂载；`GROVEFS_MCP_API_KEY` 可预鉴权整个服务） |
| 导入存量目录 | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]`（同盘硬链接零拷贝，重跑增量） |
| 卸载 | `grovefs umount -m MP` |
| 数据服务 | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| 元数据服务（默认 SQLite） | `grovefs meta-server --meta-dir D --listen IP:PORT [--token T] [--require-api-key] [--role primary\|standby --peer ADDR [--sync-repl] [--witness DATA_ADDR]]` |
| 元数据服务（PostgreSQL 后端） | `grovefs meta-server --meta-dir D --listen IP:PORT --meta-store postgres://user:pw@host:5432/dbname`（先用 `grovefs view --meta-store <同 URI> create --view V` 显式建 schema；`view`/`apikey`/`mcp`/`data-server` 用同一 `--meta-store` URI 或 env `GROVEFS_META_STORE` 接同一控制面） |

## 诚实标注的边界

以下为本项目遵循「工程诚信」原则明确标注的限制，而非隐藏假设：

- **传输协议**：当前为自定义 TCP 帧，非 gRPC。gRPC（tonic/prost）已做 go/no-go 闸门评估，结论 **GO**（tonic 0.14 免 protoc），但完整集成**有意延后**——现协议已正确且被测。
- **HA RPO**：默认 async RPO ≈ 500ms（`--sync-repl` 可降到 0）。
- **fencing**：`--witness`（逗号分隔的 N 个共享 data-server 作 `prev`-回传 epoch CAS **多数派**仲裁，需 `N/2+1` 授予）**fail-safe**，primary 持不重叠的有界多数派租约（多数活租约即独占证明，写由本地租约校验放行）、并对内容数据帧按 token 门控，把双写窗口收敛到**租约安全余量内**（保证收敛单主）。硬边界：严格两节点、**不配第三方仲裁** 时无法防脑裂、窗口不封顶（CAP/共识定理限制，须部署 witness 或第三节点）；**多数派仲裁的可用性红利**：配 `--witness` 后宕**少数**仲裁者仍不冻结写、少数盘丢不复活过期主，唯**失去多数**才 fail-safe 停写（N=1 即单仲裁者，witness 本身成为单点可用性依赖）。
- **条带**：单条已饱和链路（loopback/单网卡）上**无吞吐增益**，价值在多节点聚合容量；真实多机链路须重测后才可声称收益。
- **显示透镜（`--layout`）**：是**显示层定制而非安全边界**——绕过透镜直接挂同一 view 仍能看到全树，硬隔离请用独立 view + token/API key；向纯合成目录创建/重命名进入返回 `EPERM`、撞透镜已显示名返回 `EEXIST`、`only` 模式下白名单外不可写；symlink 目标文本不改写；共享 meta-server 下挂合成目录的条目失效推送可能不命中透镜坐标（建议 `--no-cache`）；规则体目前限同一 view 内部重排，不支持跨 view 引用。
- **服务端派生视图（`grovefs view`）**：**一律只读挂载**（硬契约）——形状集中治理、只分发不修改，写只属于基名视图；客户端据 `M_OPEN` 握手标志落内核 RO，服务端另对派生名写意图 op 设权威门禁（`E_NOTPERM`），本地派生挂载同判；若需可写的自定义形状，用客户端自备的 `mount --layout` 透镜（§每客户端显示定制，直写真实条目）。其余：同样**不是安全边界**——直挂基名仍见全树（能拦住的是 token/API key，不是形状）；`define` 替换**热生效**（服务端逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂），`view rm` 不热作用（已挂载者保持旧形状到重挂）；切换形状时旧虚拟 ino 上的合成路径/fd 可能落回 ENOENT，真实条目 inode 稳定不受影响；不嵌套（base 必须是物理视图）、不得与基同名、不得遮蔽物理 db；`meta/views/` 是控制面配置，HA 备机需手工同步；热重载仅限 `meta-server`，本地（`--home`）挂载仍在挂载时解析一次。
- **服务端纯别名（`grovefs view alias`）**：与派生视图的正反面——别名挂载全树**可写**、真实名被治理后**不可直挂**（本地与服务端均拒）；单跳解析、每 `M_OPEN` 重扫 `views/`（挂载是罕见事件）；拒绝别名链/派生目标/一真实名多别名；`import` 拒别名目标；删别名仅影响**新**挂载（已建立连接不受影响，同 `view rm` 语义）；`--retire` 原子轮换公开名（先写后删、真实名全程不可直挂、无裸露窗口，旧名落到按需新建的空命名空间而触不到真实树），但轮换/删别名都只影响新挂载；拒因**随协议回传**（`M_OPEN` 错误帧的 data 段携服务端原文），远端拒挂真实名的回显与本地逐字节相同（对接**老服务端**时 data 段为空，客户端才退回通用 token 文案）；改名不是安全机制（ 公开名终究要分发），真实名隐藏属名字治理，硬边界仍是 token + 可撤销 api-key。
- **fork 视图（`grovefs view fork`）**：创建是**点即快照**（复制 `node` 行、保留 `ino`/硬链接；不复制 `ctrl`/`locks`；基视图之后新增的名字不会注入已建 fork）；结构写（`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/空对象 `create`）放行，内容写（`write`/`truncate`）在 **FUSE 客户端**回 `EROFS`（本地与 `meta-server` 同判，后者靠 `M_OPEN` 第 3 个标志字节）——这是**诚实标注的客户端门**：绕过 FUSE 直连 data-server 的 raw 协议客户端仍可写字节，硬边界与派生只读/已读一致仍是服务端 token+API key；fork 删名字只摘自己 `node` 行（账本判定最后持有者才真删字节，基视图分毫不动）；`view rm` 连 `meta/<name>.db`（+WAL/SHM）一并删并 `refs rebuild`；基必须是**本地 SQLite 物理视图**（有定义的名字不能当基；PG 后端暂不支持 fork，权威树不在文件里），`alias` 拒指向 fork（fork 是树身份，发 key 用 `apikey create --view`），`import` 拒 fork 目标；已挂载的 fork 被 `view rm` 后保持自己已解析的名字直到重挂，新挂载被拒。由 `fork` 套件 70 条断言证明（创建/守卫/`--empty` 与 `view graft` 接枝/`view list` 裸基视图/`apikey list --view`/本地与 meta-server 双路径挂载/删除隔离/账本）。
- **可撤销凭据（`grovefs apikey` + `--require-api-key`）**：它是**入口门**而不是隔离墙——认证“哪个调用方开了哪扇门”（scope 分隔 `mcp` 与 `mount` 两面，拒因逐字节回传，每次连接/调用归属到具体 key），但不改变治理语义本身；「谁能碰哪棵树、读写还是只读」由 key 的 `--view`/`--ro` 绑定在服务端逐 op 强制，且只读↔读写可 `set-ro` 原地互切（同一 ≤1s 复验 tick 热生效，免重铸免重连）。活连接复证是 **op 驱动**的（服务端仅在流量上每秒查一次），故撤销的封停边界是“该挂载撤销后的**首个服务端往返**”——完全由内核缓存回答的读不需往返、因此不会被打断（下一次往返即封）；已建立的 `grovefs mcp` 会话保持其身份直到重连。两面门都是 **opt-in**（不开时仅落审计、不拦任何调用）；`--token` 与 `--require-api-key` 可同时存在（两道都得过）；`data-server --meta-dir` 默认取首个 `--data-dir`，数据盘与元数据根分盘部署时必须指对（指到空 key 库则 fail-closed）。key 只存 SHA-256，故 `api_keys.db` 泄露不产生可用凭据；但明文只在 `create` 时打印一次，丢了只能重铸，且该库（含 `-wal`）是控制面状态，需随 `meta/` 一起冷拷。由 `mountkey` 套件 76 条断言证明（含“撤销封停活挂载且无脏 endpoint”“其他 key 不受波及”“视图绑定 key 跨视图拒挂”“只读 key 服务端强制写拒”“set-ro 在同一活挂载 rw↔ro 热切换免重连”）。
- **零拷贝导入（`grovefs import`）**：硬链接要求源与 `blocks/` 同文件系统（跨设备自动退化为独立拷贝并如实报告）；导入后源文件若被外部工具以 **rename/替换写**方式更新（vim、rsync --inplace 关闭等默认行为），旧 inode 内容冻结在最后一次直通写入上——需对新内容重跑 `import`（已存在的名字会跳过，可用不同 `--prefix` 强制重登记）；symlink/特殊文件有意跳过（不表示）；导入对象为单文件，不走 ≥16MiB 的跨 server 条带（条带仍是经挂载写入新大文件的能力）；meta-server 模式下 import 直写权威 db 文件，需在存储机执行、已挂载客户端靠 TTL（≤2s）收敛而非即时推送；PG 后端不支持（权威树在 Postgres 里，走挂载点 rsync 灌入）；`--source` 建议与存储根同盘并**保留**——删源仅失去「源路径」这一别名，数据不丢，但外部改名/迁移后视图与文件实体的关联就只剩 `blocks/` 链接。
- **元数据读并行 / 目录分页的作用范围**：只读连接池与 `LIMIT/OFFSET` 分页分布在**两个存储引擎**里——SQLite `MetaEngine` 与 PG `PgEngine`（各 4 连接只读池）。本地挂载（`--home` SQLite 或 PG）直接吃到分页与 PLUS；`meta-server` 的并发 RPC 处理器吃各自后端的只读池，且 `M_READDIRP` 帧带 `offset/limit` 把分页下推到服务端，远端客户端每个内核缓冲只取一页并协商 `FUSE_READDIRPLUS`——同一 5001 条目录远端实测 **2.29s**（`--no-readdirplus` 只留分页为 4.22s，两份输出逐字节相同）。不吃这条路径的是**派生/透镜视图**（`LayoutMeta`：其隐藏/映射形状无法表示为单条 SQL 页，走 trait 默认、不协商 PLUS）与对接**不含该能力位的老 `meta-server`** 的挂载（`M_OPEN` ack 缺第二字节则回退全量列表）。元数据**写**并行（多权威）没有，两后端各自单写连接串行。开关判据是 `MetaProvider::supports_paged_readdir()`，`cargo test` 里 `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` 钉住该契约。
- **验证环境**：全部在 127.0.0.1 回环（模拟 LAN）验证，尚未做真实多机与 fio 级基准。

凡未实测数字处，文档不做收益声称；凡有限制处，均在代码注释与本节明示。

**文档口径：只陈述当前版本的行为。** 不记录已移除的参数、环境变量或开关（提及它们的名字，就等于给旧行为招魂）；不做版本前后对比，不引用历史基线数字；不写「新增了 / 现已支持 / 与旧版一致 / 退回旧行为」这类变更叙述——演变史归 git 提交记录，手册只管现在怎么用。两类内容不在此列，应当照常保留：① 当前版本内的真实回退语义（跨设备导入退化为独立拷贝、对接不含某能力位的老服务端时退回全量列表）；② 上面这类「未实现（诚实标注）」的负空间清单。

## 许可

MIT。

---

<a id="english"></a>
# English

## What it is

`GroveFS` is a **user-view FUSE filesystem**. It cleanly separates *who sees what* (the view) from *where bytes actually live* (the object):

- **View**: each user gets a private directory tree, stored as one SQLite DB `<home>/meta/<view>.db`. Users cannot see each other's files.
- **Object**: the real bytes of a file, stored as UUID-named object files under `blocks/`, auto-fanned-out over **256 bucket dirs** keyed by a hash of the object id (no single directory grows with the total, so backup/traversal tools stay usable at tens of millions of objects); the view's directory entries only hold object keys (pointers).
- **Base dir `--home`**: in local mode, the physical root holding both `blocks/` (data) and `meta/` (views). A remote mount needs no local store at all; the central `meta-server` points its authoritative metadata root at `--meta-dir` instead.

On top of that, metadata and data can each be served as a network service (`meta-server` / `data-server`) so **many client machines mount and share one authoritative view over the LAN** — a central-storage + in-rack LAN-access topology, and **no NFS**.

## Key features

- **Per-user view isolation** with full custom nesting (`mkdir`/`mknod`/`rename`/`rmdir`).
- **Pluggable storage/metadata backends** via traits: local, remote, and cross-server striped (`ShardedBackend`). The authoritative metadata store can additionally be swapped to **PostgreSQL** (`meta-server --meta-store postgres://…`): one PG schema per view, transactions around rename/unlink/lock; under PG the in-tree HA (snapshot shipping) is intentionally off — use PG's own streaming replication / PITR instead.
- **View-bound / read-only API keys (the recommended access control)**: `grovefs apikey create --for mount --view V [--ro]` mints a credential that carries *which tree it may open* and *read-write or read-only*. A key bound with `--view V` can only `M_OPEN` view V (opening any other view is refused at the handshake with `E_AUTH`); an `--ro` key is refused **server-side on every write-intent op** (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server), and the `M_OPEN` ack tells the client to mount read-only, so “read-only” never depends on the client remembering `-o ro`. A key is a real, revocable door (unlike a self-reported uid, which is advisory and forgeable). A system-level key (no `--view`) still opens any view.
- **Deletion safety for objects shared across views (the `refs` ledger)**: one `data_ref` names exactly ONE underlying object, yet several physical views can hold names for it at once (`grovefs import` of the same source file into two views resolves to the same key; derived views/aliases share their base view's bytes). A view's own reference count cannot see other views, and trusting it would delete an object another view still references. So the bookkeeping lives in a shared ledger `<base>/meta/refs.db` (PG: table `grovefs_refs.holders`), which every view ATTACHes so the holder count and the node change commit **atomically in one IMMEDIATE transaction**. A blob is physically removed only when “the last name for that key *in this view* is unlinked **and** no other view still holds it”. Out-of-band view-db removals (`rm meta/x.db`, a fresh `--home`) can only make the ledger *over*-count — bytes are kept, never lost; `grovefs refs --meta-dir D rebuild` recomputes holders from the trees that actually exist. **Only BASE views (physical trees created explicitly via `grovefs view create`) ever free bytes on delete**; derived/alias mounts are read-only and their removal never GCs. Removing a base view is an explicit act: `grovefs view rm --name V --base` deletes its `meta/V.db` (+WAL/SHM), rebuilds the ledger, and frees every blob no other view holds (bytes resolve under `--data-dir`, default `--home`; a governed name, a live `grovefs:<V>` mount, or a `--meta-store postgres://…` deployment is refused — USAGE §3.4.3). A FORK holds real names for shared blobs, so a delete inside a fork frees an object only when it is the last name across every view (base included); removing the fork tree triggers `refs rebuild` to drop its holds. A key with no ledger row is simply private to one view, so an empty ledger needs no initialization step. Orphan pages that no surviving tree names and the ledger holds no row for (an out-of-band `rm meta/x.db`, or a crash between object creation and the node insert) are audited and reclaimed offline with `grovefs gc --meta-dir D [--data-dir R] [--apply]`: the scan is the default; `--apply` only deletes pages older than `--older-than` (default 24h), never a key the ledger still holds, and refuses while the store is mounted or a local grovefs process uses it (a PostgreSQL deployment is refused outright).
- **Per-client presentation overlay (`mount --layout rules.txt`)**: one authoritative tree, one copy of bytes, a different *displayed* shape per mount — `hide` trims a subtree, `only` whitelists, `map` relocates an entry to another path (its source spot vacates), `link` adds an alias visible in both places at once (same inode). The lens is pure `MetaProvider`-layer path translation: real entries keep their real inodes, so data IO, locks, GC and invalidation push go straight through; nothing is copied and other mounts are unaffected.
- **Server-side derived views (`grovefs view define` + `mount --view <name>`)**: when shapes should be managed **centrally**, `grovefs view define --name docsonly --base team --rules F` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`) and clients simply `mount --view docsonly` — **no local rules file needed**. Derived (virtual) views mount **READ-ONLY, always**: the shape is centrally governed — distribute it, never modify it — so every write belongs to the original (base) view; a mount of a derived name negotiates the read-only flag at `M_OPEN` and lands on a kernel read-only mount (writes give `EROFS`), with an independent authoritative per-op gate on the `meta-server` and the same check applied to local (`--home`) mounts. One definition renders the same shape for every machine (virtual inodes agree across hosts), **edited definitions hot-apply** (the server checks the definition's mtime per op; live mounts reshape within their ≤2s cache TTL, no remount). A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` when no `--layout` file is given.
- **Server-side pure aliases (`grovefs view alias`)**: governance of *what a view is called* rather than *what it looks like* — a public name maps 1:1 onto a physical view (the definition is a single `alias <real view>` line, no rules), so an alias mount shows the **FULL tree and stays WRITABLE**, while the **real name stops being a mount entry point** (both `meta-server` and local `--home` mounts of a governed real name are refused). Public names can rotate freely without exposing the storage identity — guessing the real name no longer opens a door, and a leaked public name is retired in **one gapless command** (`alias --retire`: write-before-remove so the real name stays unmountable throughout). Aliases share `meta/views/` and `list`/`rm` with derived views; alias chains and derived targets are rejected; `grovefs import` refuses an alias target (an alias is not a storage identity).
- **Zero-copy import of existing data (`grovefs import`)**: when the server already holds a pile of directory data (say `/data/legacy`), `grovefs import --view V --source DIR` registers the whole tree into a view without moving bytes — on the same filesystem each object is a **hard link** of the source file (zero extra bytes; both paths read/write one inode, live in both directions), degrading to a streamed copy across devices. Structure/timestamps/mode/owner are preserved, re-running is incremental, and deleting a name in the view only unlinks the `blocks/` path — **the user's source data can never be destroyed by the view**. Composes with derived views/lenses for virtual-directory shapes (`map` the imported paths anywhere; the resulting derived mount is **read-only** — write through the imported base view). `grovefs import` also refuses a derived target name, which would produce a shadow `<view>.db` nobody can read.
- **Custom binary framing over TCP** (not NFS, not gRPC) with a connection pool.
- **Real-time cross-client invalidation push** (default): after a write, the center broadcasts `inval_entry`+`inval_inode` so peers see changes immediately, no polling; `--no-cache` gives a zero-caching fallback.
- **Distributed advisory locks**: `flock` / `fcntl F_SETLK/F_GETLK` arbitrated centrally, effective across processes and machines; crash reclaim via FUSE `release`, plus a short lease (default 15s) and renewal heartbeats.
- **High availability**: primary/standby `meta-server` with heartbeat/epoch promotion and automatic demotion; `--sync-repl` drops write RPO to 0; `--witness` takes a comma-separated list of shared `data-server` arbiters (odd `N`) and arbitrates by **majority** (`quorum_size = N/2+1`), **fail-safe**, holding a non-overlapping lease at a majority (renewed every tick; each arbiter refuses overlapping leases, so a majority live lease is itself proof of exclusive authority and a write is admitted from a local lease check) and stamping content frames so the arbiters can fence a stale writer, bounding the double-write window to the lease safety margin; losing a minority of arbiters still allows writes (`N=1` = a lone witness).
- **Token authentication** (shared cluster passphrase; both planes), **revocable per-caller API-key gates** (`meta-server`/`data-server --require-api-key` + `mount --api-key`; a key can be bound to ONE view with `--view` and forced read-only with `--ro`, both enforced server-side), **read-only mounts** (`--ro`), **multi-threaded `fuser 0.18`** (`clone_fd`), **chunked parallel IO**, and **cross-server striping**.
- **Per-mount content bandwidth shaping (`--bw-read` / `--bw-write`)**: a client-side GCRA slot schedule paces the bytes actually delivered — reads before the reply, writes after persistence — so the long-run rate is exactly the cap (bytes are paced from the first op; a single op larger than one second of budget may start up to 1 s early) and nothing is dropped or truncated, and write-side delay is natural backpressure. `0`/omitted = unlimited (one `rate == 0` branch on the hot path); `--stats`' `bytes read`/`bytes written` counters verify achieved throughput. Honest boundary: self-discipline, not isolation — a raw data-plane client that bypasses FUSE, or N mounts of one view, each get their own budget, and orphaned objects are keyed by UUID and shared across views, so they cannot be attributed to a view server-side.
- **Agent-facing MCP interface (`grovefs mcp`)**: the view governance above (`define`/`alias`/`list`/`rm` of derived views and pure aliases) is also exposed as **MCP (Model Context Protocol) tools** over stdio, so an AI agent can customize a data listing's shape programmatically — rules travel as **inline text** through `tools/call` and the agent never touches a file. Two read-only introspection tools close the discovery loop: `list_view_tree` renders the real listing a view shows (a derived view through its own lens, or an unpublished `base`+`rules` previewed without committing) and `get_view` reads back a single definition, so an agent writes rules against actual paths instead of guessing; both open the LOCAL SQLite metadata read-only, mount nothing, and touch no object bytes; a `validate_definition` dry run additionally checks a whole candidate (name+base+rules) against every governance guard — writing nothing — and returns the exact refusal a real publish would give. Shares the exact validation and error wording with the CLI (one contract, two entrances) and is control-plane only: it mounts nothing and writes no data. Authentication is opt-in — `grovefs mcp --require-api-key` gates the state-touching methods behind a `grovefs apikey` credential (hashed at rest, scoped `mcp`/`any`) and logs every call; the same `--require-api-key` also fronts `meta-server`/`data-server` for mounts (scope `mount`/`any`), with revocation closing live mounts. See the “Customizing data listings for agents: the MCP interface” section and USAGE §3.5.
- **Parallel metadata reads + paged directory listings + `FUSE_READDIRPLUS`**: alongside its single writer connection, each view's `MetaEngine` keeps 4 extra connections (`READ_POOL_SIZE`) that serve `lookup`/`getattr`/`readdir` and the nlink resolution, so concurrent reads never queue behind the write lock (**writes still serialize on that one authoritative connection** — single-writer semantics are untouched); the server-side engine uses the same pool, so concurrent `meta-server` RPC handlers stop waiting on each other too. Directory listing goes through `readdir_page(parent, offset, limit)`, which the SQLite engine pushes down as SQL `LIMIT/OFFSET` while the FUSE callbacks fill each reply buffer with geometrically growing 32→1024 chunks, so one kernel request costs `O(buffer)` instead of re-scanning the whole directory; and `FUSE_DO_READDIRPLUS` (kernel >= 4.6) is negotiated by default, shipping attributes with the entries so `ls -l` stops degrading into N `LOOKUP`+`GETATTR` round-trips. Measured on one 5001-entry directory, same mount point, each pass behind a fresh mount (empty kernel dentry cache), `ls -l`: **2.56s**, or 5.22s with `--no-readdirplus` (paging only) — both listings byte-identical once sorted. Paging and READDIRPLUS are enabled on any provider that pages *inside its own store*: local `--home` SQLite and PostgreSQL (each with its own 4-connection read pool + SQL `LIMIT/OFFSET`), and remote mounts against a SQLite/PG `meta-server` — the `M_READDIRP` frame pushes `offset/limit` to the server, so a remote client fetches one page per kernel buffer and negotiates READDIRPLUS. Same 5001-entry dataset over `meta-server`+`data-server`: **2.29s** (4.22s with `--no-readdirplus`, paging only; listings byte-identical once sorted). Only derived/lens views (`LayoutMeta`) and mounts against a server too old to advertise the paging cap still fall through to one full listing per buffer with no READDIRPLUS. `--no-readdirplus` stays as the A/B and compatibility knob.
- **Full regression**: `run_all.sh` — **34 stages (33 gated + bench), all GREEN** (`layout` presentation-overlay e2e incl. server-side derived views and read-only derived mounts, remote & local, and pure aliases — writable alias mounts, real-name mount refusal (remote & local), chain/derived-target guards, rm reopens the real name, and gapless name rotation via `--retire` (a leaked public name is swapped in one step — old name stops resolving, the real name stays unmountable throughout); 78 assertions + `bwlimit` per-mount content bandwidth-shaping e2e (17 assertions: a 24MiB read/write at 8MiB/s is held to a >=1.4s floor while an unlimited mount of the SAME view is materially faster, throttled bytes are intact, `GROVEFS_BW_READ` env fallback, `--bw-read 0` = unlimited, invalid rates refused at parse time, `--bw-write` stays inert on a read-only mount) + `import` zero-copy import e2e + `migrate` metadata-export/import e2e (one view's tree travels as host-neutral JSONL to a different store path — no absolute path/hostname/blocks path in the file; ids/inodes/hard links/mode/owner/symlink targets survive, the ledger is rebuilt, import is atomic so a failed `--replace` keeps the old tree, `--as-fork` restores fork semantics with a shared ledger, and several base views stay independent; 74 assertions) + `mcp` agent-governance e2e + `apikey` MCP-interface API-key auth & call-audit e2e (33 assertions: create/list/revoke/set-ro in-place ro↔rw flip (the flipped key still authenticates), hashed-at-rest, no-key `-32001` refusal, good-key admit + attributed call log, `GROVEFS_MCP_API_KEY` env pre-auth, default no-gate server) + `mountkey` revocable mount-credential e2e (76 assertions: scope minting + typo refusal, refusal reasons carried in the protocol, admission & env fallback with the secret absent from argv, connection audit down to key and view, scope isolation both ways, a revoke closing a LIVE mount with no stale endpoint, other keys unaffected, token+key double gate, an independent data-plane gate, failover carrying the credential, ungated default unchanged, a view-bound key refused across views with its refusal audited, an `--ro` key's writes refused server-side, `set-ro` flipping rw↔ro in place on a LIVE mount with no reconnect) + `config` command-shorthand e2e (auto-loaded `KEY=VALUE` file + env fallbacks: the flag > env > file > default chain for GROVEFS_HOME/META/DATA/TOKEN/DB_PATH, parser edge cases — comments, `export` prefix, quoted values, non-`GROVEFS_*` keys and malformed lines warned-and-ignored, an unreadable explicit `$GROVEFS_CONFIG` non-fatal — and a real token-gated FUSE mount whose coordinates come entirely from the file; 28 assertions) + `gc` offline orphan-object audit/reclaim e2e (41 assertions: a clean store scans clean; an out-of-band `rm meta/<view>.db` leaves its objects reported as ledger-held and NOT deleted; after `refs rebuild` they surface as orphans, the default age gate spares a fresh object and `--older-than 0 --apply` unlinks it; loose files outside the bucket layout are reported but never deleted; `--apply` refuses while THIS store has live mounts yet a same-named view mounted from another store does not block; a `--data-dir` sharing no object with any tree is refused, with `--force` clearing only that guard; a PG store and an invalid duration are refused; `--json` field shapes) + `pg` PostgreSQL backend e2e; the PG suite self-skips on hosts without a reachable PostgreSQL, so GREEN is preserved either way).

## Architecture

Three layers, all trait-decoupled:

```
┌─ FUSE mount process  (fs_impl.rs: GroveFS, &self, thread-safe)
│    ├── MetaProvider trait   view-tree access
│    │     ├── local:  MetaEngine  → <home>/meta/<view>.db
│    │     ├── remote: RemoteMeta  → meta-server (authoritative)
│    │     └── overlay: LayoutMeta → mount --layout (per-client presentation, decorates either)
│    │           ├── derived view: server-side LayoutMeta singleton → mount --view <name> (rules defined once via grovefs view define, stored in meta/views/)
│    │           ├── fork view: `fork <base>` → independent meta/<name>.db (structure writable, content EROFS, bytes shared)
│    │           └── pure alias: `alias <real view>` → public name maps 1:1 (full tree, writable; real name not mountable — name governance)
│    └── StorageBackend trait  object IO
│          ├── local:  LocalBackend  → <home>/blocks/<2hex>/<key>
│          ├── remote: RemoteBackend → data-server
│          └── striped: ShardedBackend → multiple data-servers
└─ Transport: custom binary frames over TCP (fixed header + variable body), pooled
```

- **Pluggable trait seams**: local/remote/striped are interchangeable; the interface surfaces, wire-protocol constants and the new-backend on-ramp are summarized in USAGE §16 (tiered walkthrough in docs.html §18/§19).
- **Central MetaServer**: one authoritative view per user shared across machines, so no diverging local copies.
- **Event bus + Notifier**: pushes invalidations after writes for content freshness.
- **fuser 0.18 multi-threading**: each worker gets its own `/dev/fuse` fd (`clone_fd`, Linux 4.5+), requests dispatch concurrently.

## Three questions about views: how to customize, where metadata lives, how permissions work

> A map of the three headline topics; each subsection points to its detailed sections below and in USAGE.

### 1. Customizing a view: a rule lens that reshapes presentation, never moving bytes

The only legal material for customization is the **base view's real display paths** (look them up first — walk the mount, or use MCP `list_view_tree`, which renders metadata without mounting; there is no standalone CLI tree-listing subcommand; never guess from the source layout, since `import` nests the source under a prefix like `/seeds`). Rule text: one verb per line (`#` comments):

```text
hide <src>          prune a subtree
only <src>          whitelist: serve only these subtrees (plus the ancestors needed to reach them)
map  <src> <dst>    move: the entry disappears from <src> and is served at <dst> (parent dirs synthesized)
link <src> <dst>    mirror: <src> stays and <dst> adds another name (same inode, multi-view presentation)
```

Four entrances share one `LayoutMeta` lens; only the rule source differs:

| Entrance | Rule source | Scope | Write semantics |
|---|---|---|---|
| `mount --layout F` | client-local file | this mount only | fully writable, writes hit the real entries |
| `grovefs view define --name D --base B --rules F` | server-side `meta/views/D.layout` | every client mounting `--view D` | derived mounts read-only, writes go to the base |
| `grovefs view fork --name X --base B [--empty]` | independent metadata tree `meta/X.db` (`--empty`: root row only) | its own organizable tree | structure writable, content `EROFS`, bytes shared |
| `grovefs view graft --name X --map P [--src S] [--dest D]` | a plan of `<src>[TAB<dst>]` lines | entries placed in the fork | one transaction, zero bytes copied |
| `grovefs view alias --name P --real R` | a single `alias <R>` line | public name 1:1 governance | full tree writable, real name refuses mounts |

The customized shape **does not have to resemble the base tree**: `<src>` is pinned to the base, `<dst>` is yours to invent; add `only <new-root>` to make the custom tree the whole view. The one thing a lens cannot do is invent content — every rendered entry is some base entry re-pathed, mirrored or pruned, plus synthesized scaffolding; bytes are never copied. Hot-reload: mounts through `meta-server` reshape within ≤2s (the server re-checks the definition's mtime per op); a bare local `--home` mount resolves once at mount time and needs a remount.

> Details: README "Per-client presentation / Server-side derived views / Pure aliases" (fork: USAGE §3.4.1), USAGE §3.1–§3.4.1; the agent workflow in README "Customizing data listings for agents" and `skills/grovefs-mcp/`.

### 2. Where view metadata lives: one engine per view, control plane follows the backend

```text
<meta-dir>/meta/
├── <view>.db            one authoritative tree per physical view (SQLite WAL): node table
│                        (id/ino, parent_id, name, is_dir, data_ref, link_target,
│                        mode/uid/gid, mtime/atime/ctime, UNIQUE(parent_id,name)
│                        + ino/parent/data_ref indexes) + ctrl + locks (POSIX lock leases)
├── refs.db              cross-view holder ledger for shared objects (GC's sole "last name" authority;
│                        derived/alias/fork share the base's bytes)
├── views/*.layout       derived/alias/fork definitions (derived = first line `base <B>` + rule
│                        lines; alias = a single `alias <R>` line; fork = a single `fork <B>` line)
└── api_keys.db          API key SHA-256 digests + call audit (the SQLite control plane; under PG
│                        it lives in `grovefs_control.api_keys` instead)
Data bytes: blocks/<2hex>/<key> (key = UUID, hash-fanned into 256 buckets, so per-directory
  entry count never grows with total objects)
PG backend (meta-server --meta-store postgres://…): one schema `grovefs_v_<hex(view)>` per view with
  the same node/ctrl/locks + the shared `grovefs_refs.holders` ledger; view trees then stop
  landing in .db files — and the control plane follows: view definitions, keys and audit all move
  into the shared `grovefs_control` schema (`view_defs`/`api_keys`/`mcp_calls`), leaving no
  control-plane files on disk. `grovefs view`/`apikey`/`mcp`/`data-server` take the same
  `--meta-store postgres://…` (or `GROVEFS_META_STORE`) to read and write that one system of record.
```

A write splits across two halves: metadata ops land in `node` rows, content writes land in `blocks/`, and `data_ref` is their only join point (striped objects encode `s<N>:<stripe_bytes>:<uuid>`). Backup checklists, the exact GC rule and cross-machine migration paths live in the detailed sections.

> Details: USAGE §5.0 (one-screen anatomy) / §5.1 / §5.3 / §5.4, README "Sharing across machines & storage backends" (SQLite↔PG is either/or), USAGE §3.4.2 (`view export/import` moves one tree across machines).

### 3. Managing permissions: a two-layer stack plus one revocable key

The admission stack has two layers — **token = cluster admission, view name = tree selection**. The `--view` string is not an identity: any uid mounting the same `--view alice` lands in alice's one tree. For **per-caller differentiated access**, add the third layer: revocable API keys (gate with `--require-api-key`, enforced server-side per op):

```text
M_OPEN / OP_OPEN (TCP frame handshake), gates in order:
① token        mismatch → E_AUTH (the only deterministic gate; client-reported uid is advisory)
② api-key      scope split (--for mcp|mount|any); revoked → refused; a key bound with --view opening
               another view → E_AUTH right at the handshake
③ read-only    a --ro key: every write-intent op on the meta plane → E_NOTPERM, content writes on
   write gate   the data plane → EROFS, lock ops → EPERM; M_OPEN replies "read-only mount" —
               never trusts the client's -o ro
④ view name → engine cache   opened at the first M_OPEN, shared by every caller of that view
Governance plane (grovefs mcp): the same key registry (SQLite: api_keys.db; PG:
grovefs_control.api_keys), scope must include `mcp`; every tools/call and
resources/read is audited; revocation freezes live mounts within ≤1s (op-driven re-check), while
historical calls keep their attribution.
```

Honest boundary: display governance (derived/alias/fork) is **not a security boundary** — it only reshapes the presented name tree; what is visible is readable and readable is writable back (a fork's content excepted). The real doors are the token and revocable keys. Keys store only SHA-256 digests and print their plaintext once; the control plane (keys, audit, view definitions) **follows the metadata backend** — SQLite keeps `api_keys.db` + `views/*.layout`, PG keeps everything in the `grovefs_control` schema; one system of record, never split.

> Details: README "Shared views & the permission model" + the API-key subsection, USAGE §6 (incl. §6.1 view-binding / read-only key measurements), §3.5.1 (ten-step key drill).

## Sharing across machines & storage backends (how it works, common questions)

> This section answers the two most-confused questions: (1) does "many servers sharing one directory tree over the LAN" **require PostgreSQL**, and (2) which side does PG actually run on?

### The full path of one write (data plane and metadata plane, half each)

GroveFS splits a file's *bytes* from its *directory tree* into two independent channels — this is precisely why it needs no NFS:

```
app write()/close()
   │
   ├─ data plane ─→ RemoteBackend ─TCP frame→ data-server ─→ blocks/<2hex>/<key>  (UUID-named, 256-bucket fan-out)
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
**No.** The "many machines share one tree" capability comes from **`meta-server` + the same `--view`**, independent of whether the backend is SQLite or PG. The default embedded SQLite (`rusqlite bundled`, compiled into the binary, zero external deps) was built for exactly this topology — deploy `data-server` + `meta-server` on a central node and have clients `mount --view alice --meta-server <center>:50110 --data-servers <center>:50100`; they share one authoritative tree with real-time cross-client visibility. This whole path is proven on a pure-SQLite backend by the `metasrv` / `freshness` / `locks` suites — **you do not need to install PostgreSQL first**. The two backends are an **either/or** choice, not a layered deployment: one authoritative tree lives on exactly one backend — stay on SQLite and you never install PG at all; switch to PG and the server stops creating/reading any `<meta-dir>/meta/<view>.db` (the only ever-SQLite exceptions are the control-plane `api_keys.db` and derived-view `.layout` definitions, which are independent of backend choice).

**Q: Which side does PG run on?**
**Server-side only** (on the `meta-server`), as its **pluggable authoritative storage backend**:

```
client (just the grovefs binary)   server-side                 data tier
mount --view alice   ─TCP→        meta-server   ─postgres://→  PostgreSQL server
  · no PG lib / creds / awareness  · --meta-store is ITS flag       (same host as meta-server, or a dedicated DB box)
```

`--meta-store postgres://…` appears only on the `meta-server` subcommand; clients have no such option and never see the connection string. The PostgreSQL server process runs physically in the center (co-located with `meta-server` over loopback, or a dedicated DB machine reached over the LAN) but is **always server-side**.

**Q: So when is PG actually worth it?**
When you want to hand the authoritative tree's **durability / HA / backup / centralized operations** to a mature database: PG brings streaming / logical replication, PITR, `pg_basebackup`, monitoring and DBA tooling, and can tune very large metadata (millions of nodes) with indexes/VACUUM. Note **the in-tree HA (snapshot shipping) is mutually exclusive with PG** (see "Shared views & the permission model") — once on PG, use PG's own replication, not GroveFS's file-level snapshots.

> **Honest boundary**: the write model **differs** between the two engines — the default SQLite backend keeps `meta-server` a **single writer** per view (one authoritative connection + mutex, writes serialized); `--meta-store postgres://…` widens writes to a bounded **SERIALIZABLE write pool** (`PG_WRITE_POOL_SIZE = 8`, the HopsFS route with the transactional substrate handed to PostgreSQL's own SSI) — concurrent writes on disjoint `(parent_id, name)` rows commit in parallel, read-write overlaps abort (`SQLSTATE 40001/40P01`) and are **transparently retried** (200µs–25ms backoff, ≤12 attempts, never leaked to POSIX callers), proven by the `pg_ssi` (5 Rust tests) and `pg_parallel` (bash, 3 mounts × 45 concurrent creates) suites with auto-SKIP on PG-less hosts; governance invariants I1–I4 hold unchanged under any SSI-accepted schedule. Parallelism stays **within one PG cluster** — cross-cluster metadata partitioning is not done (hand that to PG's own replication/sharding). Metadata *read* parallelism spans **both** engines — SQLite `MetaEngine` and PostgreSQL `PgEngine` each keep their own read pool (both 4 connections: `READ_POOL_SIZE` / `PG_READ_POOL_SIZE`) and page listings with SQL `LIMIT/OFFSET`. For default deployments, keep the zero-dependency SQLite; PG's incremental value is **write parallelism + durability / replication / operational maturity**.

## Build

**Prerequisites** (Linux): Rust (edition 2021) only. **No `pkg-config`, no libfuse development package** — the FUSE client is the pure-Rust `fuser` (its `libfuse*` features are not enabled); a full `PKG_CONFIG=/nonexistent cargo build --release --features fdb` succeeds and the resulting binary has no fuse library in `DT_NEEDED` (only `libfdb_c` plus five glibc libs). The metadata engine uses `rusqlite` with the `bundled` feature — SQLite is compiled from source, **no system dependency**. Mounting needs the kernel's `/dev/fuse`, plus the distro's setuid helper `fusermount`/`fusermount3` **when unprivileged** (with `CAP_SYS_ADMIN` it goes straight to `mount(2)`) — a userspace tool, not a development package.

```bash
cargo build --release          # artifact: target/release/grovefs
```

> Note: the repo still ships `.build-bin/pkg-config` and the suite scripts still `export PKG_CONFIG=…` —
> that is **legacy** (it mattered when `fuser` used the libfuse backend). No crate in the current
> dependency graph queries `pkg-config`, so the shim can stay or go without affecting the build.
> On a fresh machine just run:
> ```bash
> cargo build --release && export LC_ALL=C LANG=C
> ```

## Parameter meanings at a glance

> Names like `--home` / `--meta-dir` below are the literal CLI flags (copy them as-is); what each one actually points at:

| What it controls (plain meaning) | Flag |
| --- | --- |
| **Local storage root** — the directory holding `blocks/` (data) and `meta/` (views) in single-node mode | `--home <dir>` |
| **Server metadata root** — the host directory holding the authoritative view DBs, view definitions and API keys for `meta-server` | `--meta-dir <dir>` |
| **Data landing root** — where `data-server` writes bytes (defaults to the metadata root) | `--data-dir <dir>` |
| **Existing directory to take over** — data already on disk that you want to turn into a view | `--source <dir>` |
| **Mount point** — the local directory you use to reach the tree | `-m <dir>` (`--mountpoint`) |
| **Central service address** — the meta / data endpoints a client connects to (host:port) | `--meta-server <addr>` / `--data-servers <addr>` |
| **View name** — an arbitrary label for one logical tree (unrelated to Linux usernames) | `--view <name>` |
| **Cluster admission secret** — the shared token a server demands (data face and meta face can differ) | server `--token <str>` + client `mount --token <str>` |
| **Revocable per-caller credential** — the server admits a live API key instead (`grovefs apikey create --for mount`); listable, auditable per caller, and a revoke closes **already-established** mounts | server `--require-api-key` (+ `--meta-dir D` on `data-server`) + client `mount --api-key <ID:SECRET>` (or env `GROVEFS_API_KEY`) |
| **Mount as a given uid** — lock arbitration and audit record this uid as the entry owner (advisory) | `--run-as-uid <uid>` |
| **Read-only / zero-cache strong consistency** | `--ro` / `--no-cache` |
| **Directory-listing fast path escape hatch** — turn off READDIRPLUS (attributes shipped with each entry), falling back to plain READDIR + per-entry `LOOKUP`; A/B benchmarks and compatibility debugging only (mounts whose provider cannot page in its own store never negotiate READDIRPLUS, so this flag is a no-op there) | `--no-readdirplus` |

## Quick start (single-node, local)

```bash
Y=./target/release/grovefs
BASE=/data/myfs

$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

$Y --home $BASE mount --view alice --mountpoint /mnt/alice   # detaches by default; --foreground to stay attached
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob

mkdir -p /mnt/alice/projects/2026
echo "report" > /mnt/alice/projects/2026/report.txt
ls /mnt/bob                                # bob cannot see alice's files

$Y umount --mountpoint /mnt/alice
```

Seed a hierarchy: `init --view alice --manifest seed.txt` (lines of `mkdir <path>` / `touch <path>`, `#` comments, idempotent).

## Shrinking long commands: environment variables & the auto-loaded config file

Repeat-heavy flags don't have to be retyped. Every knob resolves through one chain — **explicit flag > live environment variable > config file > built-in default** — and on startup grovefs auto-loads a `KEY=VALUE` file into any `GROVEFS_*` variable not already set, searching `$GROVEFS_CONFIG` (explicit path) first, then `~/.config/grovefs/grovefs.conf`:

```bash
# ~/.config/grovefs/grovefs.conf (chmod 600 if it carries a secret — same contract as
# putting one in an environment variable)
GROVEFS_META_SERVER=192.168.1.10:50110
GROVEFS_DATA_SERVERS=192.168.1.10:50100
GROVEFS_TOKEN=s3cret

# before: grovefs --home /data/myfs mount --view alice -m /mnt/alice --meta-server … --data-servers … --token …
# after:  only "which view, where" is left on the line
grovefs mount --view alice -m /mnt/alice
```

Knobs resolvable from env/file: `GROVEFS_HOME` (`--home`), `GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS` / `GROVEFS_TOKEN` (`mount --meta-server/--data-servers/--token`, and the servers' `--token` reads the same knob), `GROVEFS_META_DIR` (`meta-server --meta-dir`), `GROVEFS_API_KEY` / `GROVEFS_MCP_API_KEY` (the credential envs, also settable from the config file), `GROVEFS_STATS`. The syntax honors `#` comments, an optional `export ` prefix, and quoted values; non-`GROVEFS_*` keys and malformed lines print a `file:line` warning and are ignored. `--view`/`-m` are deliberately NOT file-servable — they differ per mount, and baking them into a default is the wrong shape. The detached-mount re-exec inherits the environment, so the shorthand survives daemonization. Regression: the `config` suite (28 assertions: the precedence chain, parser edge cases, and a real token-gated FUSE mount driven entirely from the file).

## About `--view` and per-view path capacity

- **`--view` is a view-namespace label, unrelated to Linux usernames**: any legal filename works (`alice`, `team-a`, `u42`, `research_group`). It is used only to (1) name `<home>/meta/<view>.db` and (2) tag the FUSE mount as `grovefs:<view>`. The binary does not call `getpwnam`, does not verify uid, does not require `useradd`. **Access authority comes from being able to reach `--home` / `blocks/` (or holding the server's `--token` / a live `--require-api-key` credential), not from the Linux user**. Under the same `--home`, distinct `--view` values map to distinct, mutually invisible views.
- **One user = one tree, unlimited paths inside**: once mounted you may `mkdir`/`touch` any number of files and directories at any depth under that mountpoint, exactly as in your own home. Per-user isolation applies to the whole tree, not to individual paths. `--manifest` is only a convenience for pre-seeding a skeleton, not a cap on path count or depth.

## Shared views & the permission model

GroveFS has exactly two authorization layers — **token = cluster admission, view name = tree choice**. `--view` is not an identity: any uid that mounts the same `--meta-server <addr>` with the same `--view alice` lands on alice's single authoritative tree and sees the other writers. That is intentional and makes "one view, many users" work out of the box.

```text
Client A (uid=1000)  mount --view alice ─┐
Client B (uid=2000)  mount --view alice ─┤ same --view = one shared tree; one client
Client C (uid=1000)  mount --view bob   ─┘ may mount many views at once (N:M)
        │
        ▼ TCP frame M_OPEN(view, token, uid, api_key)   (clients never open .db files or hold PG conn strings)
┌─ meta-server (single authoritative writer) — gates run in order, then routing
│  1) token gate    mismatch -> E_AUTH (cluster admission; the only hard gate; client-reported uid is advisory)
│  1') api-key gate  missing/invalid/revoked/wrong-scope -> E_AUTH; a key bound with --view is refused on any other view
│  1'') ro key gate   an --ro key is refused every write-intent op (E_NOTPERM; content writes EROFS at the data server)
│  2) view name -> engine cache   first M_OPEN opens the existing tree (never creates one); later mounts reuse it
└──┬──────────────────────────────────
   ▼ view=alice                          ▼ view=bob
  alice's authoritative tree (shared A+B, writes serialized)   bob's tree (C only, invisible to alice)
   ├─ SQLite (default):      one view = one DB file   <meta-dir>/meta/alice.db · bob.db
   └─ PostgreSQL (--meta-store):  one view = one schema    grovefs_v_616c696365 · grovefs_v_626f62

The unit of isolated storage is the *view*, not the client; content bytes live in the data-server's `<root>/blocks/<key>` (its `--data-dir`), referenced by data_ref inside each tree.
```

- **Many OS users sharing one view**: point every client at the same `mount --view alice --meta-server ... --data-servers ...` and both metadata (single SQLite/PG tree, writes serialized through the central engine) and content (single `blocks/`) converge in-place. If you need **differentiated rights per uid** inside that shared tree (e.g. an auditor mount that can list but never mutate), the recommended way is now a **view-bound / read-only API key** (`grovefs apikey create --for mount --view alice [--ro]` — enforced server-side, revocable, attributable); see “Locking the doors” below. The uid a mount presents is `geteuid()` by default and can be overridden with `mount --run-as-uid N` (handy for role-based mounts against a shared `meta-server`); it is recorded as the entry owner for lock arbitration and audit and is **advisory** — a TCP client can forge it, so it is never an access-control boundary. Real admission is the `--token` / `--require-api-key` gate, and “which tree, read-only or not” is hard-enforced server-side by a key's `--view`/`--ro` binding. A lock op (`flock`/`F_SETLK`, incl. lease renewal) writes the central `locks` table and counts as write-intent, so an `--ro` key is refused on it too (`E_NOTPERM`, surfacing as `EPERM` at the client — deliberately distinct from an HA standby's `E_ROFS` failover signal); a revoke is op-driven and so *does* reach live connections (re-checked about once per second under load).

- **Metadata backend is swappable: SQLite ↔ PostgreSQL** (this is the `meta-server`'s *authoritative* store only; the wire protocol clients see does not change):

  ```bash
  # default: embedded SQLite, one `<home>/meta/<view>.db` per view
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL: one `grovefs_v_<hex(view)>` schema per view, tx-guarded rename/unlink/lock
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
       --meta-store postgres://user:pw@10.0.0.9:5432/grovefs
  ```

  Clients mount exactly the same way (`--meta-server host:port --view alice`). Notes that matter:
  - **PG is mutually exclusive with the in-tree HA flags**: `--meta-store postgres://…` combined with any of `--role standby` / `--peer` / `--witness` fails fast at startup — GroveFS-native HA ships snapshots via SQLite `VACUUM INTO`, which has no PG analogue. On PG, use PostgreSQL's own streaming / logical replication / PITR, which is strictly stronger than the file-snapshot mechanism anyway.
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
$ grovefs --home .demo/home mount --view alice -m .demo/mnt --layout .demo/lens.txt &
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

Rules are parsed once at mount time (an invalid file fails fast rather than silently showing everything). The lens is a **per-mount-instance** presentation layer — nothing is persisted, no server state changes — so it composes with local `MetaEngine` or remote `RemoteMeta`, with `--ro`, and with a read-only view-bound API key. Against a shared `meta-server`, invalidations for entries living under synthesized dirs are broadcast by *source* path and may miss lens coordinates; add `--no-cache` (zero attribute/entry cache) in that setup.

## Server-side derived views: one copy of data, a shape per `--view` name (`grovefs view`)

`--layout` is configured per client. When shapes should be **managed centrally** — so that a team shares one display contract under a name — use derived views: `grovefs view define` stores the rules in the meta root (`<meta-dir>/meta/views/<name>.layout`, first line `base <physical view>`, remaining lines are the same four verbs as `--layout`), and clients simply `mount --view <name>` with **no local rules file**. Derived (virtual) views mount **read-only, always** — the shape is a centrally governed distribution, every write belongs to the original (base) view. One definition renders the same shape on every machine (a server-side `LayoutMeta` singleton per derived name, so virtual inodes agree across hosts). A local (`--home`) mount resolves the same `<home>/meta/views/<view>.layout` automatically when no `--layout` file is passed.

```bash
$ cat .demo5/docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir .demo5/home define --name docsonly --base team --rules .demo5/docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir .demo5/home define --name team --base team --rules .demo5/docsonly.rules   # name == base
error: derived view name must differ from its base
$ grovefs view --meta-dir .demo5/home list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs --home .demo5/home mount --view docsonly -m .demo5/mnt &    # mount by name, no --layout
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
$ grovefs view --meta-dir .demo5/home rm --name docsonly
removed derived view 'docsonly' (underlying data untouched)
```

Guards and semantics: `define` refuses `name == base`, refuses a base that is itself a derived view (no nested lenses) and refuses to shadow an existing physical `<name>.db`; invalid rules fail fast (never degrading into "show everything"). **Virtual views are read-only by hard contract**: writes belong to the original (base) view only. The `meta-server` ships a derived-name flag in the `M_OPEN` ack, so the client lands a kernel read-only mount immediately (every mutation — and `flock` write locks — give `EROFS` with zero round-trips); the server additionally keeps an **independent authoritative per-op gate** on derived names (answering `E_NOTPERM`, deliberately *not* `E_ROFS`, which clients read as an HA failover signal), so old or crafted clients cannot bypass it; a local (`--home`) mount of a derived name enforces the same RO at mount time. **Edits hot-apply, no remount**: the `meta-server` checks the definition file's mtime before dispatching every op, so a replaced definition reaches established mounts within the client's cache TTL (≤2s) — and the read-only contract survives the reshape; a `view rm` deliberately does **not** hot-apply (live mounts keep the shape they opened with until remount — nobody has the tree yanked out from under them). Behind a shared `meta-server`, a derived view's subscribers register in **both its own bucket and the base name's bucket** and every write is broadcast to both, so base writes land in derived mounts even without `--no-cache` (verified by section 9 of the `layout` e2e: base writes visible in the derived mount, derived mounts refuse writes/deletes with `EROFS` and leave the base tree untouched while the base name stays writable on the same server, a re-define reshapes a live mount, and local derived mounts are read-only too). `grovefs view rm` deletes only the definition, never data. `grovefs view` operates on server-local files — run it where the `meta-server` lives (or share the same `--meta-dir`); under HA, `meta/views/` is control-plane config and must be synced to the standby by hand. **Base views are a first-class act**: `grovefs view create --view NAME` (equivalently `init --view NAME`) stands up a physical tree — it is the *only* kind of view whose deletes can free bytes, and only after the cross-view `refs` ledger (§Key features) confirms no other view still holds the object; derived views and aliases never own physical deletion. `grovefs refs --meta-dir D rebuild` repairs the ledger after an out-of-band `rm meta/x.db`.

Hot reload on one established connection (no remount anywhere in this transcript):

```bash
$ ls .demo3/mnt                             # docsonly = only /docs, mounted
docs
$ grovefs view --meta-dir .demo3/home define --name docsonly --base team --rules .demo3/broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls .demo3/mnt                             # same mount, shape widened by itself
docs  photos
```

## Pure aliases: name governance (`grovefs view alias`)

Derived views govern *what a view looks like*; **pure aliases** govern *what it is called* — the name is the alias, the mapping is the entry. The definition shares `<meta-dir>/meta/views/<name>.layout`, first line `alias <real view>`, no rule lines. The mirror image of a derived view: an **alias mount shows the FULL tree and is WRITABLE** (it simply is the real view under a public name), while the **real name stops being a mount entry point** — `meta-server` and local (`--home`) mounts alike refuse a governed real name, so public names rotate freely and the storage identity stays unexposed (guessing the real name no longer opens a door):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs mount --view teamwork -m /mnt/work --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # full tree, writable
$ grovefs mount --view team -m /mnt/t --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240          # the governed real name: refused
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
$ grovefs --home $BASE mount --view team -m /mnt/t                                        # local mount: same contract
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

When a public name leaks, rotate it in **one gapless command** with `--retire` (write the new alias before removing the old, so the real name stays governed throughout — there is no window where it becomes mountable by itself):

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork   # old public name leaked, swap to teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
$ grovefs mount --view teamwork -m /mnt/leak --meta-server ... --data-servers ...                     # old leaked name remounts as an empty view (never the real tree)
```

Guards and semantics: `alias` rejects `name == real`, shadowing a physical `<name>.db`, derived-view targets, and alias chains (resolution is single-hop by design); one real view may carry at most one public name (`--retire` rotates within one command — write-before-remove, so it is never blocked by this invariant — and refuses a `--retire` pointing at a derived view or a non-existent alias, so a shape definition can never be dropped by accident). `grovefs import` refuses an alias target — an alias is not a storage identity, and writing a separate `<alias>.db` would create a shadow tree nothing reads. `view rm` on an alias reopens the real name for NEW mounts (established connections are untouched); rotation/rm only affect NEW mounts, and a retired name resolves to a fresh empty namespace that never reaches the real tree. Note the honest boundary: hiding/rotating the real name is name governance (leak hygiene), not a security mechanism — public names must be distributed to be used and rotation cannot revoke an existing token-holder's access, so real isolation stays token + api-key. (Verified by sections 10/11 of the `layout` e2e suite — 26 alias assertions; see USAGE §3.4 for full transcripts.)

## Customizing data listings for agents: the MCP interface (`grovefs mcp`)

View governance is also a set of **MCP (Model Context Protocol) tools**: `grovefs mcp` speaks newline-delimited JSON-RPC 2.0 on stdio (the `initialize` / `ping` / `tools/list` / `tools/call` core of the 2025-06-18 protocol revision), so any MCP client — Claude Desktop, an IDE agent, any agent runtime — spawns it as a subprocess and lets the agent **customize a data listing's shape programmatically**: list views, inspect the real directory listing a view shows (`list_view_tree`, which renders a derived view through its own lens and can preview an unpublished `base`+`rules` lens without committing), read back a single definition (`get_view`), dry-run a whole candidate definition against every governance guard without writing (`validate_definition`), publish a read-only derived lens from inline rule text (the four `--layout` verbs `hide`/`only`/`map`/`link` travel as text lines — the agent never touches a file), rotate public names, remove definitions. Every transcript below is captured from a real `mcp`-suite run:

```bash
$ grovefs mcp --meta-dir /data/xy/grovefs/.demomcp/home    # spawned by the MCP client; stdin/stdout are the channel
# agent-side tools/call, tool result text shown verbatim:
[list_views]   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{
  "db_path": "/data/xy/grovefs/.demomcp/home",
  "defined": [],
  "physical": [
    "team"
  ]
}
[preview_rules] "rules": "bog /x"                        # preview before publishing: a bad verb is caught first
bad layout rules: line 1: unknown layout verb 'bog'
[define_derived_view] {"name":"release","base":"team","rules":"# public release: no private photos\nhide /photos/private"}
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
# user side mounts the agent-defined name directly (no --layout, no local file):
$ grovefs --home $HOME_DIR mount --view release -m .demomcp/mnt &
derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ ls .demomcp/mnt
docs  photos
[list_view_tree] {"view":"team"}                       # discover the real paths first (reads local metadata, mounts nothing, touches no data bytes): rules written against these, not guesses
{"kind":"physical","rendered_from":"team","dir_count":4,"file_count":3,"truncated":false,"entries":[
  {"path":"/docs","kind":"dir"},{"path":"/docs/2026","kind":"dir"},{"path":"/docs/2026/b.md","kind":"file"},
  {"path":"/docs/a.md","kind":"file"},{"path":"/photos","kind":"dir"},{"path":"/photos/private","kind":"dir"},
  {"path":"/photos/private/x.jpg","kind":"file"}]}
[list_view_tree] {"view":"release"}                    # rendered THROUGH the published lens: display paths /docs /docs/2026 /docs/2026/b.md /docs/a.md /photos (/photos/private pruned — exactly what the mount shows)
[list_view_tree] {"base":"team","rules":"only /docs"}  # preview an unpublished lens without writing any .layout: only the /docs subtree renders
[validate_definition] {"name":"release","base":"team","rules":"hide /photos/private"}  # pre-publish dry run: walk the whole name+base+rules through every governance guard, write nothing, show the verdict
{"accepted":true,"dry_run":true,"kind":"derived","hide":1,"only":0,"reloc":0,"would_replace_existing_definition":false,"base_view_db_present":true,"note":""}
[get_view] {"name":"release"}                          # read a single lens's raw rules back for read-modify-write iteration
{"kind":"derived","base":"team","hide":1,"only":0,"reloc":0,"definition":"base team\nhide /photos/private","mount":"read-only (virtual shape; writes go to the base view)"}
[list_views] again                    # structured JSON: name/kind/real_view/rule counts
view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)   # CLI cross-check: byte-identical wording
[define_derived_view] {"name":"team","base":"team",...}                        # name == base
isError → derived view name must differ from its base               # the CLI's refusal string, verbatim
```

Semantics and boundaries: ten tools — eight governance ops (`list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`) plus two read-only introspection tools (`list_view_tree` / `get_view`) — and the `grovefs view` subcommands share **one library implementation** (`grovefs::mcp`): validations, refusal strings and file mutations are byte-identical, one contract with two entrances — an agent cannot do anything the CLI cannot, nor bypass a guard (derived mounts stay read-only, aliases keep refusing the governed real name, `--retire` stays write-before-remove, `fork_view` copies namespace rows only — never bytes — into an independent per-view metadata db whose structure is writable while content stays `EROFS`, and `remove_view` on a fork drops only that tree and rebuilds the shared ledger; hot-reload likewise belongs to the `meta-server` entrance — a local mount resolves its definition once at mount time). `list_view_tree`/`get_view` are **metadata-only introspection**: they open a view's LOCAL `<meta-dir>/meta/<view>.db` read-only and walk it through the same `LayoutMeta` lens a mount uses (no mount, no object bytes), so an agent writes rules against real display paths and previews a shape before committing it; remote/PG-backed trees are not walked here (they need a local SQLite metadata root), and the `size` in a rendered entry is the metadata field rather than a byte count (dirs 4096, files 0, symlinks their target length) — stat through a mount for real bytes. `validate_definition` is a pre-publish dry run: it walks a whole (name, base, rules) through the full `define_derived_view` guard set — invalid names, name==base, shadowing a physical db, replacing a pure alias, bad rule text — returning the exact byte-identical refusal a real publish would produce, or an `accepted` JSON verdict, and it writes NOTHING. Governance failures come back INSIDE the result (`isError: true` + the CLI text) per MCP convention; only protocol-level faults use JSON-RPC errors. Notifications (no id) are never answered; a malformed frame gets `-32700` and the stream recovers. **Control plane only**: it never mounts and never writes data. By default it trusts whoever can spawn the process (the same OS-level boundary as a local mount); cross-machine auth remains the server's token / API key. When it fronts many distinct callers, `grovefs mcp --require-api-key` opens an opt-in API-key gate (next subsection) — the same credential family, extended by scope to the mount/data doors as well. Wire-up: register `grovefs mcp --meta-dir D` as a stdio server in the client config (a per-call `db_path` argument overrides the default). E2E: the `mcp` suite, 65 assertions (handshake/discovery/inline-rule reshapes/path discovery + shape preview/pre-publish dry-run/alias rotation/fork lifecycle/CLI-identical wording/protocol error paths) and the `apikey` suite, 33 assertions; see USAGE §3.5. The agent-side operating manual (in English, for international deployments) ships with the repository under `skills/grovefs-mcp/` (`SKILL.md` + `reference.md` + `examples.md`, plus the dependency-free stdio client `scripts/mcp_call.py` and `scripts/smoke.sh`, a connection self-test that runs the whole workflow against a throwaway store).

### Locking the doors: API-key auth, scopes, view binding / read-only, and call audit (`grovefs apikey`)

One server often faces MANY distinct callers — an agent runtime fronts a `grovefs mcp`, several client machines mount the same `meta-server` over the LAN — and that needs revocable credentials plus per-caller attribution. `grovefs apikey` mints keys (**stored as SHA-256 only** — the plaintext prints once at `create`, so a leaked `api_keys.db` yields nothing usable) and tags each with a **scope**: `--for mcp` (opens the governance interface only), `--for mount` (opens a `--require-api-key` meta/data server only), `--for any` (the **default**; every key minted before scopes existed is this one, so older deployments need no migration). Both entrances share ONE registry (`<meta-dir>/meta/api_keys.db`); the scope keeps them isolated — a mount credential cannot be replayed against the governance API, an agent key cannot open the data plane. A mount key can additionally carry **two access-control dimensions**: `--view V` binds the credential to ONE physical view — `M_OPEN`ing any other view is refused at the handshake with `E_AUTH` (a system-level key without `--view` opens any view); `--ro` marks it read-only — the server refuses every write-intent op (`E_NOTPERM` on the meta plane, `EROFS` on content writes at the data server) and the `M_OPEN` ack tells the client to mount read-only, so read-only is enforced rather than trusted. **Governance gate**: `grovefs mcp --require-api-key` gates every method that reads or writes governance state — `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `completion/complete` (`initialize` / `ping` / `prompts/*` / `logging/*` always stay open so a client can handshake and show its key). **Mount gate**: `meta-server --require-api-key` gates `M_OPEN` and `data-server --require-api-key` gates `OP_OPEN` (`--meta-dir` points the data server at the metadata root, defaulting to its first `--data-dir`); the client presents `mount --api-key <ID:SECRET>` (or exports `GROVEFS_API_KEY` so the secret never lands in argv). **Audit gate**: every `tools/call`, every `resources/read` and every mount connection — refusals included — is appended to one table (timestamp, key, plane (`method` = meta|data|the MCP method), tool, success, detail) that `grovefs apikey calls` reads back. Revoking only marks the row `revoked` (past calls keep their attribution); a revoked key immediately fails authentication **and closes mounts that are already up**. Storage is its own local SQLite file (`meta/api_keys.db`), control-plane state. Transcripts below are from a real run:

```
$ grovefs apikey --meta-dir <home> create --label laptop-alice --for mount   # mint + print the plaintext <id>:<secret> once
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  key:   aac1f51a0d5641ddaa8dbb83dc1cf646:a5d5450a7cf4...(64 hex secret)

$ grovefs apikey --meta-dir <home> create --label reader --for mount --view alice --ro   # view-bound + read-only
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…            # opens view alice only; every write is refused server-side

# using a bound/read-only key where it doesn't belong is refused outright:
$ grovefs mount --view bob -m /mnt/b --meta-server ... --data-servers ... --api-key "$READER_KEY"
error: permission denied: this api key is bound to view 'alice', not 'bob'   # E_AUTH at M_OPEN
$ mkdir /mnt/a/should_fail                                                          # alice's read-only key
bash: /mnt/a/should_fail: Read-only file system                                     # kernel RO from the handshake flag; the server also refuses every write

$ grovefs apikey --meta-dir <home> create --label x --for both              # a typo never silently widens a privilege
error: unknown api key scope 'both' (expected any|mcp|mount)

$ grovefs meta-server --meta-dir <home> --listen 0.0.0.0:50110 --require-api-key
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op

# refusal reasons travel byte-for-byte with the handshake (not just an error code):
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ...                       # no key at all
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "$MCP_KEY"  # an agent key mounting the view
error: authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> calls --limit 6        # mount connections and governance calls, one table
last 6 call(s), newest first:
ts   key_id   method   tool   ok   detail
...  aac1f51a0d5641ddaa8dbb83dc1cf646  meta  M_OPEN   yes  view='alice' uid=1005
...  aac1f51a0d5641ddaa8dbb83dc1cf646  data  OP_OPEN  yes
...  -                                 meta  M_OPEN   no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646   # the row stays for historical attribution
apikey: revoked id='aac1f51a0d5641ddaa8dbb83dc1cf646' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# closing a live mount, measured (no operator action needed):
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: Permission denied                     # EACCES, carrying the server's reason
$ cat /mnt/a/grovefs-mount.log
  grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
  received termination signal, unmounting cleanly...
  unmounted view 'alice' at /mnt/a                          # afterwards /mnt/a is an empty dir, not a stale endpoint

# the governance gate's shape: an initialize without apiKey is answered, the next tools/list is refused
$ grovefs mcp --meta-dir <home> --require-api-key
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize answered (the gate holds only the state-touching methods)
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}
# with "apiKey":"<id>:<secret>" the session authenticates and the tool surface opens:
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)

$ grovefs apikey --meta-dir <home> list                            # id/label/scope/view/rw/created/last_used/revoked — never the secret
```

A client that cannot customize its handshake is covered by exporting `GROVEFS_MCP_API_KEY='<id>:<secret>'`, which pre-authenticates the whole governance server at launch (`GROVEFS_API_KEY` is the mount-side counterpart). Honest boundary: the key authenticates **the caller at the door** — it lets (or stops) a caller issuing governance writes, lets or stops it opening a view tree, and makes each admission attributable — without changing governance semantics; its `--view`/`--ro` dimensions are enforced server-side. Live-connection re-checking is **op-driven** (the server only re-verifies when traffic arrives), so the revocation boundary is “the first server round-trip after the revoke”: reads fully answered by the kernel cache need no round-trip and are therefore not interrupted (the next round-trip is). `--token` and `--require-api-key` can both be set — then both gates must pass. With `--require-api-key` off the server audits calls but gates nothing. E2E: the `apikey` suite, 33 assertions (create/list/hash-not-on-disk/refuse-without-key/refuse-wrong-secret/admit-with-valid-key/call-attribution/revoke-fails-closed-but-stays-listed/prefix-revoke/env-pre-auth/default-ungated-regression/set-ro in-place ro↔rw flip with authentication kept) and the `mountkey` suite, 76 assertions (scope minting + typo refusal/refusal reasons in the protocol/admission and env fallback (secret absent from argv)/connection audit down to key and view/scope isolation both ways/revocation closing a LIVE mount with no stale endpoint/other keys unaffected/token+key double gate/independent data-plane gate/failover carrying the credential/default ungated path unchanged/a view-bound key refused across views with its refusal audited/an `--ro` key's writes refused server-side/`set-ro` hot-flipping rw↔ro on a live mount with no reconnect); see USAGE §3.5.

## Importing existing data: zero-copy passthrough (`grovefs import`)

When the server already holds a pile of data under some directory and clients should get it through a view, there is no need to “stream it in” first: `grovefs import` registers an **existing local directory tree** into a view with the bytes staying exactly where they are —

```bash
$ tree .demo4/legacy
.demo4/legacy
|-- docs
|   `-- quarter.txt
`-- photos
    `-- 2026
        `-- y.jpg

3 directories, 2 files

$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home .demo4/home import --view legacy --source .demo4/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grovefs mount --view legacy -m .demo4/mnt &    # a client mount now sees the tree
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

Semantics (34 assertions in the `import` suite): **zero copies on one filesystem** — object and source share the inode (nothing shows up in `df`), with an honest streamed-copy fallback across devices; **live passthrough both ways** — an edit through the mount or an external rewrite of the source file shows up on the other side within the client’s ≤2s TTL; **GC only unlinks** — removing the name in the view deletes only the `blocks/` path, never the source; **idempotent and incremental** — re-running registers just the new names; mtime/atime/mode/owner preserved (`--dry-run` does not even create the view DB). Imported entries are then first-class: reads/writes, locks, snapshots and invalidation push all apply, and derived views/lenses can reshape them into virtual directories with `map`/`link` — that virtual shape then mounts **read-only** (writes go to the imported base view, visible through the shape via the double-broadcast pushes), and `grovefs import` refuses a derived target name outright, since it would write a shadow `<view>.db` no mount ever reads. Like `grovefs view`, run it on the machine holding the store (the `--home`, or the meta/data servers’ root).

## Production topology: central storage + LAN mounts

```bash
# central node
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

# each client (a remote mount needs no --home: nothing authoritative lives on the client; no local writes at all)
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

| Option | Effect |
| --- | --- |
| `--data-servers host:port` | File IO shipped over the network to `data-server` (no NFS) |
| `--meta-server host:port` | View served by the central `meta-server` → same user across machines shares one tree |
| `--data-servers a,b,c` | Comma-separated list enables deterministic cross-server striping |
| `--meta-server primary,standby` | Failover list; falls over automatically when the primary dies |

## Consistency

| Mode | Command | Behavior |
| --- | --- | --- |
| **Real-time invalidation push** (default, recommended) | just `--meta-server` | After a client's write succeeds, the center broadcasts invalidation (`inval_entry`+`inval_inode`) to that user's other subscribers; peers see it immediately, no polling |
| `--no-cache` strong-coherence fallback | add `--no-cache` | Disables kernel attr/dentry cache and reads straight through (`FOPEN_DIRECT_IO`). Trade-off: no client caching, more round-trips |

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
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111
# standby
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# primary with RPO=0 synchronous replication
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --sync-repl
# true fencing via a majority of independent arbiters (comma-separated list, odd N)
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --witness 10.0.0.7:50100,10.0.0.8:50100,10.0.0.9:50100
```

- The two nodes must use **separate `--meta-dir`s** (each keeps its own copy; replication ships whole snapshots).
- `--sync-repl` (opt-in): each write blocks until the standby durably installs the snapshot and ACKs, dropping **RPO from ~500ms to 0**; on ACK timeout it honestly degrades to async with a warn (never falsely reports 0).
- `--witness` (opt-in, **fail-safe fencing**): one or more `data-server`s reachable by both halves (a comma-separated list; an odd count `N` is recommended, and `N=1` is simply a single witness) act as independent arbiters. Authority needs a strict majority `quorum_size(N) = N/2 + 1` to grant: the meta side fans out to the whole list concurrently and applies majority logic (`witness_quorum`). Because two majorities always intersect, only the first claimer can raise a shared arbiter (the later sees `prev == epoch`, a renew, not an advance), so a same-value race yields exactly one primary. Each arbiter keeps a monotonic fence token; `OP_FENCE` folds your epoch via `fetch_max` and returns the **value held before the fold (`prev`)**. A claim wins iff `prev < your epoch`; `prev >= your epoch` means you are fenced — returning `prev` (not the post-fold max) is exactly what resolves same-value collisions so a recovering arbiter never ends up with two primaries. A live primary renews a bounded lease at a majority every tick (self-demotes if a majority reports `prev > mine`); each arbiter refuses overlapping leases — at most one live lease at a time — so a majority live lease is itself proof of exclusive authority, and a write is admitted from a local lease check (one fan-out per renewal, not per write). A would-be promoter may seize a lease only after the incumbent's expires (it refuses if `prev >= candidate`, a tie or peer-ahead, or the incumbent lease is still live). **Fail-safe**: if a configured witness set cannot form a lease quorum beyond a 3s grace, a node gives up write authority (primary self-demotes, standby refuses to promote → writes freeze) rather than risk split-brain. **Availability gain over a lone witness**: losing a *minority* of arbiters neither freezes writes (a quorum is still reachable) nor resurrects a stale primary (a quorum still holds the monotonic token even if a minority of `.fence_token` stores are lost); only losing a *majority* freezes. Each arbiter persists its token at `<root>/.fence_token`, so a restart does not zero it. Beyond the meta-plane proof, the DATA plane is fenced too: the client stamps its live epoch onto every write-intent content frame (create/write/truncate/remove/fsync) and the witness data-server refuses one whose epoch lags its token (`EROFS`), since a `write()` otherwise lands bytes on the backend before the debounced meta touch — a fenced frame re-pulls the epoch from the authority and retries once. The data-server furthermore linearizes the epoch check and the backend write against a promotion (a claim takes an exclusive lock that drains in-flight writes, and the witness co-locates with storage), so a stale write can never land concurrently with the new primary's — content torn-writes are closed on the arbiter, not just bounded (this content gate is effective only on data-servers inside the `--witness` list, so deploy the witness co-located with the storage clients actually write to; a content store outside the set keeps a zero token, its data-plane gate stays inert, and only the meta-plane fence protects it).

## Auth / read-only / performance

**Token auth** covers both the data plane (`OP_OPEN`) and the metadata plane (`M_OPEN`); a mismatch fails at connect with `PermissionDenied`. Empty token (default) = LAN-trusted, no auth — trusted networks only.

**A revocable per-caller credential** guards the same two doors, but audits and kicks one caller at a time: a token is one shared secret — every host holding it can mount, nobody sees *who* mounted, and ejecting a caller means rotating the token and breaking everyone. Mint an API key instead (`grovefs apikey create --for mount`, opens only the mount/data doors):

```bash
$Y apikey --meta-dir /data/cluster create --label client-a --for mount    # plaintext prints once; only SHA-256 is stored
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
$Y mount --view alice --data-servers ... --meta-server ... --api-key '<id>:<secret>' -m /mnt/alice   # or export GROVEFS_API_KEY to keep it out of argv
$Y apikey --meta-dir /data/cluster calls --limit 20                       # every connection (refusals too) attributed to its key
$Y apikey --meta-dir /data/cluster set-ro --id <id> --rw                    # flip an --ro key rw↔ro in place; a live mount follows within ≤1s, no reconnect
$Y apikey --meta-dir /data/cluster revoke --id <id>                       # closes that key's LIVE mounts (seconds)
```

The two substantive differences from a token: ① refusal reasons travel byte-for-byte back with the handshake (missing key / unknown id / wrong secret / wrong scope / revoked each print their own text), and the server **re-checks live connections once a second** — after a revoke that mount is interrupted on its first server round-trip, prints the server's own wording and unmounts itself cleanly (no `Transport endpoint is not connected` left behind), while mounts on other keys keep running; ② mount connections and governance calls share one audit table. `--token` and `--require-api-key` can both be set (then both gates must pass); with the flag off behaviour is exactly as ungated (auditing still happens). See USAGE §3.5.1/§6.

**Read-only**: `mount ... --ro` → every mutating op returns `EROFS` (kernel `MS_RDONLY` + filesystem-side guard).

**Performance knobs**: `--threads N` (default 4, per-thread `/dev/fuse` fd), chunked parallel IO (reads ≥1MiB, writes ≥4MiB), large-file striping (≥16MiB), `touch_mtime` debouncing (200ms window), a 4-connection read pool per view engine on both backends (`READ_POOL_SIZE` / `PG_READ_POOL_SIZE`) that unblocks concurrent metadata reads while writes stay serialized, SQL-paged directory listings (`readdir_page`, `LIMIT/OFFSET` filled into the reply buffer in 32→1024 geometric chunks), and `FUSE_READDIRPLUS` negotiation (on by default wherever the provider pages in its own store — SQLite/PG local, and remote against a capable `meta-server` via `M_READDIRP`; `--no-readdirplus` to fall back to plain READDIR + per-entry LOOKUP).

## Deployment & operations (hands-on)

The full step-by-step guide lives in **[USAGE.md](USAGE.md) §11–§14** (mirrored in [docs.html](docs.html), re-sequenced into a four-tier zero-to-expert manual with new beginner-prerequisites, internals & extension-development, and performance-tuning methodology sections, plus an auto/light/dark reading-theme toggle): §11 walks through environment preflight → build & install → topology planning → shared token → systemd units (two central services + a templated client mount unit) → firewall → an 8-step acceptance checklist → HA / PostgreSQL variants; §12 covers routine inspection (log-keyword → action table), backup/restore (single-node / SQLite+HA / PG), rolling upgrades, a troubleshooting table and post-change regression; §13 is a **fixed-IP worked example** (storage server 192.168.1.10 with data in /data on the SQLite backend, one read-write client at /mnt/da and one read-only client at /mnt/db) explaining every flag hands-on; §14 answers “custom data directories, bulk import of existing data (zero-copy `grovefs import` first, streamed rsync through a mount as the alternative; hardlink aliasing, view splitting, and an explicit not-implemented list), and per-client display layouts”. The shortest production path in six steps:

```bash
# 1) build & install the same binary on every node
cargo build --release && sudo install -m755 target/release/grovefs /usr/local/bin/grovefs
# 2) center: shared token + the two services (use systemd in production; units in USAGE §11.4)
TOK=$(openssl rand -hex 16)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token $TOK &
grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token $TOK &
# 3) open 50100/50110 to the internal subnet only
# 4) create the view explicitly (a mount never provisions), then mount it
grovefs view --meta-dir /data/cluster create --view alice
grovefs mount --view alice --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token $TOK -m /mnt/alice
# 5) accept: A-write/B-read instantly visible across two clients + cross-host flock + findmnt shows grovefs:alice
# 6) operate: watch journalctl -u grove-meta-server for degrade/freeze keywords; a cold copy of blocks/ + meta/*.db is a complete backup
```

## Testing

```bash
export LC_ALL=C LANG=C            # no PKG_CONFIG needed: no crate queries pkg-config (see Build)
cargo test --release
bash .build-bin/run_all.sh                 # 34 stages (33 gated + bench: build/unit + 31 suites)
SKIP_BENCH=1 bash .build-bin/run_all.sh    # skip throughput bench
```

## Honest limitations

These are explicit, honest boundaries, not hidden assumptions:

- **Transport**: custom TCP frames today, not gRPC. gRPC (tonic/prost) was gate-evaluated as **GO** but full integration is **deliberately deferred**; the current protocol is correct and tested.
- **HA RPO**: ~500ms async by default (0 with `--sync-repl`).
- **Fencing**: `--witness` is implemented and **fail-safe**: a `data-server` list (comma-separated, odd `N`) arbitrates by **majority** (`quorum_size = N/2+1`); a primary holds a non-overlapping lease at a majority (renewed every tick), so a majority live lease is itself proof of exclusive authority and a meta write is admitted from a local lease check (one fan-out per renewal, not per write) AND gates content frames by token at the witness data-servers, bounding the double-write window to the lease safety margin (converging to one primary). The irreducible boundary: a strict two-node pair **without arbiters** cannot prevent split-brain and leaves the window unbounded (CAP/consensus). The **availability gain of a quorum**: with `--witness`, losing a *minority* of arbiters neither freezes writes nor resurrects a stale primary (a quorum still holds the monotonic token); only losing a *majority* fails safe and freezes (`N=1` reverts to a lone witness, which becomes a single availability dependency).
- **Striping**: no throughput gain on a single saturated link (loopback/one NIC); the value is aggregate capacity across nodes — real multi-machine links must be re-benchmarked before claiming gains.
- **Presentation overlay (`--layout`)**: it is a **display customization, not a security boundary** — mounting the same view without the lens still shows the full tree; use separate views + token/API key for hard isolation. Creating/renaming into a purely synthesized dir gives `EPERM`, a name colliding with a lens-shown entry gives `EEXIST`, and `only` mode refuses writes outside the whitelist; symlink target text is not rewritten; with a shared `meta-server`, invalidations for entries under synthesized dirs may miss lens coordinates (use `--no-cache`); rules relocate entries within one view only — cross-view references are not supported.
- **Server-side derived views (`grovefs view`)**: mounts of a derived name are **read-only by hard contract** — the shape is centrally governed, distributed but never modified; every write belongs to the base view. Enforcement is three-layered (kernel RO negotiated at `M_OPEN` → `EROFS` at the client; an authoritative per-op gate on the meta-server answering `E_NOTPERM`; the same check at local `--home` mount time). If you need a *writable* custom shape, use the client-side `mount --layout` lens (§ Per-client presentation overlay), which writes through to the real entries. Otherwise still **not a security boundary** — mounting the base name directly still shows the full tree (token/API key gate access, not shapes); definition **edits hot-apply** (the server checks the file's mtime per op; live mounts switch shape within their ≤2s cache TTL, no remount), while `view rm` does not hot-apply (established mounts keep their shape until remount); during a shape swap, paths/fds on old virtual inodes may resolve to ENOENT — real entries keep stable inodes; no nesting (the base must be a physical view), no `name == base`, no shadowing a physical db; `meta/views/` (SQLite backend) is control-plane config that an HA standby must sync by hand (under PG the control plane sits in `grovefs_control` and replicates with the trees); hot reload is `meta-server`-only — a local (`--home`) mount still resolves its definition once at mount time.
- **Server-side pure aliases (`grovefs view alias`)**: the mirror image of derived views — an alias mount shows the full tree and is WRITABLE, while a governed real name is NOT mountable by its own name (refused locally and by the `meta-server`). Resolution is single-hop, re-scanned per `M_OPEN` (mounts are rare); alias chains, derived targets and one-real-name-with-two-public-aliases are rejected at the command; `import` refuses alias targets; removing an alias affects only NEW mounts (established connections keep flowing, like `view rm`); `--retire` rotates a leaked public name in one gapless step (write-before-remove so the real name is never momentarily mountable; the retired name resolves to a fresh empty namespace that never reaches the real tree) — but rotation, like `rm`, only affects NEW mounts and cannot revoke an existing token-holder, so it is leak hygiene, not a security boundary. One honest wording note: refusals travel **in the protocol** — the `M_OPEN` error frame carries the server's reason text, so a remote alias refusal prints exactly the local wording above (ditto token mismatch, missing/invalid/revoked/wrong-scope api key; against an OLD server the payload is empty and the client falls back to its generic text). And the honest boundary: name hiding is governance, not security — the public name must be distributed anyway; token + api-key admission remain the hard gates.
- **Zero-copy import (`grovefs import`)**: hard links require source and `blocks/` to share a filesystem (cross-device falls back to an independent copy, reported honestly); if an external tool later updates the source file via **rename/replace-write** (the default of vim, rsync without `--inplace`, …) the imported inode keeps the bytes from the last passthrough write — re-run `import` for new content (existing names are skipped; use a different `--prefix` to force a re-registration); symlinks/special files are deliberately skipped (not representable); imported objects are single files and do not participate in the ≥16MiB cross-server striping (striping remains a property of fresh writes through a mount); in a `meta-server` deployment import writes the authoritative DB file directly, so run it on the storage host and expect live mounts to converge by TTL (≤2s) rather than push; the PostgreSQL backend is not supported (its authoritative tree lives in Postgres — stream data in through a mounted path); keep `--source` on the store’s filesystem and **around** — deleting the source only drops that alias name (data survives in `blocks/`), but after external renames the `blocks/` link is the only remaining path to the bytes.
- **Scope of the metadata read pool and directory paging**: both live in **two storage engines** — SQLite `MetaEngine` and PostgreSQL `PgEngine` (each a 4-connection read pool + SQL `LIMIT/OFFSET` paging). Local mounts (`--home` SQLite or PG) get paging and READDIRPLUS directly; `meta-server` handlers use their backend's read pool, and the `M_READDIRP` frame carries `offset/limit` so a remote client fetches one page per kernel buffer and negotiates `FUSE_READDIRPLUS` — the same 5001-entry directory over `meta-server`+`data-server` measures **2.29s** (4.22s paging-only, listings byte-identical). What still falls through to one full listing per buffer with no READDIRPLUS: **derived/lens views** (`LayoutMeta` — its hidden/mapped shape can't be one SQL page) and mounts against a **server too old to advertise the paging cap** (a short `M_OPEN` ack with no second byte). Metadata *write* concurrency (multi-writer) is not supported **on the SQLite backend** (single writer connection); the PostgreSQL backend's opt-in SSI write pool (see “Common questions”) is the exception. The gate is `MetaProvider::supports_paged_readdir()`, pinned by the `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` test.
- **Verification environment**: everything validated on 127.0.0.1 (simulated LAN); no real multi-machine or fio-level benchmark yet.

Where numbers are not measured, no benefit is claimed; wherever there is a limitation, it is stated in code comments and here.

## License

MIT.
