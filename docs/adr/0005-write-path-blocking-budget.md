# ADR-0005 · 写路径上的阻塞是有**预算**的，不是"反正 worker 会等"

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（2026-11-27；收口的是 `f7592c9` 引入 epoll 时明确记下的前置条件） |
| 日期 | 2026-11-27 |
| 前序 | `f7592c9`（挂载面/治理面改走 epoll 事件循环）在 `src/remote/meta/epoll.rs` 头部写下过一句前提：**本层的请求处理本身就是阻塞的**——引擎往返、`--witness` 的仲裁器同步投票、`--sync-repl` 的推备库。前一项的边界由 `DISPATCH_WAIT=50ms` + 满队列即断连兜住；后两项当时仍是**无界**的。本份就是把它们改成有界，并把"阻塞可以接受"从口头假设变成可测契约 |
| 实现 | `src/remote/server.rs`（`Fence::lease` 拆成 `decide`/`promote`，新增模块级 `enum Verdict`）、`src/remote/meta.rs`（`PROOF_JOIN_WAIT`/`SYNC_PUSH_CONNECT`/`SYNC_PUSH_SLOT_WAIT`）、`src/remote/meta/server_ext.rs`（`HaState` 单飞三字段 + `proof_done`、`fenced_for_write` 单飞循环 + 抽出 `arbiter_reproof`、`witness_quorum` 提前收口、`connect_bounded`、`push_snapshot_now` 有界化、replicator 连接改有界）、`src/stats.rs`（三枚计数器） |
| 验收 | `ha_tests::witness_quorum_decides_before_the_slowest_arbiter_answers`、`ha_tests::fenced_for_write_proves_at_the_arbiter_once_per_burst`、`ha_tests::sync_push_degrades_instead_of_queueing_behind_a_slow_standby`、`remote::server::tests::lease_renew_answers_without_draining_writes`（§6 数字即这四条跑出来的）；drain 语义未回归由既有 `remote::server::tests::claim_drains_inflight_writes_before_promoting` 钉住；e2e `.build-bin/test_ha.sh` **pass=33 / fail=0**（2026-11-27，段 7 RPO=0 / 段 8 分区降级 / 段 9 过期主被仲裁者挡成 standby / 段 10 仲裁宕机 fail-safe 冻写 + 恢复后恰好一主 / 段 11 三仲裁宕 1 不冻、宕 2 才 fail-safe）——单飞与提前收口都没碰零窗口 fencing 的语义 |

## 1. 决策

**写路径上每一次可能阻塞后端的调用，都必须有一个写死的预算，且超预算的后果是"降级"而不是"失败"或"无等"。** 四处，各自的预算与超支后果：

| 调用 | 预算 | 超了会怎样 | 绝不 |
| --- | --- | --- | --- |
| `Fence::lease`（数据面仲裁者，续约/落后判定） | 不等 —— 走 SHARED 门，与在飞写并行 | 无 | 让一次例行心跳去 drain 全节点在飞写 |
| `witness_quorum`（meta 侧同步重证明扇出） | 一个探测窗口 `WITNESS_PROBE_TIMEOUT=1s`，且**多数派到齐即返回** | 迟到的仲裁器被丢弃，本轮少采纳它的 `prev_max` | 让整轮等最慢那台 |
| `fenced_for_write` 的并发 burst | 单飞：一人扇出，其余 join `PROOF_JOIN_WAIT=1250ms` | join 超时判"本轮没有新证明"（同仲裁器报错的判词） | 每个写各起一轮 `n` 线程扇出 |
| `push_snapshot_now`（`--sync-repl`） | 推送槽 `try_lock_for(SYNC_PUSH_SLOT_WAIT=200ms)` + `connect_bounded(SYNC_PUSH_CONNECT=1s)` | 该 op 降级为 async 并 warn，计一次 `HaSyncPushDegrade` | 让全局锁横跨一次网络往返 |

**为什么"单飞"而不是"跳过"**：一个线程做扇出、其余人 join 它的判决，每个写仍然只依赖"至多一个探测窗口那么新"的证明——安全性判词与原来逐写复证完全一致，消失的只是线程风暴。若改成"别人在证我就直接放行"，那才是真正的语义退化。

