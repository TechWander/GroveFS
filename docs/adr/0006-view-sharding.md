# ADR-0006 · 视图分片：多个 meta-server **进程**，一台 PostgreSQL **控制面**

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（2026-11-27） |
| 日期 | 2026-11-27 |
| 前序 | ADR-0004（视图→PG 实例的负载感知放置，作用在**一个 meta-server 进程内部**）、ADR-0005（写路径阻塞预算，作用在**一对主备之间**）。本份补第三根轴：**服务同一个集群的多个 meta-server 进程**之间怎么分工，以及为什么分工的答案必须落在 PostgreSQL 里 |
| 实现 | `src/remote/meta/shard.rs`（新：`parse_ring`/`MetaNode`/`Shard`/`Home`、`grovefs_control.meta_placement` 的 DDL+advisory lock 读写、`route`/`peek`/`adopt`/`forget`/`placement_map`）、`src/remote/meta/server_ext.rs`（`MetaServer::with_control_plane`/`with_sharding`、`meta_open` 的 `M_OPEN` 路由门、`meta_work` 的逐 op 放置门、`evict_misplaced` 清扫（间隔可配：`MetaServer::with_shard_sweep_secs`）、`view.create`/`view.rm_base`/`view.move_meta` 三处治理门、`physical_tree_exists`/`physical_tree_list`/`fork_tree_exists` 的"树在本地、注册表在远端"分流、`store.info` 的 `shard` 块）、`src/remote/meta.rs`（`E_SHARD = 11`、`SHARD_FOLLOW_MAX = 3`、跟跳与 home pin、`round_trip` 的挂断重放）、`src/main.rs`（`--control-plane`/`--shard-ring`/`--shard-id`/`--shard-sweep-secs`、`view move --to-meta`）、`src/mcp.rs`（`ControlStore::tree_backend()`，以及视图列表按树后端报落点）、`src/control_remote.rs`（远程档的树后端跟随 `store.info` 回包里的 `metadataBackend`）、`src/stats.rs`（四枚计数器） |
| 验收 | 单测 `remote::meta::shard::tests` 9 条 + `remote::meta::shard_follow_tests` 9 条 + `remote::meta::server_ext::shard_report_tests` 6 条 + `mcp::tests::list_views_text_labels_the_tree_by_the_tree_backend` 1 条 + `control::tests::a_failed_connect_names_the_host_and_not_the_password` 1 条（连不上 PG 时那句错误只点名 host:port/db，URI 里的口令不出进程），`remote::meta::server_ext::msg_shape_tests::server_replies_are_single_line` 1 条（扫 `server_ext.rs`/`shard.rs`/`main.rs`/`web.rs`/`apikeys.rs` 五个文件的非注释行：回包路径出现「句中 `\n` + 2 格以上缩进」即红，CLI/控制台要 12 格以上才红，另有一条「非注释行以空格结尾」抓跨真实换行的字面量），`--lib` 全量 **341 passed / 0 failed**；e2e `tests/shard_routing.rs` 9 条（SQLite 树跨实例分片、PG 树跨实例移动后挂载跟随、lens 只认 base 的行、被拒挂载不留行、事件流跨实例推送、三节点环按行路由（含行指向环外节点、以及行点名的后端该进程并不服务两种破损）、同一视图上的并发 `view move`、清扫间隔可配、`0` 拒绝启动、`store.info` shard 块的行数/时间戳/清扫窗口/错配与孤儿行/清扫已做账）**9 passed / 0 failed**（`--test-threads=1`，15.81 / 15.74 / 15.71s；每条真拒因另过一遍 `one_line`——回包必须是一个逻辑行）；另有 `tests/cli_msg_shape.rs` 1 条 / 11 个拒绝组合（跑二进制读 stderr，量的是编译进去的字符串：无缩进的续行误打与两格手折都跑不过它）；§9 的延迟数字即其中三条 e2e 实跑打印的 |

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
- **行 `meta` 点名自己、`store` 却不是本进程服务的后端**：同样是 **Err**，不是「这行大概旧了，按我现在的后端接着用」。树在另一个库里，本地照样能开出一个引擎并写进去，而真正的持有者毫不知情——两个实例同写一棵树正是这张表唯一要防的事。拒绝发生在 `M_OPEN` 的门上，而 `peek()` 在任何认领之前，所以被拒的挂载**不会**改写表；文本点名表里的后端名与 `--meta-store` 这条修法，且跨重定向原样回到客户端（入口不是 home 时也一样）。行由成功挂载与 `view move` 盖章，因此现场只有两条修法：把 `--meta-store` 改回表里点名的那个后端，或 `view move --to-meta` 到真在服务它的实例（必要时先搬出去再搬回来重新盖章——搬回本节点是早退，不重盖章，见 §8）。
- **`--shard-ring` 与放置行互相矛盾**（两行互指）：客户端的跟跳有 `SHARD_FOLLOW_MAX = 3` 上限并带环路检测，失败时给的是**点名原因**的错误（"placement for view 'X' points back at …"），不是泛化的"连不上"。
- **失效事件流也是 `M_OPEN` 的消费者**：挂载除了工作 socket 还有一条只读的订阅 socket（peer 的 create/delete/rename 靠它作废内核缓存）。它同样跟 `E_SHARD` 跳，同一个上限、同一个环路检测——否则非 home 实例上的挂载会「op 读对的树、作废收不到」，是所有分片故障里最难发现的一种（无任何错误码可见）。订阅被拒时服务端写的拒因透传到挂载日志，不再是一个裸 EIO。

