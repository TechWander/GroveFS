# ADR-0001 · manager：挂载引导服务与注册防伪

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（范围＝M1 全量 + HMAC 防伪档）；**后续修订**：手敲的 `--meta-server`/`--data-servers` 挂载坐标已整体移除，`mount` 只能经 `--manager` 引导（见下 Q2 与 ADR-0002） |
| 日期 | 2026-11-19 |
| 实现 | `src/manager.rs`（roster/announcement 类型、帧编解码、签发/验签、客户端调用、心跳节拍）、`src/manager/server.rs`（daemon、声明存储、注册策略、fleet view）、`src/remote/proto.rs`（`OP_REGISTER=14`/`OP_LOOKUP=15`/`OP_STATUS=16`）、`src/main.rs`（CLI 接线） |
| 验收 | `cargo test --lib manager::` → 24 passed；`.build-bin/test_manager.sh` → `SUMMARY: pass=41 fail=0`（§0–§10，含真实挂载与 40 MiB 读写）；`cargo test --lib` → 267 passed（本文写作时 264，移除单机模式后复跑为 267） |

## 1. 结论与角色边界

manager 是**挂载引导（bootstrap）服务**：它回答「这个 deployment 的元数据与数据服务在哪」，除此之外什么都不做。它是挂载**唯一**的坐标来源。它**不在 IO 路径上**——客户端解析出坐标后就与 meta-/data-server 直接建连（不再经过 manager），挂载生命周期内不再联系 manager。

两条边界是设计的前提，不是实现细节：

1. **roster 的内容是「operator 声明过的那一套」，不是「此刻活着的那一组」**。GroveFS 把分条对象钉死在其 member-id 集合上（`ShardedBackend::check_layout`），也在 roster 解析出的 meta 列表的**顺序**上做故障转移优先级，所以 manager 没有「挑一个健康子集返回」这种能力可给——返回子集等于返回一个读不出自己数据的客户端。心跳只决定 `manager status` 上的一列，以及 `--require-registered` 是否整刀拒绝这次 lookup（门，不是滤镜）。
2. **manager 唯一的权力是不给 roster**，它不能停掉任何已有挂载，也不该被用来停：节点连不上 manager 时照常起服务（见 §6 R7）。

## 2. 可行性判断（四个问题逐一回答）

**Q1 这个方案可行吗？** 可行，且代价局限在挂载引导期。可行性来自一个既有事实：客户端本来就需要两个坐标（有序故障转移全列表 + 分条成员全集），而这两个坐标都是 **provisioning 事实**——由 operator 决定，不由运行时涌现。既然它们本来就是「一次定、到处抄」的常量，把它们收到服务端一处声明、用签名分发，就是纯减少客户端负担的变换，不引入新的运行时依赖。

**Q2 与原来的 `--meta-server` / `--data-servers` 逗号列表 + 故障转移是替代还是叠加？** 本 ADR 落地的**当时是叠加**（两种入口并存、`--manager` 与显式坐标互斥）；**随后已被取代为替代**：手敲的 `--meta-server`/`--data-servers` 从 `mount` 上整体移除，`--manager`（env `GROVEFS_MANAGER`）+ `--roster-key-file` 成为挂载唯一的坐标来源，一次挂载再没有第二个真值入口。不变的是**语义**：两个列表仍是唯一的运行时真值——roster 解析结果喂进同一对值（`Roster::meta_list()` / `Roster::data_list()`），后端构造路径与当初手抄坐标是同一条（不改代码分支，也不新增后端类型）；故障转移与分条语义完全由既有的 `RemoteMeta`/`ShardedBackend` 承担，manager 不解释也不改写它们。被改变的只是**入口**：把「抄坐标」这条口子关掉，让引导成为唯一路径。

**Q3 manager 挂了之后已有挂载是否受影响？** 不受影响，这是 §1 的直接推论，且被实测钉住（`test_manager.sh` §8）：杀掉 manager 后，一个挂载写的文件经**另一个挂载进程**读回（真实数据面往返，不是同一进程的 page cache 自洽），40 MiB 分条对象仍按原 md5 读回。受影响的是**新挂载**：`mount --manager` 在解析失败时直接退出，不猜地址、不退回本地——fail-closed。manager 重启即恢复：声明存在文件里（`<state-dir>/manager/deployments.json`），进程起来重读即可，无需重新 define。

**Q4 对网络拓扑和延迟有什么额外代价？**
- 拓扑：多一个需要被「所有节点 + 所有客户端」可达的监听端口（默认 `127.0.0.1:50055`，与 meta/data 面分端口；跨机部署要单独放行）。它承载的量级是每节点每 ~30s 一问一答 + 每次挂载一问一答。
- 延迟：**每次挂载 +1 次 TCP 往返**（一问一答，连接用完即弃，不做池化）+ 一次本地 HMAC-SHA256 验签（微秒级）。IO 路径增量 = 0：挂载建立后没有任何一次调用会经过 manager。
- 实测旁证（同一台 loopback，manager 解析出的坐标）：`41943040 bytes (42 MB, 40 MiB) copied, 0.498079 s, 84.2 MB/s`——写入落在两块声明的数据盘上（`d1` 2 个对象 + `d2` 1 个对象），读回 md5 一致。

