# ADR-0006 · 视图分片：多个 meta-server **进程**，一台 PostgreSQL **控制面**

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（2026-11-27） |
| 日期 | 2026-11-27 |
| 前序 | ADR-0004（视图→PG 实例的负载感知放置，作用在**一个 meta-server 进程内部**）、ADR-0005（写路径阻塞预算，作用在**一对主备之间**）。本份补第三根轴：**服务同一个集群的多个 meta-server 进程**之间怎么分工，以及为什么分工的答案必须落在 PostgreSQL 里 |
| 实现 | `src/remote/meta/shard.rs`（新：`parse_ring`/`MetaNode`/`Shard`/`Home`、`grovefs_control.meta_placement` 的 DDL+advisory lock 读写、`route`/`peek`/`adopt`/`forget`/`placement_map`）、`src/remote/meta/server_ext.rs`（`MetaServer::with_control_plane`/`with_sharding`、`meta_open` 的 `M_OPEN` 路由门、`meta_work` 的逐 op 放置门、`evict_misplaced` 清扫、`view.create`/`view.rm_base`/`view.move_meta` 三处治理门、`physical_tree_exists`/`physical_tree_list`/`fork_tree_exists` 的"树在本地、注册表在远端"分流、`store.info` 的 `shard` 块）、`src/remote/meta.rs`（`E_SHARD = 11`、`SHARD_FOLLOW_MAX = 3`、跟跳与 home pin、`round_trip` 的挂断重放）、`src/main.rs`（`--control-plane`/`--shard-ring`/`--shard-id`、`view move --to-meta`）、`src/mcp.rs`（`ControlStore::tree_backend()`，以及视图列表按树后端报落点）、`src/control_remote.rs`（远程档的树后端跟随 `store.info` 回包里的 `metadataBackend`）、`src/stats.rs`（三枚计数器） |
| 验收 | 单测 `remote::meta::shard::tests` 7 条 + `remote::meta::shard_follow_tests` 4 条 + `mcp::tests::list_views_text_labels_the_tree_by_the_tree_backend` 1 条；e2e `tests/shard_routing.rs` 2 条（SQLite 树跨实例分片、PG 树跨实例移动后挂载跟随）**2 passed / 0 failed**；`cargo test --release --features treedb` 全量 **429 passed / 0 failed / 4 ignored**（本主题 +14 条）；§9 的延迟数字即这两条 e2e 实跑打印的 |

## 1. 决策

**控制面（视图注册表、两张放置表、api_keys、mcp_calls、web_accounts/web_grants、审计）唯一权威落在 PostgreSQL；视图树落哪儿是每实例自己的 `--meta-store` 决定，SQLite / PostgreSQL / treedb 三选一。多个 meta-server 进程组成一个 ring，谁服务哪个视图由 `grovefs_control.meta_placement` 一行决定，客户端只连一个入口，由被连上的实例用 `E_SHARD` 把它指引到 home 实例。**

三条推论，都是这份决策的实际内容：

1. **路由是"指引"，不是"代理"**。入口实例从不转发数据面请求：`M_OPEN` 上答 `E_SHARD` + home 地址，连接由客户端重拨。于是入口不是带宽与 fd 的瓶颈，LB 后面随便哪台都可以是入口。
2. **一个视图只有一个服务实例**，且 `meta_placement` 的那一行是唯一答案。本模块**故意不放任何放置缓存**：一次 `M_OPEN` 读一条主键行，是这条路径上最少见的事件，而一个过期的路由表会让后续 op 打到自己已不再拥有的后端。
3. **管理动词跨实例只拒绝、不代办**。`view create`/`view rm` 落在非 home 实例上是**拒绝**并给出可粘贴的 `--control-server` 命令；只有 `view move --to-meta` 是**改行**的动作，而且必须在 TARGET 的控制端口上执行（由它自己探测"树在这里能不能打开"再写行）。

## 2. 分片键是什么：物理 base 视图名

