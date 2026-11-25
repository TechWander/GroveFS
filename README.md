# GroveFS

> **用户视图 FUSE 文件系统** —— 数据集中存放于硬盘，不同用户挂载各自看到**独立的文件树**（支持自定义层级），彻底脱离 NFS。

[English](README.en.md)

---

## 是什么

`GroveFS` 把「谁能看到什么（视图）」与「数据实际存在哪（对象）」彻底分开：

- **视图 view**：每个用户一棵独立目录树，存为一个 SQLite 库 `<home>/meta/<view>/<view>.db`。不同用户互不可见。
- **数据对象 object**：文件的真实字节，存在 `blocks/` 下的对象文件里、以 UUID 命名；对象按 key 的哈希自动扇出到 `blocks/` 内 **256 个桶目录**（单目录不随总量增长，千万级对象下每台目录只有数万条目，备份/遍历工具不退化）；视图目录项只持有对象 key（指针）。
- **基目录 `--home`**：同时容纳 `blocks/`（数据对象）与 `meta/<view>/<view>.db`（每视图一棵树）的物理根，是**节点侧动词**（`view create`、`gc`、`import`、`apikey`、`du`）与两个服务自己的存储根，取值顺序 `--home` > `$GROVEFS_HOME` > `~/.grovefs`。**挂载从不读写它**：一次挂载必须同时解析出元数据服务与数据服务，客户机磁盘上不落任何权威数据——连这两份数据的缓存副本也不落。中心 `meta-server` 的权威元数据根用 `--meta-dir` 指定。

在此之上，`GroveFS` 可把元数据与数据分别服务化（`meta-server` / `data-server`），让**多台客户机经局域网挂载、共享同一份权威视图**——这才是「数据中心存储 + 机房内 LAN 访问」的落地形态，且**不使用 NFS**。

## 核心特性

- **多用户视图隔离**：一用户一棵树，跨用户不可见；完整的 `mkdir`/`mknod`/`rename`/`rmdir` 自定义层级。
- **可插拔存储/元数据后端**（trait 抽象）：本地 `LocalBackend`/`MetaEngine`、远程 `RemoteBackend`/`RemoteMeta`、跨多节点条带 `ShardedBackend`，对上层透明。元数据权威存储亦可切换为 **PostgreSQL**（`meta-server --meta-store postgres://…`）：服务端 schema-per-view、事务化 rename/unlink/lock；此模式下自研 HA 有意关闭，交给 PG 自己的流复制/PITR。
- **视图绑定 / 只读的 API key（推荐访问控制）**：`grovefs apikey create --for mount --view V [--ro]` 铸出的凭据自带「**能开哪棵树、读写还是只读**」两个维度——绑定 `--view V` 的 key 只能 `M_OPEN` 视图 V（开别的视图在握手处即 `E_AUTH` 拒绝），`--ro` 的 key 被服务端**逐写意图 op 拒 `E_NOTPERM`**（meta 面）与 `EROFS`（data 面内容写），并在 `M_OPEN` 回包里提示客户端只读挂载，故「只读」不依赖客户端自觉 `-o ro`。只读↔读写还可 `apikey set-ro --id X [--rw|--ro]` **原地互切**——服务端在同一 ≤1s 复验 tick 上重读凭据行当前 `ro` 值，活挂载免重铸、免重连即按新值放行/拒写。这两维都在服务端硬强制、与客户端自报 uid 无关（key 才是真正的门）。系统级 key（省略 `--view`）仍可开任意视图。
- **跨视图共享对象的删除安全（引用账本 `refs`）**：一个 `data_ref` 命名**唯一**一个底层对象，却可能被多个物理视图同时挂着名字（`grovefs import` 同一源文件进两个视图解析成同一 key、派生视图/别名共享基视图字节）。视图内的 `node.data_ref` 引用计数看不见别的视图，单看它会误删他视图仍在引用的对象。故记账落在 `<base>/meta/_control/refs.db`（PG 后端落共享表 `grovefs_refs.holders`）：每个视图 ATTACH 该账本，把 holder 计数与节点增删纳入**同一个 IMMEDIATE 事务**，跨文件原子提交。物理删除的准确规则是「**该视图内此 key 的最后一个名字被摘除、且全局账本再无其它视图持有**」——只有此时才 `backend.remove`。视图 db 被 out-of-band 删除（`rm meta/x/x.db`、新 `--home`）只会让账本**高估**、对象多留不误删；`grovefs refs --meta-dir D rebuild` 按现存视图树重算 holder 归位。**只有基视图（`grovefs view create` 显式创建的物理树）的删除才真正回收底层字节**，派生视图/别名挂载只读、其删除从不物理 GC。删基视图需显式声明 `grovefs view rm --name V --base`（删 `meta/V/V.db`+WAL/SHM、`refs rebuild` 重建账本、回收到零其他视图持有的字节；PG 部署与活挂载拒绝，字节走 `--data-dir`）。账本里没有行的键即该视图私有（空账本不需要任何初始化步骤）。账本归位后仍残留、或落在了 `blocks/` 却没有任何 `node` 行命名（外部拷入、半份恢复）的无人命名页，用离线 `grovefs gc --meta-dir D [--data-dir R] [--apply]` 审计/回收：默认只扫描，`--apply` 只删超过 `--older-than`（默认 24h）的页、永不删账本仍持有的键，并要求本存储无活挂载、本机无 grovefs 进程（PG 部署直接拒绝）。
- **每客户端显示定制（`mount --layout rules.txt`）**：同一棵权威树、同一份字节，不同挂载点各自套一层「显示透镜」——`hide` 隐藏子树、`only` 白名单、`map` 把条目重排到别的路径（源位让位）、`link` 建两处同时可见的别名。透镜只做 `MetaProvider` 层的路径翻译：真实条目保持真实 inode，数据 IO/锁/GC/失效推送全部直通，不复制目录项、不影响其它挂载的视图。
- **服务端派生视图（`grovefs view define` + `mount --view <派生名>`）**：想把形状**集中管理在服务端**——`grovefs view define --name docsonly --base team --rules F` 把规则落进 `<meta-dir>/meta/views/<name>.layout`（首行 `base <物理视图>`；PG 后端下同一条命令 `--meta-store postgres://…` 写进 `grovefs_control.view_defs` 表），客户端只 `mount --view docsonly`，**无需任何本地规则文件**。**虚拟视图一律只读挂载**：形状集中治理、只分发不修改，写入只属于原（物理）视图——客户端据 `M_OPEN` 握手标志直接落内核只读（写 `EROFS`），`meta-server` 另对派生名的写意图 op 设权威门禁（绕开挂载直接说话也过不了这道门）。同名派生视图由服务端单例渲染（虚拟 inode 跨机一致），**改定义热生效**（`meta-server` 逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂）；挂载一侧只在 `M_OPEN` 解析一次定义，换形由服务端逐 op 的检查推给它。
- **服务端纯别名（`grovefs view alias`）**：治理「叫什么」而非「长什么样」——公开名 1:1 映射物理视图（定义仅一行 `alias <真实视图>`），别名挂载**全树可写**，而真实名从此**不是挂载入口**（本地与 `meta-server` 都拒挂被治理的真实名）：对外名字可自由轮换、存储身份不外露，猜出真实名也挂不进来；公开名泄露时 `--retire` 一条命令原子轮换（旧名失效、真实名全程拒挂）。与派生视图同放 `meta/views/`、同一 `list/rm`；视图绑定 key 记在真实名，对别名与基名同时生效；拒绝别名链与派生目标；`import` 拒绝别名目标（别名不是存储身份）。
- **服务端 fork 视图（`grovefs view fork`）**：基视图与派生透镜之间的「可组织、不可上传」中间态——`grovefs view fork --name lab --base team` 把基视图的目录项**复制**成一份独立元数据树（`meta/lab/lab.db`，保留 inode/硬链接），内容字节不复制、经跨视图账本共享；定义仅一行 `fork <基视图>`。fork 挂载后 **结构可写**（`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod` 全放行）、**内容写一律 `EROFS`**（`write`/`truncate`；上传字节属于基视图），删除只摘 fork 自己的元数据、基视图分毫不动，共享对象只在**最后一个持有视图**摘名后才回收。内容只读这件事由 `M_OPEN` 回包的第 3 个标志字节告知客户端，挂载据此在 FUSE 层拒内容写；同名定义槽与派生/别名共用 `view list`/`view rm`，删 fork 会一并删掉它自己的树并重建账本。基必须是**一份物理视图**（有定义的名字不能当基）——两档后端同一语义：SQLite 复制一棵 `.db` 的行；PG 在一个事务里 `INSERT … SELECT` 跨 schema 复制（`forksrv` 套件逐档实证）。`alias` 拒绝指向 fork（fork 是树身份，不是公开名——给它发 key 用 `apikey create --view lab`）。fork 支持**策展**：`--empty` 只建根、不复制任何条目，再用 `view graft` 把别处视图里选中的条目接进来——计划一行一条（`<src>` 原样落 `--dest`，或 `<src>TAB<dst>` 同时改名/换目录），`--map -` 直接读 stdin 便于外部程序生成；整份计划**一个事务**（要么全落、要么全不落）、字节零复制（条目进 fork，对象仍经跨视图账本共享），同名默认拒绝、`--skip-existing` 改为跳过。
- **存量数据零拷贝导入（`grovefs import`）**：服务端已有一批目录数据（如 `/data/legacy`）时，`grovefs import --view V --source DIR` 把整棵树登记进视图——同文件系统下对象就是源文件的**硬链接**（零额外字节，两个路径读写同一 inode、双向实时可见），跨设备自动退化为流式拷贝；目录层级/时间戳/权限/属主保真，重跑即增量补新文件，删视图里的名字只摘 `blocks/` 链接**绝不伤源数据**。可与派生视图/透镜组合出虚拟目录（`map` 导入路径到任意形状；**派生虚拟视图挂载只读**，写走原视图）。
- **自定义二进制帧 over TCP**（非 NFS/gRPC）：定长头 + 变长体，连接池并发。
- **跨客户端实时失效推送**（默认）：一端写成功，中心经事件总线向同用户其它订阅者广播 `inval_entry`+`inval_inode`，对端下次访问**即时可见**，无需轮询；`--no-cache` 提供零缓存兜底。
- **分布式 advisory 锁**：`flock` / `fcntl F_SETLK/F_GETLK` 经中心 `locks` 表集中仲裁，跨进程、跨机生效；崩溃即时回收（FUSE `release`）+ 短 lease（默认 15s）+ 续约心跳。
- **高可用主备**：`meta-server --role primary|standby --peer` 心跳/epoch 提升/自动降级；`--sync-repl` 把写 RPO 降到 0；`--witness` 取逗号分隔的 N 个共享 `data-server` 作**独立仲裁者集合**（推荐奇数），**fail-safe、持不重叠的有界多数派租约（需 `N/2+1` 授予；多数活租约即独占证明，写由本地租约校验放行）并对内容数据帧按 token 门控，把双写窗口收敛到租约安全余量内**；宕少数仲裁者仍不冻结写（N=1 即单仲裁者）。
- **挂载引导（`grovefs manager`，唯一入口）**：把「每台客户机抄两个坐标」换成「服务端声明一次」——`manager define --meta … --data …` 落一个 deployment 的**权威拓扑**，`meta-server`/`data-server --register-with` 上报自己活着，客户机 `mount --manager H:P --roster-key-file F` 拿回**签名 roster**、解析出那对坐标后再与两个服务连接。**`mount` 不再接受手敲的 `--meta-server`/`--data-servers`（已从 mount 整体移除）**：故障转移与条带语义原样保留，只是这两个值现在**只能**来自 roster 解析，手动抄坐标的路子彻底关闭——这是**替代**而非叠加。它是引导服务、**绝不在 IO 路径上**：manager 重启或宕机不动任何已有挂载，只让**新**挂载 fail-closed。防伪分两层——注册要 join key 的 HMAC，且**只对 operator 声明过的地址生效**（未声明地址即使 MAC 正确也拒收，泄露 join key 插不进攻击者的机器）；应答由 roster key 签名，客户端验不过就拒挂而不是警告。roster 永远是**声明集**而不是健康子集（条带成员不是候选池）。见 USAGE §4.2 与 `docs/adr/0001-manager.md`。
- **Token 双面鉴权**（集群共享口令）、**可撤销的 per-caller API key 门**（`meta-server`/`data-server --require-api-key` + `mount --api-key`，key 可 `--view` 绑定单一视图、`--ro` 服务端强制只读）、**数据面准入绑定 Data-Access Capability**（`meta-server`/`data-server --data-access-key` 共享 HMAC 密钥：真 meta 在 `M_OPEN` 签发能力、`data-server` 在 `OP_OPEN` 校验，挡住 rogue meta-server 复用合法 data-server；非 per-view ACL，opt-in 默认关，USAGE §6.1.1）、**只读挂载**（`--ro`）、多线程 `fuser 0.18`（`clone_fd` 并发派发）、**分块并行 IO** 与 **跨 server 条带**。
- **显式视图名（唯一规则）**：视图只能由 `view create` / `view import` / `import --view` 显式建出来（SQLite `meta/<view>/<view>.db`，PG `grovefs_v_<hex>` schema）；`M_OPEN` 一律先探测存在性，未知名字**直接拒绝**并给出可读原因（客户端直接打印，服务端日志同一句），**不落盘**，开 `--require-api-key` 时还进 `apikey calls` 审计。派生/别名/fork 等治理定义照常可挂；拼错名字不会长出一棵空树（USAGE §4）。
- **智能体 MCP 接口（`grovefs mcp`）**：把上述视图治理（派生视图/纯别名/fork 的 `define/alias/fork/graft/list/rm`）以 MCP（Model Context Protocol）暴露给 AI 智能体：规则以**内联文本**经 `tools/call` 提交，智能体无需碰任何文件即可自动定制数据列表的形状（含 `list_view_tree` 只读探查真实目录清单 / 不落盘预览未发布规则的形状、`get_view` 读回单个定义、`validate_definition` 发布前把整副定义走完整套治理守卫而不写盘）；同一套治理再以 2025-06-18 协议的其余常用方法面二次暴露——资源（`resources/list`/`resources/templates/list`/`resources/read`，把视图作为可附着上下文）、提示（`prompts/list`/`prompts/get`，参数化治理剧本）、补全（`completion/complete`，用真实视图名前缀补全）、日志（`logging/setLevel` → 服务端 `notifications/message`），且复用完全相同的渲染器（故资源/提示/补全与工具结果逐字节一致）；与 CLI 共用同一套校验与错误文案（一个契约、两个入口），控制面 only——不挂载、不触数据；`grovefs mcp --require-api-key` 可选地为接口上 API key 门，同一份 `grovefs apikey` 注册表也可经 `meta-server`/`data-server --require-api-key` 门住挂载与数据入口（key 带 scope：`--for mcp|mount|any|admin`；`admin` scope 经 `meta-server --control-listen` 顶替数据库密码，让任意机器用账号密钥治理同一份元数据，见 USAGE §3.5.2），并记录每次调用/连接的审计日志；撤销可逐调用方生效并封停**已建立**的挂载（详见 USAGE §3.5.1/§6）。详见「智能体定制数据列表」一节与 `USAGE.md` §3.5。
- **网页管理台（`grovefs web`）**：远程治理面（`meta-server --control-listen`，上一条）之上的一个**独立子命令** HTTP 网关：内嵌单页应用（总览 / 视图 / 密钥 / 账号与授权 / 审计五页，构建期 `include_str!` 进二进制——页面运行时不碰 CDN、不带 node_modules，离线主机也打得开；前端源码在 `console/`（Vue 3 + TypeScript + Vite），产物提交在 `src/webui/`，故 `cargo build` 仍不需要 node 工具链，改前端才跑 `npm ci && npm run build`，见 `console/README.md`），把浏览器会话变成一个**账号**、把点击转成一次治理 op 转发。转发走的是**常驻控制连接**：一条 socket 钉住 admin 凭据，每 op 省下 TCP 建连、服务端线程与认证那两次查询（实测同机两臂交错对比：串行 p50 −19…−43%、服务端 CPU/op −10…−50%，P=16/32 吞吐 +18%/+25% 且 P=32 p50 83ms→26ms，P≤8 的差值压在轮间散布里），而**钉住的是凭据不是身份**：每条 op 仍照自带账号归属与判定、审计一行不少（`GROVEFS_GATEWAY_CTL_PIN=0` 可退回一请求一连接）。账号可创建、可被划**基视图**：`web grant add --account bob --base main` 之后，bob 对这棵基树就是完整管理权——它的 `fork`/`alias`/`lens` 定义与派生树、以及绑在这些名字上的密钥，全部自动落在授权范围内（一个 `lens`/`alias`/`fork` 名在服务端沿定义链 ≤4 跳归一到它的基视图），不必逐名追加。**“谁能碰哪棵树”的判定仍然只在服务端一处**：网关只做翻译与**减法**（写死的路由白名单，`gc.scan`/`gc.apply`/`refs.rebuild`/`view.rm_base`/`fork.graft` 不过路由），账号、授权与口令摘要（PBKDF2-HMAC-SHA256 21 万轮）都存在**服务端**（SQLite 档 `<meta-dir>/meta/_control/web_accounts.db`，PG 档是控制库里的表），登录校验由持有存储的进程做——网关拿不到密码库。第一任管理员只能由 `meta-server --admin-user/--admin-password-file` 在账号表**为空**时放入（治理入口若能自铸管理员，这道门就白设了）。详见「网页管理台」一节与 `USAGE.md` §3.5.3。
- **元数据读并行 + 目录列举分页 + `FUSE_READDIRPLUS`**：每个视图的 `MetaEngine` 在单条写连接之外持一池暖读连接（**默认暖 1 条、按需保留至多 8 条**：`READ_POOL_SIZE=1` + `READ_POOL_RETAIN=8`，env `GROVEFS_SQLITE_READ_POOL` 抬高暖连接地板、保留天花板取两者较大、`0` 完全关池），`lookup`/`getattr`/`readdir` 及 nlink 解析都走这条只读池，元数据**读**不必排队等写锁（SQLite 的**写仍串行在同一权威连接上**，单写者语义不动；PG 后端写另走 SERIALIZABLE 写池并行，见下文 PG 诚实边界）。挂载进程里没有本地引擎，这条池因此属于服务端的每个视图引擎（以及把库嵌进自己进程的调用方），`meta-server` 的并发 RPC 处理器各走自己后端的池、互不等待。目录列举走 `readdir_page(parent, offset, limit)`，SQLite 引擎把它下推成 SQL `LIMIT/OFFSET`，FUSE 回调按几何块（READDIRPLUS 32→1024、普通 READDIR 128→1024）流式填回复缓冲，一个内核请求付 `O(缓冲)` 而非 `O(整棵目录)`；并默认协商 `FUSE_DO_READDIRPLUS`（内核 ≥4.6），目录项连同属性一次带回，`ls -l` 不必逐条 `LOOKUP`+`GETATTR`。客户端一侧由 `M_READDIRP` 帧把 `offset/limit` 下推到服务端，每个内核缓冲只取一页。实测（本机回环上的 `meta-server`+`data-server`，同一挂载点、每趟新挂载、内核 dentry 缓存为空）：5001 条目录 `ls -l` **0.270–0.274s**（`--no-readdirplus` 只留分页为 **0.575–0.629s**，两份输出排序后逐字节相同）；10,002 条目录 `ls -f` **0.116s**、`ls -l` **0.547s**；100,002 条目录 `ls -f` **0.925s**。分页与 PLUS 在每一类 provider 上都生效：SQLite/PG/treedb 引擎各自把页下推进存储，派生/透镜视图（`LayoutMeta`）覆写 `readdir_page`（按父目录物化形状后切片；物化即缓存，**必须由失效推送 flush**）因此同样吃到两者——同一 100,002 条目录经 100 条规则的透镜 `ls -f` **1.612s**、TTL 内重读 **0.429s**。`--no-readdirplus` 保留作 A/B 排障开关，复跑脚本 `.build-bin/probe_readdir_cluster.sh`。
- **分阶段延迟遥测（`--stats`，opt-in、默认关闭零成本）**：给一次读/写/元数据 op 内部各阶段（端到端 `op_total`、key 解析、后端 IO、分片扇出、元数据 DB、远程 RPC）打时间戳，用 hdrhistogram 汇成 p50/p90/p99/max 离线定位瓶颈。**默认关闭时每个埋点仅一次 relaxed 原子读即短路**（连 `Instant::now()` 都不调用），读写热路径不受影响；仅显式开启才采样，直方图按 worker 线程分片不串行化到全局锁。开关 `--stats` / `--stats-file <路径>`（周期覆盖写）/ `--stats-interval <秒>` / `GROVEFS_STATS=1`；退出时经 `stderr` 打印报告，只读控制节点 `<mnt>/.grove-stats` 仅在开启时出现（保留 inode、不落盘，治理不变量不受影响）。是排障工具而非吞吐测量，详见 `USAGE.md` §8.1。
- **挂载级内容带宽整形（`--bw-read` / `--bw-write`）**：给一次挂载的内容读、写各上一道**客户端 GCRA 令牌槽**（按字节铺开互不重叠的时间槽，长跑速率精确等于上限），按实际交付字节计费——读在回复前、写在落盘后，超额的 op 只睡到自己的时间槽，慢的是速度而不是丢数据；写侧延迟天然形成背压。`0`/缺省 = 不限，热路径零成本（一次 `rate == 0` 分支）；`--stats` 的 `bytes read`/`bytes written` 计数可直接核对实际吞吐。**诚实边界**：这是自限速而非隔离——绕过 FUSE 的原始协议客户端、或同一视图的多个挂载各自拿满额度；服务端也无法把内容寻址的字节归到某棵视图上。回归 `bwlimit` 套件钉住速率下限、env 回退、`0`=不限与非法速率拒绝。
- **完整回归**：`run_all.sh` **全板 43 段（42 门控 + bench）GREEN**（`layout` 显示透镜与服务端派生视图 e2e、`fork` 独立元数据树 fork 视图 e2e、`viewrm` 删整个基视图与零持有字节回收 e2e（四重守卫/最后命名者死才释放/分家根 `--data-dir`）、`gc` 离线孤儿对象审计与回收 e2e（账本持有不删/grace 年龄门/本库活挂载拒 apply 而他库同名视图不拦/零交集数据根防呆）、`bwlimit` 每挂载内容读/写带宽整形 e2e（GCRA 令牌槽 24MiB@8MiB/s 读写下限断言/超额睡眠不丢字节/env `GROVEFS_BW_READ` 回退/`0`=不限/非法速率拒绝/只读挂载下 `--bw-write` 空转）、`migrate` 视图元数据导出/导入与跨机器迁移 e2e、`import` 零拷贝导入 e2e、`mcp` 智能体治理接口 e2e、`apikey` MCP 接口 API key 认证与调用审计 e2e、`mountkey` 可撤销挂载凭据（scope 分区/拒因入协议/撤销封停活挂载/视图绑定与只读 key/`set-ro` 就地翻转写裁决热生效）e2e、`manager` 挂载引导 e2e（声明与签名 roster、未声明地址拒收、杀 manager 后活挂载不受扰、声明集收缩时分条读回 `EIO` 而非空）、`config` 配置文件与 env 回退 e2e、`pg` PostgreSQL 后端 e2e、`ctlplane` 远程治理面 e2e（`meta-server --control-listen` + 账号密钥跨机治理：首把 admin key 必须在持盘主机铸造/远程视图与密钥全套且管理机无落盘/`any`·`mount` 与垃圾 key 被拒/远程撤销即失效/本地 flag 混用拒绝/远程删树后守卫仍生效/审计保留期：`apikey purge` 只数不删与实删报数、`--audit-retention` 的两类拒绝与 `never` 被放行为显式全保留、默认 7d 与 `store.info` 上报）、`web` 网页管理台 e2e（安装期 `--admin-user`+`--admin-password-file` 只在空表播种第一任管理员·同 flag 重启幂等·换名重启拒启动/网关启动前四道拒绝/CSRF·Origin·op 白名单与五个仍留在 CLI 的动词/被授权账号**走页面路径**管得住自己的基视图与 family、只给自己的视图铸 key、越界被服务端原句拒/撤权下一个调用即失效/审计以 `account@ip` 到人/登录限速按客户端 IP 且第 6 次即使口令对也 429、常驻控制连接两臂（默认臂留活连接、`GROVEFS_GATEWAY_CTL_PIN=0` 臂留 0 条而两臂回答逐字节相同））、`forksrv` 两档后端远程 fork/graft e2e（整树复制零字节/独立树结构与 EROFS/`--empty`+`graft` 策展/账本精确归零）、`xfer` 两档后端远程 export/import e2e（dump 落在操作员磁盘、按行对齐分片、60B 与 4MiB 窗口同结果、`--as-fork`/`--replace`/守卫、往返逐行精确、账本无泄漏）、`gcremote` 远程 gc e2e（服务端读自己的树、字节向 data-server 枚举）、`control` 控制面统一 e2e（视图定义/key 随 `--meta-store` 进 `grovefs_control`、本地不落控制面文件、PG meta-server 从 PG 解派生挂载/热换形/key 门控）、`client` 平台客户端 e2e（预编译 `grovefs-mac` 挂本内核的 meta-server+data-server：SQLite/PG 两档各跑一次挂载读写、未知视图拒挂、token 门拒挂与带 token 放行、字节只落 data-server 而非客户端主机、`--manager` 引导一个坐标都不给也挂得上并打印解析出的两坐标、冲突/缺 key/验签不过/形态不对/manager 不可达五道拒因、多成员 roster 客户端拒挂而同一份 roster 内核 `mount` 挂得上）；`pg`、`control` 会在无 PG 主机上自动 SKIP，`client` 在没有同级 `grovefs-client` 检出或没有 `fusermount` 助手的机器上整段 SKIP，`treedb`（接入面形状自检 53 条）与 `treedbmatrix`（把 `posix`+`e2e` 两套断言矩阵在 `--meta-store treedb://` 上重放，49 条 = 39 OK + 10 SKIP + 0 FAIL，按实测三元组 `pass=39 fail=0 skip=10` 门控：那 10 条 SKIP 全是硬链接，是引擎无 `Link` op 的定价代价，三元组一动即红；真跑需 `FEATURES_TREEDB=1`）在未开 `--features treedb` 的构建上整段 SELF-SKIP）。两档能力是否**对齐**由 `bash .bneck/engine_matrix.sh` 单独回答：同一串治理命令在 SQLite/PG 上各跑一遍，**20 项能力**（含 `fork`/`graft`、`export`/`import --as-fork`、远程 `gc`、`du` 及其 no-tree 守卫、`apikey`/审计）并排成一张表，OK/REFUSED（设计边界，引拒绝原文）/ERROR（缺口）；最近一次 20 行零 ERROR、`TEARDOWN CLEAN`，见 USAGE §10.1。