**最小实现路径（本轮已走完的那条）**：① roster/announcement 两种签名帧 + `check_dialable` 一条地址规则（define 与 advertise 共用，杜绝「签名字节里写着一个拨不通的地址」）；② `Manager` 状态（声明文件 + 心跳表 + `record`/`build_roster` 两个纯函数）与 `serve` 循环；③ 节点侧 `spawn_registrar` 守护线程（TTL/3 心跳、1s→30s 退避、按状态变化而非按次打日志）；④ 客户端 `mount --manager` 解析 + 验签 + 互斥；⑤ e2e 套件。协议没有改动数据面任何一帧：三个新 op 只在 manager 自己的端口上被认，其它端口遇到它们回 `ENOSYS` 并说明「这个端口不服务该 op」。

## 3. 契约

### 3.1 三个 op

| op | 发起方 | 鉴权 | 语义 |
| --- | --- | --- | --- |
| `OP_REGISTER`(14) | meta-/data-server | join key HMAC | 「role R 在 addr A，属于 deployment D，ttl=T」。重发即心跳 |
| `OP_LOOKUP`(15) | 客户端 | **不鉴权**（公开读） | 返回 D 的签名 roster。答案是地址集合而不是秘密，可信度来自签名，不来自传输 |
| `OP_STATUS`(16) | operator | join key HMAC | fleet view（JSON）：每个声明地址 + 谁最后注册、还剩多久寿命、报了什么 member-id |

### 3.2 帧

统一走 `sign_block`：`[u8 algo][u16 mac_len][mac][payload]`，`algo=1` 为 HMAC-SHA256，`algo=2`（非对称）**预留未实现**——`sign_block` 不会发出它，`split_signed` 不会接受它。

- announcement payload：`[role u8][deployment][addr][member_id u64][ttl u64]`
- roster payload：`[version u8][deployment][epoch u64][issued_at u64][expires_at u64][n_meta u32 + addrs][n_data u32 + addrs]`
- `ROSTER_VERSION=1`：版本不符回 `Refusal::Version`，老客户端**拒读**而不是误析成一个错误的地址集合。
- 验签失败的原因**故意粗粒度**（`Malformed`/`Version`/`Signature`/`Expired`），且调用方拿到的是同一句话：区分「伪造」与「过期」等于给试探密钥的人一个 oracle。

### 3.3 时间常数

| 常数 | 值 | 理由 |
| --- | --- | --- |
| `DEFAULT_ROSTER_TTL` | 300s | 长到没有挂载会跟自己的引导抢时间；短到一次被截获的 roster 在下一次拓扑变化时就失去路由效力。客户端只查一次，所以它约束的是**重放窗口**，不是刷新周期 |
| `DEFAULT_REGISTER_TTL` | 90s | 一条注册在多久沉默后算失效 |
| 心跳节拍 | `ttl/3` clamp `[2,30]`s | 除三买到「丢两次心跳才算掉线」的容错 |
| `REGISTER_TTL_MIN/MAX` | 5s / 3600s | 节点自报的 ttl 一律夹到这里：一个写错的 `--register-ttl` 不该把陈旧条目停一年，也不该把注册表做成看起来在抖 |
| `expires_at == 0` | **不是**「永不过期」 | 与 DAC 的语义故意相反：roster 是可变注册表的一次快照，无界快照会活过 operator 之后的每一次成员变更 |

### 3.4 状态与并发

`<state-dir>/manager/deployments.json` 是唯一真值，`manager define` 原子写它、`serve` 每次请求前 `stat` 一次 mtime，变了才重读——这就是 `define` 可以是一条独立的短命令、而不必做成第五个带鉴权的在线 op 的原因，也是声明集能像别的运维配置一样进 git review 的原因。**读文件失败时保留已加载的声明**：一个写坏或手改过的文件不能把注册表清空、从而拒掉全集群的挂载。监听线程模型是一连接一线程（引导服务，每集群小时级只有几次查询），注册表本身 `parking_lot::Mutex` 保护。

## 4. 注册防伪设计

选用的机制是**预共享密钥分层 + 服务端二次授权**，不是单一手段：