放置行的主键是 `view TEXT`，写进去的是**别名解析之后、lens/fork 展开之前的物理 base 视图名**。理由是唯一的：一个 base 可以套出 lens、fork、alias 三个客户端可见名字，若分片键是可见名，同一棵树就会在表里有多行、可以指向不同实例——这正是"一个视图一个服务实例"要避免的第一种破法。`M_OPEN` 在别名解析之后才过路由门，逐 op 门则用 `rt.bases` 把派生名折回 base 再查。

`--shard-ring` 的条目名是**分片身份**，地址只是它当前指向哪台机器：

```
m1=10.0.0.1:50053,m2=10.0.0.2:50053                    # 两个进程
m1=10.0.0.1:50053+10.0.0.2:50053,m2=10.0.0.3:50053     # m1 是一对 HA 主备
```

行里存名字，所以重排 ring、改端口都不动一个字节。裸地址写法（`10.0.0.1:50053,10.0.0.2:50053`）按位置自动命名 `m1..mN`，只在顺序不变时稳定——与 PG 实例注册表同样的取舍。本进程是谁由 `--shard-id`（名字 / 下标 / 地址）指定，未给则用 `--advertise-addr` 在环里找 carrying 该地址的那一项；两者都对不上就**启动失败**，不去猜。

单节点环（`--shard-ring m1=x`）不查任何表、行为与不加该 flag 完全一致，为的是"环写一次、以后再加节点"这种部署可以放心提前写进 unit 文件。

**没有 `--shard-total N` 这种一致性哈希**，是有意的：分片键是视图名，但**归属是记录出来的、不是算出来的**。哈希环会在"加一个节点"时把 N 分之一的既有视图改判到别的实例，而那些实例并没有那棵树——在 SQLite 后端下这意味着一个打不开的挂载，在共享后端下意味着一个空 schema 被当成视图创建出来。记录式的行让"扩容"退化成一条运维动作（`view move --to-meta`），且只在人点名的视图上发生。

## 3. 无行即落家：bootstrap 与它的方向

`route()` 的顺序是**单节点即自己 → 有行按行判 → 无行就把本实例 pin 成 home**。第三条 arm 是这套东西能与"分片开关之前就在盘上的视图"共存的原因：打开 `--shard-ring` 时已有的树，谁第一个服务它，谁就写下那一行，环再长大也不会搬动它、不需要复制任何一棵树。pin 是**一个带 advisory lock 的事务**（`INSERT … ON CONFLICT DO NOTHING` 后读回），所以两个实例同时看到同一个未放置的视图时，两边拿到的是同一个答案，其中一边会 redirect。

行里还存 `store`：home 实例写行时的树后端指纹（去 credential）。它存在的唯一理由是最坏的那种静默错误——某实例被点名成 home，但它的 `--meta-store` 已经不是当年建树的那个：此时 `decide` 返回 **Err**（"这棵树是在后端 X 上建的，本进程服务的是 Y"），而不是在本进程里开一个空 schema、把一个空视图当成合法内容端出去。宁可拒挂载。

**例外：会 DESTROY 的动词不 pin。** bootstrap arm 的语义是"我即将服务这棵树，所以它归我"，只有真要端内容的路径才配得上这个断言。`view rm` 用 `route()` 时，一个"树已被 home 删掉、行随之消失"的名字在 peer 眼里就是**未放置视图**：peer 先把它 pin 给自己，再在"本地没有物理树"上失败退出——行留下了，树没有了，此后所有实例对这个名字的答复都指向一台什么都没有的机器，而 `view create` 同名会被"已放置在 X"拒掉。改判用 `peek_forget()`（只读 + 无行即本实例，遗留部署的清理仍能做完）。这条是 `tests/shard_routing.rs::a_three_node_ring_routes_by_the_row_not_by_adjacency` 逼出来的：两节点环上"行指向第三方"这个形状根本不可达，三节点才有它（§9）。

## 4. 组合矩阵

分片与既有两根轴的正交关系，以及叠加时的实际形状：

