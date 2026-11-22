# ADR-0002 · 挂载只有远程形态：移除单机模式

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施；**后续修订**：手敲坐标的 `--meta-server`/`--data-servers` 挂载入口也已移除（ADR-0001 Q2），下表中「两个地址出路」与「半个拓扑」两行是当时契约、现由 `--manager` 单一入口取代 |
| 日期 | 2026-11-20 |
| 前序 | ADR-0001 §7 把「删单机 `--home` 模式」列为本轮未做的负空间；本份兑现它。自由度来自 2026-11-19 的约束放宽（不再支持单机单盘、无需向后兼容、可自由改模块边界与协议），删除方式取自同一轮的裁决：「彻底移除」 |
| 实现 | `src/main.rs`（`NO_COORDINATES` 常量 + 两处出口；删 `Cmd::Init` 子命令与其分支；删挂载的本地元数据装配臂；删无条件 `create_dir_all(<home>/meta)`）、`src/fs_impl.rs`（−327/+10：客户端 CREATE/UNLINK 组提交管线随其唯一消费者一起删）、`src/meta.rs`（`MetaProvider` 去掉 5 个方法：`supports_create_batch`/`create_nodes_batch`/`supports_unlink_batch`/`unlink_nodes_batch`/`set_read_pool`）、`src/meta/engine.rs`（删 `set_read_pool_size`/`set_read_pool`——挂载进程里已经没有引擎，那条 setter 是死管线）、`.build-bin/suite_helpers.sh`（`gfs_cluster` / `gfs_cluster_view` 预置） |
| 验收 | 整板 **GREEN**：`.build-bin/logs/20261120-102856`，43 段、其中 40 段打出 `SUMMARY`、合计 **1,555 条断言**、无 RED；`cargo test --lib` **267 passed**；`bash skills/grovefs-docs/scripts/doccheck.sh` 退出码 0（含套件计数对账 drift 0）；`.build-bin/test_config.sh` §7 把本文 §3 的每一条可观测契约钉成断言（零坐标硬拒、单坐标同拒、横幅逐字、`--home` 零足迹）；13 个 POSIX/语义套件改用 `gfs_cluster` 预置后断言主体一字未改地跑通 |

## 1. 结论与动机

**一次挂载必须同时指向一个元数据服务与一个数据服务，否则就在拨号之前失败。** `--home` 退回它本来的角色：节点侧动词（`view create`、`gc`、`import`、`apikey`、`du`）与服务端自己的存储根，而不是任何客户端的存储根。

动机只有一条，但它是正确性而不是审美：**一份权威存储根必须唯一**。单机模式让每个客户端的 `<home>/meta/<view>/<view>.db` 都成为那棵树的权威副本，于是

- 同一个视图名在两台机器上就是两棵互不知情的树——两边都写、都成功、都没有任何机制发现分叉，因为本地臂里根本没有跨主机收敛这个东西；
- 笔记本可能持有某视图的唯一一份元数据（`meta/` 在客户端本地，而 `blocks/` 早就在服务端了，两份东西各在两台机器上，谁都不是完整备份）；
- 「视图」这个词因此有两种含义，而代码里所有关于视图的收敛、租约、治理、审计都只对服务端那棵成立。

删掉之后，这些命题不再是运维纪律，而是**类型层面的不可能**：`MetaProvider` 只剩远程实现，挂载分支里连「没有地址」这个状态都无法表达（坐标被解构成两个非 `Option` 的名字）。

## 2. 否决的方案

| 方案 | 为什么否决 |
| --- | --- |
| **降级为「仅测试用途」**（保留本地臂，只在 CLI 里藏起来或加警告） | 用户给的是「可以移除」，不是「必须保留但别说」。留着就仍然是一条会被走到的分支：测试套件依赖它，就等于让回归覆盖的那条路径不是交付出去的那条路径。而且 §1 的三条后果不会因为加了警告而消失——它们只在有人真用本地挂载时发生，而「只有测试用」的约束在文档里靠人守。 |
| **feature flag / `--allow-local` 逃生舱** | 明确被约束 ④ 否掉（不为旧路径保留 shim）。更实际的理由：一条只在 flag 打开时才编译的分支，要么进整板（那还是双路径回归），要么不进（那它就是没测的代码）。 |
| **把 `init` 留成 `view create` 的别名** | 它不是一个别名，而是另一种存储位置：`init` 当年写的是「本机的 `<home>/meta/<view>/<view>.db`」。`grovefs view --meta-dir D create --view V` 已经是它的远程等价物，且 `--meta-dir` 指的就是 meta-server 那个根。保留别名只会让人以为挂之前还有一步「先在本机建个库」。 |
| **只删 CLI 入口，保留库内 API** | 库内那条路径的唯一调用者就是 CLI 的本地臂；留下它等于留下一份没人测的元数据装配逻辑，而它正是 §1 里最危险的那块。 |
| **顺手把 `--home` 从全局 flag 里删掉** | 节点侧动词与服务端确实需要一个存储根默认值。挂载接受 `--home` 但**零足迹**（连目录都不建）才是正确的形状，这条被断言钉住，而不是靠「那个 flag 不存在」。 |

## 3. 契约（可观测的东西，逐条有断言）