## 架构与原理

三层解耦，全走 trait：

```
┌─ FUSE 挂载进程  (fs_impl.rs: GroveFS，&self 多线程安全)
│    ├── MetaProvider trait   视图树访问
│    │     ├── 本地:  MetaEngine  → <home>/meta/<view>/<view>.db
│    │     ├── 远程:  RemoteMeta  → meta-server（中心权威）
│    │     └── 透镜:  LayoutMeta  → mount --layout（每客户端显示定制，装饰以上任一）
│    │           ├── 派生视图: 服务端 LayoutMeta 单例 → mount --view <派生名>（ 规则由 grovefs view define 集中定义于 meta/views/）
│    │           ├── fork 视图: fork <基视图> → 独立 meta/<名>/<名>.db（结构可写、内容 EROFS，字节共享）
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
| `grovefs view fork --name X --base B` | 独立元数据树 `meta/X/X.db` | 单独一棵可组织的树 | 结构可写、内容 `EROFS`、字节共享 |
| `grovefs view fork --name X --base B --empty` + `grovefs view graft --name X --map P` | 空树 + 接枝计划 | 策展后的 fork | 只落选中的条目，字节仍共享 |
| `grovefs view alias --name P --real R` | `alias <R>` 一行 | 公开名 1:1 治理 | 全树可写，真实名拒挂 |

定制出的形状**可以和基视图完全不一样**：`<src>` 端被基树钉死，`<dst>` 端随你发明；想让自定义树成为视图全貌，配一条 `only <新树根>` 即可。透镜做不到的只有一件事——凭空造内容：渲染出的每个条目都是基树某条目的换名/镜像/裁剪加合成目录，字节永远不复制。热生效：挂载在 ≤2s 内自动换形——`meta-server` 逐 op 复查定义的变更票据，已建立的连接不必重挂；`M_OPEN` 时解析一次定义。

> 详节：README「显示定制 / 服务端派生视图 / 纯别名 / fork 视图」四节、USAGE §3.1–§3.4.1、MCP 工作流见 README「智能体定制数据列表」与 `skills/grovefs-mcp/`。

### 二、视图元数据如何存储：一 view 一引擎，控制面跟随后端

```text
<meta-dir>/meta/
├── <view>/                每物理视图一棵权威树，自成一个目录（`ls meta/` 列出的就是树）
│   └── <view>.db          树本体（SQLite WAL）：node 表（id/ino, parent_id, name,
│                          is_dir, data_ref, link_target, mode/uid/gid, mtime/atime/ctime,
│                          UNIQUE(parent_id,name) + ino/parent/data_ref 索引）+ ctrl + locks（POSIX 锁租约）
├── _control/              控制面（不是树：目录名是保留视图名，永不被当视图枚举）
│   ├── refs.db            跨视图共享对象持有者账本（GC 依它判「最后一个名字」；派生/别名/fork 共享基字节）
│   ├── api_keys.db        API key 摘要 + 调用审计（SQLite 后端的控制面；PG 后端进 grovefs_control）
│   └── store.key          开启 api_keys.db 里那份密文副本的密钥（0600，首次使用时原子创建）
├── views/*.layout         派生/别名/fork 的定义(派生=首行 `base <B>` + 规则行;别名=单行 `alias <R>`;fork=单行 `fork <B>`)
└── .xfer/                 export/import 的服务端暂存区（点开头，永不是树）
数据字节：blocks/<2hex>/<key>（key=UUID，按哈希扇出 256 桶，单目录条目不随总量增长）
PG 后端（meta-server --meta-store postgres://…）：一 view 一 schema `grovefs_v_<hex(view)>`，同构 node/ctrl/locks
  + 共享表 grovefs_refs.holders；控制面另进共享 schema grovefs_control（view_defs / api_keys / mcp_calls 三表）——
  换 PG 后视图树与控制面都不落本地文件；`grovefs view`/`apikey`/`mcp`/`data-server` 加 `--meta-store postgres://…`
  （或 env `GROVEFS_META_STORE`）指向同一 URI，与 meta-server 读写同一份系统记录。
```

写路径上「树」与「字节」各走一半：元数据 op 落 `node` 行、内容写落 `blocks/`，`data_ref` 是两者唯一的连接点（条带对象编码为 `s<N>:<stripe_bytes>:<uuid>`）。

**v2 布局与存量迁移**：每棵物理视图树自成一个目录（`meta/<view>/<view>.db`）、控制面另置 `meta/_control/`——**目录名就是类型**（点开头与 `_` 开头的目录永不当视图枚举）。老的扁平存储根（`meta/` 下平铺 `team.db`/`refs.db`）没有命令读它：用 `grovefs layout --meta-dir D show` 看形态，`grovefs layout --meta-dir D migrate` 离线迁到 v2（先 `--dry-run` 看计划；要求该存储无活挂载、本机无 grovefs 进程）。备份要带的清单、GC 的精确规则与跨机迁移路径都在详节。

> 详节：USAGE §5.1（物理布局）/ §5.3（删除与 refs 账本）/ §5.4（备份迁移清单）、README「多机共享与存储选型」（SQLite↔PG 二选一）、USAGE §3.4.2（`view export/import` 跨机搬一棵树）。

### 三、权限如何管理：两层栈 + 一把可撤销的钥匙

准入栈只有两层——**token = 集群准入，view 名 = 树选择**；`--view` 字符串不是身份，任何 uid 挂同一个 `--view alice` 就落进 alice 的同一棵树。需要**逐调用方差异化权限**时上第三层：可撤销 API key（`--require-api-key` 开门，服务端逐 op 硬强制）：

```text
M_OPEN / OP_OPEN（TCP 帧握手）按序过门：
① token          不匹配 → E_AUTH（唯一确定性门；客户端自报 uid 属 advisory）
② api-key 门     scope 分区（--for mcp|mount|any|admin）；撤销即拒；key 若 --view V 绑定：开其它视图握手处即 E_AUTH
③ ro 写门        key 若 --ro：meta 面逐写意图 op E_NOTPERM、data 面内容写 EROFS、加锁类 op EPERM；
                 M_OPEN 回包提示只读挂载——不依赖客户端 -o ro
④ view 名 → 引擎缓存  打开已存在的树，之后同 view 命中同一实例
治理面（grovefs mcp）：同一份 key 注册表（SQLite：api_keys.db；PG：grovefs_control.api_keys）经 scope=mcp 开门，tools/call 与 resources/read 全部落审计；
撤销后活挂载 ≤1s 封停（op 驱动逐秒复证），历史调用仍保留归属。
```

诚实边界：显示治理（派生/别名/fork）**不是安全边界**——它只改呈现的名字树，可见即可读、可读即可写回（fork 除外）；真正的门是 token 与可撤销 key。鉴权只认入库的 SHA-256 摘要，库里另存一份要 store key 才开得动的可逆密文副本（`apikey secret --id X` 读回）；控制面（key/审计/视图定义）**跟随元数据后端**——SQLite 部署落 `meta/_control/api_keys.db` + `views/*.layout`，PG 部署全部进 `grovefs_control` schema，同一份状态两种形态、绝不双写。

> 详节：README「共享视图与权限模型」「给入口上把锁：API key 认证…」「网页管理台…」、USAGE §6（含 §6.1 视图绑定/只读 key 实测）、§3.5.1（十步 key 演练）、§3.5.3（账号 + 基视图授权）。

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
**不需要。**「多机共享一棵树」的能力来自 **`meta-server` + 相同 `--view`**，与底层是 SQLite 还是 PG **无关**。默认的 embedded SQLite（`rusqlite bundled` 编译进二进制、零外部依赖）本来就是为这个拓扑设计的——把 `data-server` + `meta-server` 部署在中心节点，多台客户机 `mount --view alice --manager <中心>:50055 --roster-key-file /etc/grovefs/roster.key`（坐标由 roster 解析）即共享同一棵权威树、跨端实时可见。这一整套由 `metasrv` / `freshness` / `locks` 套件在纯 SQLite 后端上证明,**不需要先装 PostgreSQL**。两种后端是**二选一**的替代关系、不是叠加部署：同一棵权威树只落在其中一种后端上——用 SQLite 就不需要装 PG，换 PG 后视图树只进 PG schema、服务端不再创建/读写任何 `<meta-dir>/meta/<view>/<view>.db`；控制面同样跟随后端——`grovefs view`/`apikey`/`mcp`/`data-server` 也接 `--meta-store postgres://…`（或 env `GROVEFS_META_STORE`），key 与审计进 `grovefs_control.api_keys`/`mcp_calls`、视图定义进 `grovefs_control.view_defs`，PG 部署下本地不落任何控制面文件（`control` 套件逐节实证）。

**Q：那 PG 到底跑在哪一侧？**
**只在服务端**（`meta-server` 那一侧），是它的**可插拔权威存储后端**：

```
客户端(仅 grovefs 二进制)      服务端                    数据层
mount --view alice   ─TCP→   meta-server   ─postgres://→  PostgreSQL 服务器
  · 无 PG 库/凭据/感知        · --meta-store 是它的启动参数     (与 meta-server 同机或独立 DB 机)
```

`--meta-store postgres://…` 只出现在 `meta-server` 子命令上，客户端没有这个选项、也拿不到连接串。PG 服务器进程物理上跑在中心（可与 meta-server 同机走 loopback，或独立一台 DB 机经局域网连），但**始终在服务端**。

**Q：那什么时候才值得上 PG？**
当你想把权威树的**持久化 / 高可用 / 备份 / 集中运维**交给一个成熟数据库时:PG 自带流复制 / logical replication / PITR、`pg_basebackup`、监控与 DBA 工具链,海量元数据(百万级节点)也可借 PG 的索引/VACUUM 调优。注意**自研 HA(快照复制)与 PG 互斥**(见「共享视图与权限模型」),上了 PG 就用 PG 自己的复制、不再用 GroveFS 的文件级快照。选型是**二选一**而非叠加：后端在 `meta-server` 启动时经 `--meta-store` 定死，同一棵权威树不会同时落 SQLite 又落 PG；中途换库用 `view export/import`（两档后端通用，含 `--control-server` 远程分片上下行）或 `pg_dump` 单向搬移，不支持双写。

> **诚实边界**：写模型在 SQLite 与 PostgreSQL 下**不同**——默认 SQLite 后端 `meta-server` 对同一 view 仍是**单写者**（一条权威连接 `conn` + 互斥，写串行）；换 `--meta-store postgres://…` 后写路径放宽为**有界 SERIALIZABLE 写连接池**（`PG_WRITE_POOL_SIZE = 8`，HopsFS 路线，事务基座交给 PostgreSQL 自带 SSI；这 8 写 + 对等 4 读是**暖地板**，突发保留上限由 `--pg-*-pool-max` 定、静默过 `--pg-pool-idle-secs` 自动收回，单视图可用 `grovefs view pool` 覆盖，USAGE §6.2）——不相交 `(parent_id, name)` 行上的并发写真正并行提交，读写重叠的一例由 SSI 中止（`SQLSTATE 40001/40P01`）并**透明重试**（200µs–25ms 退避、≤12 次、绝不外泄给 POSIX 调用方），由 `pg_ssi`（Rust 5 项）与 `pg_parallel`（bash 三挂载并发）套件证明、无 PG 主机自动 SKIP。这把“单一权威写者”放宽为“单一权威调度（serially equivalent）”，治理不变量 I1–I4 在任意 SSI 接受的调度下逐条不变。元数据**读**并行则覆盖两个引擎——SQLite `MetaEngine` 与 PG `PgEngine`：前者持 1 条暖读连接（`GROVEFS_SQLITE_READ_POOL` 可调）、后者持 4 条只读连接且各自缓存 prepared statement；写侧同样缓存（每写连接 ≤32 条，单连接一次事务 577–670µs → 316–330µs，台架写吞吐 N=1/4/8 3,048–3,118 / 4,784–5,202 / 4,518–4,680 ops/s），目录列举各下推 SQL `LIMIT/OFFSET`。**仍未做的是跨节点多权威**（把权威分区、Spanner 式外部一致性）——PG 写并行只在单集群内；单权威下的**多 PG 实例放置**是正式支持面（`--meta-store file:注册表`，放置表决定每视图落哪个实例，`view create --pg-instance` 钉住（管理台「新建基视图」下拉同样可选）、`view move` 四步搬迁，`refs rebuild` 逐实例重算后求和、服务端 `gc` 跨实例并集扫描；每个实例必须配自己的 data-server），取证 `test_pg.sh` 第 8 节与 `.build-bin/probe_pg_shard.sh`，详见 USAGE §6.2 与 docs.html §24。默认场景请继续用零依赖的 SQLite；PG 的增量价值是**写并行 + 持久化/复制/运维成熟度**。

## 构建

**前置**（Linux）：只需 Rust 工具链（edition 2021）。**不需要 `pkg-config`，也不需要 libfuse 开发包**——FUSE 客户端是纯 Rust 的 `fuser`（其 `libfuse*` feature 未启用），实测 `PKG_CONFIG=/nonexistent cargo build --release` 全量构建通过，产物 `DT_NEEDED` 里没有任何 fuse 库（只有 glibc 五件）。元数据引擎 `rusqlite` 用 `bundled` 特性从源码编译 SQLite，同样**无系统依赖**。挂载路径要的是内核 `/dev/fuse`，以及**非 root 时**发行版自带的 setuid 助手 `fusermount`/`fusermount3`（有 `CAP_SYS_ADMIN` 则直连 `mount(2)`）——那是用户态工具，不是开发包。

```bash
cargo build --release          # 产物：target/release/grovefs
```

> 注：仓库里的 `.build-bin/pkg-config` shim 与套件脚本的 `export PKG_CONFIG=…` **不影响构建**——
> 现行依赖图里没有任何 crate 查询 `pkg-config`，shim 可留可删。新环境直接：
> ```bash
> cargo build --release && export LC_ALL=C LANG=C
> ```

## 快速上手（本机最小集群）

一棵视图**永远由两个服务提供**：`meta-server` 管树、`data-server` 管字节；而挂载**只从
`manager` 的签名 roster 拿到这两个坐标**——不再手敲 IP:PORT。最小的集群就是在本机回环上
把 `manager` + 这两个服务起起来——同一个存储根 `BASE`，`meta/` 落每视图一棵树、`blocks/`
落内容对象（换成两台机器，命令一字不改，只是地址不同）。挂载为什么**没有**「本机单盘」这
条退路、坐标又为什么**只能**经 manager 引导——动机、否决项与代价见
`docs/adr/0001-manager.md` 与 `docs/adr/0002-no-local-mount.md`：

```bash
Y=./target/release/grovefs
BASE=/data/myfs

# 1) 声明拓扑并起 manager（首次 serve 会签发 join.key / roster.key 到 $BASE/manager/）
$Y --home $BASE manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
$Y --home $BASE manager serve --listen 127.0.0.1:50055 &

# 2) 两个服务向 manager 上报自己活着（同一存储根 = 最小集群）
$Y meta-server --meta-dir $BASE --listen 127.0.0.1:50053 --advertise-addr 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &
$Y data-server --data-dir $BASE --listen 127.0.0.1:50100 --advertise-addr 127.0.0.1:50100 --member-id 0 --register-with 127.0.0.1:50055 --join-key-file $BASE/manager/join.key &

# 3) 显式建两个视图（唯一的建视图入口）
$Y view --meta-dir $BASE create --view alice
$Y view --meta-dir $BASE create --view bob

# 4) 挂载：两个坐标只从 manager 的签名 roster 解析（默认转后台——挂载点就绪即返回；前台附着用 `--foreground`）
$Y mount --view alice --mountpoint /mnt/alice --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key
$Y mount --view bob   --mountpoint /mnt/bob   --manager 127.0.0.1:50055 --roster-key-file $BASE/manager/roster.key

# 5) 像普通目录一样读写、建自定义层级
mkdir -p /mnt/alice/projects/2026
echo "年度报告" > /mnt/alice/projects/2026/报告.txt
ls /mnt/bob                               # bob 看不到 alice 的文件

# 6) 卸载
$Y umount --mountpoint /mnt/alice
```

逐字回显（存储根与挂载点用临时目录跑的那一遍）：

```text
created BASE view 'alice' at /tmp/qseCnd/meta/alice/alice.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
$ grovefs view --meta-dir /tmp/qseCnd list
no alias/fork/lens definitions in /tmp/qseCnd (local files) — the 2 base view(s) below each map to their own tree
base view 'alice' -> own tree meta/alice/alice.db (read-write; deletes free a shared object only when no view holds it)
base view 'bob' -> own tree meta/bob/bob.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --meta-dir /tmp/qseCnd list [--view NAME]` (add --view to see only that view's keys)
$ grovefs mount --view alice --mountpoint /tmp/qs_mnt_443443/alice --manager 127.0.0.1:50055 --roster-key-file /tmp/qseCnd/manager/roster.key
manager 127.0.0.1:50055: deployment 'default' epoch 1 resolved meta [127.0.0.1:50053] data [127.0.0.1:50100] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:50053
using remote data service at 127.0.0.1:50100
mounting view 'alice' at /tmp/qs_mnt_443443/alice (fully remote: no local store)
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
grovefs: mounted view 'alice' at /tmp/qs_mnt_443443/alice (background pid 443480)
$ ls /tmp/qs_mnt_443443/alice/projects/2026
报告.txt
$ cat /tmp/qs_mnt_443443/alice/projects/2026/报告.txt
年度报告
$ find /tmp/qseCnd -path '*/blocks/*' -type f   # 字节落在 data-server 的根里
/tmp/qseCnd/blocks/2a/abebde35-b88d-4f1f-96ff-ac856af3795b
$ grovefs umount --mountpoint /tmp/qs_mnt_443443/alice
unmounted /tmp/qs_mnt_443443/alice
```

坐标不再手敲：每个挂载都用 `mount --manager H:P --roster-key-file F` 从 manager 的签名
roster 解析出上面这对坐标（解析结果喂给服务端的正是 meta/data 两个地址，故障转移与条带
语义不变）。见「挂载引导」与 `docs/adr/0001-manager.md`。

预建层级用 manifest：`view --meta-dir $BASE create --view alice --manifest seed.txt`
（每行 `mkdir <path>` 或 `touch <path>`，`#` 注释；清单里已存在的名字跳过，所以重复
应用同一份种子不会种出两份。`touch` 建的空对象落在**与该 meta 根相邻的 `blocks/`**，
即最小集群的同一根；data-server 用的是另一个 `--data-dir` 时，改成建完视图后经挂载写
文件）。视图名已存在时 `create` 直接拒绝，不会静默复用：

```text
error: view 'alice' already exists — remove it first (`grovefs view rm --name alice --base`) or choose a different name
```

## 命令瘦身：环境变量与自动加载的配置文件

重复敲的长命令可以瘦身。每个旋钮的取值优先级恒为：**命令行参数 > 当前环境变量 > 配置文件 > 内置默认值**。grovefs 启动时自动加载一个 `KEY=VALUE` 文件填入尚未设置的 `GROVEFS_*` 环境变量，查找顺序：`$GROVEFS_CONFIG` 指定路径 > `~/.config/grovefs/grovefs.conf`：

```bash
# ~/.config/grovefs/grovefs.conf（有口令/密钥就 chmod 600，与 env 里放口令同一契约）
GROVEFS_MANAGER=192.168.1.10:50055
GROVEFS_ROSTER_KEY_FILE=/etc/grovefs/roster.key
GROVEFS_TOKEN=s3cret

# 之前：grovefs mount --view alice -m /mnt/alice --manager … --roster-key-file … --token …
# 之后：只剩“这次挂哪个视图到哪儿”
grovefs mount --view alice -m /mnt/alice
```

可被 env/文件顶替的旋钮：`GROVEFS_HOME`（`--home`）、`GROVEFS_MANAGER` / `GROVEFS_ROSTER_KEY_FILE` / `GROVEFS_DEPLOYMENT`（挂载引导的 `mount --manager/--roster-key-file/--deployment`，现在这是挂载唯一的坐标来源）、`GROVEFS_TOKEN`（`mount --token`，服务端 `--token` 同源）、`GROVEFS_META_DIR`（`meta-server --meta-dir`）、`GROVEFS_API_KEY` / `GROVEFS_MCP_API_KEY`（早已存在的凭据 env，现在也能写进文件）、`GROVEFS_STATS`。（`GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS` 不再驱动 `mount`——它们只在 `du`/`gc --control-server` 这类直接对服务地址说话的运维命令上保留。）语法支持 `#` 注释、`export ` 前缀、引号包裹值；非 `GROVEFS_*` 键与坏行警告后忽略。`--view`/`-m` 刻意不收录——每次挂载都不同，写进默认值是错的；后台挂载的 re-exec 子进程继承环境，简写同样生效。回归：`config` 套件（37 条断言：优先级链/解析边界/纯由文件驱动的真实 FUSE 挂载/一个引导坐标都不给（既无 `--manager` 又无 `GROVEFS_MANAGER`）时的硬拒——退出码、逐字文案、不留挂载点，三段都钉住/挂载横幅逐字写着「fully remote: no local store」，且它给自己的 `--home` 连一个目录都不建）。

## 关于 `--view` 与路径容量

- **`--view` 是视图命名空间的标签，与 Linux 用户名无关**：任意合法文件名字符串都可以（如 `alice`、`team-a`、`u42`、`研究组`）——它只用来 ① 在服务端定位 `<meta-dir>/meta/<view>/<view>.db`，② 在 FUSE 挂载上打 `grovefs:<view>` 标签。二进制不调 `getpwnam`、不做 uid 校验、不需要 `useradd`。**权限来自服务端准入（`--token`，或可撤销的 `--require-api-key` + `mount --api-key`）**，不来自 Linux 用户身份；能连上两个服务不等于能读任何一棵树。同一对服务下不同 `--view` = 不同视图，跨视图不可见。
- **一个用户一棵树，树内路径完全自主**：挂载后该 mountpoint 下可 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意自定义层级，与在自己 home 里的操作一致；视图隔离的粒度是「整棵树」而非「单一路径」。`--manifest` 只是预建骨架的便捷入口，不是路径数量的上限。

## 共享视图与权限模型

GroveFS 的权限栈只有两层——**令牌 = 集群准入，view 名 = 树选择**。`--view` 字符串本身不是身份：任何 uid 只要经同一个 manager 解析出同一对服务坐标、挂同一个 `--view alice` 都会落到 alice 的同一棵权威树上，看到彼此写入。这是有意的，也让「多人共享一个视图」开箱即用：

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
   ├─ SQLite（默认）：       一 view = 一棵独立树目录  <meta-dir>/meta/alice/alice.db · bob/bob.db
   └─ PostgreSQL（--meta-store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62

数据库里独立存储的单位是「视图」而非「每个客户端」；数据字节走 data-server → 存进其 --data-dir 下的 blocks/<2hex>/<key>（桶由 key 哈希决定），树里只存 data_ref 引用。
```

- **多人共享同一 view**：多台机器、不同 OS 用户，都 `mount --view alice --manager ... --roster-key-file ...` → 元数据经 `meta-server` 集中（一棵 SQLite/PG 树，串行化写）、内容经 `data-server` 集中（一份 `blocks/`）；`ls` 或 `cat` 立刻跨端可见，实时失效推送保证收敛。若希望**不同调用方有不同权限**（例如只读旁观者、或某客户端只许碰某一棵树），用视图绑定 / 只读 API key（服务端需 `--require-api-key`）：

  ```bash
  # 中心节点上：给每个调用方铸一把绑定 alice 的 key（apikey 自带 --meta-dir，与 meta-server 同名，指同一个元数据根）
  $Y apikey --meta-dir /data/cluster create --label rw-peer --for mount --view alice        # rw 协作者
  $Y apikey --meta-dir /data/cluster create --label reader  --for mount --view alice --ro   # 只读订阅者
  $Y apikey --meta-dir /data/cluster list                                                   # 查看已铸造的 key（含 view/rw 列）
$Y apikey --meta-dir /data/cluster set-ro --id <ID> --rw                          # 只读→读写原地热切换（不带 --rw 切回只读；活挂载 ≤1s 生效免重连）
  $Y apikey --meta-dir /data/cluster rotate --id <ID>                           # secret 丢了：同一 id 重铸 secret（绑定与审计不动）
  $Y apikey --meta-dir /data/cluster revoke --id <ID>                                       # 撤销单条（对已建立挂载 ≤1s 内封停）
  ```

  `--view alice` 把 key 绑定到单一物理视图 alice：拿它去 `M_OPEN` 别的视图会在握手处直接 `E_AUTH` 拒（视图隔离由凭据本身承载，无需任何按 uid 的表）。`--ro` 的 key 被服务端逐写意图 op 拒 `E_NOTPERM`（meta 面）/`EROFS`（data 面内容写），并在 `M_OPEN` 回包里提示只读挂载——只读由服务端强制，不依赖客户端 `-o ro`。撤销只是把行标记 revoked（历史调用仍留归属），且能打到活连接（服务端逐秒复证、op 驱动）。省略 `--view` 则是系统级 key，可开任意视图。

  > **诚实边界**：`--api-key` 认证的是**入口的调用方**（key 可撤销、可归属），客户端自报 uid 属 advisory、不参与准入。鉴权只认入库的 SHA-256 摘要，库里另存一份要 store key 才开得动的可逆密文副本（`apikey secret --id X` 读回）；绑定/只读两维都在服务端硬强制。加锁类 op（`flock`/`F_SETLK`，含锁续约）归为写意图，`--ro` key 对它们一律 `EPERM`（只读角色需共享锁就不能用 `--ro`）。不开 `--require-api-key` 时，服务端行为与不设门一致（仅照常落审计）。详见「可撤销凭据」一节与 USAGE §3.5.1/§6。

- **元数据后端可切换：SQLite ↔ PostgreSQL**（仅指 `meta-server` 权威存储）：

  ```bash
  # 默认（embedded SQLite，一 view 一 `<meta-dir>/meta/<view>/<view>.db`）
  $Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110

  # PostgreSQL：一 view 一个 schema `grovefs_v_<hex(view)>`，事务化 rename/unlink/lock
  $Y meta-server --meta-store postgres://user:pw@10.0.0.9:5432/grovefs --listen 0.0.0.0:50110   # PG 下 --meta-dir 可省，服务端本地不落权威数据
  ```

  客户端挂载命令**完全一致**（仍 `--manager host:port --view alice`，坐标由 roster 解析）—— 线协议不变。注意：
  - **PG 模式与自研 HA 互斥**：`--meta-store postgres://…` + 任一 `--role standby / --peer / --witness` 在启动时直接 fail-fast（`VACUUM INTO` 快照复制是 SQLite 特性）；PG 上 HA 请交给 PostgreSQL 自己的**流复制/logical replication/PITR**——那本来就是比文件级快照更强的机制。
  - **控制面随元数据后端走**：SQLite 部署下 key/审计落 `<meta-dir>/meta/_control/api_keys.db`、视图定义落 `meta/views/*.layout`；PG 部署下两者进共享 schema `grovefs_control`（`api_keys`/`mcp_calls`/`view_defs`）——`grovefs view`/`apikey`/`mcp`/`data-server` 加 `--meta-store postgres://…`（或 env `GROVEFS_META_STORE`）指向与 meta-server 同一 URI 即可，一次 `pg_dump` 同时覆盖视图树（`grovefs_v_*`）、锁与控制面，无需另备文件。
  - 建表由服务端在 view 首次 `M_OPEN` 时幂等 `CREATE SCHEMA IF NOT EXISTS` + `CREATE TABLE IF NOT EXISTS` 完成，不需要预迁移脚本；根节点固定 id=1，`setval(pg_get_serial_sequence(...), MAX(id))` 对齐序列，`ROOT_INO` 语义与 SQLite 完全一致。

  为什么没做 Redis：Redis 的 K/V+TTL 模型与视图树（rename/unlink/hardlink/refcount GC 需事务、锁需要行级仲裁）不同构——把 relational 语义硬塞到 Redis 里等于在应用层重做一个 PG，收益为负。**GroveFS 只加 PG，不加 Redis**。

  > **两档怎么选**（而不是“功能有没有”）：SQLite / PG 的硬指标（写吞吐 **767–1,109 ops/s** vs **~4.6k–5.2k ops/s**，差 4–6 倍，差距来自单写者模型本身；可用性形态：SQLite 原生主备 HA vs PG 交给流复制/PITR；视图规模：一视图一文件一引擎 vs 一视图一个连接池 + 空闲驱逐 + 地板/上限分离；独占命令只剩三处「只能 SQLite」——原生 HA、目录零拷贝纳管、就地 `gc`）见 USAGE §4.1.1，同一件事在两档的命令写法见 docs.html §22.1 大表。

## 显示定制：同一份数据、每个客户端不同形状（`--layout`）

视图隔离是「不同 view = 不同树」；`--layout` 解决另一半问题：**同一棵存储树，不同客户端挂载时定制显示方式**——只看部分、或把条目重组到自己习惯的路径。规则文件每行一条（`#` 注释，路径为绝对路径，源路径 = 存储树坐标）：

| 规则 | 效果 | 典型用途 |
| --- | --- | --- |
| `hide <src>` | 在源位置隐藏整棵子树 | 屏蔽 private 目录 |
| `only <src>` | 白名单：只显示这些子树（祖先目录自动放行） | 客户端只暴露 /docs |
| `map <src> <dst>` | 重组：源位置消失，条目出现在 `<dst>`（必要时自动合成中间目录） | 把 /data/raw 摆进 /team/raw |
| `link <src> <dst>` | 别名：两处同时可见，同一 inode（不复制字节、不增引用） | 快捷入口 /proj/mylib |

语义要点（均有测试断言）：真实条目保持**真实 inode**，读写/锁/条带/GC 直通数据面；经透镜 `rm` 删的是源条目；`map`/`link` 子树内部仍按源路径接受 `hide`/`only` 过滤；向纯合成的脚手架目录创建返回 `EPERM`，普通创建撞透镜已显示的名字返回 `EEXIST`；symlink 目标文本不改写。`only` 模式下向 pass-through 祖先写入同样返回 `EPERM`（白名单外不可写）。

一次真实运行：同一棵 `alice` 树挂两处，一个带本地规则文件、一个不带
（`meta-server`+`data-server` 在本机回环上，两个挂载点指向同一份数据）：

```text
$ cat lens.txt                     # 客户端 C 的透镜
hide /photos/private
map /data/raw /team/raw
$ grovefs mount --view alice -m mnt --manager 127.0.0.1:16520 --roster-key-file ~/roster.key --layout lens.txt
manager 127.0.0.1:16520: deployment 'default' epoch 1 resolved meta [127.0.0.1:16521] data [127.0.0.1:16522] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16521
layout overlay /tmp/lensWFuU/lens.txt: 1 hide, 0 only, 1 reloc rule(s)
using remote data service at 127.0.0.1:16522
mounting view 'alice' at /tmp/lensWFuU/mnt (fully remote: no local store)

$ ls mnt                                           # 带透镜：多出一个合成名 team
data  docs  photos  team
$ ls mnt/photos                                    # private 已被隐藏
public
$ echo "hello via lens" > mnt/team/raw/note.txt    # 写直通源对象，无复制
$ ls mnt2/data/raw                                 # 另一无透镜挂载即时看到，且在源路径
big.bin
note.txt
$ ls -i mnt/team/raw/note.txt mnt2/data/raw/note.txt   # 同一个 inode
9 /tmp/lensWFuU/mnt/team/raw/note.txt
9 /tmp/lensWFuU/mnt2/data/raw/note.txt
$ mkdir mnt/team                                   # 'team' 是透镜合成名，已占用
mkdir: cannot create directory '/tmp/lensWFuU/mnt/team': File exists
$ rm mnt/team/raw/big.bin                          # 经透镜删的就是源条目
$ ls mnt2/data/raw
note.txt
```

规则在挂载时一次性解析（非法规则 fail-fast，不降级为「静默全显示」）；透镜是**每挂载实例**的显示层，不落盘、不改变服务端任何状态，因此与其它挂载正交：叠在 `RemoteMeta` 之上（挂载只有这一条元数据路径），也可与 `--ro`、视图绑定 key 组合。共享 meta-server 时，挂在合成目录下的条目其失效推送按源路径广播、可能不命中透镜坐标，建议配 `--no-cache` 关掉属性/条目缓存。

## 服务端派生视图：一份数据、不同 `--view` 名不同形状（`grovefs view`）

`--layout` 由每个客户端自备规则；当希望把形状**集中管理在服务端**（团队内不同名字共享同一显示契约）时，用派生视图：`grovefs view define` 把规则存进 meta 根目录的 `<meta-dir>/meta/views/<name>.layout`（首行 `base <物理视图>`，余行同 `--layout` 四动词；SQLite 后端落此文件，`view … --meta-store postgres://…` 时写进 PG 的 `grovefs_control.view_defs` 表），客户端只需 `mount --view <派生名>`，**不需本地规则文件**。派生（虚拟）视图**一律只读挂载**——形状是集中治理的分发视图，所有写入都属于原（基名）视图。同名派生视图在不同机器上由服务端同一份定义渲染（虚拟 inode 跨机一致）；访问控制仍由视图绑定 / 只读 API key 承担（形状与权限两面正交）。挂载不带 `--layout` 时，形状由服务端解析 `<meta-dir>/meta/views/<view>.layout` 后经 `M_OPEN` 回包告知客户端——客户端手里永远不需要一份规则文件。

```text
$ cat docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir $BASE define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir $BASE define --name team --base team --rules docsonly.rules   # 名与基同名
error: derived view name must differ from its base
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
base view 'team' -> own tree meta/team/team.db (read-write; deletes free a shared object only when no view holds it)

$ grovefs mount --view docsonly -m mnt --manager 127.0.0.1:16510 --roster-key-file ~/roster.key   # 派生名直接挂，无 --layout
manager 127.0.0.1:16510: deployment 'default' epoch 1 resolved meta [127.0.0.1:16511] data [127.0.0.1:16512] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16511
derived view 'docsonly' over base 'team': read-only lens
note: derived (virtual) views mount read-only — write to base view 'team'
using remote data service at 127.0.0.1:16512
mounting view 'docsonly' at /tmp/vdemo0j0V/mnt (fully remote: no local store, read-only)
$ ls mnt                                       # 派生名挂载点：只有 docs
docs
$ ls mnt_full                                  # 基名挂的是同一棵完整树（可写）
docs
photos
$ echo "new entry" > mnt_full/docs/2026/b.md   # 基树写入……
$ ls mnt/docs/2026                             # ……派生视图短 TTL 后看到（共享 meta-server 时失效推送双播、即时）
a.md
b.md
$ echo "from lens" > mnt/docs/2026/c.md        # 经虚拟视图写一律拒绝
bash: /tmp/vdemo0j0V/mnt/docs/2026/c.md: Read-only file system
$ grovefs view --meta-dir $BASE rm --name docsonly
removed view definition 'docsonly' (underlying data untouched)
```

语义与守卫：define 拒绝 `name == base`、拒绝 base 本身是派生视图（不嵌套透镜）、拒绝遮蔽已有物理 `<name>.db`；非法规则 fail-fast（不会退化成「全显示」）。**虚拟视图一律只读（硬契约）**：写只属于原（基名）视图——`meta-server` 在 `M_OPEN` 握手回包里带派生标志，客户端直接落内核只读挂载（写 `EROFS`、`flock` 写锁同拒），服务端另对派生名的写意图 op 加独立权威门禁（回 `E_NOTPERM`，有意不用 `E_ROFS` 以免被误读为 HA 故障转移信号），改过的客户端也绕不过——判定在持有存储的那一侧。**改定义热生效、免重挂**：`meta-server` 每次分发 op 前检查定义文件 mtime，`define` 替换后已建立的挂载连接在客户端缓存 TTL（≤2s）内自动切到新形状（换形后只读契约不变）；`view rm` 则**不**热作用（已挂载者保持打开时的形状直到重挂，避免抽掉正在使用的树）。共享 meta-server 下，派生视图的订阅者**同时注册进自身名与 base 名两个桶**，基侧写双播即时失效、对派生挂载即时可见（`layout` 套件第 9 节 e2e 验证：基写→派生立即可见、派生挂载写/删被 `EROFS` 拒且基树无痕、re-define→同连接换形）；`view rm` 只删定义不动数据。`grovefs view` 与 `grovefs apikey` 一样操作服务端本地文件，需在 meta-server 所在机器执行（或共享同一 `--meta-dir`）；HA 部署下 `meta/views/` 目录是配置状态、随 `api_keys.db` 一样手工同步到备机。

热重载实录（同一个已挂载连接，未重挂）：

```text
$ ls mnt                                    # docsonly = only /docs，挂载中
docs
$ grovefs view --meta-dir $BASE define --name docsonly --base team --rules broader.rules
view 'docsonly' already defined — replacing it (live meta-server mounts hot-reload it within their cache TTL)
defined derived view 'docsonly' over base 'team': 0 hide, 2 only, 0 reloc rule(s)
$ sleep 3
$ ls mnt                                    # 同一个挂载，没有重挂，形状自动变宽
docs
photos
```

## 纯别名：名字治理（`grovefs view alias`）

派生视图治理「长什么样」；**纯别名**治理「叫什么」：名字就是别名，映射值才是真实名。定义同放 `<meta-dir>/meta/views/<name>.layout`（PG 后端：`grovefs_control.view_defs` 表行），首行 `alias <真实视图>`、不带任何规则行。与派生视图恰成正反两面：**别名挂载全树可写**（它就是真实视图的原形），而**真实名从此不是挂载入口**——`meta-server` 拒挂被治理的真实名，对外名字可以自由轮换，存储身份不随名字外露：

```text
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (the meta-server refuses a mount of the real name); alias mounts show the FULL tree and are WRITABLE
$ grovefs mount --view teamwork -m mnt_leak --manager 127.0.0.1:16510 --roster-key-file ~/roster.key   # 全树、可写
manager 127.0.0.1:16510: deployment 'default' epoch 1 resolved meta [127.0.0.1:16511] data [127.0.0.1:16512] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16511
alias view 'teamwork' resolves to real view 'team' (full tree, writable)
using remote data service at 127.0.0.1:16512
mounting view 'teamwork' at /tmp/vdemo0j0V/mnt_leak (fully remote: no local store)
$ echo "hello alias" > mnt_leak/docs/hello.md
alias write: OK (hello alias)
$ grovefs mount --view team -m mnt_t --manager 127.0.0.1:16510 --roster-key-file ~/roster.key          # 直挂真实名被拒
manager 127.0.0.1:16510: deployment 'default' epoch 1 resolved meta [127.0.0.1:16511] data [127.0.0.1:16512] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16511
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
error: grovefs: background mount exited (exit status: 1) before mounting
```

公开名泄露后，**一条 `--retire` 命令原子轮换**（先写新名再删旧名，真实名全程被治理、无裸露窗口；视图绑定 key 记在真实名上，对别名与基名同时生效，故轮换公开名不需重铸 key）：

```bash
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork     # 旧公开名已泄露，换成 teamwork2
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
$ grovefs mount --view teamwork -m /mnt/leak --manager 127.0.0.1:16510 --roster-key-file ~/roster.key   # 拿旧泄露名再挂
error: view 'teamwork' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --meta-dir /tmp/retiretvmc/home create --view teamwork` (or seed a tree with `grovefs view import`)
error: grovefs: background mount exited (exit status: 1) before mounting
```

守卫与语义：拒绝 `name == real`、遮蔽物理 db、派生目标、别名链；一个真实名同时只允许一个对外名（`--retire` 在同一条命令内先写后删，不受此约束卡住，且拒绝误指向派生视图或不存在的别名）；视图绑定 key 记在真实名上，对别名与基名同时生效（也因此**轮换公开名不需重铸 key**）；`grovefs import` 拒绝别名目标（别名不是存储身份）；`view rm` 删别名后真实名对新挂载恢复可挂。轮换/删别名只影响**新**挂载（已建立连接持续到断开），被轮换掉的旧名不再解析到任何一棵树（`M_OPEN` 对一个既无树又无定义的名字直接拒绝，不会当场长出一棵空的）——**轮换是名字治理（泄露止损）而非安全边界**，硬隔离仍靠 token+API key。详见 USAGE §3.4（`layout` 套件第 10/11 节 29 条断言）。

## fork 视图：可组织但不可上传的独立元数据树（`grovefs view fork`）

派生视图是只读的「显示透镜」，别名是全树可写的「公开名」；两者之间还缺一档真实需求：**能整理目录树、但不能上传内容，且删掉自己的名字绝不伤基视图**。`grovefs view fork` 就是这一档——它把基视图的目录项**复制**成一份独立元数据树（`<meta-dir>/meta/<name>/<name>.db`，保留 inode 与硬链接），内容字节一个不拷、经跨视图账本共享；定义仅一行 `fork <基视图>`：

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team
forked view 'lab' from base 'team': 8 entries, 4 shared object(s)
note: 'lab' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch 'team'
note: content bytes are SHARED with 'team' (cross-view ledger): the object is freed only when no view holds a name for it anymore
$ grovefs view --meta-dir $BASE list            # 一条命令看清所有视图：定义槽 + 未加定义的裸基视图
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team/team.db (read-write; deletes free a shared object only when no view holds it)
$ grovefs mount --view lab -m mnt_lab --manager 127.0.0.1:16510 --roster-key-file ~/roster.key
manager 127.0.0.1:16510: deployment 'default' epoch 1 resolved meta [127.0.0.1:16511] data [127.0.0.1:16512] (signature verified, valid for 3599s more)
using remote metadata service at 127.0.0.1:16511
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
using remote data service at 127.0.0.1:16512
mounting view 'lab' at /tmp/vdemo0j0V/mnt_lab (fully remote: no local store)
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=false ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
$ echo hello > mnt_lab/notes.md            # 名字建得出来（结构写放行），内容写 → EROFS
bash: echo: write error: Read-only file system
$ mkdir mnt_lab/sorted && mv mnt_lab/docs/2026/a.md mnt_lab/sorted/   # 结构写 → 放行
structure write: OK (a.md)
$ rm mnt_lab/photos/big.bin                # 只摘 lab 自己的名字
fork unlink: OK; base still sees: big.bin
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
- **删除隔离 + 跨视图账本**：fork 里删名字只删自己 `node` 行；共享对象只有在「本视图无同名、且账本显示无其它视图持有」时才真正 `backend.remove`。`view rm --name <fork>` 会删掉它自己的 `meta/<name>/<name>.db`（+ WAL/SHM）并 `refs rebuild` 重算账本，基视图与其余视图的字节不受影响。
- **守卫**：`name != base`；目标名已有定义或已有物理 db 拒绝；基名必须无定义（透镜/别名/fork 不能当基）且真的有树（SQLite 一个 `meta/<base>/<base>.db`、PG 一个 `grovefs_v_<hex>` schema——`has_physical` 按后端的口径回答）；`alias` 拒绝指向 fork（fork 是树身份，不是公开名，给它发 key 用 `apikey create --view lab`）；`import` 拒绝 fork 目标。
- **可见性**：`view list` 同时列出定义槽（派生/别名/fork）与**裸基视图**（无定义的物理树）；`apikey list --view V` 只列能开 V 的 key（纯别名也匹配绑定到真实名的 key），并汇总未绑定（系统级）key 数量。
- **e2e**：`fork` 套件 76 条断言——创建/守卫/`--empty` 空种子 + `view graft` 接枝（stdin 计划/改名/目录子树/同名整批拒绝/`--skip-existing`）/`view list`/`apikey list --view`/经 `meta-server` 的挂载（结构写放行、内容 EROFS、截断拒绝）/删除隔离与账本/`view rm` 删树重建账本。

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
$ grovefs mount --view release -m .demomcp/mnt --manager 127.0.0.1:50055 --roster-key-file ~/roster.key &
manager 127.0.0.1:50055: deployment 'default' epoch 1 resolved meta [127.0.0.1:50053] data [127.0.0.1:50100] (signature verified, valid for 3599s more)
derived view 'release' over base 'team': read-only lens
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

语义与边界：十个工具（治理八个 `list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`，只读探查两个 `list_view_tree` / `get_view`）与 `grovefs view` 子命令共用**同一套库函数**（`grovefs::mcp`）——校验、拒绝文案、落盘动作逐字节相同，一个契约、两个入口，智能体做不出 CLI 做不出的事，也绕不过任何守卫（派生只读、fork 结构与内容分治、别名拒挂真实名与 fork、`--retire` 先写后删等全部适用；改定义的热生效发生在 `meta-server` 一侧，挂载在 `M_OPEN` 解析一次）。`fork_view` 在同一套守备下把物理基视图复制成独立元数据树（`list_views` 以 `kind:"fork"` 报出，`get_view` 返回 `own_tree`，`remove_view` 连树一并删除并重建账本）；`fork_view` 的 `seed:false` 只建空树、`graft_view` 再按计划接枝（一行 `<src>` 或 `<src>TAB<dst>`，整份一事务、字节零复制、同名默认拒绝或 `--skip-existing`），与 `grovefs view fork --empty` / `view graft` 是同一实现。`list_view_tree`/`get_view` 是**纯元数据只读探查**：以只读方式打开本地 `<meta-dir>/meta/<view>/<view>.db`、经与挂载同一副 `LayoutMeta` 透镜渲染目录清单（不挂载、不读对象字节），让智能体拿真实显示路径写规则、并在提交前预览效果；远端/PG 后端的树不在此遍历（需本地 SQLite 元数据根）；回包里的 `size` 是元数据字段而非字节数（目录 `4096`、文件 `0`、软链为 target 长度），需真实字节请挂载后 `stat`。`validate_definition` 是**发布前干跑**：把一副 `name`+`base`+`rules` 走完 `define_derived_view` 的全部治理守卫（非法名、名与基同名、遮蔽物理 db、覆盖纯别名、规则语法），拒绝时回与真实发布**逐字节相同**的文案、接受时回 JSON 判定，且**绝不写盘**——比只校验动词语法的 `preview_rules` 更进一步，让坏定义永远落不了地。同一治理再以 2025-06-18 协议的其余常用方法面二次暴露——资源 `resources/list`/`resources/templates/list`/`resources/read`（把视图与手册作为可附着上下文：`grovefs://guide` 原样回分节操作手册、`grovefs://views` 回与 `list_views` 相同的快照、`grovefs://view/<name>/definition` 与 `/tree`，树模板带查询参数 `{?path,depth,max}`，与 `list_view_tree` 的参数同义——大树可以一次只附一个子树，被界剪断时仍报 `truncated:true`）、提示 `prompts/list`/`prompts/get`（两副参数化剧本：`reshape-view` 取 `base`/`goal`，`rotate-alias` 取 `real`/`new`/`old`）、补全 `completion/complete`（按 `ref` 做角色感知：`reshape-view.base` / `rotate-alias.real` 只给物理视图，`rotate-alias.new`/`old` 只给别名，其余给全部视图名）、日志 `logging/setLevel` → 服务端 `notifications/message`（阈值以上按成功 `info`/失败 `warning` 推送，`data` 带方法与被操作对象，如 `tools/call list_views ok`），且复用**完全相同的渲染器**（故资源/提示/补全与工具结果逐字节一致）。面向智能体运行时的元数据：每个工具带 `title` 与 `annotations`（`readOnlyHint` 标出五个纯读工具 `list_views`/`preview_rules`/`validate_definition`/`list_view_tree`/`get_view`，`destructiveHint` 只给 `alias_view`/`remove_view`，客户端据此决定能否自动放行），四个回 JSON 的工具带 `outputSchema` 且结果同时给 `structuredContent`（客户端不必再剥 `content[0].text`）；`initialize` 回的分节 `instructions`（Discover/Reshape/Curate/Validate before publishing/Golden order/Beyond tools/Mechanics）是智能体动手前唯一的路线图，客户端注入上下文即可照做。协议健壮性：`protocolVersion` 按 `2025-06-18`/`2025-03-26`/`2024-11-05` 协商，客户端报不认识的版本时回 `2025-06-18`，不原样复读；`arguments` 既收对象也收 JSON 字符串（部分客户端会多序列化一层）；缺参错误附 `(tool 'x' requires: …)`、工具名打错附 `did you mean '…'?` 与 `available:` 全清单，智能体重试一次即可自纠。治理失败按 MCP 惯例回在结果内（`isError: true` + 原文），协议层错误才走 JSON-RPC error；通知（无 id）一律不作答；非法帧回 `-32700` 后继续服务。**控制面 only**：不挂载、不写数据。默认信任能启动这个进程的调用方（操作系统级的账号边界，与直接读写 `--meta-dir` 同构），跨网鉴权仍由服务端 token/API key 负责；当它被 front 给多个不同调用方时，`grovefs mcp --require-api-key` 打开**可选的 API key 门**（见下小节）——同一套凭据家族现在经 scope 也延伸到挂载/数据入口。接入：客户端配置里把 `grovefs mcp --meta-dir D` 登记为 stdio 服务器即可（`db_path` 也可逐调用覆盖）。e2e 见 `mcp` 套件（77 条断言：握手能力面声明与协议协商/发现（`instructions` 逐个点名十工具且分节、`title`+`annotations`+`outputSchema`）/内联规则换形/路径探查与形状预览/发布前干跑校验/别名轮换/fork 生命周期（含空种子与接枝）/CLI 同文案/`structuredContent` 与文本一致/资源枚举与读取（含 `grovefs://guide`、`grovefs://views`、树查询参数下钻与 `max` 截断）/提示实例化/按 `ref` 角色感知的视图名补全/未知工具建议与缺参提示/日志阈值→通知且 `data` 带工具名/协议错误路径）与 `apikey` 套件（61 条断言），USAGE §3.5。智能体侧的接线手册（英文，便于国际化部署）随仓库一起分发在 `skills/grovefs-mcp/`（`SKILL.md` + `reference.md` + `examples.md`，附零依赖 stdio 客户端 `scripts/mcp_call.py` 与临时存储根上的接线自检 `scripts/smoke.sh`）。

### 给入口上把锁：API key 认证、scope 分区、视图绑定 / 只读与调用审计（`grovefs apikey`）

同一个服务常常要面对**多个不同调用方**——智能体把一个 `grovefs mcp` front 出去，多台客户机经 LAN 挂同一个 `meta-server`——此时需要可撤销的凭据与归属审计。`grovefs apikey` 铸造 API key（**鉴权只认入库的 SHA-256 摘要**，明文在 `create`/`rotate` 回显、库里同时留一份要 store key 才开得动的可逆密文副本，故只拿到 `api_keys.db` 那半份仍换不出可用凭据），并给每个 key 打 **scope**：`--for mcp`（只开治理接口）、`--for mount`（只开 `--require-api-key` 的 meta/data 服务端）、`--for admin`（只开治理端口 `meta-server --control-listen`，同时放行 mount+mcp；`--owner` 必填）、`--for any`（**默认**，两者皆可；未记或不可解析 scope 的行按这一档鉴权）。挂载 key 还可带**两个访问控制维度**：`--view V` 把凭据**绑定到单一物理视图** V——用它 `M_OPEN` 别的视图在握手处即被 `E_AUTH` 拒（系统级 key 省略 `--view` 则可开任意视图）；`--ro` 标为**只读**——服务端在 meta 面对其**逐写意图 op 拒 `E_NOTPERM`**、data 面对内容写拒 `EROFS`，并在 `M_OPEN` 回包提示客户端只读挂载（「只读」由服务端强制，不靠客户端自觉 `-o ro`）。两处入口共用同一份注册表（`<meta-dir>/meta/_control/api_keys.db`），靠 scope 隔离：挂载凭据重放不到治理 API，智能体 key 也开不了数据面。**治理面**：`grovefs mcp --require-api-key` 对所有**读写治理状态的方法**设门——`tools/list`、`tools/call`、`resources/list`、`resources/templates/list`、`resources/read`、`completion/complete`（`initialize` / `ping` / `prompts/*` / `logging/*` 始终开放，好让客户端完成握手并出示 key）。**挂载面**：`meta-server --require-api-key` 卡 `M_OPEN`、`data-server --require-api-key` 卡 `OP_OPEN`（`--meta-dir` 指到元数据根，默认取首个 `--data-dir`），客户端以 `mount --api-key <ID:SECRET>` 呈现（或导出 `GROVEFS_API_KEY` 使密钥不进 argv）。**审计面**：每一次 `tools/call`、`resources/read` 与每一次挂载连接（含被拒的）都追加进同一张表（时间、key、平面 method、工具 tool、成败、详情），`grovefs apikey calls` 回读。这张表**不会自己长到天边**——每个治理调用、每次 MCP 工具调用、每次挂载握手各一行，所以一个住在控制台里的部署即使没人读也在攒它：给服务端 `meta-server --audit-retention 90d`（后台按窗口 1/24 扫、开机先扫一刀再睡，需 `--control-listen`；开着控制口而不给 flag 就是 7d，`never` 才是全部保留）或手动 `grovefs apikey purge --before 90d [--dry-run]` 跑同一刀；purge 是 SUPERUSER 动词，也**故意不做成控制台按钮**——被这张表记录的账号不该是有权修剪它的人。撤销只是把行标记为 revoked（历史调用仍保留归属），撤销后的 key 立即鉴不上，**且已建立的挂载也被封停**。存储是一处本地 SQLite（`<meta-dir>/meta/_control/api_keys.db`），控制面状态。新铸 key 为短格式 `<16位hex id>:<32位hex secret>`（共 49 字符；secret 带 122 位随机），库里存的永远是 SHA-256 摘要（鉴权只认它）加一份 `sbx1.<nonce>.<ct>.<mac>` 的可逆密文副本——**明文从不以可读形态入库**，而 `apikey secret --id X` 能把 `create` 当时打印的那把凭据原样读回。副本要 store key 才开得动：env `GROVEFS_STORE_KEY` 优先，否则 `<meta-dir>/meta/_control/store.key`（0600，首次使用时原子创建）；两处材料都拿不到时该行就不存副本（`secret` 报 never stored），**绝不退化成明文入库**；PG 档则**后端隔离**——运行时既不读也不写 `<meta-dir>/meta/_control/store.key` 那类 SQLite 目录路径，wrap 材料**只认 env `GROVEFS_STORE_KEY`**（因表是共享的，须全部署同一值）；无此 env 即 show-once，`secret`/`rotate` re-wrap 前须在每个进程先导出同一 env（取证 `test_pg.sh` 第 10 节）。读回后仍以摘要比对收尾，故交出来的永远是鉴权路径认得的串。secret 若已不可读回（那一行铸造时手上没有可用的 store key，库里就没存副本），仍可用 `apikey rotate` 为**同一个 id** 重铸（label/scope/视图绑定/归属/历史审计全留，库里那份副本随之换成新的）；长格式 key 照常鉴得上（校验只比摘要、不看长度）。每一行还记着**三笔台账**：`--note` 备注（≤500 字符，超限拒而不截断）、`--ttl` 有效期（服务端算成 `expires_at`，到期后与吊销同等拒，`apikey update --ttl` 是唯一恢复路径）、`created_by` 铸造者；`list` 的 key 列显示的是入库的 `secret_hint`（**头 4 位 + 尾 4 位**指纹），它让一屏凭据能被认出来，而「需要完整明文」这件事由 `apikey secret` 读回那份副本承担（经服务端/控制台的读落 `apikey calls` 审计，CLI 直连本地库的读**不记账**——能打开那个文件的人本来就拥有它）。下列回显取自真实运行：

```
$ grovefs apikey --meta-dir <home> create --label laptop-alice --for mount   # 铸造 <id>:<secret> 并回显（库里同时落一份可逆副本）
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id 3fed9a02db704c3d` reads it back):
  id:    3fed9a02db704c3d
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   3fed9a02db704c3d:9f126bb5f3414955b17089ed89dee714

$ grovefs apikey --meta-dir <home> secret --id 3fed9a02db704c3d   # 事后再读回同一把凭据
apikey secret — the stored copy of this credential:
  id:    3fed9a02db704c3d
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  key:   3fed9a02db704c3d:9f126bb5f3414955b17089ed89dee714

  authentication still runs on the stored digest; this copy is for
  handover.
  a direct read of the local store is NOT audited — whoever can
  open that file already owns it; a read through a meta-server
  (`--control-server`) is, and shows in `apikey calls`.

$ grovefs apikey --meta-dir <home> create --label reader --for mount --view alice --ro   # 视图绑定 + 只读
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id a1d7131e05f74ce8` reads it back):
  id:    a1d7131e05f74ce8
  owner: <none>
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  expires: never (set one with --ttl, or `apikey update --ttl`)
  key:   a1d7131e05f74ce8:e6555ce683554fdd92ad877ad3b63a06   # 这把 key 只能挂 view alice，且写操作一律被服务端拒

# 用绑定/只读 key 挂别的视图 / 写数据都被拒（无需任何按 uid 的授权表）：
$ grovefs mount --view bob -m /mnt/b --manager ... --roster-key-file ~/roster.key --api-key "$READER_KEY"
error: permission denied: this api key is bound to view 'alice', not 'bob'   # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key
bash: /mnt/a/should_fail: 只读文件系统                                               # 客户端据握手标志落内核只读；服务端另逐写拒 E_NOTPERM/EROFS

$ grovefs apikey --meta-dir <home> create --label x --for both               # 打错一个词不静默放宽权限
error: unknown api key scope 'both' (expected any|mcp|mount|admin)

$ grovefs meta-server --meta-dir <home> --listen 0.0.0.0:50110 --require-api-key
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op

# 拒因逐字节随握手回传（不只是一个错误码）：
$ grovefs mount --view alice -m /mnt/a --manager ... --roster-key-file ~/roster.key                      # 不带 key
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --manager ... --roster-key-file ~/roster.key --api-key "$MCP_KEY"   # 拿治理 key 来挂
error: authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> calls --limit 6        # 挂载连接与治理调用同表审计
last 6 call(s), newest first:
ts   key_id   method   tool   ok   detail
...  aac1f51a0d5641dd  meta  M_OPEN   yes  view='alice' uid=1005
...  aac1f51a0d5641dd  data  OP_OPEN  yes
...  -                                 meta  M_OPEN   no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'

$ grovefs apikey --meta-dir <home> revoke --id aac1f51a0d5641dd   # 撤销（行保留供历史归属）
apikey: revoked id='aac1f51a0d5641dd' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

$ grovefs apikey --meta-dir <home> rotate --id aac1f51a0d5641dd   # secret 丢了不换身份：同一 id 重铸 secret
apikey rotated (the new credential's copy is stored — `grovefs apikey secret --id aac1f51a0d5641dd` reads it back):
  id:    aac1f51a0d5641dd
  owner: <none>
  label: laptop-alice
  scope: mount
  view:  <any>
  key:   aac1f51a0d5641dd:<新的 32 位 hex secret>

  the id and every binding are unchanged — only the secret was replaced;
  the OLD secret fails its next handshake, and a live mount established on it
  stays open until it reconnects — `apikey revoke` is the kill-now verb.

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

$ grovefs apikey --meta-dir <home> create --label nightly --for mount --note "staging box, hand over end of March" --ttl 30d   # 备注与有效期一起入账
apikey created (a recoverable copy is now stored — `grovefs apikey secret --id 3dfd73c7b4684edc` reads it back):
  id:    3dfd73c7b4684edc
  owner: <none>
  label: nightly
  scope: mount
  view:  <any>
  rw:    rw (read-write)
  note:  staging box, hand over end of March
  expires: 2026-12-18T14:52:10.242021918+00:00      # 期限由服务端在每次握手时判，过期即与吊销同等拒
  key:   3dfd73c7b4684edc:0609b45f71ce44bf9d0f2fca8d44a89e

$ grovefs apikey --meta-dir <home> list                    # key 列是「头4…尾4」指纹，不是密文；备注/期限/吊销共用末列
#   id                 key          owner            label            scope   view         rw   created                   last_used                 expires / revoked / note
1   3dfd73c7b4684edc   0609…a89e    -                nightly          mount   <any>        rw   2026-11-18T14:52:10.242127448+00:00  -   expires 2026-12-18T14:52:10.242021918+00:00 | note: staging box, hand over end of March
2   a1d7131e05f74ce8   e655…3a06    -                reader           mount   alice        ro   2026-11-18T14:52:10.223378367+00:00  -   never
3   3fed9a02db704c3d   9f12…e714    -                laptop-alice     mount   <any>        rw   2026-11-18T14:52:10.190396303+00:00  -   never

$ grovefs apikey --meta-dir <home> update --id 3dfd --note "handed to ops" --ttl never   # 只动备注与期限；id/绑定/归属/审计一律不碰
apikey updated: id='3dfd73c7b4684edc' (label 'nightly')
  note:    handed to ops
  expires: never

$ grovefs apikey --meta-dir <home> update --id 3dfd --ttl "3 weeks"                      # 期限拼错不静默放宽，也不静默改成「永不过期」
error: bad ttl '3 weeks': expected a count and a unit, like 45m, 24h or 7d
```

无法自定义握手的 MCP 客户端，可由运维方导出 `GROVEFS_MCP_API_KEY='<id>:<secret>'` **预鉴权整个服务进程**（挂载侧对应的是 `GROVEFS_API_KEY`）。诚实边界：这把 key 认证的是**入口的调用方**——它让某个调用方能（或不能）发治理写命令、能（或不能）开某棵视图树，并把每次准入归咎到具体 key，但不改变治理语义本身；视图绑定/只读（`--view`/`--ro`）维度由服务端强制，且只读↔读写可 `set-ro` 原地翻转、在同一 ≤1s 复验 tick 热生效（免重铸免重连）；活连接复证是 **op 驱动**的（有流量才查），故封停边界是“撤销后的首个服务端往返”——完全由内核缓存回答的读不需要往返，因此不会被打断（下一次往返即封）；`--token` 与 `--require-api-key` 可同时存在，两道都得过。不开 `--require-api-key` 时，服务行为与不设门完全一致（只是照样落审计）。e2e 见 `apikey` 套件（61 条断言：铸造/列表/盘上无明文（只有 sbx1 信封）与 0600 `store.key`/`secret` 读回与 create 打印的逐字节相同·未知 id 拒/无 key 拒·错 secret 拒·对 key 放行/调用归属/撤销即失效且行保留/短 id 前缀撤销/env 预鉴权/`list --view` 过滤/默认不设门回归/`set-ro` 原地翻转/短格式 16+32 hex·长格式 key 照样鉴上/`rotate` 保 id 换 secret 且旧 secret 立即鉴不上/`list --label·--owner·--scope` 过滤/`--note·--ttl` 入账与回显/key 列只出指纹且盘上无明文/`list --note` 子串过滤/`update` 改备注与期限（`--ttl never` 清期限、无 flag 拒绝）/过期行标 `EXPIRED` 且 MCP 门 `-32001` 拒/同 key 靠新 `--ttl` 复活/revoked id 拒 `update`/`rotate` 后读回跟随新 secret）与 `mountkey` 套件（76 条断言：scope 铸造与 typo 拒绝/六类拒因入协议/准入与 env 回退（argv 不泄密）/连接审计到具体 key 与视图/scope 双向隔离/撤销封停活挂载且无脏 endpoint/其他 key 不受波及/token 与 key 双门 AND/数据面独立门/故障转移携带凭据/不设门默认路径无回归/视图绑定 key 跨视图拒挂与审计归属/只读 key 服务端强制写拒/`set-ro` 在同一活挂载 rw→ro 拒写·ro→rw 放行·免重连）。

## 网页管理台：浏览器里管视图与密钥、给账号划基视图（`grovefs web`）

远程治理面让任意一台机器都能治理同一份元数据，但入口仍是终端：动词要记、key 要带、拒因要读。高频的三件事——看视图、发密钥、给同事划一块地方——在浏览器里点更省事；而“**这个账号管这块地方，包括它派生出的 fork、别名和这块地方的密钥**”这种归属，一把共享的 admin key 说不清。`grovefs web` 就是治理口前面的一个 HTTP 网关（**独立子命令**，HTTP 不进 `meta-server`），它**不新增一套治理逻辑**：

```bash
# 安装期：第一任管理员只能由持有存储的进程放（仅当账号表为空；口令只有 file/stdin，没有 --admin-password）
$ grovefs meta-server --meta-dir .webdoc/srv --listen 127.0.0.1:15698 --control-listen 127.0.0.1:15699 \
    --admin-user rootops --admin-password-file ./root.pw
meta-server: web accounts — created the first web account 'rootops' (role admin) from the install-time flags — sign in with it, then delegate base views
meta-server: web console sign-ins are verified HERE (`grovefs web --control-server 127.0.0.1:15699`); accounts and their base-view delegation live in meta/_control/web_accounts.db

# 网关自己不存任何东西：它需要治理口地址 + 一把 --for admin 的账号密钥
$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key --listen 127.0.0.1:15687
grovefs web: management console on http://127.0.0.1:15687/
  control plane : 127.0.0.1:15699 (backend sqlite)
  sign-in       : accounts of that server's web store; the first owner comes from its
                    --admin-user flag
  sessions      : 8h idle window, 4 worker thread(s)

# 建号、划地方（这三条在页面 Accounts 页里点也一样，headless 与浏览器走同一套 op）
$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key account create --account bob --password-file ./bob.pw
web account created: bob
  role:  user
  bases: none — it can sign in and govern nothing yet
  note:  delegate a base view before this account can manage anything

$ grovefs web --control-server 127.0.0.1:15699 --api-key-file ./control.key grant add --account bob --base main
bob now governs 'main' and everything derived from it, as granted by rootops.
That covers its forks, aliases and lenses, and the keys bound to any of those names.
```

授权的对象是**基视图**而不是叶子：此后这棵基树下新长出的 fork、改的别名、加的透镜全部自动在授权范围内，绑在这些名字上的密钥也归它管——这正是“给一块地方”与“给一个名字”的区别。`bob` 登录后拿到的是自己的 reach（`{ "account": "bob", "role": "user", "bases": ["main"] }`），能列自己的视图、建 lens/alias、`fork` 基视图、看 `view.stats`（它的 `family` 含自己的 fork）、只给自己的视图铸 `--for mount`/`--for mcp` 密钥并可 `set-ro`/轮换/撤销/改备注与期限；超出自己那块的尝试，回的是**服务端的原句**而不是网关编的：

```json
{ "ok": false, "error": "account 'bob' is not an administrator, and 'view.create' is not per-view governance: a new base view picks a storage home (PG instance / data root), which is a deployment decision — an admin must run it (or grant you the base view it acts on)" }
{ "ok": false, "error": "'gc.scan' is not available from the console: it stays an operator CLI command, where its dry-run report and confirmations are part of calling it" }
```

诚实边界：网关手里持着一把 `admin` scope 的 key，能读到它配置的人已经治理得了整个部署——所以它默认只听 `127.0.0.1`（要绑可路由地址得显式 `--allow-insecure-http`，否则它在绑端口之前就拒绝启动并给出反代配方），**登录不产生能力，只限制并归属能力**：一个会话只能以一个账号的身份行动，而每一次调用都以 `account@ip` 落审计。会话是 `gfs_sid`（`HttpOnly; SameSite=Strict`）+ 写操作要带 `X-GroveFS-CSRF` 且 `Origin` 主机部分必须等于 `Host`；同一客户端 IP 在 15 分钟内错满 5 次就 429（基础 30s、每多错一次翻倍，**即使那次口令是对的**），“无此账号”与“口令错”回复逐字节相同。路由白名单写死 32 个 op，`gc.scan`/`gc.apply`/`refs.rebuild`/`view.rm_base`/`fork.graft` 不过路由。`grant rm` 不必等会话过期——服务端每请求重读主体，撤权后**下一个调用**就撞墙。`account rm` 连授权一起删，但**它的 api key 故意留着**（删号不该顺手停掉一个在跑的生产挂载，回显会报有几把待处理）。e2e 见 `web` 套件（196 条断言：含经页面路由 `apikey.rotate` 轮换同一 id 的 secret 与跨账号拒旋、`apikey.update` 改备注与期限、过期行由服务端标 `expired:true` 且同 key 靠新 `--ttl` 救活、台账字段（`note`/`created_by`/`secret_hint`）出 HTTP 回包、存储的指纹恰等于创建明文的头 4 尾 4、经页面路由 `apikey.secret` 读回属主自己的副本（与 rotate 回包逐字节相同、读回不往盘上添加明文、跨账号拒）、`store.info` 带出 `pgInstances` 键（SQLite 档 null、多实例档为实例账本——名字、每实例放置视图数与控制面标记，放置下拉与总览页的服务端契约）、`backendLayout`（总览页据此在非 PG 档才叫「元数据根」）、“路由白名单恰好 32 个 op”的计数断言、下发 bundle 实含跨页跳转串（key↔视图、审计↔实体）两档词典）；命令、五页能力表、全部拒因原文与 systemd/反代配方见 `USAGE.md` §3.5.3 与 §11.4、`docs/deploy.html` §2.4。

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

$ grovefs data-server --listen 127.0.0.1:50100 --data-dir .demo4/home &
data-server listening on 127.0.0.1:50100 (host lc, 1 data root(s): .demo4/home)
$ grovefs meta-server --meta-dir .demo4/home --listen 127.0.0.1:50053 &
meta-server listening on 127.0.0.1:50053 (base .demo4/home, role Primary)

$ grovefs mount --view legacy -m .demo4/mnt --manager 127.0.0.1:50055 --roster-key-file ~/roster.key &   # 客户端经 manager 解析坐标即得这棵树
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

语义要点（`import` 套件 35 条断言）：**同文件系统零拷贝**（对象与源文件共享 inode，`df` 看不出差异），跨设备自动退化为独立拷贝并如实计数；**双向直通**——经挂载写、或外部直接改源文件，另一侧都在客户端 TTL（≤2s）内可见；**GC 只摘链接**——视图里删名字仅 unlink `blocks/` 那条路径，源路径与 inode 不受影响；**幂等增量**——重跑只登记新名字；mtime/atime/权限/属主保真（`--dry-run` 连视图 db 都不创建）。导入后的条目与原生写入的条目完全同权：读写/锁/快照/失效推送全部适用，还能被派生视图 `map`/`link` 组成任意虚拟目录形状——该形状挂载**只读**（写走导入的原视图，推送即时可见；`grovefs import` 也拒绝向派生名导入，防写出无人可读的影子库）。运行位置与 `grovefs view` 相同：在持有存储根（`--home` 或 meta/data server 的 root）的机器上执行。

## 生产拓扑：中心存储 + 多客户机 LAN 挂载

```bash
# ===== 中心节点：声明拓扑 + 起引导服务 + 两个服务上报 =====
$Y manager --state-dir /srv/mgr define --name prod --meta 10.0.0.5:50110 --data 10.0.0.5:50100
$Y manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --member-id 0 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --register-with 10.0.0.5:50055 --join-key-file /srv/mgr/manager/join.key

# ===== 每台客户机（远程挂载不需要 --home：权威数据、白名单、缓存全在服务端，客户机本地不落盘；两个坐标只从 manager 的签名 roster 解析，不手敲 IP:PORT）=====
$Y mount --view alice \
     --manager 10.0.0.5:50055 --deployment prod --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

| 选项 | 作用 |
| --- | --- |
| `--manager host:port` | 挂载**唯一**的引导入口：客户机只指这一个地址，元数据/数据服务坐标全部由它的签名 roster 解析（手敲 `--meta-server`/`--data-servers` 已从 mount 移除） |
| `--roster-key-file PATH` | roster 验签 key（64 hex，与 manager 自签同一把）：验不过即 fail-closed 拒挂，绝不降级成警告后照连 |
| roster 的 meta 列表 | 解析出的是**故障转移列表**（主挂自动切备），语义同旧 `--meta-server 主,备`；视图由中心 `meta-server` 提供 → 多客户机挂同一视图 = 共享同一棵权威树 |
| roster 的 data 列表 | 解析出的是**完整条带成员集**（逗号多地址即跨 server 确定性条带）；成员不是候选池，短列表会被拒 |
| `--data-dir /d1,/d2` | data-server 的逗号分隔多存储根 → 机内多盘自动放置（整对象按 key 哈希选定一条盘；落盘位置是 key 哈希 mod 根数现场重算，列表与对象都不记录盘序号，故等盘克隆**原位顶替**照常定位；但增删盘或调换 `--data-dir` 顺序会重映射所有 key 使旧对象不可达，扩容宜一次性配齐多根或用 LVM/mdadm 合盘） |

### 挂载引导：坐标在服务端声明一次，客户机只指一个地址

上面的两个坐标都是 **provisioning 事实**（operator 定的常量，不是运行时涌现量），所以可以只写一处、签名分发：

```bash
# 中心节点：声明拓扑（写入 <state-dir>/manager/deployments.json），再起引导服务
$Y manager --state-dir /srv/mgr define --name prod --meta 10.0.0.5:50110 \
     --data 10.0.0.5:50100,10.0.0.6:50100
$Y manager --state-dir /srv/mgr serve --listen 0.0.0.0:50055

# 节点：起服务时顺手注册（连不上 manager 也照常服务，退避重试）
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --member-id 0 \
     --register-with 10.0.0.9:50055 --advertise-addr 10.0.0.5:50100 \
     --join-key-file /etc/grovefs/join.key --deployment prod

# 每台客户机：只指 manager
$Y mount --view alice --manager 10.0.0.9:50055 --deployment prod \
     --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
```

| 要点 | 行为 |
| --- | --- |
| roster 内容 | operator **声明过的那一套**，不是「此刻活着的那一组」；`--require-registered` 是预检门（整刀拒 lookup），开了也不会收缩 roster |
| 两把 key | join key 授权**注册**（只有服务端持有，客户机永不持有）；roster key 签**应答**（manager 签、客户机验） |
| 防伪 | 未声明的地址即使 HMAC 正确也被拒收；`manager status` 逐地址显示谁注册了、报了什么 member-id |
| 故障边界 | manager 宕机不影响已有挂载（数据面从不经它）；新挂载验不过就拒挂（fail-closed） |
| 代价 | 拓扑上多一个需全集群可达的端口；延迟上每次挂载 +1 次 TCP 往返 + 一次本地 HMAC 验签；IO 路径增量 0 |

完整命令、逐字回显、否决项与残余风险见 USAGE §4.2 与 `docs/adr/0001-manager.md`。

## 跨客户端一致性

| 模式 | 命令 | 行为 |
| --- | --- | --- |
| **实时失效推送**（默认，推荐） | 远程挂载（经 manager 解析出 meta 服务） | 一客户端写成功后，meta-server 经事件总线向同用户其它订阅者广播失效（`inval_entry`+`inval_inode`），对端下次访问**即时可见**，无需轮询 |
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
# 客户机：故障转移列表来自 manager define --meta 主,备，roster 原样下发（客户机不手敲坐标）
$Y mount --view alice --manager 10.0.0.9:50055 --deployment prod --roster-key-file /etc/grovefs/roster.key -m /mnt/alice
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
$Y mount --view alice --manager ... --roster-key-file ~/roster.key --token s3cr3t -m /mnt/alice
```

token 不匹配 → 连接时直接 `PermissionDenied`，不重试。空 token（默认）= LAN-trusted 无鉴权，仅限可信内网。

**可撤销的 per-caller 凭据**（同一入口，但逐调用方可审计、可踢人）：token 是一份共享密钥——拿到它的主机都能挂，也分不出「是谁挂的」，要踢一个调用方只能轮换 token 并断全部人。改用 `grovefs apikey` 铸造的 key 做门（`--for mount`，只开挂载/数据入口）：

```bash
$Y apikey --meta-dir /data/cluster create --label client-a --for mount      # 回显明文，库里落摘要 + 可逆副本
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
$Y mount --view alice --manager ... --roster-key-file ~/roster.key --api-key '<id>:<secret>' -m /mnt/alice   # 或 export GROVEFS_API_KEY 使密钥不进 argv
$Y apikey --meta-dir /data/cluster calls --limit 20                          # 每次挂载连接（含被拒）都归属到具体 key
$Y apikey --meta-dir /data/cluster revoke --id <id>                          # 封停该 key 的**活**挂载（秒级）
```

与 token 的两处关键差异：① 拒因逐字节随握手回客户端（缺失/未知 id/密文不符/scope 不符/已撤销各自成文），且服务端**每秒复证活连接**——撤销后该挂载在首个服务端往返上被打断、打印服务端原文并干净自卸载（不留下 `Transport endpoint is not connected`），其他 key 的挂载不受影响；② 连接与治理调用同表可审计。`--token` 与 `--require-api-key` 可同时开（两道都得过）；不开时行为与不设门完全一致（仍落审计）。详见 `USAGE.md` §3.5.1/§6。

**只读挂载**：`mount ... --ro`，所有写操作返回 `EROFS`（内核 `MS_RDONLY` + 文件系统侧守卫双保险）。

**性能旋钮**：

| 项 | 说明 |
| --- | --- |
| `--threads N`（默认 4） | FUSE 并发派发工作线程，每线程独立 `/dev/fuse` fd；`0`=单线程（A/B 对照） |
| 分块并行 IO | 客户端读 ≥1MiB 按 1MiB 切片并发；写 ≥4MiB 分片并发 |
| 大文件条带 | 文件 ≥16MiB 才分块；`data_ref` = `s<N>:<stripe>:<uuid>`，chunk i 落 `servers[i%N]`；无前缀 = 单对象（未启用条带的存储形态） |
| touch 去抖 | 元数据 mtime 只有 epoch **秒**精度，所以 `write()` 只在「当前秒 ≠ 该 inode 记着的秒」时才写一次 `mtime/ctime`；新建文件把创建那一秒直接记入（同 秒首次写不再 touch），流式写 1000 次/10ms 只 touch ~10 次。close 时内核回填的 `SETATTR` 也享同一条口径：回填的 mtime 落在已存的同一秒就不落库，回填的 size 与对象当前长度相同就不 ftruncate——一个「建→写→关」的小文件因此只付一次 create 事务 |
| 元数据读连接池 | 每个视图的 SQLite 引擎在单条权威写连接之外持一池暖读连接：默认**暖 1 条**（`READ_POOL_SIZE = 1`，一条空闲读连接约 3 个 fd）、突发**保留至 8 条**（`READ_POOL_RETAIN = 8`，实测收益在 ~8 个并发读者之后压平）；env `GROVEFS_SQLITE_READ_POOL` 抬的是**暖连接地板**，保留天花板取两者较大，`0` 完全关池。`lookup`/`getattr`/`readdir` 与目录 nlink 解析都走这条只读池，元数据**读**不必排队等写锁（SQLite 的**写仍串行在同一权威连接上**，单写者语义不动；PG 后端写另走 SERIALIZABLE 写池并行，见下文 PG 诚实边界）；池空时按需开、用完归还，超过天花板就关掉。挂载进程里没有引擎，所以这条池属于 `meta-server` 的每一个视图引擎，以及把库嵌进自己进程的调用方——服务端并发的 RPC 处理器各走自己后端的池、互不等待。PG 引擎持对等的只读连接暖**地板**（`PG_READ_POOL_SIZE` = 4，env `GROVEFS_PG_READ_POOL`；`GROVEFS_PG_READ_POOL_MAX` 只定突发保留上限，默认 = 地板，§6.2），每条读连接缓存自己的 prepared statement（≤16 条）；实测（本机回环、PG 后端的 `meta-server` + `data-server`、`--no-cache` 下连续 `stat`）**0.358–0.360 ms/op**（≈2.79k/s，脚本 `.build-bin/probe_pg_read.sh`）。写仍各走单一权威连接（SQLite `conn` 串行 / PG 8 连接 SERIALIZABLE 写**地板**，env `GROVEFS_PG_WRITE_POOL`，突发保留上限 `GROVEFS_PG_WRITE_POOL_MAX`） |
| 写语句缓存（SQLite） | 引擎把 `create` 的「本视图是否已有该 key」`COUNT`/`INSERT`/`ino` 回填、`unlink` 的查行/删行/引用计数、账本 `holders` 的增减都缓存到连接上（`prepare_cached`），同一段 SQL 不再每行现场解析；PG 侧对等机制见下文 PG 写语句缓存。**一次挂载的一个 create/unlink 就是一次 RPC、服务端一次提交**：客户端不并批，因为它不持有权威树、也无从把 N 个请求折成一笔事务。`MetaEngine::create_nodes_batch`/`unlink_nodes_batch` 那套「一笔事务提交 N 行」因此是**引擎级 API**（给把库嵌进自己进程的调用方；引擎级微基准，同一事务、同一批语句，N=1200：单条 create **83µs/行**、组 3 **38µs/行**、组 8 **25µs/行**）。删除侧同一挂载点、单线程 `rm` 2000 个小文件实测 **3,013 文件/s**（`meta.unlink` p50 **101µs**、mean 126µs），并行侧见下一条 |
| 并行删除 | 挂载上没有客户端组提交（见上一条），并行删除的收益来自**提交在服务端重叠**：SQLite 的写仍串行在同一权威连接上，但 WAL 让并发提交的 fsync 互相重叠，每 op 也不必再等别人那一笔。同一挂载点、`--threads 16`、每线程一个父目录、每线程 400 个文件（python 驱动）：T=1 **2,082** / T=2 **4,270** / T=4 **5,062** / T=8 **5,120** / T=16 **5,767** 文件/s。这一档与上一条的 3,013 文件/s 来自不同的驱动形状（父目录新建、批量长度、进程内循环），只比同档内的斜率。批事务作为引擎能力仍然成立：同一事务删 N 行（N=2400）批 1/2/4/8/16 是 **53.9 / 30.0 / 18.7 / 13.2 / 10.8 µs/行**。脚本：`.build-bin/probe_unlink_cluster.sh` |
| PG 写语句缓存 | 每个 PG 写连接缓存 ≤32 条 prepared statement（`WRITE_STMT_CACHE_CAP`：`INSERT`/按名 `lookup`/`DELETE`/`rename`/`setattr`/锁 op/三条账本语句）；`postgres` 0.19 无 `prepare_cached`，故自建 per-connection 语句表，事务体内 SQL 零改动即命中，失效（`0A000`）时驱逐并回落 unnamed。单连接一次事务 577–670µs → 316–330µs；台架写吞吐 N=1/4/8 3,048–3,118 / 4,784–5,202 / 4,518–4,680 ops/s |
| 一次 create 的成本构成 | 集群档（本机回环上的 `meta-server` + `data-server`）、N=2000、同一 fresh 视图三臂对照（`--stats` 在客户端采样，读到的是端到端往返）：**建一个只命名不写字节的文件 0.281ms/op**（3,560 ops/s，`meta.create` p50 127µs、`remeta.rpc` p50 86µs）、**`mkdir` 0.211ms**（4,741 ops/s，`meta.create` p50 117µs）、**`utime` 0.391ms**（2,561 ops/s，`meta.setattr` p50 109µs）。前两条每 op 两个 RPC（`M_CREATE` 回包内联刚提交的行，省掉补一次 `M_GETATTR`），`utime` 四个（`lookup` + 两次 `getattr` + `M_SETTIMES`）——所以 `mkdir → utime` 的差主要是**往返次数**，而 `file − mkdir ≈ 70µs` 才是 data_ref 命名 + 跨视图账本那笔服务端工作。**create 不碰数据面**：名字落库即返回，后端对象由首次 `write`/`truncate` 现开（各自的 `O_CREAT`），所以「只建不写」的创建（`touch`、空文件树 `cp -r`）既不付一次 open/close、也不占一个缓存 fd。语义不变量不变：未写过的文件 `getattr`/`read` 都是 0 字节、`fsync` 成功、`unlink` 只删名字（账本里没有对象可回收）。「只建不写」的并发曲线（同一挂载点、每线程一个父目录、`--threads 16`，files/s）：T=1 **3,201** / T=2 **6,329** / T=4 **8,683** / T=8 **4,846** / T=16 **4,640**——T≤4 是提交重叠的净收益，T≥8 起单写连接与 RPC 线程排队把增益抵掉；把服务端的暖读地板抬到 8 不改变这个形状（同臂 **3,213 / 6,344 / 4,864 / 6,101 / 4,337**），因为瓶颈在写侧、不在读池。**SQL 不是大头**：同卷 raw SQLite 每次提交 WAL+NORMAL p50 46µs（均值 404µs，尾刺来自周期 checkpoint 的 fsync）、`synchronous=OFF` 30µs、`FULL` 16.7ms、`journal_mode=DELETE` 20.5ms，同代码放 tmpfs 全部塌到 ~26µs。故**不提供** `synchronous=OFF` 之类降耐久旋钮。脚本：`probe_sqlite_create_breakdown.sh`、`probe_wal_modes.sh` |
| 目录分页 + READDIRPLUS | `readdir_page` 下推 `LIMIT/OFFSET`，回复缓冲按几何块（READDIRPLUS 32→1024、普通 READDIR 128→1024）填充；默认协商 `FUSE_DO_READDIRPLUS`，`--no-readdirplus` 可关。客户端一侧由 `M_READDIRP` 帧把 `offset/limit` 下推给服务端，每个内核缓冲只取一页。分页能力不是猜的：`M_OPEN` ack 的第 2 个字节就是服务端**活引擎**报的 `supports_paged_readdir()`，缺这个字节的应答按协议不符直接拒掉握手（一个「以为可写」的只读挂载比一次清楚的握手失败糟糕得多）。当前每一种引擎都在存储层分页——SQLite/PG 下推 SQL 页、treedb 用引擎自己的 offset/limit、派生/透镜（`LayoutMeta`）物化形状后切片——所以 `--no-readdirplus` 实际只影响 PLUS 本身。实测见「元数据读并行」一节，复跑 `.build-bin/probe_readdir_cluster.sh` |
| PG 提交耐久 | `GROVEFS_PG_SYNC_COMMIT=off`（默认 `on`）让写连接会话级 `SET synchronous_commit = off`：同台架（写语句缓存已在位）PG N=1/4/8 由 3,305 / 5,225 / 4,284 变为 3,194 / 6,142 / 4,180 ops/s——唯一稳定收益是 **N=4 的 +18%**（更早一轮同为 N=4 独赢 +29%），N=1/N=8 在单点抖动内。代价：崩溃可丢最近若干已提交事务，配了同步备机时也不再等确认；SSI、账本与治理不变量不变 |
| 远端建名往返 | `M_CREATE`/`M_SYMLINK` 回包内联刚提交的行，客户端直接填 FUSE 回复，一次 create 只付 `LOOKUP` + `M_CREATE` 两个往返：1500 次顺序 create 的 `remeta.rpc` 每 create **2.003**、**0.360–0.407ms/op**；回包里那一行是服务端 best-effort 读回（`node_reply` 的 `getattr` 未命中就没有尾段），此时客户端自己补一次 `M_GETATTR`——省不掉的那一次往返是正确性代价，不是版本开关 |
| 分阶段延迟遥测 `--stats` | 把读/写/元数据 op 各阶段延迟汇成分位数离线排瓶颈；**默认关闭、零成本**（埋点仅一次原子读短路），仅 `--stats` 才采样，直方图按线程分片；退出 `stderr` + 可选周期文件 + 只读节点 `.grove-stats`。详见 §8.1（USAGE.md） |
| 后台 WAL checkpoint | 每个可写视图引擎一条后台 `wal_checkpoint(PASSIVE)` 线程（每 1s，~30s 一次 TRUNCATE）+ 写连接 `wal_autocheckpoint=0`，把提交里的同步 checkpoint 停顿移出写热路径；经 `Arc<StopSignal>` 随引擎 `Drop` 关停，只读/备机不启动。热读路径另 `prepare_cached` 复用语句。实测同 3000 文件创建 `meta.create` total 0.38s、max 33ms |
| 读缓存 TTL `--cache-ttl` | 把内核 attr/dentry 缓存 TTL 从默认 2s 放大，削减读多/不可变树的重复 LOOKUP/GETATTR；跨端新鲜度仍靠默认 `notify` 失效推送，新内核（≥3.15）协商 `AUTO_INVAL_DATA`（size/mtime 变则丢陈旧页）保证安全，`--no-cache` 恒钉 0。与 `max_write`/`readahead` 按 `FUSE_MAX_MAX_PAGES` 自动截断、`PARALLEL_DIROPS`/`WRITEBACK` 能力协商同属“新内核快、老内核降级”（详见 §8.2 USAGE.md） |
| 内容带宽整形 `--bw-read` / `--bw-write` | 给本挂载的内容读、写各上一道令牌槽（GCRA 排程：按字节铺开互不重叠的时间槽，长跑速率精确等于上限）：24MiB@8MiB/s 约 3 秒读完，单个大于 1 秒额度的 op 最多提前 1 秒放行；超额只**延迟**回复、绝不丢字节，写侧延迟即背压。`0`=不限（默认）；env `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE` 回退，非法速率在解析处即拒。**诚实边界**：客户端自限速，不是隔离——绕开 FUSE 直连 data-server、或多挂载同视图各拿满额度；元数据 op 不整形（详见 §8.2 USAGE.md） |

## 备份与迁移

元数据**不绑定任何硬件**：`node` 行只存 `data_ref`（对象 key），对象的物理位置由 key 纯函数推出——`blocks/<FNV1a(key)%256 两 hex>/<key>`，没有绝对路径、设备号、主机名或宿主 inode。GroveFS 自己的 `ino` 也在 DB 里，与宿主文件系统无关。所以换机器、换系统、换目录都能直接用，只要把**元数据**和**内容字节**两半一起带走：

| 要迁的东西 | 位置 | 说明 |
| --- | --- | --- |
| 内容字节 | 存储根的 `blocks/`，也就是 `data-server --data-dir` 下的 `blocks/`（最小集群里两者同根） | 普通文件，`rsync -a -H`/快照直接拷；桶布局由 key 纯函数决定，拷贝后无需重建索引 |
| 视图元数据 | `<meta-dir>/meta/<view>/<view>.db` | 每个视图一棵独立 SQLite 树；**勿在有写活动时裸拷**（WAL 可能未合并） |
| 跨视图账本 | `<meta-dir>/meta/_control/refs.db` | 记录每个对象被几个视图持有；导入/冷拷后可用 `grovefs refs --meta-dir D rebuild` 重算 |
| 视图定义 | `<meta-dir>/meta/views/*.layout` | 派生/别名/fork 的纯文本定义，无主机路径，随控制面一起拷（PG 后端：`grovefs_control.view_defs`，随 `pg_dump` 走） |
| API key / 审计 | `<meta-dir>/meta/_control/api_keys.db` | 用了 `apikey` 才需要；密钥存 SHA-256 摘要 + secret 的可逆密文副本（开副本要 `<meta-dir>/meta/_control/store.key` 或 env `GROVEFS_STORE_KEY`；PG 后端：`grovefs_control.api_keys`/`mcp_calls`，随 `pg_dump` 走；PG 运行根本不落本地 `store.key` 文件可一起冷拷——wrap 材料是全部署统一的 env `GROVEFS_STORE_KEY`，须单独备份该密钥（后端隔离，见 `test_pg.sh` 第 10 节） |

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
# 起 manager 并声明拓扑，两个服务向它注册（同一个存储根 = 最小集群），客户机经 roster 解析坐标挂载
grovefs --home /data/dst manager define --meta 127.0.0.1:50053 --data 127.0.0.1:50100
grovefs --home /data/dst manager serve --listen 127.0.0.1:50055 &
grovefs meta-server --meta-dir /data/dst --listen 127.0.0.1:50053 --register-with 127.0.0.1:50055 --join-key-file /data/dst/manager/join.key &
grovefs data-server --data-dir /data/dst --listen 127.0.0.1:50100 --member-id 0 --register-with 127.0.0.1:50055 --join-key-file /data/dst/manager/join.key &
grovefs mount --view team -m /mnt/team --manager 127.0.0.1:50055 --roster-key-file /data/dst/manager/roster.key   # 树、硬链接、权限、属主、符号链接原样
```

`view import` 的守卫：目标已有树必须显式 `--replace`；目标名已有定义槽（派生/别名/fork）先 `view rm`；表头格式/版本、根节点、悬挂父节点、表头计数与正文不符都会整单拒绝。要恢复 fork 语义（结构可写、内容只读）加 `--as-fork <基视图>`，导入后会写入 `fork <基视图>` 定义并跨视图共享字节。

**treedb 后端换一种备法**（`--meta-store treedb://host:port`）：这一档的元数据**分两半**：视图树与「有哪些视图」在 `treedb-server` 的 `--dir` 目录里（一份快照 + 一条 WAL；那串 URI 只是一条 TCP 坐标，`<meta-dir>/meta/<view>/<view>.db` 在这种部署下不存在），而 apikey / web 账户与授权 / 别名·fork·lens 定义**仍在服务端本地的 SQLite**（`<meta-dir>/meta/api_keys.db`、`web_accounts.db`），不随引擎备份走——所以要冷拷的是**引擎目录 + `<meta-dir>` 两样**，只拷一样会丢另一半。可用形状三种：停服后 `cp -a` 引擎目录（优雅停服自带最后一次 checkpoint，RPO = 停服那一刻）；`treedb-backup --primary HOST:PORT --out F.dump` 从**在线**主取一致切面（不停服、不必先 checkpoint，`--out -` 可接 gzip）；以及让服务端从第一天起带 `--archive-dir`，之后 `treedb-pitr --dir D --archive A --to-seq N --out NEW` 能回到两次 checkpoint **之间**的任意一条 op 末尾——这是唯一落在 checkpoint 之间的形状。**没有增量备份、没有加密备份、没有归档保留策略**（归档只增不减，清老段之前要确认被当锚点的快照还在）。两条坑与可照抄的命令在 [USAGE.md §12.3](USAGE.md)：归档开关必须常开（中途关掉会在归档里留洞，PITR 越洞报缺口而不拼历史）、换代必须服务端与接入方**成对**升（`treedb-migrate` 只抬目录格式不抬线协议；不开认证时跨代混跑**不报错、只读回垃圾**，要这道闸门就开 `GROVEFS_TREEDB_KEY_FILE`）。**照抄之前先看 pin**：`treedb-backup` / `treedb-dump` / `treedb-load` / `treedb-pitr` / `--archive-dir` 都是引擎 v0.6.0 才有的，`--promote` 要 v0.4.0，而本仓 `master` 目前仍钉 `v0.3.7`（那一版只有冷拷与副本两形），集成前沿在 `feat/treedb-hardlink`（已钉 v0.7.0）—— 版本以 `Cargo.toml` 里 `treedb = { ..., tag = ... }` 那行为准。引擎侧三种形状都实测过（treedb DESIGN §6.7 / §6.10 / §6.11）；本仓端到端也跑通了：`bash .build-bin/test_treedb_backup.sh` 起真集群（带归档的引擎 + 真挂载写文件），三种形状各恢复成新集群再挂载，60 条断言全绿（2026-11-24，rev `3b2da1b`，引擎 pin v0.7.0）。

**规模实测**（默认 IO，可用 `bash .build-bin/perf_migrate_scale.sh` 复跑）：迁移一棵 5,052 节点、含 5,000 个对象的视图，`export` **0.07s** 产出 1,161,499 B 的 JSONL（= 视图库文件 1,056,768 B 的 1.10 倍，约 230 B/节点），内容字节那一半 `rsync -a -H blocks/` 落位 0.1s，`import` 到另一个绝对路径的存储根 **0.53s**（含跨视图账本重建），把新根交给一对 `meta-server` + `data-server` 后挂载、**0.22s** 枚举完 5,000 个文件且内容逐字节一致、权限与属主原样（`664 uid=1005`）。作为对比，把同一批文件注册进源视图（零拷贝导入，逐对象工作）花了 1.2s——**迁移代价由命名空间大小决定，不由数据量决定**，字节那一半另走 `rsync -a -H blocks/`。

**迁移时唯一需要注意的“拓扑”约束**（不是硬件绑定，是数据分布）：

- **多盘 `--data-dir` 分盘**：对象按 `FNV1a(key) % root 数量` 选盘，目标机 root 的**数量与顺序必须一致**；想合并成单盘也可以（把各盘 `blocks/` 合并即可，单 root 下所有 key 都命中同一棵）。
- **跨 server 条带对象**（`data_ref = s<N>:…`）：第 i 块在 `servers[i % N]`——**是列表下标，不是地址**。目标仍需 N 个 data-server，但两种错配后果不同：**数量不符会被明确拒绝**（不会静默读到错的 chunk）；**顺序错位检不出来**（守卫只比 N），接手那台缺它现在被派到的 `{uuid}#{i}`，症状是那批文件读失败。由此得到一个常见运维结论：**换机器 / 换 IP 本身不是约束**——只要在 `-o data=` 里**原位替换**那一项就能自动识别，元数据不用改、已写对象不需 `rebal`（轮转放置 `i % N` 只决定新写入对象落哪台）。
- **HA 的 `epoch`** 存在每个视图 `ctrl` 表里；整目录拷贝会把它带过去，迁移后**旧主必须停止**（或用新的存储根重新初始化），否则旧主仍在线可能造成双主。`view export/import` 与 `view fork` 有意不复制 `ctrl`，即新树从干净的 epoch 开始。
- **PostgreSQL 后端**不能文件拷贝：整库迁移用 `pg_dump`/逻辑复制；单视图迁移用 `view export/import`（两档后端通用，PG 档经 `--meta-store` 直连解析到的实例）；多实例间搬已存在视图用 `view move`（metadata only，内容字节留在各自 data 域，USAGE §6.2）。
- `uid/gid` 是数值元数据（advisory），换系统后按数字保留；symlink 的目标文本属于用户数据，原样保留，不会因为换机器被改写。
- **`st_ino` 随库走，`st_dev` 不随**：`node.ino` 是 GroveFS 自己的编号（存在 DB 里，硬链接共享它，`fork` 与 `export/import` 连号原样复制），迁机器、换目录都不变；但 `st_dev` 是 FUSE 每次挂载新分配的，所以按 `(dev, ino)` 二元组去重的备份/同步工具在重挂或迁库后要重建映射。
- **拷完怎么确认没漏**：对象名是 UUID（**不是内容哈希，也不按内容去重**），读侧没有校验环节，漏拷只能主动比——树上引用的键数对 `grovefs gc --meta-dir D --data-dir R` 输出里的 `matched`，`matched < refs` 就是有键没有对象（漏拷或 `--data-dir` 列表不对的典型症状）；`rsync -a` 保 mtime（`gc --older-than` 靠它判宽限），用过 `import` 的存储必带 `-H`。

**多个基视图**：可以建任意多个基视图，每个一棵自己的 `meta/<name>/<name>.db`，树、字节引用、权限和删除都各自独立（`grovefs view --meta-dir D create --view A`，`view list` 会把未定义的裸基视图全部列出）。`view export/import` 也是**按视图**进行的，迁移时可以只搬其中一棵；不同基视图可以绑定不同的 API key（`apikey create --view A`），互不影响。

## 选一种数据库，照一条路走

GroveFS 元数据两档：SQLite 与 PostgreSQL。选定一种就照着**一整条完整路径**（部署 → 使用 → 运维 → 验收）走即可，不必在章节间跳转：

- **SQLite（默认）**：零依赖单二进制，单写者、原生主备 HA。完整路径：[USAGE.md §15.2 / §15.17.1](USAGE.md)，网页版 [docs.html §6.9.1](docs/docs.html#case-backend-paths)。
- **PostgreSQL**：并行写（每视图 8 写连接暖地板，突发上限与空闲收回经 `--pg-*-pool[-max]`/`--pg-pool-idle-secs` 可调），HA / 备份 / PITR 交给 PG 自身；连接数封顶时上多实例注册表 + 放置（`--meta-store file:…` + `view move`）。完整路径：[USAGE.md §15.3–§15.4 / §15.17.2](USAGE.md)，网页版 [docs.html §6.9.2](docs/docs.html#case-backend-paths)。

两档只有 **3 处不一样**：原生 HA、目录零拷贝入库 `grovefs import`、就地 `gc` 仅 SQLite；其余命令同名同参，只把寻址旋钮 `--meta-dir` 换成 `--meta-store <URI>`，客户端挂载命令两档逐字相同。选型依据见 [docs.html §4.1](docs/docs.html#prod)，逐命令对照见 [docs.html §22.1](docs/docs.html#ref)。

## 部署与维护（手把手）

完整的分阶段教程在 **[USAGE.md](USAGE.md) §11–§14**（网页版 [docs.html](docs/docs.html) 同步覆盖，并按读者阶段重排为「零基础→专家」四档：新增零基础预备知识、内部机制与扩展开发、性能调优方法论三章，支持自动/浅色/深色阅读主题）：§11 从环境预检 → 构建安装 → 拓扑规划 → token → systemd 常驻（中心双服务 + 客户端模板挂载单元）→ 防火墙 → 8 步验收清单 → HA / PostgreSQL 变体；§12 给日常巡检（日志关键字→处置对照表）、备份恢复（SQLite / SQLite+HA / PG 三流程）、滚动升级、故障速查表与变更后回归；§13 是一组**固定 IP 的完整案例实操**（存储机 192.168.1.10 存 /data、SQLite 后端，客户端读写挂 /mnt/da、只读挂 /mnt/db），手把手逐参数解释；§14 进阶回答“数据目录怎么定制/存量大数据怎么导入（首选 `grovefs import` 零拷贝登记，备选经挂载 rsync 灌入，硬链接多入口、拆 view、未实现清单）/不同客户端想要不同显示方式”。最短生产路径六步（起服务与挂载的命令见上文「快速上手」与「生产拓扑」，地址换成中心机、按需加 `--token`，不在此重复）：

1. **构建安装**：每台节点同一个二进制——`cargo build --release && sudo install -m755 target/release/grovefs /usr/local/bin/grovefs`。
2. **中心双服务 + 共享 token**：`TOK=$(openssl rand -hex 16)`，两个服务都加 `--token $TOK`；生产用 systemd 常驻（unit 模板 USAGE §11.4）。
3. **防火墙**：只对内网段开 50100/50110（USAGE §11.6）。
4. **显式建视图再挂载**：挂载从不建树；客户机出示同一 `--token`。
5. **验收**：双客户端 A 写 B 即时可见 + 跨机 flock + `findmnt` 显示 grovefs:alice（USAGE §11.7）。
6. **日常**：`journalctl -u grove-meta-server` 盯降级/冻结关键字；冷拷 `blocks/` + 整个 `meta/` 即完整备份（USAGE §12）。

## 测试

```bash
export LC_ALL=C LANG=C             # PKG_CONFIG 不必设：没有 crate 查 pkg-config（见「构建」一节）
cargo test --release                 # 单元 + 集成测试
bash .build-bin/run_all.sh           # 全板 43 段（42 门控 + bench：build/unit + 40 个功能套件）
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准（纯功能回归）
# 扇出规模化基准（opt-in，默认 #[ignore] 不拖慢 CI）：批量建对象 + 随机读，
# 断言 256 桶全用上·单目录条目数受控(≈N/256)·路径 O(1)。默认 30 万对象，秒级完成；
# 拉到千万级验证 S1 主张：GROVEFS_FANOUT_SCALE=10000000 cargo test --release -- --ignored --nocapture
```

套件（重点节选，逐段清单与超时见 `USAGE.md` §10）：`build`、`unit`、`posix`、`edge`、`e2e`、`remote`、`errors`、`resilience`、`concurrent`、`metasrv`、`ha`（含 `--witness` fencing / fail-safe 场景）、`striping`、`locks`（含崩溃即时回收 / 续约跨过 lease）、`freshness`、`layout`（显示透镜 e2e：hide/only/map/link + 直通写删 + 脚手架守卫 + 服务端派生视图：`view define/list/rm` + 跨形状推送 + re-define 热换形免重挂 + 派生挂载只读 + 纯别名：全树可写/真实名拒挂/链与派生目标守卫/删别名恢复基名可挂/原子轮换 `--retire`〔泄露止损：旧名即刻失效、真实名全程拒挂〕，共 82 条断言）、`fork`（独立元数据树 fork 视图 e2e：创建/守卫/`view list` 裸基视图/`apikey list --view` 过滤/meta-server 挂载〔结构写放行、内容 EROFS、截断拒绝〕/删除隔离与账本/`view rm` 删树重建账本（含 `--empty` 空种子与 `view graft` 接枝），共 76 条断言）、`migrate`（视图元数据迁移 e2e：`view export` 把单视图目录项导出为宿主无关的 JSONL〔无绝对路径/主机名/blocks 路径〕、`view import` 导入另一存储路径后 id/inode/硬链接/权限/属主/符号链接全部保真，账本重建、导入原子化〔失败的 `--replace` 不伤旧树〕、`--as-fork` 恢复 fork 语义并跨视图共享字节、多个基视图互不干扰，共 74 条断言）、`import`（零拷贝导入 e2e：硬链接直通/mtime 保真/GC 不伤源/增量重跑/派生视图组合〔只读〕）、`mcp`（智能体治理接口 e2e：握手与工具发现/内联规则定制数据列表/路径探查与形状预览〔物理全树、经透镜、不落盘模拟〕/发布前干跑校验〔治理守卫全跑·不写盘〕/别名无空轮换/`fork_view` 建独立树与空种子/`graft_view` 接枝/与 CLI 逐字节同文案/通知不作答与非法帧恢复，共 77 条断言）、`apikey`（MCP 接口 API key 认证与调用审计 e2e：铸造·列表·盘上无明文（只有 sbx1 信封）·无 key 拒·对 key 放行·调用归属·撤销即失效且行保留·短 id 前缀撤销·env 预鉴权·`list --view` 过滤·默认不设门回归·`set-ro` 原地 ro↔rw 翻转·翻转后仍鉴权·新铸 key 短格式（16 hex id + 32 hex secret）且长格式 key 照样鉴上·`rotate` 保 id 重铸 secret 而旧 secret 下次握手即拒·`list --label·--owner·--scope` 过滤·`--note`/`--ttl` 入账与回显·key 列只出指纹且盘上无明文·`list --note` 子串过滤·`update` 改备注与期限（`--ttl never` 清期限、无 flag 拒绝）·过期行标 `EXPIRED` 且门拒绝、同 key 靠新 `--ttl` 复活·revoked id 拒 `update`·`secret` 读回与 create 打印的逐字节相同·副本跟随 `rotate`·未知 id 拒·`purge` 的 dry-run 只数与实删报数，共 61 条断言）、`mountkey`（可撤销的挂载凭据 e2e：scope 铸造与 typo 拒绝·六类拒因入协议·准入与 env 回退（argv 不泄密）·连接审计到具体 key 与视图·scope 双向隔离·撤销封停活挂载且无脏 endpoint·其他 key 不受波及·token 与 key 双门 AND·数据面独立门·故障转移携带凭据·不设门默认路径无回归·视图绑定 key 跨视图拒挂与审计归属·只读 key 服务端强制写拒·`set-ro` 同一活挂载 rw→ro 拒写·ro→rw 放行·免重连，共 76 条断言）、`config`（命令瘦身 e2e：自动加载的 `KEY=VALUE` 配置文件与 env 回退——flag > env > 文件 > 默认值优先级链（GROVEFS_HOME/META/DATA/TOKEN/DB_PATH）、解析边界（注释/`export` 前缀/引号值/非 `GROVEFS_*` 键警告后忽略/坏行定位警告/读不到的显式 `$GROVEFS_CONFIG` 不致命）、以及一台命令行不写坐标、坐标全部来自配置文件的带 token 门真实 FUSE 挂载，再加上任何来源都没给坐标时的硬拒（退出码 1、拒绝文案逐字、不留下挂载点、挂载不碰 `--home`），共 37 条断言）、`manager`（挂载引导 e2e，共 41 条断言：套件自己的 6 个端口先预检·`define` 幂等与 epoch/通配地址拒收·注册要 join key 且**未声明地址被拒收而被拒节点照常服务**·`mount --manager` 解析并验签（错 key 拒挂且不留挂载点）·真实挂载上 40 MiB 分条对象读写·杀掉 manager 后活挂载经**另一个挂载进程**读回原字节而新挂载 fail-closed、重启即恢复·`--require-registered` 是门不是滤镜·收缩声明集 → 分条读拿到 `EIO` 而不是空文件，且恢复全集后另一挂载读到同一份字节）、`pg`（PostgreSQL 后端 e2e，共 95 条断言，含第 10 节的后端隔离守卫：PG 运行绝不读/写本地 `store.key`（wrap 只认 env `GROVEFS_STORE_KEY`、无 env 即 show-once）、同一 env 跨节点可读回、`rotate` re-wrap；无 PG 主机自动 SKIP、不拉低 GREEN）、`ctlplane`（远程治理面 95 条，含审计保留期与 `apikey purge` 的权限门）、`web`（控制台 196 条，含常驻控制连接两臂与 bundle 跨页跳转串断言）、`forksrv`/`xfer`/`gcremote`（服务端 fork 与跨机传输的两档后端）。

## 命令速查

> 先说人话:下表里的 `--home`、`--meta-dir` 等是**参数写法**(照抄用),它们各自管的东西如下(以**实际含义**为准):

| 它管什么(实际含义) | 命令行写法 |
| --- | --- |
| **节点侧存储根** — 放 `blocks/`(数据)与 `meta/`(视图)的总目录，只有跑服务与 node-side 动词（`view create`/`gc`/`import`/`apikey`/`du`）的机器用到它；**挂载从不读也不写它** | `--home <目录>`（服务端各自用 `--meta-dir`/`--data-dir` 指名） |
| **服务端元数据根** — `meta-server` 上放权威视图库、视图定义、API key 的总目录 | `--meta-dir <目录>` |
| **数据落盘位置** — `data-server` 实际写字节的存储根(默认同元数据根) | `--data-dir <目录>` |
| **要被纳管的已有目录** — 本就躺在盘上、想变成一个视图的那批数据 | `--source <目录>` |
| **挂载点** — 本机用来访问这棵树的目录 | `-m <目录>`(即 `--mountpoint`) |
| **挂载引导地址** — 客户机唯一手指的 manager(主机:端口)；元数据 / 数据服务坐标由它的签名 roster 解析 | `--manager <地址>` + `--roster-key-file <文件>` |
| **视图名** — 给一棵逻辑树起的任意标签(与 Linux 用户名无关) | `--view <名>` |
| **接入凭证** — 进入服务端的 token(数据面与元数据面可分别设) | `--token <串>` |
| **以指定身份挂载** — 锁仲裁与审计按此 uid 记属主（advisory，可被 `--run-as-uid` 覆盖） | `--run-as-uid <uid>` |
| **只读 / 零缓存强一致** — 挂载为只读 / 关闭客户端缓存 | `--ro` / `--no-cache` |
| **目录列举提速的回退开关** — 关掉 READDIRPLUS（随目录项一并带回属性），退回普通 READDIR + 逐条 `LOOKUP`；分页不受影响（当前每一种引擎都自己分页），所以它是纯粹的 A/B 与排障开关 | `--no-readdirplus` |

| 目的 | 命令 |
| --- | --- |
| 建视图 | `grovefs view --meta-dir D create --view U [--manifest M]`（唯一的建视图入口；PG 档换 `--meta-store <URI>`） |
| 挂载（唯一形态） | `grovefs mount --view U -m MP --manager H:P --roster-key-file F [--deployment N] [--token T] [--api-key ID:SECRET] [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--no-readdirplus] [--stats] [--foreground\|--log-file F]`（默认转后台；坐标只从 manager 的签名 roster 解析，不给 `--manager`/`GROVEFS_MANAGER` 或 `--roster-key-file` 就 fail-closed 拒挂；手敲 `--meta-server`/`--data-servers` 已从 mount 移除） |
| 挂载引导（只指一个地址） | `grovefs manager --state-dir D define --name N --meta M[,M2] --data A[,A2]` / `manager --state-dir D serve --listen H:P [--roster-ttl S] [--register-ttl S] [--require-registered]` / `manager status --server H:P [--deployment N] --join-key-file F`；节点 `meta-server`/`data-server --register-with H:P --join-key-file F [--advertise-addr A] [--deployment N]`；客户机 `mount --manager H:P --roster-key-file F [--deployment N]`（`--manager`/`--roster-key-file` 是 mount 唯一的坐标来源，旧 `--meta-server`/`--data-servers` 已从 mount 移除） |
| 显示规则文件 | 每行 `hide <src>` / `only <src>` / `map <src> <dst>` / `link <src> <dst>`（`#` 注释，绝对路径） |
| 定义派生视图 | `grovefs view --meta-dir D define --name V --base B --rules F`（存于 `D/meta/views/V.layout`，客户端 `mount --view V` 即得形状；虚拟视图挂载只读） |
| 定义纯别名 | `grovefs view --meta-dir D alias --name A --real R`（公开名 1:1 映射真实视图：`A` 全树可写，`R` 不可直挂） |
| 泄露后轮换公开名 | `grovefs view --meta-dir D alias --name A2 --real R --retire A`（先写后删零窗口：`A` 失效、`R` 全程不可直挂、授权不动） |
| 创建 fork 视图 | `grovefs view --meta-dir D fork --name L --base R`（复制目录项成独立 `meta/L/L.db`，字节共享；结构可写、内容 `EROFS`；`--empty` 只建根，供接枝策展） |
| 接枝进 fork | `grovefs view --meta-dir D graft --name L [--src S] --map PLAN\|- [--dest P] [--skip-existing]`（计划一行一条 `<src>[TAB<dst>]`，整份一事务、字节共享；目录源带整棵子树，目标基名即改名） |
| 导出/导入单视图元数据 | `grovefs view --meta-dir D export --view V --out F [--force]`（宿主无关 JSONL，不含字节）/ `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]`（原子替换；字节另拷 `blocks/`） |
| 按视图查 key | `grovefs apikey --meta-dir D list --view L`（只列能开 L 的 key；纯别名也匹配绑定到真实名的 key）；按名字/归属/备注/scope 检索：`list --label SUB` / `--owner SUB` / `--note SUB`（大小写不敏感子串）/ `--scope any\|mcp\|mount\|admin`（精确），五个条件可叠加 |
| 列出/删除视图定义 | `grovefs view --meta-dir D list`（定义槽 + 裸基视图）/ `grovefs view --meta-dir D rm --name V`（派生/别名只删定义不动数据；fork 连自己的 `meta/V/V.db` 一并删除并重建账本） |
| 智能体接口（MCP） | `grovefs mcp --meta-dir D [--meta-store postgres://…]`（stdio JSON-RPC：治理 `list_views` / `define_derived_view` / `alias_view` / `fork_view` / `graft_view` / `remove_view` / `preview_rules` / `validate_definition`，只读探查 `list_view_tree` / `get_view`；规则内联文本， 与 `grovefs view` 同校验同文案；同一治理另经 `resources/*`/`prompts/*`/`completion/complete`/`logging/setLevel` 常用方法面暴露） |
| API key 与调用审计 | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount\|admin] [--owner ACCT] [--view V] [--ro] [--note TEXT] [--ttl DUR]` / `list [--view V] [--label S] [--owner S] [--scope S] [--note S]` / `revoke --id X` / `rotate --id X`（为**同一个 id** 重铸 secret：绑定/归属/审计全留，旧 secret 下次握手即拒）/ `set-ro --id X [--rw|--ro]`（活跃 key 只读↔读写原地翻转，活挂载 ≤1s 热生效免重连） / `update --id X [--note TEXT] [--ttl DUR]`（只改备注与期限两笔台账，`--ttl never` 清期限；revoked id 拒改）/ `secret --id X`（把入库的那份可逆副本读回成完整的 `<id>:<secret>`，属主或超级账号才能读；经服务端的读落审计，本地直连库的读不记账）/ `calls [--limit N]`（鉴权只认 SHA-256 摘要、库里另存一份要 store key 才开得动的可逆副本、新铸格式 16 hex id + 32 hex secret、`list` 带 scope 列与 key 指纹列（头 4…尾 4）、备注、有效期与铸造者；过期由服务端在每次握手判，到期即与吊销同等拒；配合 `grovefs mcp --meta-dir D --require-api-key` 门住读写治理状态的方法，或 `meta-server`/`data-server --require-api-key` 门住挂载入口（客户端 `mount --api-key`/env `GROVEFS_API_KEY`）；每次调用与每次挂载连接都落审计（SQLite：`meta/_control/api_keys.db`；PG：`grovefs_control.mcp_calls`），撤销封停活挂载；`GROVEFS_MCP_API_KEY` 可预鉴权整个服务） |
| 导入存量目录 | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]`（同盘硬链接零拷贝，重跑增量） |
| 起网页管理台 | `grovefs web --control-server H:P --api-key-file F [--listen 127.0.0.1:PORT] [--session-ttl N] [--allow-insecure-http|--use-x-forwarded-for]`（治理口前面的 HTTP 网关：总览/视图/密钥/账号与授权/审计五页，共用一套设计语言（卡片布局、可折叠详情行与详情抽屉）；密钥页直接列出 key 指纹/备注/有效期/铸造者并可按视图、档口、关键字过滤；本身不存任何状态） |
| 建号与基视图授权 | `grovefs web … account create --account A --password-file P\|--password-stdin [--role admin]` / `account passwd\|role\|disable\|enable\|rm` / `grant add\|rm --account A --base V` / `grant list`（授权给的是**基视图**，其 fork/别名/透镜与其上的 key 自动继承；安装期第一任管理员由 `meta-server --admin-user --admin-password-file` 在空表上播种） |
| 卸载 | `grovefs umount -m MP` |
| 数据服务 | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| 元数据服务（默认 SQLite） | `grovefs meta-server --meta-dir D --listen IP:PORT [--token T] [--require-api-key] [--role primary\|standby --peer ADDR [--sync-repl] [--witness DATA_ADDR]]` |
| 元数据服务（PostgreSQL 后端） | `grovefs meta-server --meta-store postgres://user:pw@host:5432/dbname --listen IP:PORT`（PG 下 `--meta-dir` 可省，服务端本地不落权威数据；先用 `grovefs view --meta-store <同 URI> create --view V` 显式建 schema；`view`/`apikey`/`mcp`/`data-server` 用同一 `--meta-store` URI 或 env `GROVEFS_META_STORE` 接同一控制面） |

## 诚实标注的边界

以下为本项目遵循「工程诚信」原则明确标注的限制，而非隐藏假设：

- **传输协议**：当前为自定义 TCP 帧，非 gRPC。gRPC（tonic/prost）已做 go/no-go 闸门评估，结论 **GO**（tonic 0.14 免 protoc），但完整集成**有意延后**——现协议已正确且被测。
- **HA RPO**：默认 async RPO ≈ 500ms（`--sync-repl` 可降到 0）。
- **fencing**：`--witness`（逗号分隔的 N 个共享 data-server 作 `prev`-回传 epoch CAS **多数派**仲裁，需 `N/2+1` 授予）**fail-safe**，primary 持不重叠的有界多数派租约（多数活租约即独占证明，写由本地租约校验放行）、并对内容数据帧按 token 门控，把双写窗口收敛到**租约安全余量内**（保证收敛单主）。硬边界：严格两节点、**不配第三方仲裁** 时无法防脑裂、窗口不封顶（CAP/共识定理限制，须部署 witness 或第三节点）；**多数派仲裁的可用性红利**：配 `--witness` 后宕**少数**仲裁者仍不冻结写、少数盘丢不复活过期主，唯**失去多数**才 fail-safe 停写（N=1 即单仲裁者，witness 本身成为单点可用性依赖）。
- **条带**：单条已饱和链路（loopback/单网卡）上**无吞吐增益**，价值在多节点聚合容量；真实多机链路须重测后才可声称收益。
- **显示透镜（`--layout`）**：是**显示层定制而非安全边界**——绕过透镜直接挂同一 view 仍能看到全树，硬隔离请用独立 view + token/API key；向纯合成目录创建/重命名进入返回 `EPERM`、撞透镜已显示名返回 `EEXIST`、`only` 模式下白名单外不可写；symlink 目标文本不改写；共享 meta-server 下挂合成目录的条目失效推送可能不命中透镜坐标（建议 `--no-cache`）；规则体目前限同一 view 内部重排，不支持跨 view 引用。
- **服务端派生视图（`grovefs view`）**：**一律只读挂载**（硬契约）——形状集中治理、只分发不修改，写只属于基名视图；客户端据 `M_OPEN` 握手标志落内核 RO，服务端另对派生名写意图 op 设权威门禁（`E_NOTPERM`）；若需可写的自定义形状，用客户端自备的 `mount --layout` 透镜（§每客户端显示定制，直写真实条目）。其余：同样**不是安全边界**——直挂基名仍见全树（能拦住的是 token/API key，不是形状）；`define` 替换**热生效**（服务端逐 op 检查定义 mtime，已挂载连接 ≤2s TTL 内自动换形、免重挂），`view rm` 不热作用（已挂载者保持旧形状到重挂）；切换形状时旧虚拟 ino 上的合成路径/fd 可能落回 ENOENT，真实条目 inode 稳定不受影响；不嵌套（base 必须是物理视图）、不得与基同名、不得遮蔽物理 db；`meta/views/` 是控制面配置，HA 备机需手工同步；换形的检查在 `meta-server` 逐 op 进行，挂载只在 `M_OPEN` 解析一次定义。
- **服务端纯别名（`grovefs view alias`）**：与派生视图的正反面——别名挂载全树**可写**、真实名被治理后**不可直挂**（`meta-server` 在 `M_OPEN` 拒）；单跳解析、每 `M_OPEN` 重扫 `views/`（挂载是罕见事件）；拒绝别名链/派生目标/一真实名多别名；`import` 拒别名目标；删别名仅影响**新**挂载（已建立连接不受影响，同 `view rm` 语义）；`--retire` 原子轮换公开名（先写后删、真实名全程不可直挂、无裸露窗口；被退役的旧名连同任何未定义的名字一样在 `M_OPEN` 被直接拒挂，不会长出一棵空树来承接误挂），但轮换/删别名都只影响新挂载；拒因**随协议回传**（`M_OPEN` 错误帧的 data 段携服务端原文），服务端原文随错误帧一起回到客户端，拒挂的那句话与治理入口打印的逐字节相同；一帧不带原因的拒绝只剩通用准入文案，可读的那句留在服务端日志里；改名不是安全机制（ 公开名终究要分发），真实名隐藏属名字治理，硬边界仍是 token + 可撤销 api-key。
- **fork 视图（`grovefs view fork`）**：创建是**点即快照**（复制 `node` 行、保留 `ino`/硬链接；不复制 `ctrl`/`locks`；基视图之后新增的名字不会注入已建 fork）；结构写（`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/空对象 `create`）放行，内容写（`write`/`truncate`）在 **FUSE 客户端**回 `EROFS`（挂载按 `M_OPEN` 第 3 个标志字节判定）——这是**诚实标注的客户端门**：绕过 FUSE 直连 data-server 的 raw 协议客户端仍可写字节，硬边界与派生只读/已读一致仍是服务端 token+API key；fork 删名字只摘自己 `node` 行（账本判定最后持有者才真删字节，基视图分毫不动）；`view rm` 连 `meta/<name>/<name>.db`（+WAL/SHM）一并删并 `refs rebuild`；基必须是**该 store 里的物理视图**（有定义的名字不能当基；两档后端都可 fork：SQLite 抄文件行、PG 跨 `grovefs_v_<hex>` schema 复制，各自单事务），`alias` 拒指向 fork（fork 是树身份，发 key 用 `apikey create --view`），`import` 拒 fork 目标；已挂载的 fork 被 `view rm` 后保持自己已解析的名字直到重挂，新挂载被拒。由 `fork` 套件 76 条断言证明（创建/守卫/`--empty` 与 `view graft` 接枝/`view list` 裸基视图/`apikey list --view`/服务端挂载/删除隔离/账本）。
- **可撤销凭据（`grovefs apikey` + `--require-api-key`）**：它是**入口门**而不是隔离墙——认证“哪个调用方开了哪扇门”（scope 分隔 `mcp` 与 `mount` 两面，拒因逐字节回传，每次连接/调用归属到具体 key），但不改变治理语义本身；「谁能碰哪棵树、读写还是只读」由 key 的 `--view`/`--ro` 绑定在服务端逐 op 强制，且只读↔读写可 `set-ro` 原地互切（同一 ≤1s 复验 tick 热生效，免重铸免重连）。活连接复证是 **op 驱动**的（服务端仅在流量上每秒查一次），故撤销的封停边界是“该挂载撤销后的**首个服务端往返**”——完全由内核缓存回答的读不需往返、因此不会被打断（下一次往返即封）；已建立的 `grovefs mcp` 会话保持其身份直到重连。两面门都是 **opt-in**（不开时仅落审计、不拦任何调用）；`--token` 与 `--require-api-key` 可同时存在（两道都得过）；`data-server --meta-dir` 默认取首个 `--data-dir`，数据盘与元数据根分盘部署时必须指对（指到空 key 库则 fail-closed）。鉴权只认入库的 SHA-256 摘要，所以单拿 `api_keys.db` 结不出可用凭据；同一库里另有 `secret_enc` 一列存放完整凭据的可逆密文副本（`sbx1.<nonce>.<ct>.<mac>`），`apikey secret --id X` 读它——开副本需要 store key，而**默认布局下 `store.key` 就与 `api_keys.db` 同住 `<meta-dir>/meta/_control/`**，一次 `meta/` 冷拷就同时拿到两者（要把材料接在库外只能用 env `GROVEFS_STORE_KEY`，它优先于文件）；因此这份副本是运维便利（交接、不再靠人记住明文）而不是一道新安全边界，丢了材料则那些行停在 unopenable、只能 `rotate`。该库（含 `-wal`）是控制面状态，需随 `meta/` 一起冷拷。由 `mountkey` 套件 76 条断言证明（含“撤销封停活挂载且无脏 endpoint”“其他 key 不受波及”“视图绑定 key 跨视图拒挂”“只读 key 服务端强制写拒”“set-ro 在同一活挂载 rw↔ro 热切换免重连”）。
- **挂载引导（`grovefs manager`）**：roster 签名是 **HMAC（对称）**——能验签的客户端也能签，所以「客户机持 roster key」不等于「客户机伪造不了 roster」；缓解在于假 **data** 地址在开了 Data-Access Capability（`--data-access-key`）的 data-server 上换来的是挂载失败而不是读权限，而假 **meta** 地址仍会收割客户端在 `M_OPEN` 出示的那份凭据；闭合路径是签名块里已预留的 `algo=2`（非对称，只换 key 和一个字节）。另一条：**roster 只保证成员数与声明顺序，不保证「这台机器就是当初写数据的那台」**——把一个 data-server 的地址换成另一台**同样在声明集里**的错误主机会让分条对象读成空（而**成员数不符**会被显式拒成 `EIO`：单地址数据路径上的 `backend::UnstripedGuard`，不靠客户端自觉）；替换成员主机必须整组同换或先做 `migrate`。`--require-registered` 是预检门而非健康滤镜，开了也不会收缩 roster；多 manager 热备本轮未实现也未测试。全部否决项与残余风险见 `docs/adr/0001-manager.md`。
- **零拷贝导入（`grovefs import`）**：硬链接要求源与 `blocks/` 同文件系统（跨设备自动退化为独立拷贝并如实报告）；导入后源文件若被外部工具以 **rename/替换写**方式更新（vim、rsync --inplace 关闭等默认行为），旧 inode 内容冻结在最后一次直通写入上——需对新内容重跑 `import`（已存在的名字会跳过，可用不同 `--prefix` 强制重登记）；symlink/特殊文件有意跳过（不表示）；导入对象为单文件，不走 ≥16MiB 的跨 server 条带（条带仍是经挂载写入新大文件的能力）；meta-server 模式下 import 直写权威 db 文件，需在存储机执行、已挂载客户端靠 TTL（≤2s）收敛而非即时推送；PG 后端不支持（权威树在 Postgres 里，走挂载点 rsync 灌入）；`--source` 建议与存储根同盘并**保留**——删源仅失去「源路径」这一别名，数据不丢，但外部改名/迁移后视图与文件实体的关联就只剩 `blocks/` 链接。
- **元数据读并行 / 目录分页的作用范围**：只读连接池与 `LIMIT/OFFSET` 分页都长在**服务端存储引擎**上——SQLite `MetaEngine`（暖 1 条读连接、突发保留至 8）、PG `PgEngine`（4 条只读连接，各自缓存 prepared statement）、treedb 引擎（用它自己的 offset/limit 做 readdir）。挂载进程里没有引擎，所以它拿到的是 `M_READDIRP` 帧的下推：客户端把 `offset/limit` 交给服务端，服务端在自己的只读池上跑那一页，客户端每个内核缓冲只取一页并协商 `FUSE_READDIRPLUS`。实测（本机回环、同一挂载点、每趟新挂载、内核 dentry 缓存为空）：5001 条目录 `ls -l` **0.270–0.274s**（`--no-readdirplus` 只留分页 **0.575–0.629s**，两份输出排序后逐字节相同）；10,002 条 `ls -f` **0.116s**、`ls -l` **0.547s**；100,002 条 `ls -f` **0.925s**、同一目录的 fork 视图 **1.156s**、100 条规则的透镜 **1.612s**（TTL 内重读 **0.429s**）。**派生/透镜视图在作用范围之内**：`LayoutMeta` 覆写 `readdir_page`（按父目录物化渲染后的形状再切片），而物化即缓存，**必须被失效推送经 `flush_listing` 一并 flush**，否则新鲜度会被压回 TTL 量级（`disp_fresh` 套件就是这条契约的门）。一个 store 是否能分页由 `M_OPEN` ack 的能力位告知客户端，客户端不猜。元数据**写**并行（多权威）没有——一次部署只有一个权威：SQLite 档把写串行在单条权威写连接上，PG 档摊到一个有界 SERIALIZABLE 写连接池上，但权威仍只有一个。契约由 `cargo test` 的 `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` 钉住；复跑 `.build-bin/probe_readdir_cluster.sh`。
- **验证环境**：全部在 127.0.0.1 回环（模拟 LAN）验证，尚未做真实多机与 fio 级基准。

凡未实测数字处，文档不做收益声称；凡有限制处，均在代码注释与本节明示。

**文档口径：只陈述当前版本的行为。** 不记录已移除的参数、环境变量或开关（提及它们的名字，就等于给旧行为招魂）；不做版本前后对比，不引用历史基线数字；不写「新增了 / 现已支持 / 与旧版一致 / 退回旧行为」这类变更叙述——演变史归 git 提交记录，手册只管现在怎么用。两类内容不在此列，应当照常保留：① 当前版本内的真实回退语义（跨设备导入退化为独立拷贝、服务端不宣告分页能力位时退回每内核缓冲一次全量列表）；② 上面这类「未实现（诚实标注）」的负空间清单。

## 许可

Apache License 2.0（见仓库根目录 `LICENSE`）。