| 组合 | 是否允许 | 实际形状 |
| --- | --- | --- |
| 分片 × 单实例（无 `--peer`） | ✅ | 最常规形状：N 台各持一部分视图树，共享一台 PG 控制面 |
| 分片 × `--peer`/`--witness` HA（ADR-0005） | ✅ | **一个环条目就是一个 HA 对**：`m1=主+备`。`E_SHARD` 把整串 `a+b` 交给客户端，挂载自己的 failover 逻辑接管，因此分片不需要新的故障转移代码。pin 住的 home 含 `+` 时按 `+` 拆开入队；对内部某一半不可达属于 HA 事务，**不会**让客户端忘掉这一行 |
| 分片 × `--meta-store a,b,c`（PG 多实例，ADR-0004） | ✅ 但解决的是不同瓶颈 | PG 多实例是**同一 meta-server 进程内**把视图 schema 分散到多个 PG 库（瓶颈是单个 PG 实例）；分片是**多个 meta-server 进程**分散挂载服务（瓶颈是 meta worker / 引擎池 / 单机磁盘）。两张表、两套行：`view_placement` 记 schema 在哪台 PG，`meta_placement` 记谁服务 |
| 分片 × 一个 `--meta-store postgres://…` | ✅ | 树与放置行在同一个库里（`store.info` 的 `shard.controlPlane = "tree backend"`），`view move --to-meta` 只是改行、不复制，因为两边都打得开那棵树 |
| 分片 × SQLite 树 | ⚠️ 服务可以分，**move 不行** | 每实例只在自己 `--meta-dir` 下有树，别的实例打不开。`view move --to-meta` 在目标上探测到"本机没有这棵树"直接拒绝，并指向 `view export` / `view import`；挂载路由与治理拒绝不受影响 |
| 分片 × treedb 树 | ✅ | treedb 是树后端、不是控制面（`StoreKind::Treedb.control_uri()` 恒 `None`），所以分片必须另外给 `--control-plane postgres://…`。持有某视图的实例自然持有该引擎连接——环条目就是进程，进程就是持连接者 |
| 分片 × 无 PG 控制面 | ❌ 启动失败 | `--shard-ring` 要求一个 PostgreSQL 控制面：N 个进程不可能靠"只有其中一个读得到"的本地 SQLite 表来路由 |
| `--control-plane` × `--meta-store postgres://…`（两个不同 URI） | ❌ 启动失败 | 那会把 `grovefs_control` 劈到两个库上，本仓库没有任何代码路径支持这件事 |

## 5. 故障域

分片方案改变的只是"谁服务哪些视图"，故障域要按三个独立层次读：

- **一个 meta-server 进程死**：只有它的 home 视图受影响；客户端连别的入口也会被 `E_SHARD` 指回那台，所以**不会**自动散到别处（这是特性：树在那个进程的磁盘/连接里，指错了就是空视图）。要让它服务的内容换机，走 `view move --to-meta`（共享后端）或 export/import（SQLite）。
- **一台 PG 控制面死**：所有**新的** `M_OPEN` 被拒（"which instance serves the view is undecided, refusing to guess"），**已建立的挂载继续服务**——放置只在连接建立时被问一次。5 秒一次的清扫读不到表时**保持现场不动**（`evict_misplaced` 直接返回 0），绝不在一次瞬时错误上丢掉活的树。控制面对分片路径而言是"挂载能否新建立"的故障域，不是"数据是否可读"的故障域。
- **环里少了一个名字**：指向已退役实例的行是 **Err**，不是"按哈希自己顶上"。视图要么由人搬走，要么把那台加回环里。
- **`--shard-ring` 与放置行互相矛盾**（两行互指）：客户端的跟跳有 `SHARD_FOLLOW_MAX = 3` 上限并带环路检测，失败时给的是**点名原因**的错误（"placement for view 'X' points back at …"），不是泛化的"连不上"。

## 6. 治理动词的跨实例语义

`--control-plane` 让**定义**从每个实例都可见，这恰恰制造了新坑：`view_exists` 在远端也是真，非 home 实例若照旧执行，就会在同一个名字下建出第二棵树——后面没有任何动词能修好这件事。所以：

