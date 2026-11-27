# ADR-0004 · treedb 多实例的落点是**最少负载的可写引擎**，不是名字哈希

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（2026-11-27；替换的是同一入口上原先的 `fnv1a(view) % len(addrs)` 规则） |
| 日期 | 2026-11-27 |
| 前序 | ADR-0003 定下「treedb 实例的身份 = `host:port`」，并让 `store.info` 标出 `serving` 集合；本份回答它的下一个问题——一台服务器既然路由到 N 个地址，**新树该落哪一台**。PG 档的答案（`pg_store::place_new_view` 读 `view_placement` 取放置行最少者）在 treedb 档没有对应物：这一档**没有放置表**，「有哪些树」的权威是引擎自己的 `Op::ListViews` |
| 实现 | `src/treedb_store.rs`（`InstanceLoad`/`Placement`/`pick_least_loaded`/`engine_load`/`probe_load`/`plan_placement`/`hash_home`/`LedgerRow`/`TreedbLedger`/`storage_census`，以及 `find_tree_home` 的多命中消歧）、`src/remote/meta/server_ext.rs`（`view.create` 的报告行、`view.plan` 的 `rule`/`detail`/`load[]`、`store.info` 的 `ledger` 与 `viewsUnlistedFor`、`backend_layout` 那句）、`console/src/components/storage/InstanceCard.vue`、`console/src/components/views/NewBasePanel.vue`、`console/src/pages/{StoragePage,OverviewPage}.vue` |
| 验收 | 判据单测 3 条（`pick_least_loaded_*`，纯逻辑，不触网）；真机夹具 `bash .build-bin/test_treedb_balance.sh` 52 条断言全绿（2026-11-27，两主一副 `treedb://a,b,s`，见 §6）；`cargo test --release --features treedb` 全量 410 passed／0 failed／4 ignored |

## 1. 决策

**`--meta-store treedb://h1:p1,h2:p2[,…]` 的路由集合里，一棵新树落在「最闲的可写引擎」上；判据五条，顺序固定，全在 `pick_least_loaded` 一处：**

1. **只读副本与答不上来的地址一律不是候选**。不是降权，是排除：把树建到 `treedb-standby` 上等于建一棵每个写 op 都 `EROFS` 的树；把树建到一台没答话的引擎上等于建到一台无法验证的机器上。
2. **树数最少**（`ListViews` 的长度）。与 PG 档「放置行最少」同一口径，而且引擎重启后仍然成立。
3. **节点数最少**（`EngineStats.nodes`）。同为存量口径，用来分开「树数相同但一大一小」的两台——只看树数会把一棵挂着百万节点的树当成空实例。
4. **累计写 op 最少**（`EngineStats.writes_total`）。这是四条里唯一带冷热含义的数，但引擎重启归零，所以只能当次级键，否则刚重启的实例会被判成最闲而被灌满。
5. **serve set 顺序**。前四项全等时取靠前的那台，于是同一份账在任何进程、任何时刻都算出同一个落点——平局随机化只会让「为什么落这台」无法复现。

**只有一台地址时不探测**（`plan_placement` 直接回 `only instance`，报告行也不追加依据那句）——没有可比较的对象，为一棵树往那台发一次 `Stats` 只是让 `view create` 多等一个 RTT。

**已存在的树永远不问这条规则。** 读取走 `find_tree_home`，逐台问 `ListViews`，命中即家。这条分工是本决策可以成立的前提：改判据不会让任何一棵已存在的树读丢，因此判据可以随时替换，而不必先有一个 `view move`（这一档目前确实没有服务端搬迁，见 §5）。

**名字哈希只剩兜底身份**（`hash_home`）：只在「一台可写的都问不到」时用它，且报告行如实写 `no writable engine reachable`，不假装均衡过。

## 2. 为什么把哈希换掉

`fnv1a(view) % N` 只在**建树那一刻**看名字，对之后压在每台上的东西一无所知。后果不是分布不够均匀，而是**多实例的写吞吐天花板从来没被摊平**：一台挂着三棵热视图、另一台空着，新树照样按哈希扔给前者；而运维在控制台上看不见任何解释——哈希是确定性的，看起来却像随机，没人能回答「这台为什么空着」。

哈希在这里唯一还剩的价值是**无需账本即可重算**，而这一档的账本本来就在引擎里（`ListViews`），所以那个价值是零。留作兜底只是因为挂失的引擎无论按什么规则选都建不成树，而兜底路径也需要一个确定答案。

负载信号是**免费**的：`find_tree_home` 早已逐台 sweep，多实例路由在读取侧本来就成立，缺的只是放置判据。`Op::Stats` 属于引擎的 `is_control()`（只读副本也允许应答），grovefs 此前从未调用过它——一次连接里连同 `ListViews` 一起问 `Stats`，就多拿到 `nodes`/`writes_total`/`replica` 三个数。

## 3. 副本：一个 PG 档没有的信号，两条硬规矩

`EngineStats.replica` 让「这台是热备」成为内核事实，而不是注册表里的一个命名约定。于是：