1. **谁能注册** = 持有 **join key** 者。只有 meta-/data-server 持有，客户端永不持有。MAC 在任何字段被读取**之前**校验，所以未认证的对端连「有哪些 deployment」都试探不出来。
2. **注册能改到什么** = 什么都改不了。一条注册只有在「该 role + 该 deployment 下已被 operator 声明」的地址上才被接受（`Denial::NotDeclared`）。这是关键的一层：**join key 泄露 ≠ 能把客户端引到攻击者的机器**，泄露者只能对一个本来就在名单里的地址谎报存活——而谎报存活在数据面上买不到任何东西（客户端连不上就是连不上）。单靠 1 是做不到这个结论的，这也是为什么「二次授权」不是可选加固而是必需项。
3. **客户端凭什么信这份地址表** = **roster key** 签名。manager 唯一持有签名能力，客户端只持验签；验签不过 → 拒绝挂载（实测：伪造 key 的挂载退出非零、且不留挂载点），而不是「挂上再警告」。
4. **轮换与重放界** = `epoch`（每次声明变更 +1，只对记得上一个值的老客户端有意义）+ `expires_at`（对全新挂载唯一有意义的重放界）。两者都在签名覆盖范围内，无法在链上被改掉。换 key = 换 `serve` 的 key 文件 + 换每节点/每客户端的 key 文件，无需换格式。

已明确承认的强度上限见 §6 R1。

## 5. 否决的方案

| 方案 | 为什么否决 |
| --- | --- |
| **mTLS 双向认证** | 全仓零 TLS 栈（依赖表里没有任何 TLS crate），引入它意味着为「一次挂载一问」的引导服务换掉整条信任链的构建方式，且还要另解「客户端如何验 manager 的证书」——那把公钥仍需带外分发，与分发一把 HMAC key 是同一件运维动作。代价远大于收益。 |
| **签名 challenge-response**（manager 发 nonce，节点用私钥签回） | 它解决的是「证明你持有私钥」，而注册威胁模型里真正的问题是「这条地址是否属于这个 deployment」——那只能由 operator 的声明回答，密码学答不了（见 §4.2）。nonce 只是多一次往返。 |
| **让 roster 返回「健康的子集」** | 直接破坏语义：分条成员集少一个，客户端会撞上成员数不符的显式拒绝（本轮之前更糟——静默读到空，见 R2 与 §7.1）；meta failover 列表少一个备机，是无声削弱 HA 且永不报错。存活度在这里是诊断量，不是路由输入。 |
| **注册失败即拒绝节点起服务**（把注册当准入闸门） | 那会把一个目录服务的故障放大成存储集群的可用性事故：节点上的数据本来完好、客户端本来能直连。现在的选择是节点照常服务 + 日志说明，manager 的权力只有「不给 roster」。 |
| **把 data-server 的 member-id 签进 roster** | 放置必须由**服务器自己的说法**驱动（`OP_OPEN` 回包里的 id），而不是由注册表驱动：注册表里的 id 是它某一刻的抄录，一旦节点换了 id 而 roster 未变，客户端就会按错的身份证放置。roster 只带地址，id 走 status 给运维看。 |
| **本轮就上非对称 roster 签名** | 需要引入签名库并解决公钥带外分发；HMAC 档已能挡住注册通道的注入攻击（威胁的主体），对称性的残留风险用 R1 明说、用 `algo` 标签留好了升级位置——将来只换 key 和一个字节，不换格式。 |
| **把 manager 做成第四个数据面服务（代理读）** | 那就把它放进 IO 路径，Q3 的答案会反过来（manager 挂 = 集群不可读），且要为引导流量再做一遍限流、鉴权、故障转移。明确不做。 |

## 6. 残余风险

**R1 — HMAC 是对称的：能验签的客户端也能签 roster。** 这是真实限制，不是疏忽。缓解在于伪造地址的收益已被别处削掉：假 **data** 地址在开了 Data-Access Capability 的 data-server 上换来的是挂载失败而不是读权限（DAC 校验能力串由真 meta 签发）；假 **meta** 地址仍会收割客户端在 `M_OPEN` 出示的那份凭据。闭合它的路径就是 `algo=2`（客户端只持公钥）。运维上必须按「roster key 是 secret」对待：manager 侧 0600，客户端只读分发。

**R2 — 成员数正确、主机错误：仍是静默空读。** 复现：两块盘都空、member-id 照抄 0/1，`manager define --data wrongA,wrongB` 后挂载，`ls` 报 0、`md5sum` 报 `d41d8…`(空串哈希) 且 **rc=0**（`.build-bin/mgr_wronghosts_probe.sh`）。**成员数不符的那一半本轮已修成显式失败**（见 §7.1），而「数量对、主机错」在客户端侧不可判定：`<fp>` 指纹钉的是 member-id 集合，而 member-id 语义本身允许「同一 id 换一块盘」（换盘后数据仍可寻址），所以错的名单和一块被清空的盘在协议上长得一模一样。不要用 `node.size` 当判据——分条写不更新 meta 里的 size（复现里 `ls` 显示的 0 就来自这条），所以元数据里没有可依赖的期望值。可闭合的两条路都需要新的 durable 事实，属独立决策：**(a)** 分条 `create` 落一个 chunk-0 占位，使「声明过的对象缺 chunk 0」成为可判定的异常；**(b)** 元数据侧记每对象的 chunk 布局账（多一列/一张表），读时核对。**当前运维兜底**：`manager status` 逐地址显示谁注册了、member-id 是什么；`--require-registered` 可把「声明的地址没有一个注册过」直接拒成挂载失败。