**为什么超时不等于被 fence**：`fenced_for_write` 早就刻意**不因仲裁器出错而拦写**（一次网络抖动不该失败所有写）。加入 join 预算后必须守住同一条判词：预算耗尽 → `HaProofStale` + 放行，由异步 fail-safe（`WITNESS_FENCE_GRACE=3s`）去管"持续不可达"。把"没等到判决"当成"判决是不利"，会把可用性依赖重新引回仲裁器的抖动。

**leader 先发布判决、后释放槽**：`arbiter_reproof` 在 `proof_done()` 之前已经把 role/lease 写好，follower 持 `proof_mu` 复核时必然看得见结果。于是循环顺序是**每轮先复核判决、再抢槽**——否则被唤醒的 follower 会抢下刚释放的槽再发一轮同样的扇出（实测正是这个 bug 让 `conns` 从 1 变成 2）。同时"已经 join 过一次"的线程绝不再当 leader：仲裁器此刻并不比刚才更可达，未决态归异步 tick 管。

## 2. 为什么非改不可

epoll 之后，一条 worker = 一个同时在飞的后端调用，而 worker 只有 `GROVEFS_META_WORKERS`（默认 8）条。`Pool::send` 在队列满 50ms 后就 `meta epoll: worker queue full, dropping fd` 直接断连。于是写路径上任何一次无界阻塞，后果都不是"这条请求慢"，而是**整面挂载开始拒连**。改前三处正是这个形状：

- **`Fence::lease` 的续约拿的是排他门。** 在任 primary 每 `REPL_INTERVAL=500ms` 就要向每个 witness 续一次 `sent == cur`，每一次都要求全节点在飞写排空才答。续约不动 token，没有任何东西需要与写线性化——这个 drain 是纯浪费，且它的代价是**周期性、集群级**的。
- **`witness_quorum` 用 `thread::scope` join 所有探测线程。** 多数派早已授予租约，函数仍然要等那台黑洞仲裁器把 `timeout` 走完。
- **`push_snapshot_now` 里 `rt.standby_push.lock()` 无界，且锁内是裸 `TcpStream::connect`。** 一次 `VACUUM INTO` + 连接 + ACK 全在一把全局锁里；备机端口被**过滤**（而非 RST 拒绝）时，`connect` 走的是内核 SYN 重传序列（本机 `net.ipv4.tcp_syn_retries=6`，约 127s）。这是四处里唯一分钟级的阻塞。

## 3. 各处的具体形状

**`Fence::lease`：upgradable read + 只在真要动时升排他。** `parking_lot` 的 `RwLockUpgradableReadGuard::upgrade` 保证升排他时排队优先，不存在"我放下读锁、别人先抢了写锁"的插入窗口；而 `decide` 里 `cur` 之所以能被安全假定不变，是因为并发的另一个**读者**要改 token 也必须先拿到同一把排他门。真升主（`sent > cur`）保留完整 drain 与 `persist_current` 的 fsync。

**`witness_quorum`：按票到达判决。** `classify` 的 `fenced(q)`（`ahead ≥ q`）与 `leased(q)`（`advances + held ≥ q`）对已收票数**单调**，早判决不会被迟到票推翻。早退 `leased`/`won` 的健全性靠仲裁器集合的多数派交集（对端能顶到更高 epoch，自己必然也推进过多数，交集那台必答 `prev > epoch`）——这与提升路径本来就是同一个论证。放弃的只是迟到票对 `prev_max` 的贡献，下一轮纠正。探测线程**故意 detach**：join 正是要摆脱的那个迟到者，每跳自身已有界（connect/read/write 全取 `timeout`）。

**`connect_bounded`：`to_socket_addrs` + `connect_timeout`。** 域名解析仍拿不到 deadline（`std` 不给 resolver 超时），所以 **peer/witness 地址部署时用 IP**——这条残差写进了函数 doc，不假装已解决。后台 replicator 那处 `TcpStream::connect` 同步换成 `connect_bounded(_, CONTROL_TIMEOUT)`，保持它原有的宽松预算。

## 4. 可观测

三枚新计数器（`src/stats.rs`，`stats` 报告的"非计时计数器"段）：

- `ha arbiter proofs joined` —— 有写 join 了别人的证明（单飞真的在生效；healthy 集群里应该只在租约失效的瞬间跳）
- `ha writes without a fresh proof` —— join 预算耗尽，该写是在**没有新鲜证明**的情况下放行的（§6 的那次抖动会把它顶起来）
- `ha sync-repl pushes degraded` —— 因槽忙/connect 超预算而把某个 op 降级为 async 的次数（RPO=0 的承诺实际打了多少折扣）