| 可观测 | 内容 | 钉它的地方 |
| --- | --- | --- |
| 无坐标挂载 | 退出码 **1**，不 attach 任何东西，且在 daemonize **之前**打印——operator 在自己的终端上看到它，而不是在没人 tail 的日志里 | `test_config.sh` §7 |
| 拒绝文案 | `NO_COORDINATES` 整段（`src/main.rs`），USAGE 中英各两处 + `docs/deploy.html` 逐字引用。落地当时文案给出两条出路（两个地址 / `--manager` + `--roster-key-file`）；**后续修订后只剩一条**：`--manager` + `--roster-key-file`，手敲坐标的出路已删除 | 同上（`grep -qF`） |
| 半个拓扑 | 落地当时只给一个手敲坐标（`--meta-server` 或 `--data-servers`）也拒：**半个拓扑不是拓扑**；**后续修订**后手敲坐标整体不存在，拒的是「没给 `--manager`/`GROVEFS_MANAGER` 或 `--roster-key-file`」 | 同上 |
| `grovefs init` | 已不是子命令：clap 报 `unrecognized subcommand 'init'`，退出码 **2** | 手工核验；`grovefs --help` 的子命令清单里没有它 |
| 建树入口 | `grovefs view --meta-dir D create --view V [--manifest M]`，其中 `D` 是 meta-server 的根 | 全套件共用 |
| 挂载横幅 | `mounting view 'x' at /mnt (fully remote: no local store)` | `test_config.sh` §7 |
| 零足迹 | 给了 `--home` 的挂载不创建 `<home>`、不创建 `<home>/meta`，被删掉的正是那句无条件 `create_dir_all` | 同上 |
| 服务端通告 | `meta-server` 启动即声明「视图必须显式创建，`M_OPEN` 拒无树无名」，绝不自动建空树。引用面核实过：横幅**逐字**贴在 USAGE 中英各五处；README 中英、`docs.html`、`deploy.html` 贴的是同源的 `M_OPEN` 拒绝文案，两者共享 `views are created explicitly` 子句——不是一句话被六份文档逐字引用 | `metasrv` §1 钉横幅逐字；`viewrm` 钉 `M_OPEN` 拒绝（`does not exist on this meta-server`） |

## 4. 迁移是怎么做的（平价证明）

13 个套件（`posix` `edge` `e2e` `remote` `errors` `resilience` `concurrent` `striping` `locks` `bwlimit` `migrate` `govcheck` `viewrm`）改为 source 同一个预置：`gfs_cluster SCRATCH PORT [视图…]` 起 `meta-server` + `data-server`、等端口打开、导出 `GROVEFS_META_SERVER` / `GROVEFS_DATA_SERVERS`，再按需 `view create`。自持并会杀重启 data-server 的套件用 `GFS_NO_DATA=1` 只要元数据面，免得板上多出一个没人认领的服务。

关键在于**断言主体一行没改**：这些套件里那句「rename 之后旧名字必须 ENOENT」「硬链接两边同 inode」在两个服务后面照样成立，才叫平价。测量与拒绝形状的变化（例如挂不上就是挂不上、不会退到本地）由 §3 那批新断言覆盖。

## 5. 代价

- **起步三个进程**：一台之前「一个 `--home` 就能挂」的机器，现在要能连上两个服务；本机自用也一样（`meta-server` + `data-server` + 挂载，即入门文档里那套「一台机器三进程」的拓扑）。
- **每次元数据 op 两个 RPC**（`M_CREATE` 的回包内联刚提交那一行，省掉补一次 `M_GETATTR`）。本机回环实测：**`create` 0.281ms/op**（3,560 ops/s，`meta.create` p50 127µs、`remeta.rpc` p50 86µs）、`mkdir` 0.211ms、`utime` 0.391ms（四个 RPC）——数字与拆解见 USAGE §8，脚本 `.build-bin/probe_sqlite_create_breakdown.sh`。
- **客户端不再有任何「权威树」**，因此也不再能离线读。这是本次删除换来的一致性，代价要按一致性来算。
- **一条诚实边界**：本地臂已从 HEAD 移除，「比本机 SQLite 快/慢多少」这种对照**从今天起无法在当前代码上复现**。本轮文档里所有性能数字都是集群档实测，没有一处拿已经不存在的路径做分母。

## 6. 未兑现 / 遗留

- **引擎批接口现在没有生产调用者**：`MetaEngine::create_nodes_batch` / `unlink_nodes_batch` 随客户端组提交一起失去了唯一消费者，现只被单测（`create_nodes_batch_commits_every_item` 等）与文档引用。两条出路都是独立决策：**(a)** 给服务端加一条批量入口（一个 RPC 折 N 个 create），那才是远程形态下真正的组提交位置；**(b)** 连同其单测一起删。本轮选「保留并说明」，因为 §5 的成本曲线说明批量是集群档下一步最值钱的一项，先删再写会丢掉那三条不变量测试（整批一次提交、批内重名只废一条、共享 `data_ref` 只记一次账）。
- **`--home` 仍是全局 flag**，挂载接受它并忽略它（零足迹）。没做成「挂载上传了 `--home` 就报错」：那会把一个无害的默认值变成新的失败模式，而节点侧动词与服务端共用同一份 `--home` 解析。
- **旧格式数据库文件不做迁移**（约束 ② 明确允许）：本轮没写过一行读旧布局的兼容代码。
- **`M_OPEN` ack 的尾部随本轮长过**（`src/remote/meta/server_ext.rs` 里那段注释：flag 字节 `[0]` 派生/只读、`[1]` 服务端分页、`[2]` fork，再加 `[3]`–`[5]` 的名字与类别），**没有版本协商**——客户端无条件读完整个尾部，所以源码里写明「a body without them is a build mismatch, not an optional tail」。按约束 ② 可接受，代价要写明：**新老二进制不能混部，一个集群要一起升级**（先停客户端、再换两端、再重挂）。