- **`view create`**：先过 `route()`。行指向 peer → 拒绝，并把"该找哪台、命令怎么写、要搬就 `view move --to-meta`"三句一起给出。行指向自己 → 行**写在树之前**，与 PG home 行同一个理由：那一行就是告诉其他实例"这名字占了"。
- **`view rm`**：删树是 home 的活。peer 若删掉行而树还在别处活着，留下的就是一个"挂载再也解析不到放置"的悬树。删成功后 `forget()` 删行，best-effort——树已经没了，读不到的控制面只会让计数偏一点，不会复活视图。
- **`view move --to-meta NODE`**：**必须在 TARGET 的控制端口上执行**。要为新家答复的那个进程才有权声明"这棵树我打得开"，它探测通过才写行。派生视图（lens/fork/bias）随 base 一起走。
- **`E_SHARD` 出现在 op 上（不只是 `M_OPEN`）**：一个已经挂载着的连接，在它的视图被搬走之后，旧实例会在清扫时丢掉引擎缓存；下一次 op 是 MISS，此时才查表，指向别人 → `E_SHARD` + 新 home + **挂断**。客户端认得这个码：采纳新 home、清空连接池、重握手、**重放那个 op**。被拒的 op 从未执行过，所以重放是安全的。于是 `view move --to-meta` 对**已建立的挂载**也生效，运维不需要挨个 remount。

## 7. 可观测

三枚计数器（`stats` 报告的"非计时计数器"段）：

- `shard mounts redirected` —— 本实例把挂载指引给别人（入口侧 + 客户端跟跳侧都计）
- `shard management verbs refused` —— 治理动词因为"树在 peer 那里"被拒的次数；它涨而 `views` 不涨，说明有人在对着错误的端口敲命令
- `shard placement read/write errors` —— 放置表读不到/写不进（每一次都以拒挂载或拒动词收场，绝不以猜收场）

`store.info` 的 `shard` 块只在该进程真的在环里时出现（`null` = "本进程服务所有视图"，与 `ha` 块同样的形状）：`self`/`addr`/`ring`/`backend`/`controlPlane` 加每节点 `{name, addr, views, here}`。地址随块外发（它是客户端要被指引去的地方，不是秘密），**后端 URI 永不外发**——`backend` 是去 credential 的指纹。`controlPlane` 说清放置表到底与树同库（`"tree backend"`）还是被 `--control-plane` 另指了一台（`"separate --control-plane"`），两边都先过 `resolved_control_uri` 展开，因为一个裸 `--meta-store` URI 会被采纳成 `<base>/instances.conf` 托管注册表，同一台库有两种写法。审计侧每个分片决定都落一行：`M_OPEN` 重定向记 `method=meta / M_OPEN`（"view='x': redirected to meta-server 'm1'"），逐 op 门记 `method=meta / M_SHARD`（"…moved to meta-server 'm1', this mount follows it"），治理动词被拒也各留一行——运维在一台不服务的实例上敲 `view rm`，审计里看得见它被拒的原因。

## 8. 负空间（这份决策没有解决的）

- **不做再平衡**：加节点不会自动分走任何既有视图，`view move --to-meta` 只按人点名的视图搬。放置是记录，不是策略。
- **不复制树**：共享后端下 move 是一次改行；SQLite 后端下的跨机搬迁仍然只有 export/import 这一条路。
- **不代理数据面**：入口实例永远不把 op 转发给 peer，因此**没有**"两实例间的一条内部连接"可以观测——故障域是每个实例自己到 PG 的连接。
- **控制面仍是单点**（就挂载建立这件事而言）：本份给它的是**有界**（`SHARD_DB_TIMEOUT = 2s`，连接与 statement 双端设限，ADR-0005 的规则搬到分片路径）与**只影响新连接**，不是多副本。
- **home 实例重启期间的挂载**：与不分片时一样由客户端 failover 列表处理；环不会替一个不在场的实例回答。
- **数据一致性仍由单 home 保证**：分片没有引入任何跨实例的写协调，也不需要。

## 9. 实测

数字来自本轮 `cargo test --release --features treedb --test shard_routing -- --test-threads=1 --nocapture` 的三次连跑（两台 meta-server + 一台本机 PostgreSQL 5433，loopback）：