## 6. 治理动词的跨实例语义

`--control-plane` 让**定义**从每个实例都可见，这恰恰制造了新坑：`view_exists` 在远端也是真，非 home 实例若照旧执行，就会在同一个名字下建出第二棵树——后面没有任何动词能修好这件事。所以：

- **`view create`**：先过 `route()`。行指向 peer → 拒绝，并把"该找哪台、命令怎么写、要搬就 `view move --to-meta`"三句一起给出。行指向自己 → 行**写在树之前**，与 PG home 行同一个理由：那一行就是告诉其他实例"这名字占了"。
- **`view rm`**：删树是 home 的活。peer 若删掉行而树还在别处活着，留下的就是一个"挂载再也解析不到放置"的悬树。删成功后 `forget()` 删行，best-effort——树已经没了，读不到的控制面只会让计数偏一点，不会复活视图。**这一条走 `peek_forget()` 而不是 `route()`**：见 §3 的例外。
- **`view move --to-meta NODE`**：**必须在 TARGET 的控制端口上执行**。要为新家答复的那个进程才有权声明"这棵树我打得开"，它探测通过才写行。派生视图（lens/fork/bias）随 base 一起走。**同一视图上的两次 `view move` 没有人排的队**，所以线性化写在行本身：改行是一条`INSERT … ON CONFLICT DO UPDATE` 单事务，`meta`/`store`/`updated_at` 同生同死，任意瞬间表里对该名字只有一行完整的、属于同一个进程的放置；没被点名那台由清扫放手，与两次调用的先后无关。`prev == self` 的早退（「已经在我这儿，无需搬」）**不重写 `store`**——重新盖章只能靠搬到对端再搬回来。
- **`E_SHARD` 出现在 op 上（不只是 `M_OPEN`）**：一个已经挂载着的连接，在它的视图被搬走之后，旧实例会在清扫时丢掉引擎缓存（清扫间隔 = `--shard-sweep-secs`，默认 5s，env `GROVEFS_SHARD_SWEEP_SECS`；`0` **启动拒绝**——永不重读放置表不是「维持现状」，是两个实例同写一棵树）；下一次 op 是 MISS，此时才查表，指向别人 → `E_SHARD` + 新 home + **挂断**。客户端认得这个码：采纳新 home、清空连接池、重握手、**重放那个 op**，并且**循环跟到不再被弹**——上限同样是 `SHARD_FOLLOW_MAX = 3`（一个视图可以在 op 排队期间被搬两次：`view move`，然后运维改主意）。被拒的 op 从未执行过，所以重放是安全的。于是 `view move --to-meta` 对**已建立的挂载**也生效，运维不需要挨个 remount。
  跟不完的时候必须说清是**路由**坏了：预算用尽回 `MetaError::Shard`（Linux 挂载 `EIO`，Windows 客户端 `ERROR_BAD_NETPATH`），重定向里没带可拨地址时 `err_from_code` 给同一个变体。两者都**不是** `Db` ——一条「metadata db error」会把人派去看树，而坏的是放置表，这是这条错误最坏的失败方式。