- **放置侧**：副本被排除，不参与四键比较。它看起来永远最闲（清单是主的拷贝、`writes_total` 归零时刻还不一样），任何降权方案都会被它骗过。
- **读取侧**：`find_tree_home` 命中多台时**再问一轮** `Stats` 把可写者分出来——恰好一台可写就落它（副本上那些名字是主的影子，不是「部署被旁路写坏」）；零台可写给专错（点名要主，而不是挑一台副本去开可写句柄）；两台以上仍可写才报契约违背。这一轮额外 RPC 只落在多命中路径上，常见的「树只在一台」仍是一次 `ListViews` 就返回，不给 `store_for` 的快路径加往返。
- **显式点名侧**：`view create --pg-instance <副本名>` 被拒（`is a read-only replica`），而不是静默改投别台——点名是操作者的决定，拒绝才看得见。

把 `treedb-standby` 的地址写进 `--meta-store` 逗号列表是**受支持的形状**（它是唯一能让只读副本被 grovefs 读到的方式），代价是主上每棵树都在 serve set 里出现两次；§3 的三条规矩就是为了让这个代价不变成假故障。

## 4. 账本：`placedTrees` 之外有四桶，不是 PG 那三桶

PG 档的账本差是 `placed / unplaced / dangling`——差的是**放置行**。treedb 没有放置行，所以要说清的也不是「有没有行」，而是**「这棵树的落点能不能被说清」**，四类各是一种不同的说不清（`TreedbLedger`，`store.info.storage.ledger`）：

| 桶 | 含义 | 为什么会 nonzero |
| --- | --- | --- |
| `placedTrees` | 路由集内、可达、可写、注册表里有名、且不与别台重名的树数 | 正常态 |
| `unnamedHome` | 路由集内的地址持有树，但注册表没有任何名字指向它 | 单条 `--meta-store` 没起操作者文件；或有人删了行没停服 |
| `offServing` | 注册表里有名、但本机 `--meta-store` 不路由过去 | 加实例只改了书没重启；或那台属于另一个部署 |
| `unreachable` | 路由集内答不上来的地址 | 引擎宕机或网络黑洞——**这些实例的树数不可知，不当 0** |
| `duplicates` | 同一个名字落在两台**可写**可达实例上 | 真的有人绕过服务器手建了树 |

桶互斥由构造保证（非 serving → `offServing`；不可达 → `unreachable`；副本整行跳过；重名从 `placedTrees` 扣除）。副本行不进任何桶：它的树是主机那行已经计过的影子。

`store.info` 的实例行与这份账本**同出一次 sweep**（`storage_census`），且那次 sweep 是并行的：改之前逐台串行 `list_views`，一台黑洞主机会把整张页面钉住一整个连接超时。同一次 sweep 还把 `views` 的口径带了出来——答不上来的那台少掉的是**未知**而非零，所以顶层多了 `viewsUnlistedFor`（没被问到的服务地址数），总览页据此把那个数标成下限。

## 5. 负空间（这份决策没有解决的）

- **没有 `view move`**：treedb 档至今没有服务端搬迁（PG 有 `move_view`）。改判据只影响**之后**新建的树，已经歪在一台上的负载不会自己挪走——要挪只能 `view export`/`import` 重灌。本份的账本正是那条入口的判据来源。
- **没有冷热历史**：`writes_total` 是进程累计，重启归零；§1 第 4 键因此只能当次要 tie-break，不能当主键。没有滑动窗口、没有采样、没有 `ops_total` 的读侧权重。
- **不看容量**：引擎的 `nodes`/`writes` 都是存量与冷热，没有「那台盘还剩多少」。放置不看磁盘水位是这一档的既有边界（PG 档同样不看）。
- **平局不随机**：同一份账永远落同一台，稳定优先于「把新建摊得更细」。
- **答不上来的那台仍挡新建**：`find_tree_home` 在「零命中但确有实例不可达」时仍旧拒——这是「不把读不到当不存在」的既有纪律，代价是**任一**服务实例宕机时新机不能建（不是只有目标宕机）。这条可用性代价是已知的，留给与 `view move` 同批的处置入口。

## 6. 实测（夹具与数字）

`bash .build-bin/test_treedb_balance.sh`（自跳过：无 `--features treedb` 构建、无同级引擎二进制）起两主一副 + `meta-server --meta-store treedb://a,b,s`，**52 条断言全绿**：

- **反证哈希**：九枚视图名是**挑出来的**——`fnv1a(name) % 3 == 0` 全部指向第一项，旧的哈希规则会把 9 棵全塞进 `boxa`。实测落点 `boxa=5 / boxb=4`，差 ≤1，且九行报告行都写着 `least loaded: N trees / N nodes / N writes`。
- **副本**：`store.info` 在建树之前就把 `boxs` 标成 `replica: true`（证明 `Stats.replica` 真到得了内核之外）；没有任何新树落它；它的 `viewNames` 恰等于主机的树数；**影子名不被报成 `duplicates`**（这条是 §3 与 §4 必须同时成立的地方）；`--pg-instance boxs` 被专错拒绝且不留残树。
- **消歧**：一台同时被主与副本报告的视图，`view.plan` 回 `already placed` 且实例是**可写那台**；只在一台上的回 `boxb`（单命中，不多花一轮）。
- **宕机**：kill `boxb` 后 `store.info` 仍答（改前这一调用整体失败），`boxb.placedViews` 为 `null`、进 `ledger.unreachable`、`instanceError` 有句子、`viewsUnlistedFor=1`，而 `boxa` 的树数照计；副本不会因为主宕了就升格成落点。
- **兜底仍诚实**：一台可写的都问不到时，落点退回名字哈希并在 `rule` 里写明 `no writable engine reachable`。