- **路由的代价**：同一个实例上先挂自己 home 的视图 **12.08 / 12.64 / 12.39 ms**，再挂需要被重定向的视图 **19.69 / 18.73 / 18.43 ms**——多出来的是**一条主键读 + 一次额外握手**（约 +6 ms，与轮间抖动同量级），且只在挂载建立时付一次。
- **搬走的视图被旧实例放手的窗口**：行改写后，旧实例的下一次清扫在 **4.691 / 4.701 / 4.707 s** 内丢掉该树（`SHARD_SWEEP_MS = 5000 ms` 的上界，实测贴着它）。
- **已建立挂载跟着走**：清扫之后第一个 op 在 **71.48 / 80.57 / 83.71 ms** 内落到新家并成功（`E_SHARD` 挂断 + 重握手 + 同一 op 重放），无需 remount；此后 `readdir` 同时看得见搬之前建的 `before` 与搬之后建的 `after`——**一棵树，两个实例先后服务过它**。
- **稳态成本**：被重定向那一侧的 `residentEngines` 在挂载前后不变（e2e 直接断言），即非 home 实例对别人的视图不留任何缓存。
- **e2e 与真实回显逼出的五处真缺陷**（都已修，列在这里说明为什么它们值得进 ADR）：① `SELECT to_regclass(...)` 原样取列在 rust-postgres 下是**反序列化失败**，而它跑在清扫线程与 `store.info` 上——一个 panic 的 worker 会让整台实例的控制端口不再应答；② `--control-plane postgres://…` + SQLite 树时，"这棵树在不在"被拿去问 PG（查 schema），于是**每个实例都看不见自己的本地树**，自家挂载被自家拒绝；③ 放置表要拨的那个 URI，若直接拿 `--meta-store` 的原文会得到一个 `file:` 托管注册表路径，`postgres::Config` 当场解析失败。④ 视图列表里「这棵树落在哪儿」是按**控制面**后端渲染的，于是 `--control-plane postgres://… + SQLite 树` 把本地文件说成 `own schema grovefs_v_…`；而同一条渲染路径在远程 SQLite 档下（自 `283dd4a` 起）一直印着 `own tree ` 加一个空路径——因为远程 store 没实现 `tree_home_rel`。修法是给 `ControlStore` 加 `tree_backend()`（默认等于 `backend()`，`FileControlStore` 在引擎档答 `treedb` 并把 `tree_home_rel` 交回空串，`RemoteControlStore` 从 `store.info` 新增的 `metadataBackend` 字段探得，老服务端缺该字段时回退控制面档）：**凡是回答「这棵树在哪里」的行都得改问树后端**（`M_OPEN` 的「视图不存在」提示里那两条按后端的分支同样改了判据）。⑤ 分片拒绝文案把环里的 **mount 地址**填进了建议命令的 `--control-server`，照抄必然连不上治理协议——治理口是 `--control-listen`，与挂载口不同号，而且代码里没有任何「mount+1」这种约定可依赖。现在文案点名 mount 地址（那正是客户端要被指引去的地方），并把命令指向「该实例自己的 `--control-listen` 口」；`tests/shard_routing.rs` 里加了一条**反向断言**盯住这个形状（拒绝文本里不得出现 `--control-server <mount 地址>`）。④⑤ 都不是逻辑错而是**说错话**：前者让运维照着屏幕去找一个不存在的 schema，后者让他连一个不该连的端口——两处都只有在真人读回显时才会被发现，所以它们连同 ①②③ 一起写在这里。五处都不是单测能发现的形状：**只有真起两个进程、真共享一台库、真把回显读一遍**才会撞见，这就是 `tests/shard_routing.rs` 存在的理由。

单测计数：`remote::meta::shard::tests` 7 条（环解析、非法环、单节点短路、行判定、后端不符拒绝、指纹去 credential、自身定位）+ `remote::meta::shard_follow_tests` 4 条（跟跳并 pin、home 是 HA 对时落到活的一半、互指的行以点名原因快速失败、空重定向不致命而是继续走 failover 列表）；`cargo test --release --features treedb` 全量 **429 passed / 0 failed / 4 ignored**，本主题新增 14 条（含视图落点标签那条回归单测）。