- **重放的次数口径写在这里，因为它决定写语义**：跟 `E_SHARD` 的那条重放的是**被拒**的 op，从未执行，所以是 exactly-once；`round_trip` 的**传输错误**那一侧重放的是「已发出、回包丢失」的 op，那是 **at-least-once** —— 一次写可能在服务端落地两次。GroveFS 的元数据协议不带客户端请求号，去重靠树本身：`M_CREATE` 对重复行回 `E_EXISTS`，同目标的 `M_RENAME`/`M_SETATTR` 天然幂等，`M_UNLINK` 回 `ENOENT`。这个窗口对命名空间操作无害，也正是不做写请求幂等令牌的原因 —— 但它必须被写成口径，而不是留在注释里等人发现。

## 7. 可观测

四枚分片计数器（`stats` 报告的"非计时计数器"段）：

- `shard mounts redirected` —— 本实例把挂载指引给别人（入口侧 + 客户端跟跳侧都计；工作 socket 与失效事件流是同一条 `M_OPEN` 的两个消费者，任一处跟一跳都算）
- `shard management verbs refused` —— 治理动词因为"树在 peer 那里"被拒的次数；它涨而 `views` 不涨，说明有人在对着错误的端口敲命令
- `shard placement read/write errors` —— 放置表读不到/写不进（每一次都以拒挂载或拒动词收场，绝不以猜收场）
- `shard engines dropped by the placement sweep` —— 清扫因为行已点名他者而放手的树数。注意落点：遥测注册表只在挂载进程调过 `stats::enable()` 后才记账，而 `--stats` 是挂载旗标，meta-server 从不调它，所以这一枚在服务端的真答案是下面 `store.info` 的 `evictedTotal`；计数器留给开了遥测的嵌入方与挂载侧。

跟分片有关但不只属于分片的另一枚：`invalidation stream reconnects` —— 挂载的失效订阅没能（重新）建立的一次尝试。它和 `remote reconnects` 不同涨才有意义：后者不涨而它一直加，说明挂载在正常服务而内核缓存已经过期（监听退避 300ms→5s，第 1 次失败与此后每 20 次各打一行原因，一次会话撑过 30s 后退归零并打一行「streamed for Ns before dropping again」）。

