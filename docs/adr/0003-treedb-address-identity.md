# ADR-0003 · treedb 实例的身份就是网络地址：注册表里没有凭据字段

| 项 | 内容 |
| --- | --- |
| 状态 | 已实施（引擎接入侧自 2026-11-20 起即为此形状；本份把它从代码注释升格为决策，并钉住网页控制台的呈现契约） |
| 日期 | 2026-11-26 |
| 前序 | ADR-0002 的「一份权威存储根必须唯一」同样适用于此处：实例身份必须只有一个名字来源。treedb 接入定案（`src/treedb_store.rs` 模块注释「明确接不了的东西」一节）已经写明认证入口是环境变量而非 URI 参数，本份补齐「为什么控制台也不该问」 |
| 实现 | `src/treedb_store.rs`（`parse_addr`/`parse_addrs`、`KEY_FILE_ENV`、`auth_key`、四处 `Client::connect_auth`、`treedb_registry`/`write_treedb_registry`、`probe_treedb_addr`）、`src/remote/meta/server_ext.rs`（`storage.add`/`storage.update` 的 treedb 臂、`store.info` 的 `serving` 标记、`view.create` 的实例钉位与 `view.plan`）、`console/src/components/storage/{AddInstancePanel,InstanceCard}.vue`、`console/src/components/views/NewBasePanel.vue` |
| 验收 | 见 §4；本轮以 `cargo test --features treedb,web --lib` 305 passed／0 failed 与控制台四项质量门为凭 |

## 1. 决策

**一个 treedb 实例在 GroveFS 里的全部身份就是它的 `host:port`。实例注册表 (`<meta-dir>/treedb_instances.conf`) 的每一行是 `name = host:port`，没有第三列；网页控制台的注册表单只有一个地址字段，不问用户名也不问口令。**

引擎侧的认证确实存在，但它不是 per-instance 属性：`GROVEFS_TREEDB_KEY_FILE` 给的是**密钥文件路径**（与 `GROVEFS_JOIN_KEY_FILE`、`GROVEFS_ROSTER_KEY_FILE` 同规——密钥字节永不进 argv），设了之后 meta-server 进程对它拨出的**每一条** treedb 连接都跑 v5 双向握手（`Client::connect_auth`），没设就不握手，设了但读不动则**每次 open 都失败**，绝不降级成不认证。所以：

- 一个 meta-server 进程持有一把引擎密钥，服务它那组 `--meta-store` 地址；
- 「这台实例的密码是什么」是一个没有答案的问题——不是遗漏，而是这一层的认证根本不是按实例发凭据的。

因此控制台若摆出一个空的密码框，它会同时撒两个谎：暗示存在 per-instance 凭据，又暗示操作员漏填了必填项。本轮的修复正是把这个歧义从界面上消掉（§3）。

## 2. 为什么不给 URI 加 user/password

| 方案 | 为什么否决 |
| --- | --- |
| **把凭据塞进 `treedb://` URI**（`treedb://user:pw@host:port`） | `serve()` 解析完就把 `StoreKind::Treedb` 的 payload 定成裸 `host:port`，`server_ext.rs` 里有十处操作者可见的文案在插 `{addr}`；带 query/userinfo 就得改那个形状。更要紧的是 `parse_addr` 故意拒绝 `/`、`?`、`#`——一个 multi-instance 集合是逗号分隔的裸 `host:port` 清单，不是 URL。让凭住进 URI 等于让同一个字段在两种后端里有两种含义（PG 的 DSN 里 `user:pw@` 是真的，treedb 里没有）。 |
| **注册表加第三列 `name = host:port:password`** | 分隔符本身就要重新设计（地址里的 IPv6 已带冒号），而且它会把「引擎协议是否有 per-instance 认证」这个尚未存在的能力，写成一份数据格式的永久承诺。 |
| **在实例卡上留一个「凭据：未填」的空位** | 这是本轮报上来的实际症状：操作员以为漏填。诚实的形状是标注「无需凭据」并解释信任来自哪里，而不是显示一个永远为空的字段。 |
| **让控制台代管密钥文件路径**（把 env 搬进注册表） | env 是进程启动时就固定的信任根；把它变成网页上可改的字段，等于给「谁能改写认证根」再加一条与 `storage.add` 同级的权限，而 `storage.add` 现在只是 Super。真要按实例区分身份，前置条件是引擎协议先有 per-instance 凭据（§5）。 |

## 3. 控制台的呈现契约（操作员能看到的）

三处，全部是「如实说明」而不是「留空等填」：

| 位置 | 内容 |
| --- | --- |
| 存储页 · 实例卡（treedb 档） | URI 行旁一枚 `no credentials` 徽标（与「服务中」「实验性」同一色带但不同调），卡下一句小字：地址按现状信任；本部署若启用引擎认证，那是**一份服务器全局密钥文件** `GROVEFS_TREEDB_KEY_FILE`，不是按实例存储的凭据 |
| 存储页 · 注册实例表单 | 地址字段 placeholder 为 `host:port`，hint 说明只接 `host:port`、地址即身份、引擎认证走服务器全局 env；服务端拨不通时回显 `probe_treedb_addr` 的原文，不改写 |
| 视图页／存储页 · 新建基视图 | treedb 档的实例下拉只列 `store.info` 标为 `serving` 的行；hint 说明树会建在这台服务器上、后续写入始终跟随它，且**不询问用户名或密码**。落位预测由 `view.plan` 给出（同一份哈希规则、同一个函数），不是前端复刻 |

PG 档不受影响：那里的 `connection string` 里 `user:pw@` 是真凭据，两档共用一个面板但字段名、placeholder 与 hint 都按 `catalog.engine` 分流。

## 4. 为什么「地址即身份」是安全的（以及它的边界）

它只在既有的信任边界内成立，没有新增任何面：

- 能注册实例、能改注册表的，只有持有 admin key 的 Super 调用者（`storage.add`/`storage.update`/`storage.remove` 全是 `Gate::Super`）。而这把 key 在本部署模型里已经是 root——网关持有它就是 root 凭据。地址信任没有把「能碰存储」的门槛降到「能碰网络」之下，因为**能操作存储面的人本来就已经是 root**。
- `view.create` 拒绝把视图钉到一个不在 `--meta-store` 集合内的已注册地址（`… which this server's --meta-store does not route to — it cannot hold a view later reads would find`）。这条守卫使注册表不能被人当成交换机：写了清单不等于就路由得到。
- 边界要说清楚：treedb 档下「谁能访问那个 `host:port`」≈「谁能读写那些树」。所以引擎端口应当只暴露给运行 meta-server 的主机/受控网段；若这不够，正确做法是把 `GROVEFS_TREEDB_KEY_FILE` 设上（引擎侧仍按连接握手，读不到密钥就 fail-closed），而不是给实例编一个假的密码字段。

## 5. 未来若真要 per-instance 凭据

前置条件是引擎协议先给出按连接/按实例区分的身份，届时需要一起改的东西至少包括：`StoreKind::Treedb` 的 payload 形状（裸 `host:port` → 结构化）、`treedb_instances.conf` 的行格式与向后兼容、`{addr}` 文案的十处插入点、`storage.add` 的字段与密钥落地位置（明文字段不可接受，需与 `GROVEFS_JOIN_KEY_FILE` 同规的路径/句柄语义）、以及本 ADR 的状态行。**在此之前，控制台不问凭据是设计，不是待办。**