**R3 — join key 泄露。** 能对已声明地址谎报存活，不能注入新地址（§4.2）。不能读到任何数据面内容。

**R4 — 两把 key 同宿主。** 默认落在 `<state-dir>/manager/{join,roster}.key`（0600，`create_new` 原子铸造），manager 主机失守 = 两把全丢。要分开就用 `--join-key-file/--roster-key-file` 指到不同材料源；本轮没有引入 KMS 集成。

**R5 — manager 是单点（就新挂载而言）。** 多 manager 热备本轮未实现也未测试：`epoch` 是每实例自己文件的计数，两实例各持一份相同文件时 epoch 不联动。要横向冗余，目前的做法是把 `deployments.json` 与两把 key 放在同一份可回放的位置、由同一个 supervisor 起多份（**此路径未实测，不做收益声称**）。

**R6 — 时钟偏移。** `expires_at` 是绝对秒：客户端与管理端时钟差超过 roster TTL 会拒挂（`Refusal::Expired`）。部署 manager 与客户端同 NTP 是要求，不是建议。

**R7 — 节点侧「连不上 manager」不是错误。** 退避重试到 30s 封顶、按状态变化而非按次打日志。这意味着一个持久错配的 `--register-with` 可以长期只留下日志而不留任何硬失败——`manager status` 是发现它的唯一入口（`--require-registered` 则把它变成挂载侧的硬失败）。

## 7. 未实现（负空间）

非对称 roster 签名；per-deployment 独立 key 对；客户端缓存上一份 roster 做 epoch 单调性检查；注册/变更的事件推送（客户端不会被告知 roster 变了，重挂才拿新的）；从心跳自动生成拓扑；manager 侧的审计台账（谁查了哪个 deployment）；多 manager 共识。**单机 `--home` 模式已在下一份 ADR 里整体移除**（`docs/adr/0002-no-local-mount.md`）：从此挂载只有远程形态，本文的 roster 解析出来的就是那一对服务地址。

### 7.1 顺带闭合的一个既有数据面缺陷（成员数不符）

单地址数据路径（roster 解析出的 data 列表只有一项）以前会这样回答一个分条 ref：那台机器上没有这个对象 → `NotFound` → 而 FUSE 读路径把「对象不存在」当作「建了从没写过的空文件」（`fs_impl::read`），于是 40 MiB 的对象在 `ls` 里是 0 字节、`md5sum` 是空串哈希且 **退出码 0**，`cp` 高高兴兴拷走一个空文件。`du` 早就有整树口径的拒绝（"this tree was written across 2 data-servers but --data-servers names 1"，`du` 仍保留它自己的 `--data-servers`），挂载路径没有等价物。

`backend::UnstripedGuard` 就是这条等价物：**只约束「自己声明了服务器数」的 key**（`s<N>:`，N>1），裸 uuid 照原样通过（单服务器挂载读非分条树是完全正常的路径）。命中时返回显式错误，并把同一句话打进挂载自己的日志一次（内核只传 errno，`Input/output error` 本身不解释任何事）。修复前后同一套件的实测对比：修复前 `md5sum` rc=0/空哈希；修复后 `md5sum` rc≠0 + `Input/output error`，且 `manager define` 改回全集后第二个挂载读到同一份字节（`test_manager.sh` §7 断言这三件事，另有 `a_single_address_mount_refuses_a_striped_ref_instead_of_answering_empty` 一条单测钉住「分条 ref 拒、裸 uuid 通、`remove` 也不许半删」）。

## 8. 本轮用到的自由度、以及没有用到的

用上了：新 op 直接追加进 `proto` 的 op 空间（不为旧二进制保留兼容档，数据面 dispatch 的 catch-all 把未知 op 回成 `ENOSYS`）；`check_dialable` 一处规则两处用（define 与 advertise），不为旧的「允许通配地址进 roster」留 shim；`UnstripedGuard` 直接改成显式拒绝，不做「警告模式 + 严格模式」两档；`manager` 服务端状态用 JSON 文件，不背历史格式。**没有**用这份自由度去改数据面帧格式（三个新 op 只在 manager 自己的端口上被认）。至于单机模式：它随后被这份自由度删掉了，决策、否决项与代价单独成文，见 `docs/adr/0002-no-local-mount.md`。