**治理回包是一个逻辑行**：`M_OPEN` 的拒因、`view create`/`view rm`/`view move` 的拒绝与建议、`store.info` 与网页控制台转发的原文、审计行的 detail，全都按单行写。这不是格式洁癖，是 CLI 输出契约（`grovefs --help` 的 OUTPUT CONTRACT：「Pipes: ANSI is disabled, soft wrapping is off (one row per line)」）的下游——`cli_style::width()` 在非 tty 时返回 `usize::MAX`，`ui::wrap` 于是**不改写**文本，只有终端上才会被重新折行。所以一句在中间断开、后面跟一串缩进列的文案会原样进日志、进审计、进文档示例，而且照抄回去的命令带着换行。守卫三处：源码侧 `remote::meta::server_ext::msg_shape_tests::server_replies_are_single_line`，端到端侧 `tests/shard_routing.rs::one_line`，以及起二进制读 stderr 的 `tests/cli_msg_shape.rs`——它读的是**编译进去的字符串**，所以两格的手折也照样测得到（源码扫描看不出那种形状，跑二进制的那条看得出）。阈值按文件分档：回包路径「`\n` 后跟 2 格以上」即红，`src/main.rs`、`src/web.rs`、`src/apikeys.rs` 要 12 格以上才红，因为 `--help` 的示例与 hint 本就是两格折行。源码守卫还盯第二种长相：**字面量跨真实换行续写**（行尾留一个空格），它不带 `\n`，扫转义序列完全看不见，所以判据里加了「非注释行以空格结尾即判红」——全 `src/` 只有 5 行命中，恰好是病灶的全体；确实该换行的地方（SQL、文件模板）在上一行注释里写 `msg-shape: keep-line-break` 豁免。**CLI 自己的 flag 拒绝也在契约里**：`grovefs web`、`meta-server --admin-user`、`--password-stdin`、`--session-ttl` 那些检查同样写成句中换行加一串缩进列，实测 `2>&1 | cat -A` 修复前 2~5 行、修复后各 1 行。病灶在源码里有三种长相，修复前的读数用守卫自己的判据在 `git show HEAD:…` 上数出来：**形 A**「`\n` + 一串缩进列」37 处（`server_ext.rs` 26、`shard.rs` 1、`main.rs` 8、`web.rs` 2）；**形 C**「跨真实换行续写」4 个站点（`main.rs` 1、`apikeys.rs` 2、`server_ext.rs` 1），源码里 5 行以空格结尾；**形 B**「`\n` 紧跟续行反斜杠」5 处（`main.rs` 1、`web.rs` 4），外加 2 条 CLI `error:` 里的 7 处两格手折——这两类编译出的换行要么不带缩进、要么只带两格，12 列阈值按设计放过，由 `tests/cli_msg_shape.rs` 从二进制层面量回。合计 54 处硬换行、5 个文件；真该多行的只剩 clap 的 `--help`/EXAMPLES 块，以及 `pg_store.rs`、`treedb_store.rs` 里 instances.conf 的文件模板。

`store.info` 的 `shard` 块只在该进程真的在环里时出现（`null` = "本进程服务所有视图"，与 `ha` 块同样的形状）：`self`/`addr`/`ring`/`backend`/`controlPlane`/`sweepMs`/`placementRows`/`orphanViews`/`homeMismatch`/`evictedTotal`/`lastEvictedAt` 加每节点 `{name, addr, views, rows, here, lastUpdatedAt}`。`views` 按**去重后的物理 base 树**计数（lens 与它的 base 是一棵树），折回用的就是路由那条 `shard_routing_key`；`rows` 仍给原始行数，因为一条派生行本身就是上面 §2 的破损现场，把数字折掉等于把发现也折掉。`orphanViews` 列出指向环外节点的行——它不进任何节点的计数，而挂载那个名字仍然被拒，所以必须单独看得见。`lastUpdatedAt` 按 instant 比较、发出表里的原文（`updated_at` 带本地偏移，文本序不等于时间序），无行的节点是 `null` 而不是编造的时间。`sweepMs` 是本实例配置的窗口；`homeMismatch` 说出「表已点名他者、本进程还开着引擎」的视图名，`view move` 后到下一次清扫之间恰有一条是正常态，把它清空的是 `evict_misplaced`。`evictedTotal`/`lastEvictedAt` 是同一件事的另一半——清扫**已经做过**什么（本实例放手的树数、最后一次的时刻），而不是它还没做什么；一次搬家之后先只有 `homeMismatch` 有条目，那一轮清扫跑过才进 `evictedTotal`，从未发生过就是 `0` 与 `null`，两个真答案而不是缺字段。时刻用与 `updated_at` 同一格式的本地 RFC3339 文本发出，运维拿它去 grep 对家的表。同一件事也打在清扫那一行上：点名每一棵树与它的新 home（超过 `EVICT_LINE_PAIRS` 条折成 `+N more`），于是「这台实例的 `residentEngines` 为什么掉了」这一问有现成的答案可查，不必临时插桩。地址随块外发（它是客户端要被指引去的地方，不是秘密），**后端 URI 永不外发**——`backend` 是去 credential 的指纹。`controlPlane` 说清放置表到底与树同库（`"tree backend"`）还是被 `--control-plane` 另指了一台（`"separate --control-plane"`），两边都先过 `resolved_control_uri` 展开，因为一个裸 `--meta-store` URI 会被采纳成 `<base>/instances.conf` 托管注册表，同一台库有两种写法。审计侧每个分片决定都落一行：`M_OPEN` 重定向记 `method=meta / M_OPEN`（"view='x': redirected to meta-server 'm1'"），逐 op 门记 `method=meta / M_SHARD`（"…moved to meta-server 'm1', this mount follows it"），治理动词被拒也各留一行——运维在一台不服务的实例上敲 `view rm`，审计里看得见它被拒的原因。