三条都只增不减，与既有的 `ha fencing demotions` / `ha quorum refusals` 并列；日志侧沿用既有那句 `meta sync-repl: ... degraded to async for this op`，新增的"槽忙"分支也走同一措辞，运维的 grep 不用改。

## 5. 负空间（这份决策没有解决的）

- **DNS 解析仍无界**：`connect_bounded` 只 bound 了 TCP 握手。缓解是部署约定（witness/peer 用 IP），不是代码保证。
- **`dispatch_meta` 的引擎往返仍在 worker 上**：本份不动后端调用本身，兜住它的是 `Pool::send` 的 50ms 背压 + 断连重拨。treedb/SQLite/PG 各自慢不慢，是另一档事。
- **数据面 `dispatch` 的磁盘阻塞**：`write_gate()` 持 SHARED 横跨 backend 写，这是**特性**（正是它让 claim 能 drain），代价是慢盘会拉长一次真升主的 drain。本份只取消续约那一侧的 drain。
- **多个迟到者仍可能各起一轮**：若一轮扇出结束时既没 fence 也没 lease（低于多数、或全体无声），后续 burst 会再起一轮。这是有意的——单飞的边界是"一次 burst 一次扇出"，不是"仲裁器不可达时只试一次"，后者归异步 fail-safe。
- **不重叠租约的残差未变**：被 fence 的旧主在学到自己已被 fence 之前仍在服务，那是"提升要不要等前任租约到期"的共识时钟问题，与 f7592c9 记下时同为 out of scope。

## 6. 实测

全部数字来自本轮 `cargo test --release --features treedb` 的一次实跑（临时 A/B 探针取完即删，提交树不留）：

- **续约不再 drain**（`lease_renew_answers_without_draining_writes`）：一条写正持着 SHARED 门时，改后的 renew **194 µs** 答完（同轮另两次 207 µs / 217 µs）；把改前的排他门版本放进同一场景，**800 ms 内没有答**，直到写门放开后 **48 µs** 才返回（总 800.169 ms）——即它的延迟恰好等于"最慢那条在飞写"的长度。
- **提前收口**（`witness_quorum_decides_before_the_slowest_arbiter_answers`）：3 台仲裁者，2 真 1 黑洞（只 bind 不 accept，故 connect 成功、read 超时），预算 `timeout=1.5s`。改后 **699 µs** 返回且 `leased(q)=true`；同一地址集跑改前的 `thread::scope` join-all 版本是 **1.5102 s**，判决相同。**≈2160 倍**，且这 1.5s 是压在 worker 上的。
- **单飞**（`fenced_for_write_proves_at_the_arbiter_once_per_burst`）：手写仲裁者对每个 `OP_FENCE` 睡 400 ms，8 线程 Barrier 同放。改后仲裁者侧**接受连接数 = 1**、burst 墙钟 **401.06 ms**；把同样的 burst 换成"每写一轮扇出"，接受连接数 = **8**、墙钟 429.70 ms——墙钟接近是因为 8 轮本来就并行，省下的不是延迟而是 **8 条 worker 与 8n 条探测线程**（n 为仲裁器数）。
- **推送槽降级**（`sync_push_degrades_instead_of_queueing_behind_a_slow_standby`）：主线程占住 `standby_push`，子线程调 `push_snapshot_now` 在 **210.98 ms** 返回（预算 `SYNC_PUSH_SLOT_WAIT=200ms`），并打出 `push slot busy >200ms ... degraded to async`；改前它会等到持有者释放（无上限）。role 仍是 Primary——降级不是自降级。
- **connect 预算**（一次性探针）：`192.0.2.1:9`（TEST-NET-1，SYN 被静默丢弃，正是"端口被过滤的备机"形状）。`connect_bounded(_, 1s)` **1.0001 s** 返回 `TimedOut`；裸 `TcpStream::connect` 观测 8 s 仍停在 SYN-SENT，按本机 `tcp_syn_retries=6` 的退避序列约 **127 s** 才会失败。这 127s 过去是发生在**持有全局推送锁**的时候。

单测计数：`ha_tests::` 14 passed / 0 failed（含新增 3 条），`remote::server::` 侧 Fence 9 条全绿（含新增 1 条）；`cargo test --release --features treedb` 全量 **414 passed / 0 failed / 4 ignored**（上一笔为 410/0/4，+4 即本轮新增）。