## 8. 负空间（这份决策没有解决的）

- **`backend` 指纹记的是进程的数据目录，不是数据库**：一个裸 `--meta-store` URI 会被采纳成 `<base>/instances.conf` 托管注册表，而指纹用的就是那个文件路径，所以**同一台 PostgreSQL 在两个 `--meta-dir` 下有两种指纹**。后果落在 §5 那条 Err 上：换过数据目录的实例会拒绝自己留下的行，而 `view move --to-meta` 搬回本节点是早退、不重新盖章，能修好现场的只有「搬到对端再搬回来」。`TODO(severity=Medium)`（`src/remote/meta/shard.rs` 的 `store_fingerprint`）给了两条候选：指纹改记数据库本身（host/port/dbname），或者允许被点名的实例在树确实在自己后端时重盖章——两者都在重新定义「什么算换过 `--meta-store`」，所以必须连同 §5 那条拒绝一起决定，不能只动指纹。
- **不做再平衡**：加节点不会自动分走任何既有视图，`view move --to-meta` 只按人点名的视图搬。放置是记录，不是策略。
- **不复制树**：共享后端下 move 是一次改行；SQLite 后端下的跨机搬迁仍然只有 export/import 这一条路。
- **不代理数据面**：入口实例永远不把 op 转发给 peer，因此**没有**"两实例间的一条内部连接"可以观测——故障域是每个实例自己到 PG 的连接。
- **控制面仍是单点**（就挂载建立这件事而言）：本份给它的是**有界**（`SHARD_DB_TIMEOUT = 2s`，连接与 statement 双端设限，ADR-0005 的规则搬到分片路径）与**只影响新连接**，不是多副本。
- **home 实例重启期间的挂载**：与不分片时一样由客户端 failover 列表处理；环不会替一个不在场的实例回答。
- **数据一致性仍由单 home 保证**：分片没有引入任何跨实例的写协调，也不需要。

## 9. 实测

数字来自本轮 `cargo test --release --features treedb --test shard_routing -- --test-threads=1 --nocapture` 的三次连跑（两台 meta-server + 一台本机 PostgreSQL 5433，loopback；三节点环那一条临时起第三台，全程 PG 侧连接峰值 82 / `max_connections=100`——采样的是 `pg_stat_activity` 里 `datname='grovefs' AND usename='grove'` 的后端数，排除采样会话自己，每 0.1s 一次；空闲时同一查询是 0，所以这个数就是套件自己的占用）：

- **路由的代价**：同一个实例上先挂自己 home 的视图 **4.86 / 5.92 / 4.86 ms**，再挂需要被重定向的视图 **8.17 / 8.86 / 7.71 ms**——多出来的是**一条主键读 + 一次额外握手**（逐轮 +3.31 / +2.94 / +2.85 ms；同一指标三轮之间的抖动是 1.1 ms 量级，所以这个差值不是抖动造出来的），且只在挂载建立时付一次。
- **搬走的视图被旧实例放手的窗口**：行改写后，旧实例的下一次清扫在 **4.800 / 4.785 / 4.794 s** 内丢掉该树（默认 `--shard-sweep-secs 5` = 5000 ms 的上界，实测贴着它）。同一个动作用 `1s` 配置重测：**0.783 / 0.810 / 0.799 s** —— 间隔是真的被读，而不是写在常量里（`tests/shard_routing.rs::a_shorter_sweep_shortens_the_window_a_deposed_instance_holds_the_tree`；`view move` 的回包也按配置值答复，不再印固定数字）。
- **已建立挂载跟着走**：清扫之后第一个 op 在 **62.3 / 63.5 / 60.8 ms** 内落到新家并成功（`E_SHARD` 挂断 + 重握手 + 同一 op 重放），无需 remount；此后 `readdir` 同时看得见搬之前建的 `before` 与搬之后建的 `after`——**一棵树，两个实例先后服务过它**。
- **同一视图上的并发 `view move`**：12 次调用（6 轮 × 2 个线程，各打自己那台的控制口、目标点名对面）全部成功，回包各自点名自己那个目标，只有真改了行的那半句点名对面；结束时表里**恰一行**，`(meta, store)` 属于同一个进程（`store` 等于胜出那台 `store.info` 的 `backend`，且不含口令）；落败那台在 1s 清扫下 `residentEngines` 归零、`homeMismatch` 为空，之后在它上面挂载仍被指引到胜出那台，挂载并建目录成功（`tests/shard_routing.rs::a_shorter_sweep_shortens_the_window_a_deposed_instance_holds_the_tree`）。
- **行点名的后端该进程并不服务**：把 `store` 列改成另一个 URI 后，环里三台的挂载全被拒，拒因原文带着那个不存在的后端名与 `--meta-store` 的修法（跨一次重定向也不改写），表一行没动（`(view, meta)` 集合与改列前逐字节相同），`store.info` 仍把该视图计入行所点名那台；把列改回该实例自己的后端，挂载立刻恢复（`tests/shard_routing.rs::a_three_node_ring_routes_by_the_row_not_by_adjacency`）。同一条用例顺带量出：同库两个节点的 `backend` 指纹并不相同（各记自己的`instances.conf`），所以这一段注入的必须是**别的库**的名字，见 §8。
- **稳态成本**：被重定向那一侧的 `residentEngines` 在挂载前后不变（e2e 直接断言），即非 home 实例对别人的视图不留任何缓存。
- **e2e 与真实回显逼出的八处真缺陷**（都已修，列在这里说明为什么它们值得进 ADR）：① `SELECT to_regclass(...)` 原样取列在 rust-postgres 下是**反序列化失败**，而它跑在清扫线程与 `store.info` 上——一个 panic 的 worker 会让整台实例的控制端口不再应答；② `--control-plane postgres://…` + SQLite 树时，"这棵树在不在"被拿去问 PG（查 schema），于是**每个实例都看不见自己的本地树**，自家挂载被自家拒绝；③ 放置表要拨的那个 URI，若直接拿 `--meta-store` 的原文会得到一个 `file:` 托管注册表路径，`postgres::Config` 当场解析失败。④ 视图列表里「这棵树落在哪儿」是按**控制面**后端渲染的，于是 `--control-plane postgres://… + SQLite 树` 把本地文件说成 `own schema grovefs_v_…`；而同一条渲染路径在远程 SQLite 档下（自 `283dd4a` 起）一直印着 `own tree ` 加一个空路径——因为远程 store 没实现 `tree_home_rel`。修法是给 `ControlStore` 加 `tree_backend()`（默认等于 `backend()`，`FileControlStore` 在引擎档答 `treedb` 并把 `tree_home_rel` 交回空串，`RemoteControlStore` 从 `store.info` 新增的 `metadataBackend` 字段探得，老服务端缺该字段时回退控制面档）：**凡是回答「这棵树在哪里」的行都得改问树后端**（`M_OPEN` 的「视图不存在」提示里那两条按后端的分支同样改了判据）。⑤ 分片拒绝文案把环里的 **mount 地址**填进了建议命令的 `--control-server`，照抄必然连不上治理协议——治理口是 `--control-listen`，与挂载口不同号，而且代码里没有任何「mount+1」这种约定可依赖。现在文案点名 mount 地址（那正是客户端要被指引去的地方），并把命令指向「该实例自己的 `--control-listen` 口」；`tests/shard_routing.rs` 里加了一条**反向断言**盯住这个形状（拒绝文本里不得出现 `--control-server <mount 地址>`）。④⑤ 都不是逻辑错而是**说错话**：前者让运维照着屏幕去找一个不存在的 schema，后者让他连一个不该连的端口——两处都只有在真人读回显时才会被发现，所以它们连同 ①②③ 一起写在这里。⑥ `view rm` 用的是带 bootstrap pin 的 `route()`：一台已经把该树删掉的实例把行删干净了，于是另一台 peer 收到一次普通的收尾 `view rm` 时，这个名字在表里已是「未放置」，`route()` 把行**写成了指向 peer 自己**，然后才在「本地没有物理树」上失败——结果是一个会说谎的放置行，它让这个名字在下一次 `view create` 时被「已放置在 X」拒掉，而 X 上什么都没有。两节点环撞不出这个形状（行只能指向「对家」，永远指不到第三方），是三节点 e2e 的第一轮就跑出来的。**它和 ①②③⑤ 一样是判据错，不是文案错，但只有把行真的读回来才看得见：所以 `tests/shard_routing.rs` 现在每一条收尾都断言「本 run 的放置行为空」。**⑦ 一次 PG 连不上的错误文本把**整串 URI 连同口令**带进了 `postgres connect …`。它不是日志里的小事：同一句文本随控制面回包上终端与网页控制台，随被拒挂载上挂载用户的屏幕，并落进审计行。全库十几处 `postgres connect` 都只点名实例名，漏的是一处直接插 URI 的；现在它过一次 `pg_store::redact_uri`，并由 `control::tests::a_failed_connect_names_the_host_and_not_the_password` 钉住（打一个必然拒连的端口，断言错误里有 host:port/db、没有口令）。这一处是并发`view move` 那一段的失败回显里读出来的——**又一个只有真跑才撞见的形状**。⑧ 服务端文案里把行连接 `\` 误打成了字符串内的 `\n` 再加一串缩进：句子在中间断开，下一行凭空缩进十几个列。终端上看不出来（`ui::wrap` 按空格重排把它抹平了），**管道与重定向里就是原样**——实测 2026-11-28 修复前，在没有 `--shard-ring` 的实例上敲 `view move`，`grovefs … 2>&1 | cat -A` 打出两行，第二行前面 21 个空格；同一命令在修复后是一行。这类文案同时是 `store.info` 的回包、网页控制台的原文、审计行的 detail，也是文档里那些「真实回显」示例的来源：`USAGE.md`、`USAGE.en.md`、`docs/deploy.html`、`docs/docs.html` 四处示例当初就是照抄这个畸形形状写的。同一形状在 CLI 的 flag 拒绝和控制台的启动文案里也有一份，本轮一并改掉（5 个文件、54 处硬换行，逐类读数见 §7），并把 `tests/cli_msg_shape.rs` 加上——它起真二进制跑 11 个拒绝组合，读回的 stderr 必须恰好一行。**判据只有一条：`--help` 之外的服务端回包必须是单行**，所以它有三处守卫（源码分档 + `shard_routing` 的 `one_line` + 起二进制的 `cli_msg_shape`）而不是靠人读；三条都靠往源码里注入一处畸形自证过会报红。八处都不是单测能发现的形状：**只有真起两个进程、真共享一台库、真把回显读一遍**才会撞见，这就是 `tests/shard_routing.rs` 存在的理由。

单测计数：`remote::meta::shard::tests` 9 条（环解析、非法环、单节点短路、行判定、后端不符拒绝、指纹去 credential、自身定位、「销毁动词只读行、无行即本实例」，以及「清扫间隔有默认值、可覆盖、`0` 原样传递以便服务端拒绝」那条）+ `remote::meta::shard_report_tests` 6 条（lens 折成一条 base 树却仍是两行、指向环外节点的行谁都不算但列进 `orphanViews`、`lastUpdatedAt` 取的是更新的 instant 而非更大的字符串，以及把折回规则本身按真控制面读一遍：lens 与 fork 落到 base、alias 留在自己名下、解析不了的 def 不成树、清扫的记账（没放手过就是 `0` 与 `null`，时刻只在真放手时移动）与驱逐行的点名（每棵树带上它的新 home，超过上限折成 `+N more`））+ `remote::meta::shard_follow_tests` 9 条（跟跳并 pin、home 是 HA 对时落到活的一半、互指的行以点名原因快速失败、空重定向不致命而是继续走 failover 列表，事件流那三条：跟 `E_SHARD` 到新家并在同一条 socket 上收到 push、互指环以点名原因收场、非 `E_SHARD` 的拒因透传到客户端，以及 op 侧那两条：一个 op 被连弹两次仍跟到 settles 且每跳只重握手一次、两行互弹时以 `MetaError::Shard` 点名收场且总次数恰为 `SHARD_FOLLOW_MAX + 1`）；`tests/shard_routing.rs` 九条端到端（两实例分流、PG 共享后端下搬家的视图跟着已开挂载、lens 只认 base 的行、被拒挂载不留行、事件流跨实例的失效推送、三节点环按行路由（同一条里再跑一段「行的 `store` 列点名这台进程并不服务的后端」：三个入口都硬拒、拒因原文跨重定向不改写、表一行不动、`store.info` 仍按行点名那台计数、把列改回就能挂），以及清扫旗标的两面：`1s` 真的把旧实例握着同一棵树的窗口缩到一秒内、且放手之后 `evictedTotal` 立刻见数、`lastEvictedAt` 解得开 RFC3339、`0` 直接启动失败；同一条用例还在同一视图上并发 12 次 `view move`（6 轮 × 2 个线程，各打自己那台的控制口、目标点名对面）：每轮两次调用都 `Ok`、回包各自点名自己的目标、结束时表里恰一行且 `(meta, store)` 属于同一个进程、落败那台在 1s 内 `residentEngines` 归零且 `homeMismatch` 清空、在它上面挂载仍被指引到胜出那台并建得出目录。再加 `store.info` shard 块那一条：真表上读出 `rows`/`lastUpdatedAt`/`sweepMs`/`homeMismatch`/`evictedTotal`/`lastEvictedAt`（行翻了而 600s 的清扫还没跑，计数必须仍是 `0`、时刻仍是 `null`），并把 600s 的窗口钉在那里让「表说对家、进程还握着引擎」这一条被断言而不是被推断）。上面的 9/9/6/9 与 `control::tests::a_failed_connect_names_the_host_and_not_the_password`（连不上 PG 时错误里只有 host:port/db，没有 URI 里的口令）、`remote::meta::server_ext::msg_shape_tests::server_replies_are_single_line`（回包与 CLI 文案的形状，按文件分档——回包路径 2 格以上缩进即红，`main.rs`/`web.rs` 12 格以上才红，注入一处 `\n` + 缩进即以 `文件:行号` 报红），以及全量 **341 passed / 0 failed**（`cargo test --release --features treedb --lib`）均取自当轮实跑。
