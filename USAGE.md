# GroveFS 使用说明

> 用户视图 FUSE 文件系统:物理数据集中存放在硬盘,不同用户挂载各自看到独立的文件列表,
> 列表支持自定义文件夹层级,彻底脱离 NFS。本文件说明**如何使用**与**如何处理数据**。

---

## 功能索引:按「我要做什么」找入口

一张表覆盖全部能力。左列是**你要办的事**,右列是入口(CLI 与 MCP 工具),末列是详细章节——每个章节都按同一套路写:**它解决什么问题 → 完整命令与参数 → 真实回显 → 边界与坑**。

| 我要做的事 | 入口 | 详细 |
| --- | --- | --- |
| 本机 5 分钟看到效果(无服务进程) | `grovefs init` → `mount` → `umount` | §2、§15.1 |
| 把盘上已有的目录纳管进来(**零字节拷贝**) | `grovefs import --view V --source DIR` | §3.3、§14.2 |
| 让同一个视图对 A/B 显示不同形状(只影响本次挂载) | `mount --layout rules` | §3.1 |
| 在服务端定义只读形状(所有客户端一致,热更新) | `view define` · MCP `define_derived_view` | §3.2 |
| 换个对外名字,全树仍可写(名字治理) | `view alias`(`--retire` 无空档轮换) | §3.4 |
| 独立目录树:随便整理,内容只读(结构治理) | `view fork` · MCP `fork_view` | §3.4.1 |
| 只装基视图的一小块,并顺手改名 | `view graft` · MCP `graft_view` | §3.4.1「选择性接枝」 |
| 看有哪些视图 / 某棵树实际长什么样 | `view list` · MCP `list_views` / `get_view` / `list_view_tree` | §3.2、§3.5 |
| 换机器/换系统迁移单个视图 | `view export` + `view import` | §3.4.2 |
| 删一个定义 / 删整棵基视图并回收字节 | `view rm --name F` / `view rm --name F --base` | §3.4.3、§5.3 |
| 排查/回收孤儿对象(离线) | `gc --meta-dir D [--data-dir R] [--apply]` | §12.6 |
| 让智能体自己定制数据列表 | `grovefs mcp`(stdio JSON-RPC) | §3.5 |
| 给接口和挂载点加锁、逐调用方撤销 | `apikey create/list/revoke/set-ro/calls` | §3.5.1、§6 |
| 只读挂载 / 零缓存强一致 | `mount --ro` / `--no-cache` | §7 |
| 性能观测与限速(分阶段延迟 / 带宽 / 缓存 TTL) | `mount --stats*` / `--bw-read|--bw-write` / `--cache-ttl` | §8 |
| 多台机器共享同一棵视图 | `data-server` + `meta-server` | §4、§11 |
| 元数据换 PostgreSQL 后端 | 各处 `--meta-store postgres://…` | §4.1、§6.2、§15.3 |
| 任意机器治理同一份元数据(不发数据库密码) | `meta-server --control-listen` + `view`/`apikey`/`refs --control-server --api-key` | §3.5.2 |
| 主备高可用与真 fencing | `meta-server --role/--peer/--sync-repl/--witness` | §5.5、§11.8 |
| 回收账本不一致 / 审计谁在调用 | `refs rebuild` / `apikey calls` | §5.3、§3.5.1 |
| 长命令瘦身(环境变量 + 配置文件) | `GROVEFS_*` / `~/.config/grovefs/grovefs.conf` | §1.1、§15.15 |

> **两个 `import` 别混**:顶层的 `grovefs import` 是「**把目录纳管进来**」(数据入口,§3.3);`grovefs view import` 是「**把 `view export` 导出的元数据 JSONL 装回去**」(迁移入口,§3.4.2)。
>
> **命令行与 MCP 是同一套实现**:`view define/alias/fork/graft/rm` 与对应 MCP 工具共用参数语义与拒绝文案(逐字节相同),只有入口不同——人在终端用 CLI,智能体用 MCP。

---

## 0. 核心概念(先理解这三点)

| 概念 | 说明 |
|------|------|
| **视图(view)** | 每个用户一棵独立的目录树(含自定义层级),存放在一个 SQLite 库里:`<home>/meta/<view>.db`。不同用户互不可见。 |
| **数据对象(object)** | 文件的真实字节。一个文件 = `blocks/` 下的一个对象文件,以 UUID 命名(或条带化的多个分块);对象按 key 哈希自动分布在 256 个桶子目录里。视图里的目录项指向对象 key。 |
| **基目录(base / `--home`)** | **单机模式**:`blocks/`(数据)+ `meta/<view>.db`(视图)的本地根,供 `init` 与不带 `--meta-server`/`--data-servers` 的挂载使用;默认 `$GROVEFS_HOME` 或 `~/.grovefs`。**服务/客户端模式**:客户端不需要它(远程挂载本地零落盘);存储机上的持库命令各自声明自己的根(`meta-server --meta-dir`、`apikey --meta-dir`、`data-server --data-dir`,优先于 `--home`;`--home` 是全局默认根)。 |

数据与视图**天然分离**:视图只是"目录树 + 指针",字节永远在 `blocks/`。

### 0.1 术语对照(新手速查)

| 文档里的说法 | 意思 |
| --- | --- |
| `--home` | 单机模式下存放 `blocks/`(数据)与 `meta/`(视图库)的总目录;服务端各用各的 `--meta-dir`/`--data-dir`,客户端不用它 |
| data-server / meta-server | 两个中心服务进程:前者管字节(对象存储),后者管目录树(元数据);客户端 `--data-servers`/`--meta-server` 分别指过去 |
| 基视图 / 派生视图 / 别名 / fork | 视图四形态:独立存储树(可写)/ 规则定义在服务端的只读显示透镜 / 换名的全树可写入口 / 只拷目录不拷字节的独立树(小白先看 §3.0 的类比与对照表;操作细节 §3.4–§3.4.1) |
| token / API key | token 是集群准入门(一串共享口令);API key 是逐调用方、可撤销、可绑定视图/只读的凭据(§3.5.1) |
| 治理面 / 挂载面 | 治理面 = `grovefs mcp` 的视图定义管理接口;挂载面 = FUSE 客户端到 meta/data 的连接——两门独立,scope 互不相通(§6) |

> 还没建立"FUSE/挂载/对象存储"心智模型的读者,网页版 docs.html 的「§0 零基础预备知识」(六个关键词、需要/不需要会什么、硬件最低要求)建议先读。

### 常用参数:先说它管什么,再看怎么写

> 下面命令里 `--home`、`--meta-dir` 这类是**参数写法**(照抄用);它们各自管的东西以**实际含义**为准:

| 它管什么(实际含义) | 命令行写法 |
| --- | --- |
| **本地存储根目录** — 单机模式下 grovefs 存放 `blocks/`(数据)与 `meta/`(视图)的总目录 | `--home <目录>` |
| **服务端元数据根** — `meta-server` 那台机器上放权威视图库、视图定义、API key 的总目录 | `--meta-dir <目录>` |
| **数据落盘位置** — `data-server` 实际写字节的存储根(默认同元数据根) | `--data-dir <目录>` |
| **要被纳管的已有目录** — 本就躺在盘上、想变成一个视图的那批数据 | `--source <目录>` |
| **挂载点** — 本机用来访问这棵树的目录 | `-m <目录>`(即 `--mountpoint`) |
| **中心服务地址** — 客户端连过去的元数据 / 数据服务(主机:端口) | `--meta-server <地址>` / `--data-servers <地址>` |
| **视图名** — 给一棵逻辑树起的任意标签(与 Linux 用户名无关) | `--view <名>` |
| **集群准入口令** — 服务端设的共享密钥,客户端挂载时递上(数据面与元数据面各设一份) | 服务端 `--token <串>` + 客户端 `--token <串>` |
| **可撤销的接入凭据** — 服务端改按 per-caller API key 放行(`grovefs apikey create --for mount` 铸造);可列表、可审计到具体调用方、可撤销,且撤销能封停**已建立**的挂载;`--view V` 把 key **绑定单一视图**(开别的视图握手即拒),`--ro` 服务端强制只读(逐写意图 op 拒) | 服务端 `--require-api-key` + 客户端 `mount --api-key <ID:SECRET>`(或 env `GROVEFS_API_KEY`) |
| **账号密钥(治理凭据)** — 用 `--for admin --owner <账号>` 铸出的 key 顶替数据库密码:拿到它的人可以从**任意**能连到治理口的机器管同一份元数据,真实读写由 `meta-server` 代做;一个账号可持多把,逐把铸造、逐把撤销 | 服务端 `--control-listen <地址>` + 客户端 `--control-server <地址> --api-key <ID:SECRET>`(或 env `GROVEFS_CONTROL_KEY`) |
| **以指定身份挂载** — 锁仲裁与审计按此 uid 记属主 | `--run-as-uid <uid>` |
| **只读 / 零缓存强一致** — 挂载为只读 / 关闭客户端缓存 | `--ro` / `--no-cache` |
| **目录列举提速的回退开关** — 关掉 READDIRPLUS（随目录项一并带回属性），退回普通 READDIR + 逐条 `LOOKUP`，仅供 A/B 性能排障（只有 provider 能在自己存储里分页的挂载才协商 READDIRPLUS：本地 `--home` 的 SQLite/PG，以及对接能分页的 `meta-server` 的远端挂载；派生/透镜视图不协商） | `--no-readdirplus` |
| **分阶段延迟遥测** — opt-in 记录读/写/元数据各阶段耗时(离线聚合 p50/p90/p99/max 排障定位瓶颈；退出打印 + 周期落盘 + 隐藏节点 `.grove-stats`；非吞吐声明) | `--stats` / `--stats-file <路径>` / `--stats-interval <秒>` / `GROVEFS_STATS=1`(详见 §8.1) |
| **读缓存 TTL 调优** — 放大内核 attr/dentry 缓存 TTL(默认 2s),对读多/不可变树削减重复 LOOKUP/GETATTR;跨端新鲜度仍靠默认失效推送 + 新内核 `AUTO_INVAL_DATA` 兜底,`--no-cache` 强制归 0 | `--cache-ttl <秒>`(详见 §8.2) |
| **挂载级内容带宽整形** — 给本次挂载的内容读/写各上一道 GCRA 令牌槽(按字节铺开互不重叠的时间槽,长跑速率精确等于上限,超额只睡眠不丢字节;写侧延迟即背压),`0`=不限 | `--bw-read <速率>` / `--bw-write <速率>`(如 `50M`;env `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE`;详见 §8.2) |

### 关于 `--view` 与路径容量

- **`--view` 与 Linux 用户名无关**——只是视图命名空间的**任意标签**。二进制不调 `getpwnam`、不校验 uid、也不要求 `useradd`。`alice`、`team-a`、`u42`、`研究组` 都能当作用户名——只要它是目标文件系统的合法文件名。它的作用只有两个:① 确定 `<home>/meta/<view>.db` 库文件名;② 在 FUSE mount 上打 `grovefs:<view>` 标签供 `mount`/`findmnt` 展示。**鉴权来自「能访问 `--home` / `blocks/`」+ 服务端的 `--token` / `--require-api-key` 门**,不来自 Linux 用户。
- **一个用户一棵树、树内路径完全自主**——挂载后,在该 mountpoint 下可以 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意层级,与在自己 home 里操作无差异。不同用户隔离的粒度是「整棵视图树」,不是单个路径。manifest 只是预建骨架的便捷入口,不是路径数量的上限;挂载后随时可 `mkdir -p a/b/c/d/...`。

---

## 1. 环境准备

```bash
# 依赖:只需 Rust 工具链(edition 2021);不需要 libfuse 开发包,也不需要 pkg-config
# (FUSE 客户端是纯 Rust 的 fuser;非 root 挂载时靠发行版的 setuid 助手 fusermount/fusermount3)
# export PKG_CONFIG="$PWD/.build-bin/pkg-config"   # 历史遗留,现行依赖图无人查 pkg-config,删掉也照样构得成
export LC_ALL=C LANG=C
cargo build --release                                       # 产物:target/release/grovefs
```

可执行文件子命令:`init` / `mount` / `umount` / `data-server` / `meta-server`。

### 1.1 命令简写:环境变量与自动加载的配置文件

重复敲的长命令可以整体瘦身。每个旋钮的取值优先级恒为:
**命令行参数 > 当前环境变量 > 配置文件 > 内置默认值**。

| 旋钮(env) | 顶替的参数 | 说明 |
| --- | --- | --- |
| `GROVEFS_HOME` | `--home` | 本地存储根 |
| `GROVEFS_META_SERVER` | `mount --meta-server` | 元数据服务地址(含故障转移列表) |
| `GROVEFS_DATA_SERVERS` | `mount --data-servers` | 数据服务地址(含条带列表) |
| `GROVEFS_TOKEN` | `mount --token`、`meta-server/data-server --token` | 集群准入口令 |
| `GROVEFS_META_DIR` | `meta-server --meta-dir` | 服务端元数据根(优先级:`--meta-dir` 参数 > 本 env > `--home` > `~/.grovefs`) |
| `GROVEFS_META_STORE` | `--meta-store`(`view`/`apikey`/`mcp`/`meta-server`/`data-server`/`du`/`import`) | 非 SQLite 后端寻址;未给 flag 时由本 env 回退(源码 `resolve_store()`)。`import` 只写本地 SQLite 树,故这里给的是 `postgres://…`/`fdb://…` 时**如实拒绝** |
| `GROVEFS_API_KEY` | `mount --api-key` | 已有的 per-caller 凭据 env |
| `GROVEFS_MCP_API_KEY` | MCP `initialize` 握手 | 服务端整体预认证 |
| `GROVEFS_CONTROL_KEY` | `--api-key`(配合 `--control-server`) | 治理端口的管理员 key(`<id>:<secret>`);未设时回退 `GROVEFS_API_KEY`,两者都让密钥不进 argv |
| `GROVEFS_STATS` | `--stats` | `1/true` 开启遥测 |
| `GROVEFS_SQLITE_READ_POOL` | —（内部调优） | SQLite 每视图暖读连接数，范围 0–16；未设时**本地挂载跟随 `--threads`**（默认 4），库内嵌/`meta-server` 默认 1；`0` 为完全惰性（每次读开/关）。调大换读并发、代价是每视图多 ~1.3 个 fd/连接（§8 的并发建小文件曲线就按此定宽：16 worker 下读池 1 约 1.2k、读池 16 约 5.5–6.6k files/s） |
| `GROVEFS_PG_WRITE_POOL` | —（内部调优） | PG 每视图写连接暖池，默认 8、范围 0–64；池空即临时建连，硬顶是 PG `max_connections` |
| `GROVEFS_PG_READ_POOL` | —（内部调优） | PG 每视图读连接暖池，默认 4、范围 0–64 |
| `GROVEFS_PG_SYNC_COMMIT` | —（内部调优） | PG 写连接会话级 `SET synchronous_commit`；默认 `on`（PG 默认），`off`/`false`/`0`/`async` 时 COMMIT 返回不再等本地 WAL 落盘。同台架（写语句缓存已在位）N=4 写路径 5,225 → 6,142 ops/s（+18%），N=1/N=8 在抖动内，代价是崩溃可丢最近若干已提交事务、配同步备机时也放弃等待确认（RPO=0 必须留 `on`） |

在以上 env 之上,grovefs 启动时还会**自动加载一个 `KEY=VALUE` 配置文件**填入尚未设置的变量——这样连 export 都不用每次敲:

```bash
# 查找顺序: $GROVEFS_CONFIG 指定路径 > $HOME/.config/grovefs/grovefs.conf
# (只认 GROVEFS_* 键,其余行提示后忽略)
cat > ~/.config/grovefs/grovefs.conf <<'EOF'
GROVEFS_META_SERVER=192.168.1.10:50110
GROVEFS_DATA_SERVERS=192.168.1.10:50100
GROVEFS_TOKEN=s3cret            # 文件里有口令就 chmod 600(与 env 里放口令同一契约)
EOF
chmod 600 ~/.config/grovefs/grovefs.conf

# 之前: grovefs --home /data/myfs mount --view alice -m /mnt/alice \
#         --meta-server 192.168.1.10:50110 --data-servers 192.168.1.10:50100 --token s3cret
# 之后: 只剩"这次挂哪个视图到哪儿"
grovefs mount --view alice -m /mnt/alice
```

格式细节:支持 `#` 注释、空行、可选 `export ` 前缀(同一文件可兼作 shell 脚本)、单/双引号包裹的值;非 `GROVEFS_*` 键与缺 `=` 的行会打一行带文件名:行号的警告后被忽略;显式指定的 `$GROVEFS_CONFIG` 读不到时警告后继续(不致命)。后台挂载的 re-exec 子进程继承环境,简写同样生效。`--view`/`-m` 刻意不收录——每次挂载都不同,写进默认值是错的。

多集群注意:这些值是一套**进程级全局坐标**,一条命令只读一份文件,不按挂载点/视图自动匹配。切集群用逐次 `GROVEFS_CONFIG=~/grovefs-B.conf grovefs …`(每集群一份文件、`chmod 600`),不要常驻 export——已导出的同名变量会**静默压过**另一集群的配置文件(文件只填尚未设置的键);挂载回显 `using remote metadata service at …` 可核对坐标。控制面的 `--meta-dir` 没有 env 回退、逐集群显式给;`GROVEFS_META_STORE` 是全局键,残留 export 会把治理命令指向另一套 PG,切集群前 unset。

回归覆盖:`bash .build-bin/run_all.sh config`(28 条断言:优先级链、解析边界、纯由文件驱动的真实 FUSE 挂载)。

---

## 2. 快速上手:单机本地模式(最简单)

数据和视图都在本机同一个 `--home`,无需任何 server。适合先体验功能。

```bash
Y=./target/release/grovefs
BASE=/data/myfs                       # 物理基目录

# 1) 初始化两个用户的视图
$Y --home $BASE init --view alice
$Y --home $BASE init --view bob

# 2) 挂载(默认转后台:挂载点就绪即返回;要前台显示加 --foreground,Ctrl-C 即卸载)
$Y --home $BASE mount --view alice --mountpoint /mnt/alice
$Y --home $BASE mount --view bob   --mountpoint /mnt/bob     # 默认转后台,立即返回

# 3) 像普通目录一样读写
mkdir -p /mnt/alice/projects/2026
echo "年度报告" > /mnt/alice/projects/2026/报告.txt
ls /mnt/bob                                      # bob 看不到 alice 的文件

# 4) 卸载
$Y umount --mountpoint /mnt/alice
fusermount -u /mnt/alice                          # 等价
```

**视图隔离**:`/mnt/alice` 与 `/mnt/bob` 是两棵独立的树,互不可见。层级(mkdir 的嵌套目录)完全自定义。

---

## 3. 视图定制与治理:manifest、透镜、派生、别名、fork

`init --manifest` 可在创建视图时一次性铺好目录骨架。manifest 每行只支持两种指令:

```text
# 这是注释,行首 # 会被忽略
mkdir projects
mkdir projects/2026
touch projects/2026/README.txt
```

```bash
$Y --home $BASE init --view alice --manifest ./seed.txt
# 输出:applied seed manifest ./seed.txt
```

挂载后即可看到预建的层级。已存在的 mkdir/touch 是幂等的(重复执行不报错)。

### 3.0 四种视图一张表看懂(小白版)

> 这一节只回答三件事:**四种视图分别是什么、每条命令会生成哪些文件/表、参数怎么填**。
> §3.1–§3.4.1 是操作细节;第一次接触建议先读完这一节,再照 §3.2/§3.4 的例子做。

> **显式创建(必读)**:服务端 `meta-server` 与本地 `--home` 挂载走**同一条规则**——`meta/<名字>.db`(PG 下是 `grovefs_v_<hex>` schema)不存在时直接拒绝,带可读原因,**从不**在挂载时创建空库;这条规则没有开关。

**先纠正一个最常见的误解:写入数据不等于创建基视图。** 往挂载点里 `mkdir`/`echo` 只是往**已经存在**的树里加节点;本地挂载发现 `meta/<名字>.db` 不存在时**直接拒绝**,服务端同样拒绝,而且什么都不写。建基视图的入口只有 `init --view` / `view create --view`(后者会先检查名字没有被派生/别名/fork 占用),以及 `import --view V --source DIR` / `view import`(目标不存在时建树)。

#### 1. 一句话类比

| 形态 | 类比 | 一句话记住 |
| --- | --- | --- |
| **基视图**(base) | 仓库正本 | 数据归属的正本;能写内容、删名会回收字节 |
| **派生视图**(derived) | 正本上的只读滤镜 | 规则存在控制面(SQLite 本地 `meta/views/`、PG `grovefs_control` 或 FDB 集群 `ctl`),挂出来只读;数据仍是基视图那份 |
| **纯别名**(alias) | 门牌号 | 给正本换一个公开名字,挂出来就是正本,全树可写 |
| **fork** | 复印的目录册 | 目录项复制一份、可随意整理;字节仍共用,内容写不进去 |

#### 2. 四种形态一张表

| | 基视图 | 派生视图 | 纯别名 | fork |
| --- | --- | --- | --- | --- |
| **本质** | 一棵真实物理树 | 基树 + 规则(不建树) | 名字 → 真实视图(不建树) | 一棵独立物理树(目录项从基树复制) |
| **创建命令** | `init --view V` / `view create --view V` | `view define --name D --base B --rules F` | `view alias --name A --real R` | `view fork --name F --base B` |
| **新增文件** | `meta/V.db` | `meta/views/D.layout` | `meta/views/A.layout` | `meta/F.db` + `meta/views/F.layout` |
| **挂载后能改目录** | 能 | 不能(只读) | 能 | 能 |
| **挂载后能写内容** | 能 | 不能(`EROFS`) | 能 | 不能(`EROFS`) |
| **删一个名字时** | 摘基树的名字;账本归零才释放字节 | 派生挂载只读,删不了(走基) | 等同在真实视图上删;账本归零才释放 | 只摘 fork 自己的名字;基树不动;账本归零才释放 |
| **删除整个视图时** | `view rm --base`:删树 + 回收无人持有字节 | `view rm --name`:只删定义 | `view rm --name`:只删定义,真名恢复可挂 | `view rm --name`:删定义 + 自己的树 + 重建账本 |
| **典型用途** | 真正存数据 | 团队共享一份只读形状 | 隐藏存储真名/换公开名 | 在基数据上做可整理的独立分支 |

**选型一句话**:要存/改内容 → 基视图;只想只读地改形状 → 派生;只想换公开名 → 别名;想独立整理目录但不上传 → fork。

> “能不能写内容”是硬契约:派生挂载在客户端就是只读挂载;fork 的结构操作放行,`write`/`truncate` 由客户端回 `EROFS`。服务端对派生名的写意图另有 `EPERM` 门禁,绕过客户端也写不进去。**派生/别名/fork 都不是安全边界**——能挂基名的人仍看到全树;硬隔离用独立视图 + token/API key(§6)。

**写不进去时,在哪一步报错**(运维排错速查;逐 `open` 探针回显与成因见 docs.html §12):

| 形态 | 结构写<br>mkdir/rename/unlink | 内容写·截断类<br>`O_TRUNC`/`truncate` | 内容写·追加/普通写<br>`O_WRONLY`/`O_APPEND` 的 `write()` | shell `>>` 追加观感 |
| --- | --- | --- | --- | --- |
| **基视图**(可写 key) | 放行 | 放行 | 放行 | rc=0,确实落盘 |
| **纯别名** A→R(底层可写) | 放行(同一棵树) | 放行 | 放行 | rc=0,写 A 即写 R |
| **派生透镜** D over B | `EROFS`(整树只读) | `EROFS` | `EROFS` | 写不过去,`>>` 也报错 |
| **fork**(结构可写/内容只读) | 放行 | `EROFS` **在 open 即拒**(`>` rc=1) | `EROFS` **在首次 `write(2)` 即返**(新建空文件后的第一个写同样当场拒) | rc=1 报“只读文件系统”,无假成功 |
| **只读 key** | 重挂:同派生透镜整树拒;经服务端挂载被 `set-ro` 原地切换:服务端门禁拒,失败在 fsync 往返可见(本机 `--home` 无凭据门,set-ro 不影响已建挂载) | 同左两分支 | 同左两分支 | 重挂后无假成功;仅 `set-ro` 切换瞬间已有(经服务端)挂载内 `>>` 仍可能 rc=0(ro 生效/恢复各有 ≤1s 复检窗口) |

> ⚠ 内容只读的挂载不启用页缓存写回,**首次 `write(2)` 即返 `EROFS`**,shell `>>` 的 rc 可靠。唯一例外:**经服务端(meta/data-server)挂载运行中的视图被 `apikey set-ro` 原地切换**——内核挂载标志无法中途改变,切换瞬间的追加写仍可能 rc=0 而字节未落盘(失败在 fsync 往返时可见,≤1s 复检窗口);要确认这类切换已生效,重新挂载后观察 open 即拒即可。本机 `--home` 挂载没有服务端凭据门(准入由文件系统权限把关),`set-ro` 对其已建挂载不生效。

#### 2.1 三个常见追问(别把“滤镜”当成“复制”)

| 追问 | 答案 | 从哪里能看出来 |
| --- | --- | --- |
| 派生视图是不是把基视图**复制**了一份,只是对用户只读？ | **不是复制。** 派生只落一份 `.layout` 文本(`base B` + 规则),不复制任何目录项、不复制字节。每次读目录时,服务端/本地才按规则在基树上**实时算出**显示形状。 | `view define` 不建 `meta/D.db`;目录里只有 `meta/views/D.layout`;`view list` 打的是 `view 'D' -> base 'B': 1 hide, 1 only, 1 reloc rule(s) (read-only)` 而不是“一棵树”;改规则后经服务端 ≤2s 换形。代价是目录也不能改、内容写 `EROFS`——因为它本来就没有自己的目录项可改。 |
| 别名是不是就是基视图,只是换了个名字？ | **是。** 别名没有自己的元数据;解析时把名字换成真实视图 `R`,之后读目录、建目录、写文件、删文件全部落在 `R` 的**同一棵树、同一份字节**上。 | 别名只占 `meta/views/A.layout` 一行 `alias R`,没有 `meta/A.db`;`view alias` 后写 `A/` 里的文件,在 `R/` 里立刻看得到(同一个 inode)。唯一区别是**名字治理**:别名存在期间 `mount --view R` 被拒,一个真实视图同时只能有一个别名(`--retire` 轮换)。 |
| fork 是不是一份完整元数据,能改目录但删不掉底层数据？ | **前半句对;后半句要分清“删名字”与“删字节”。** fork 复制基树的 `node` 行(目录项,含 inode/硬链接关系),所以有自己完整、可改的目录树;内容写 `EROFS`。在 fork 里删名字只摘 fork 自己的目录项,基树不动;字节是否回收看 `refs.db` 账本——没有任何视图再持有该对象时才真正删除。 | `view fork` 会新建 `meta/F.db` 并在 `refs.db` 记账;`rm --name F` 删的是 fork 自己的树 + 定义,基树与仍被持有的字节不动;基树后来新增的文件不会自动出现在 fork。 |

#### 2.2 派生视图有必要吗？能不能用「别名 + 只读 API key」代替

**只在你要「整棵树只读」时可以代替;只要你需要改形状,就不能。** 两者管的是**两个正交维度**:key 管「能不能写」,派生管「看得见什么、在哪个路径」。

| 维度 | 别名 `A -> R` + 只读 key | 派生视图 `D over B` |
| --- | --- | --- |
| 挂载后看到的树 | **整棵树**(实测 `ls` 出 `docs raw secret`) | **规则留下的形状**(同一份数据,实测只有 `docs`) |
| 被排除的名字 | 看得见、读得到,只是写不了 | **名字不存在**,`ls` 报 `ENOENT`,连存在性都不泄露 |
| 能否重组路径 | 不能(形状与 `R` 完全一致) | 能(`map`/`link` 把 `/raw` 挂到 `/archive/raw`) |
| key 的粒度 | 只有「绑定某个视图 + ro/rw」两档(`apikey create --view`,没有路径参数) | 规则本身是**路径级**的,可以精确到子目录 |
| 数量与名字 | 一个真实视图**只能有一个别名**,且别名存在期间真名不能挂(第二个别名被拒,原文:`one public name per real view`,轮换用 `--retire`) | 数量不限,每个派生各占一个名字,基视图照旧可挂 |
| 只读的性质 | **权限**:经服务端挂载时可原地 `set-ro` 热切换(双向热生效;仅握手时已落内核只读的挂载解除需重挂,见下) | **契约**:换成无限制 rw key 挂载,`mkdir` 仍是 `EROFS` |
| 分发成本 | 要发密钥、要轮换/吊销 | 服务端一份文本,改一行 ≤2s 生效,不用发东西 |

实测的只读 key 热切换方向(重要,均指**经 meta/data-server 的挂载**——切换靠服务端活会话复验 tick,本机 `--home` 挂载没有服务端凭据门,`set-ro` 不影响已建挂载):**rw → ro 是热生效的**(挂载中 `set-ro` 后 `mkdir` 变 `EPERM`,不用重挂);**以 rw key 建立的挂载被 `set-ro` 后再 `--rw` 切回,恢复写同样热生效**——本机实测(`.build-bin/repro_setro_resume.sh`):切换后复检窗内仍有至多一个 tick 的残留拒绝(`EROFS`),tick 过后同一挂载直接 `write+fsync ok`,无需重挂,失败的写尝试不留字节。**只有握手时 key 已是 ro、内核已落只读的挂载**,`set-ro --rw` 改不了内核标志、`mkdir` 仍 `EROFS`,必须重挂(或直接用 rw key 重挂)。命令输出里「no reconnect, no remount」对服务端门禁的双向切换成立,对内核只读的解除不成立。

**结论**:对外分享只有两种诉求时——
- 只是「别让人写」→ 只读 key 足够,连别名都不需要(直接把 ro key 发给对方挂 `R`);
- 还要「别让人看见全部」或「换个路径给他」→ 必须用派生(或独立视图);
- 两者可以叠加:派生给形状,key 给权限。**安全边界始终是 key/视图,`.layout` 只是呈现规则。**

#### 2.3 fork 的四步账本实测:删名字 ≠ 删字节

一份 `hello world`(账本 `holders=1`),`fork` 后的走向:

| 步骤 | `refs.db` | `blocks/<bucket>/<key>` | 基视图还能读吗 |
| --- | --- | --- | --- |
| 基视图写入 `big.bin` | `holders=1` | 在 | 能 |
| `view fork --name exp --base team` | `holders=2` | 在(字节没复制) | 能 |
| 在 fork 里 `rm big.bin` | `holders=1` | **还在** | 能(`cat` 仍是 `hello world`) |
| 基视图也 `rm big.bin` | 行消失(`0`) | **回收**,桶目录空 | 不能 |

两点容易被忽略:`fork` 的「可写」只到结构层——`mkdir`/`rename`/`unlink` 放行,**连它自己 `touch` 出来的空文件也写不进内容**(`EROFS`);而且块路径不是 `data_ref` 的前缀,是 `FNV-1a(key) % 256` 的桶(`blocks/b4/<key>`),按 key 前缀去找会以为「块丢了」。

#### 2.4 能不能用「fork + 删行」代替派生,不要 `.layout`

**能物化出同样的可见形状,但换来的是快照语义。** 在 fork 里删掉行,被删的名字在该视图里就真的不存在了(`ENOENT`),基树不动——这条今天就能用,不需要任何新机制。问题不在「能不能藏」,在「之后怎么活」:

| | fork + 删行 | 派生(`.layout`) |
| --- | --- | --- |
| 可见形状 | 删行 = 隐藏,物化在 `meta/F.db` | 规则算出,不落行 |
| 基树新增/改名/改属性 | **不跟随**(复制发生在一瞬间,要重新 fork) | **立刻跟随**(每次 readdir 现算) |
| 成本(20,201 个条目实测) | `fork` 0.61s、**20,201 行 / 3.5 MB** | `define` 0.00s、**47 字节** |
| 成本(100,002 个条目实测) | `fork` **0.65s**、`meta/F.db` **12 MB** + 定义 13 字节 | `define` 0.00s、100 条规则 **2.9 KB** |
| 改一次「藏哪些」 | 重新 fork,或手改行(改完就和规则脱节) | 改一行,服务端 ≤2s 生效 |
| 只读保证 | 结构默认可写,要另加一个标志才只读 | 天然只读(换 rw key 也 `EROFS`) |
| 能否看出「藏了什么」 | 看不出来——行没了,审计无法区分「藏了」与「从来没有」 | `view list` 直接报 `1 hide, 1 only, …` |
| 多个外部视角 | N 份拷贝 × N 行 | N 份文本 × 几十字节 |

按实测外推(100,002 条 → 12 MB、0.65s,约 125 B/行):1000 万条目的视图,**每多一个「fork 当透镜」就多约 1.25 GB 元数据、约 65 秒复制**;派生 100 条规则才 2.9 KB。

> 想要「定义也是一行表」:PG 后端本来就是 `grovefs_control.view_defs` 的一行(SQLite 侧用文本文件,是为了能直接手改、能 `git diff`、不需要 schema 迁移)。

**「让 fork 跟随基树写入」能不能靠往 fork 追加元数据实现?今天不能,也没有这个入口。** fork 的 `node` 行只有自己的 `id/parent_id/name/ino/data_ref`,没有「这行来自基树哪一行」的回指,也没有变更序号;要跟随就得在基树每次 create/rename/unlink 时产出增量、写进 fork,再解决冲突(基树改名 vs fork 已把该条目整理到别处)——那是一套新的同步协议,不是「追几行元数据」。今天有三条现成路径:

- **要快照、要能整理** → fork;基树变了就重新 `view fork`(100k 0.65s / 12 MB)。重建会丢掉 fork 侧的整理,所以用「蓝绿」:fork 到新名 `F2` → 挂载验证 → 客户端切过去 → `view rm --name F`。
- **要活跟随、不要独立树** → 派生(`.layout`):基树一变,下次读目录就是新形状;代价是挂载只读、目录不能整理。
- **在别的机器上重建 fork** → `view export` + `view import --as-fork B`(§3.4.2),同样是一次性快照。

**选型**:需要「冻结一份、跟基树脱钩」→ fork(想藏就删行);需要「同一棵活树的多套只读视图」→ 派生。两者是**时间语义**不同(快照 vs 规则),不是同一个功能的两种写法。

#### 3. 每条命令会生成/改动什么

> 路径以 SQLite 存储根 `D` 为例。PG 后端没有本地 `.db`/`.layout` 文件,对应物见本节末尾。

| 命令 | 生成/改动 | 不动什么 |
| --- | --- | --- |
| `grovefs view --meta-dir D create --view V [--manifest F]` | 新建 `D/meta/V.db`(内含 `node`/`ctrl`/`locks` 三张表);`--manifest` 再往树里写目录/空文件节点 | 不改其它视图 |
| `grovefs --home D init --view V [--manifest F]` | 同上(等价入口;不检查名字是否已有定义) | 同上 |
| `grovefs --home D mount --view V`(本地,无定义) | 若 `D/meta/V.db` 不存在则**直接拒绝**,提示先 `view create`。挂载本身不落盘 | 不改已有树 |
| `grovefs view --meta-dir D define --name X --base B --rules F` | 写/覆盖 `D/meta/views/X.layout`,正文含 `base B` 一行,其余行是规则 | 不建 `X.db`,不复制/移动任何内容 |
| `grovefs view --meta-dir D alias --name A --real R [--retire OLD]` | 写/覆盖 `D/meta/views/A.layout`(仅一行 `alias R`);`--retire` 再删旧别名定义 | 不建 `A.db`;`R` 的数据不动 |
| `grovefs view --meta-dir D fork --name X --base B` | 新建 `D/meta/X.db`(复制 B 的 `node` 行,保留 inode/硬链接);写 `D/meta/views/X.layout`(仅一行 `fork B`);在 `D/meta/refs.db` 登记共享字节 | 不复制字节;不改 B 的树 |
| `grovefs view --meta-dir D rm --name X` | 删 `D/meta/views/X.layout`;若 X 是 fork,再删 `D/meta/X.db`(+WAL/SHM)并重建 `refs.db` | 派生/别名不动数据;fork 不动基树与仍被持有的字节 |
| `grovefs view --meta-dir D rm --name V --base` | 删 `D/meta/V.db`(+WAL/SHM),重建 `refs.db`,从 `--data-dir`(默认 `D`)回收**无人再持有**的对象 | 别的视图仍持有的字节保留 |
| `grovefs mount --view V --layout F` | 规则文件本身不落盘;若本地 `meta/V.db` 不存在则直接拒绝(与不带 `--layout` 一致) | 不改服务端状态;只影响本次挂载 |
| `grovefs view --meta-dir D export/import --view V` | 导出:一个 JSONL 文件;导入:新建 `D/meta/V.db`(带 `--meta-store fdb://…` 时导入开出一个 subspace,§6.3) | 不搬字节(字节要另拷 `blocks/` 或交给 data-server 的数据根) |

几条容易忽略的规则:

- **定义只是文本槽**:`view define/alias/fork` 写的是 `meta/views/<名字>.layout`,只有 fork 会额外新建一棵 `meta/<名字>.db`。
- **定义可以先于基树存在**:`view define` 允许 base 还没建(会提示),但**挂载时**服务端与本地都会强制 base 必须已创建,否则派生名拒挂。建议顺序仍是“先 `view create` 基视图,再 define”。
- **删定义不热作用**:`view rm --name` 后,新挂载立即看不到定义;已经挂着的派生/别名连接会保持旧解析,直到重挂(或服务端缓存被 HA/快照事件清掉)。
- **删基视图有四道守卫**:名字本身不能还被定义治理;不能有别的派生/别名/fork 指向它;本机不能还挂着;PG 里的树也必须**用同一条命令**删(`view --meta-store postgres://… rm --base`)——销账与 `DROP SCHEMA` 在同一事务里完成(§15.8),绕过 CLI 手工 `DROP SCHEMA` 只会留下永远多计的账本行。

#### 4. 落盘结果:文件与表

一次 `view create` + `view define` + `view alias` + `view fork` 之后,存储根大致长这样:

```text
D/
├─ blocks/<2hex>/<key>         # 所有视图共享的文件字节(key 是 UUID,不是内容哈希)
└─ meta/
   ├─ team.db                  # 基视图 team: node + ctrl + locks
   ├─ lab.db                   # fork lab: 自己的 node + ctrl + locks
   ├─ refs.db                  # 跨视图持有账本:谁还在引用哪个 key
   ├─ views/
   │  ├─ docsonly.layout       # base team + only /docs
   │  ├─ public.layout         # alias team
   │  └─ lab.layout            # fork team
   └─ api_keys.db              # 可选:API key/审计(SQLite 控制面)
```

三个 `.layout` 的正文分别长这样(纯文本,不是 SQLite 库):

```text
# meta/views/docsonly.layout
base team
only /docs

# meta/views/public.layout
alias team

# meta/views/lab.layout
fork team
```

| 落盘物 | 是什么 | 谁生成/维护 |
| --- | --- | --- |
| `meta/<view>.db` | SQLite 库;`node` 表就是目录树(每行一个名字),`ctrl` 键值表(今天只有 HA `epoch`),`locks` 锁租约 | `init`/`view create`/`view fork`/`view import` |
| `meta/views/<name>.layout` | **纯文本**定义,不是数据库;三种互斥形态:`base B`+规则 / `alias R` / `fork B` | `view define`/`alias`/`fork`;`view rm` 删除 |
| `meta/refs.db` | 共享字节账本;删名字时判断“是否还有别的视图持有” | 每次增删自动记账;`refs rebuild` 可重算 |
| `blocks/<2hex>/<key>` | 文件真实字节;所有视图只存 key | 写入/导入时生成;回收时删除 |
| `meta/api_keys.db` | 可选控制面:key 摘要与调用审计 | `apikey` 命令 / `mcp --require-api-key` |

`node` 表一行包含:`id`、`ino`、`parent_id`、`name`、`is_dir`、`data_ref`(指向 blocks 的 key)、`link_target`、`mode/uid/gid`、`mtime/atime/ctime`,并对 `(parent_id, name)` 唯一。`ctrl` 是 `(k, v)` 键值表,今天只放 HA 的 `epoch`(**不是 inode 计数器**——新 inode 不走游标分配,= 自己那行的 `id`);`locks` 是 POSIX/BSD 锁租约。`refs.db` 只有一张 `holders(data_ref TEXT PRIMARY KEY, holders INTEGER)`,记每个对象被多少个视图持有。`api_keys.db`(可选)含 `api_keys` 与 `mcp_calls` 两张表(凭据摘要/审计)。**视图之间的差别不在表结构,而在“这个名字有没有自己的 `.db`”**。

**PG 后端(`--meta-store postgres://…`)的对应关系**

| SQLite 落盘物 | PG 对应物 |
| --- | --- |
| `meta/<view>.db` | 一个 schema `grovefs_v_<hex(view)>`(内含同构 `node`/`ctrl`/`locks`) |
| `meta/views/<name>.layout` | `grovefs_control.view_defs` 表的一行 |
| `meta/refs.db` | 共享表 `grovefs_refs.holders` |
| `meta/api_keys.db` | `grovefs_control.api_keys` + `mcp_calls` |

PG 下 `view define/alias/rm --name` 照常可用(写 `view_defs`);`view --meta-store postgres://… create` 会显式建出该视图的 schema(`grovefs_v_<hex>`),`view --meta-store postgres://… rm --base` 在一个事务里销账(逐 key 减 `grovefs_refs.holders`)并 `DROP SCHEMA … CASCADE`,提交后按删除前读到的 `data_ref` 回收 `blocks/` 字节(§15.8);`view fork` 同样三档可用(复制的是 `grovefs_v_<hex>` schema,单个事务);零拷贝 `import` 仍基于本地文件树、会拒绝。

> **PG 建库入口**:`grovefs view --meta-store postgres://… --meta-dir D create --view V` 会显式建出该视图的 `grovefs_v_<hex>` schema 和根行。挂载不会建 schema——schema 不存在时 `M_OPEN` 直接拒绝并提示这条命令;已有的 schema 也可以从 dump 恢复。

#### 5. 参数怎么填

| 参数 | 用在哪 | 含义 |
| --- | --- | --- |
| `--meta-dir D` | `view`/`apikey`/`mcp`/`refs`/`import`,以及 `meta-server` | 元数据根:下面有 `meta/`;与 `meta-server --meta-dir` 指向同一个目录 |
| `--home D` | `grovefs` 的全局参数(放在子命令前) | 单机模式的本地根;未显式给 `--meta-dir` 时回退到它 |
| `--meta-store postgres://…` | `view`/`apikey`/`mcp`/`meta-server`/`import` | 控制面改走 PG:`view_defs`/`api_keys`/`mcp_calls`;本地不落 `.layout`。`import` 的产物只能是本地 SQLite 树,给非 SQLite URI 时拒绝并指明替代路径(挂载内 `cp`/`rsync`) |
| `--view V` | `init`/`mount`/`view create` | 要建/挂的视图名(一个安全路径元素) |
| `--name X` | `view define/fork/alias/rm` | 新定义的名字(派生名/fork 名/别名/要删的名字) |
| `--base B` | `view define/fork` | 被引用的**物理基视图**名 |
| `--real R` | `view alias` | 别名指向的真实物理视图名 |
| `--retire OLD` | `view alias` | 原子轮换:创建新别名后删除同一真实视图的旧别名 |
| `--rules F` | `view define` | 规则文件路径(纯文本,一行一个动词) |
| `--manifest F` | `init`/`view create` | 种子清单(`mkdir`/`touch` 行),建树时预铺骨架 |
| `--data-dir D` | `view rm --base`/`import`/`du` | `blocks/` 所在根;默认取元数据根(与 `--meta-dir`/`--home` 同一路径),meta 与 data 分家时才需点名;删基视图从这里回收字节,`du` 从这里量字节 |
| `--layout F` | `mount` | 本次挂载的本地规则文件;只影响这个挂载,可写;本地 `--home` 下与派生名同用时显式文件优先,不叠两层透镜 |

**规则文件(`--rules` / `--layout`)的四个动词**

| 写法 | 参数 | 效果 | 初学者注意 |
| --- | --- | --- | --- |
| `hide /a/b` | 一个源路径 | 这棵子树在本视图完全看不到 | 只影响显示;源数据还在 |
| `only /a` | 一个源路径,一行一个 | 只显示这些子树;沿途祖先目录自动放行 | 需要多个就写多行;白名单外不可写 |
| `map /a/b /x/y` | 源路径 + 目标路径 | 条目从源位置消失,出现在目标位置;中间目录自动合成 | 源位置是基树里的真实路径;合成目录不能往里新建 |
| `link /a/b /x/y` | 源路径 + 目标路径 | 两处都可见,同一个 inode | 不复制字节;删任意一处只摘该名字 |

- 所有路径都是**视图内绝对路径**(以 `/` 开头),`<src>` 通常必须是**基视图里真实存在的路径**;`<dst>` 可以是你发明的新路径。
- 规则一行一条,`#` 开头是注释,空行忽略;同一个 `<dst>` 不能出现在两条 `map`/`link` 里。
- 解析阶段**不校验源路径是否存在**:写错通常不会报错,只是那块内容不出现。写规则前先查基树——挂载后 `find`,或 MCP `list_view_tree`;不要照源目录猜(`import` 会加前缀)。
- **没有规则时派生与基视图逐条一致**:`.layout` 里只有 `base B` 一行时,`find` 输出与基视图的 diff 为空(实测)——「一致」是特例,不是默认约束;一旦写了 `hide`/`only`/`map`,形状就不再相同。
- **合成目录的 inode 来自保留高位**:`map`/`link` 的中间目录在基树里不存在,按 `i64::MAX` 往下发(实测 `map /raw /archive/raw` 后 `/archive` 的 inode 是 `9223372036854775807`);`only` 的祖先目录都是基树里真实存在的目录,只是「放行」,属性不变。
- **`..` 的行为按动词不同**:`map` 的叶子会改写自己报告的名字/父目录,`..` 留在透镜内;`link` 保留源身份,`..` 回到**源父目录**(同一个 inode 两个名字,路径解析走真实坐标)。

#### 6. `mount --view X` 到底选了哪棵树

按顺序判断,命中即停:

1. `meta/views/X.layout` 是 `alias R` 一行 → 换成真实视图 `R`,后续按 `R` 解析;`X` 不是独立树,`R` 从此不能按真名新挂。
2. 是 `fork B` 一行 → 打开 `X` 自己的 `meta/X.db`(建 fork 时复制的那棵),与 `B` 此后的变化无关。
3. 是 `base B` + 规则行 → 打开 `meta/B.db`,把后面的规则套成只读透镜。
4. 没有 `X.layout` → `X` 自己就是基视图,直接打开 `meta/X.db`。
5. `meta/X.db` 不存在 → **直接拒绝**(本地与服务端一致),提示先用 `view create` / `init` / `view import` 建树;挂载不会创建空树。

> 服务端解析 `<meta-dir>/meta/views/`,客户端 `--home` 解析 `<home>/meta/views/`;两边都只做**单跳**:别名不能再指向别名,有定义的名字不能当 fork 的基。**服务端**挂载时还会拒绝“派生套派生”,并且派生挂载时 base 必须已存在;本地 `--home` 同样要求 base 树已创建,否则拒绝。

#### 7. 最容易踩的坑

1. **写数据不会创建基视图。** 新建树只能用 `init`/`view create`/`import`;挂载缺库直接拒绝,不会创建任何空树。
2. **派生视图只读,而且不是权限边界。** 能挂基名的人仍能看到全树;要隔离用独立视图 + token/API key,不要靠“藏几个目录”。
3. **别名会把真实名锁住。** 别名存在期间,`mount --view R` 会被拒;新挂载用别名。删别名后真实名恢复可挂(仅新挂载)。一个真实视图同时只能有一个别名,轮换用 `--retire` 一条命令完成。
4. **fork 只复制目录,不复制字节。** 内容写 `EROFS`;删 fork 不影响基树;基树后来新增的文件不会自动出现在 fork;三档后端都能 fork,而且都是同一个动词(由持有存储的进程复制:SQLite 抄文件行、PG 跨 schema、FDB 跨子空间,各自按预算提交——装得下一个事务就是一个,否则自动分批)。
5. **`--layout` 与 `meta/views/*.layout` 是两回事。** 前者只影响本次挂载、可写;后者是服务端命名定义、只读。本地同时给 `--view 派生名 --layout F` 时,显式 `--layout` 优先、已发布定义被绕过;若目标视图根本没创建,会直接报错。两者建议二选一。
6. **只有账本归零才真正回收字节。** 删派生/别名只删定义;删 fork 只删自己的树;在任意视图里删名字,都要等 `refs.db` 里没有其它视图持有才释放对象。
7. **`view define` 可以先于基树存在。** 它会提示 base tree missing;挂载时才强制。先 `view create` 再 define,能少一次排错。
8. **拼错视图名会直接报错,不会长空树。** 本地与服务端都拒绝未创建的视图名——这是唯一行为,没有开关。

**报错对照**:`EROFS` = 只读文件系统(派生挂载、fork 内容写);`EPERM` = 操作不允许(向合成目录新建、服务端派生写门禁)。

#### 8. 从零走一遍(照抄即可)

```bash
Y=./target/release/grovefs
D=/data/myfs

# ① 建基视图(真正存数据的那棵)
$Y view --meta-dir $D create --view team
$Y --home $D mount --view team -m /mnt/team &
mkdir -p /mnt/team/docs /mnt/team/raw
echo "hello" > /mnt/team/docs/a.md

# ② 派生:只读,只显示 /docs
cat > docsonly.rules <<'EOF'
only /docs
EOF
$Y view --meta-dir $D define --name docsonly --base team --rules docsonly.rules
$Y --home $D mount --view docsonly -m /mnt/docsonly &
ls /mnt/docsonly                  # docs
echo x > /mnt/docsonly/docs/b.md  # Read-only file system

# ③ fork:独立目录树,结构可改、内容只读
$Y view --meta-dir $D fork --name lab --base team
$Y --home $D mount --view lab -m /mnt/lab &
mkdir /mnt/lab/review                        # 可以
mv /mnt/lab/docs/a.md /mnt/lab/review/a.md    # 可以
echo x > /mnt/lab/new.md                      # Read-only file system

# ④ 别名:公开名,全树可写;此后 team 不能再按真名新挂
$Y view --meta-dir $D alias --name public --real team
$Y --home $D mount --view public -m /mnt/public &
echo hi > /mnt/public/docs/c.md               # 可以,写的就是 team

# ⑤ 看清单与落盘
$Y view --meta-dir $D list
ls $D/meta $D/meta/views
```

`view list` 的输出形如(顺序可能不同):

```text
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
alias 'public' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
```

> 注意:别名存在期间,`base view 'team'` 仍会出现在清单里,但新挂载必须用 `public`;删别名后 `team` 才恢复可挂。

清理顺序(先删定义,再删基树):

```bash
$Y view --meta-dir $D rm --name docsonly       # 只删定义;team 的数据不动
$Y view --meta-dir $D rm --name lab            # 删 fork 自己的树;team 与共享字节不动
$Y view --meta-dir $D rm --name public         # 删别名;team 恢复可挂(仅新挂载)
$Y view --meta-dir $D rm --name team --base --data-dir $D   # 连树带无人持有的字节一起回收
```

### 3.1 每客户端显示定制:`mount --layout`(同一份数据,不同形状)

manifest 定的是**存储树**的形状;`--layout` 定的是**某个挂载点看到**的形状——数据不动,只改显示。规则文件每行一条动词(`#` 注释,路径均为绝对路径,源路径指存储树):

| 规则 | 参数释义 | 效果 |
| --- | --- | --- |
| `hide <src>` | 源路径 | 该子树在本挂载点整个隐藏 |
| `only <src>` | 白名单源路径,可多条 | 只显示这些子树(到达它们的祖先目录自动放行,但白名单外不可写) |
| `map <src> <dst>` | 源路径 + 显示路径 | 重组:源位置消失,条目出现在 `<dst>`,中间目录自动合成 |
| `link <src> <dst>` | 源路径 + 别名路径 | 别名:两处同时可见、同一 inode,不复制字节 |

```bash
$ cat lens.txt
hide /photos/private
map /data/raw /team/raw
$Y --home $BASE mount --view alice -m /mnt/alice --layout lens.txt
layout overlay lens.txt: 1 hide, 0 only, 1 reloc rule(s)
mounting view 'alice' at /mnt/alice (base $BASE)

$ ls /mnt/alice            # 本客户端: data docs photos team
data  docs  photos  team
$ ls /mnt/alice/photos     # private 已隐藏
public
$ echo hi > /mnt/alice/team/raw/note.txt   # 写直通源对象,无透镜的其它挂载立即可见
$ mkdir /mnt/alice/team    # 透镜合成的脚手架名已占用
mkdir: 无法创建目录 “/mnt/alice/team”: 文件已存在
```

要点(都有测试断言背书):透镜只作用于显示层,**真实条目保持真实 inode**,读写/锁/条带/GC 全部直通;经透镜 `rm` 删的是源条目;其它客户端、其它挂载完全不受影响(透镜不落盘、不改服务端状态)。它是**显示定制不是安全边界**——硬隔离请用独立 view + token / 视图绑定 key(§6.1)。限制:向纯合成目录创建返回 `EPERM`;symlink 目标文本不改写;共享 meta-server 下挂合成目录的条目失效推送可能不命中透镜坐标,建议配 `--no-cache`;非法规则文件挂载直接 fail-fast。

### 3.2 服务端派生视图:形状集中定义在服务端(`grovefs view`)

§3.1 的透镜规则由各客户端自备;想把同一套形状**命名后存到服务端**(团队共享一个显示契约、按名授权),用派生视图:`grovefs view define` 把规则写进 meta 根的 `<meta-dir>/meta/views/<name>.layout`(由 `base <物理视图>` 一行 + 余下的规则行组成,就是 §3.1 的四个动词;`view --meta-store postgres://…` 时写进 PG 的 `grovefs_control.view_defs` 表,本地不落文件),客户端只需 `mount --view <派生名>`,**不需带本地规则文件**。派生(虚拟)视图**一律只读挂载**——形状集中治理、只分发不修改,写入只属于原(基名)视图;要可写的自定义形状用 §3.1 客户端自备的 `--layout` 透镜。同一个 `views/` 目录还放另外两种定义——**纯别名**(治理"叫什么",§3.4,`grovefs view alias`)与 **fork 视图**(独立元数据树:结构可写、内容只读,§3.4.1,`grovefs view fork`):

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --meta-dir D create --view V [--manifest F]` | V=新**基视图**(物理树)名,F=可选种子清单 | 显式创建一棵**物理树** `D/meta/V.db`(等价 `init --view V`);**只有基视图的删除才真正回收底层字节**(跨视图引用账本归零才物理 GC,§5.3);派生视图/别名只 reshape/rename,从不参与物理删除 |
| `grovefs view --meta-dir D define --name V --base B --rules F` | D=meta 根(同 meta-server 的 `--meta-dir`),V=派生名,B=物理视图名,F=规则文件 | 写入/替换 `D/meta/views/V.layout`(PG 后端 `--meta-store`:改 `grovefs_control.view_defs` 行);`V` 的挂载**只读**(写走 `B`);对 `meta-server`,已挂载连接 ≤2s 内**热换形**(免重挂);本地挂载下次挂载生效 |
| `grovefs view --meta-dir D list` | — | 列出全部视图定义:派生视图(base/规则计数)、纯别名(→ 真实视图)与 fork(→ 基视图),以及**未定义的裸基视图**(→ 自己的 `meta/<名>.db`),§3.4/§3.4.1 |
| `grovefs view --meta-dir D rm --name V` | V=派生名、别名或 fork 名 | 只删定义,**不动任何数据**;例外:`rm --name <fork>` 连同该 fork 自己的 `meta/<名>.db`(+WAL/SHM)一并删除并 `refs rebuild` 重算账本(基视图与字节不受影响);删别名后基名恢复可挂(仅新挂载) |

```bash
$ cat docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --meta-dir $BASE define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --meta-dir $BASE define --name team --base team --rules docsonly.rules   # 名与基同名
error: derived view name must differ from its base
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs --home $BASE mount --view docsonly -m /mnt/lens &   # 本地：直挂派生名，无 --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ grovefs mount --view docsonly --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/lens_r &   # 远程：同样只给名，M_OPEN 握手带只读标志
$ ls /mnt/lens                       # 只有 docs;基名 team 挂出来仍是全树，且可写
docs
$ echo x > /mnt/lens/docs/new.md     # 经虚拟视图的写一律拒绝
bash: /mnt/lens/docs/new.md: Read-only file system
```

语义与守卫(第 9 节 e2e 验证):define 拒绝 `name == base`、拒绝 base 本身是派生视图(不嵌套)、拒绝遮蔽物理 `<name>.db`、非法规则 fail-fast;**虚拟视图一律只读(硬契约)**——写只属于原(基名)视图,三层强制:`meta-server` 在 `M_OPEN` 握手回包里带派生标志,客户端直接落内核只读挂载(写/`flock` 写锁即时 `EROFS`、零往返);服务端另对派生名的写意图 op 加独立权威门禁(回 `E_NOTPERM`→`EPERM`,有意不用 `E_ROFS` 以免被误读为 HA 故障转移信号),旧/伪造客户端绕不过;本地(`--home`)派生挂载在挂载时同判强制 RO。**改定义热生效**——`meta-server` 每次分发 op 前检查定义版本(SQLite 看文件 mtime、PG 看 `xmin`、FDB 看集群写版本戳),替换后已建立的挂载在客户端 TTL(≤2s)内自动切新形状、不需重挂(实测 ≤5s 断言,见第 9 节;换形后只读契约不变),`view rm` 则不热作用(已挂载者保持旧形状到重挂,避免正在用的树被抽掉);同名多客户端由服务端**同一份定义**渲染,虚拟 inode 跨机一致;共享 meta-server 下派生订阅者同时注册进自身名与 base 名两个桶、写双播,基侧写在非 `--no-cache` 下也能对派生挂载即时可见;要按名限制某调用方只能读这棵派生视图,铸一把 `--view docsonly --ro` 的 key(写永远走基名,派生挂载本就一律只读)。本地模式(`--home`)同机制:挂载不带 `--layout` 时自动解析 `<home>/meta/views/<view>.layout`(显式 `--layout` 优先,不叠两层透镜;热重载仅限 meta-server,本地挂载在挂载时解析一次)。注意:SQLite 档 `grovefs view` 操作服务端本地文件,需在 meta-server 所在机器执行,HA 部署下 `meta/views/` 要随控制面库(`api_keys.db`/`refs.db`)手工同步到备机;PG/FDB 档定义在共享存储(`grovefs_control`/集群 `ctl`),带同一 `--meta-store` 从任意可达节点治理即可、无本地文件要同步(§6.2/§6.3);它同样**不是安全边界**,直挂基名仍见全树(能拦住的是 token / key,不是形状)。

### 3.3 存量数据零拷贝导入(`grovefs import`):目录原地不动,客户端连接即得

服务端已经有一批现成数据(如 `/data/legacy`)想让客户端按视图获取时,不必先灌一遍:`grovefs import` 把**已存在的本地目录树**登记进视图,字节原地不动——同文件系统下对象就是源文件的硬链接(零额外字节、两路径同一 inode 双向实时可见),跨设备自动退化为流式拷贝:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs import --view V --source DIR` | V=目标视图(不存在则创建),DIR=已存在的源目录 | 整棵树镜像进视图(默认前缀 `/<DIR名>`):目录建节点、文件硬链接进 `blocks/` 并登记;mtime/atime/权限/属主保真 |
| `grovefs import ... --prefix P` | P=视图内目标目录(如 `/legacy`) | 镜像到指定前缀下,`P` 不存在自动建 |
| `grovefs import ... --meta-dir D [--data-dir R]` | D=meta 根,R=存储根(默认 D) | meta-server 部署下在**存储机**执行,直写该部署的权威 SQLite 树 `meta/V.db` 与 `R/blocks/`;已挂载客户端靠 TTL(≤2s)收敛 |
| `grovefs import ... --meta-store URI`(或 `GROVEFS_META_STORE`) | URI=`postgres://…` / `fdb://…` | **拒绝**:import 只能产出本地 SQLite 树,在 PG/FDB 部署下写下去是服务端读不到的影子树;字节走挂载点灌入(§14.2),报错原文点名这条替代路径 |
| `grovefs import ... --dry-run` | — | 只报告将导入多少目录/文件,**连视图 db 都不创建** |

```bash
$ grovefs --home /data/cluster import --view legacy --source /data/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home /data/cluster import --view legacy --source /data/legacy
created BASE view 'legacy' (target was absent); imported 2 object(s)
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
$ grovefs mount --view legacy --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/da &   # 客户端连接即得
$ echo "new" > /mnt/da/legacy/docs/quarter.txt   # 经挂载写 → 源路径同 inode 同步可见
$ cat /data/legacy/docs/quarter.txt
new
$ rm /mnt/da/legacy/photos/2026/y.jpg            # 视图里删名字 → 只摘 blocks/ 链接,源文件毫发无损
```

语义要点(`import` 套件 35 条断言;`IMPORT_BACKEND=fdb` 档 47 条):**重跑即增量**(已有名字跳过、新文件补登);**GC 只摘链接**(源路径永不因视图操作被删);导入后的条目与原生写入完全同权(读写/锁/快照/失效推送全适用),并能被 §3.2 派生视图 `map`/`link` 组成任意虚拟目录形状——该形状挂载**只读**(写走导入的原视图,经双播推送穿透到虚拟形状可见);`grovefs import` 也拒绝以派生名为目标(否则写出无人可读的影子 `<view>.db`)。注意三个实现事实:硬链接要求源与 `blocks/` 同盘(跨盘自动发拷贝并如实计数);外部工具以 **rename/替换写**更新源文件(vim 默认行为)时,视图侧看到的是旧 inode 的最后内容,需对新内容重跑 import;symlink/特殊文件有意跳过。非 SQLite 后端(`--meta-store postgres://…` 或 `--meta-store fdb://…`,含 `GROVEFS_META_STORE` 回退)下 `import` **直接拒绝**并给出替代路径(挂上视图后走挂载点灌字节,§14.2)——它只能产出本地 `meta/<view>.db`+`blocks/`,写下去就是服务端看不见的影子树(`import`/`govcheck` 套件实证;`view fork` 不在此列——它已能在三档后端各自的权威存储里复制)。FDB 档除了把这条拒绝本身按后端逐条复证(含 `--dry-run` 之前即拒、`GROVEFS_META_STORE` 同拒、拒绝不落任何影子 db),还把拒绝点名的替代路线端到端跑通:本地 `import` 只当暂存,真正入集群的是 `view export` → 交字节 → `view --meta-store fdb://… import`;搬完之后「源文件与视图共享 inode」这一零拷贝性质在两个方向上都不成立(视图读的是部署自己的对象),套件按这点做负向断言而不是回避。

---

### 3.4 纯别名:名字治理(`grovefs view alias`)

§3.2 的派生视图治理"长什么样"(一个视图对外呈现哪棵树);**纯别名**治理"叫什么"——把存储真实名藏到服务端,对外只发公开名:名字就是别名,映射值才是真实名。定义文件同放 `<meta-dir>/meta/views/<name>.layout`,形态互斥:首行 `alias <真实视图>`、**不带任何规则行**。与派生视图的正反面:**别名可写**(它就是真实视图,全树原形),且**真实名从此不是挂载入口**——本地(`--home`)与 `meta-server` 都拒挂被治理的真实名(公开名与存储身份解耦,对外名字可以自由轮换,猜出真实名也挂不进来):

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --meta-dir D alias --name A --real R` | A=公开名,R=物理视图名 | 写入/替换 `D/meta/views/A.layout`(内容仅 `alias R`;PG 后端 `--meta-store`:`grovefs_control.view_defs` 行);挂 `A`=全树**可写**;`R` 从此**不可直挂**(本地与服务端均拒) |
| 挂载 | 无任何额外参数 | `mount --view A` 即得真实视图全树;远程在 `M_OPEN` 服务端解析(别名解析先于准入判定,别名与基名走同一道门) |
| `grovefs view --meta-dir D rm --name A` | — | 删别名,真实名对新挂载恢复可挂(已建立连接不受影响) |
| `grovefs view --meta-dir D alias --name A2 --real R --retire A` | A2=新公开名,A=被泄露的旧别名 | **原子轮换**:先写 `A2` 再删 `A`,真实名 `R` 全程被治理、**无裸露窗口**(不会出现两步之间 `R` 短暂可直挂的空档);旧名 `A` 立即失效(挂它只落到一个全新的空视图,触不到真实树);合法用户改用 `A2` 重挂即可 |

```
$ grovefs view --meta-dir $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --meta-dir $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
alias 'teamwork' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/work --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240
mounting view 'teamwork' at /mnt/work (fully remote: no local store)
$ ls /mnt/work
docs photos
$ echo hello > /mnt/work/docs/2026/new.md   # 别名挂载可写,写入即真实树
$ ls /mnt_docs/docs/2026                    # 同一写经另一路派生名(docsonly)挂载穿透可见
a.md new.md
$ grovefs mount --view team -m /mnt/t --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)   # 直挂被治理的真实名:服务端拒挂(exit=1)
$ findmnt /mnt/work | tail -1
/mnt/work grovefs:teamwork fuse rw,nosuid,nodev,relatime   # 别名可写;派生名的挂载行是 ro
```

公开名泄露后**一条命令原子轮换**(旧名即刻失效、真实名全程不可直挂、授权不动):

```
$ grovefs view --meta-dir $BASE alias --name teamwork2 --real team --retire teamwork   # 'teamwork' 已泄露,换成 'teamwork2'
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --meta-dir $BASE list
alias 'teamwork2' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/leak --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240   # 拿旧泄露名再挂
$ ls /mnt/leak          # 空:旧名不再解析到真实树(命名空间按需新建,数据不外泄)
$ grovefs mount --view team -m /mnt/probe --meta-server 10.0.0.5:50241 --data-servers 10.0.0.5:50240        # 猜真实名同样被拒(全程治理)
error: view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)
```

守卫(`alias` 命令处 fail-fast,layout 套件第 10/11 节 26 条断言):拒绝 `name == real`、拒绝遮蔽物理 `<name>.db`、拒绝目标是派生视图(别名只映射物理视图)、拒绝**别名链**(`--real` 指向另一个别名)、一个真实视图同时只允许一个对外名(`--retire` 轮换在同一条命令内先写后删,不受此约束卡住);`--retire` 还拒绝指向派生视图或不存在的别名(不会误删形状定义)。本地(`--home`)同契约:挂载先解析 `views/<name>.layout`,别名换名后照常挂载并打印 `alias view 'A' resolves to real view 'R' (full tree, writable)`;直挂被治理的真实名回 `error: view 'R' is governed by an alias — mount the alias name instead`;`grovefs import --view A` 拒绝以别名为目标(别名不是存储身份,写出无人可读的影子 db)。实现事实:别名解析单跳、每 `M_OPEN` 重读定义(SQLite 全扫 `views/` 目录——挂载罕见事件,小目录全读比维护失效协议便宜;PG/FDB 读共享存储里的同一份别名表);轮换/`rm` 只影响**新**挂载(已建立的旧名连接持续到老连接断开,与只读门同语义);被轮换掉的旧名落到按需新建的空命名空间,触不到真实树,故泄露无害;SQLite 档 `meta/views/` 是普通文件,HA 备机随控制面库(`api_keys.db`/`refs.db`)手工同步,`grovefs view`/`import` 在存储机执行(操作服务端本地文件);PG/FDB 档别名在共享存储(`grovefs_control`/集群 `ctl`),带同一 `--meta-store` 从任意可达节点治理、无本地文件要同步(§6.2/§6.3)。拒因**逐字节入协议**(`M_OPEN` 应答的 data 段带服务端原文),故远端拒挂真实名的回显与本地完全一致——上面两处 `view 'team' is governed by an alias …` 就是真实运行输出;同样精确的还有 token 不符、api key 缺失/无效/超范围/已撤销、视图绑定 key 拒开其它视图这些拒因(对接**老服务端**时 data 段为空,客户端才退回通用文案)。**轮换是名字治理(泄露止损),不是安全边界**:硬隔离仍靠 token+key——轮换只让旧的公开名不再通向真实树,并不能撤销一个已持有 token 者的既有访问。

### 3.4.1 fork 视图:独立元数据树,结构可写、内容只读(`grovefs view fork`)

派生视图是只读的「显示透镜」,别名是全树可写的「公开名」;两者之间还缺一档真实需求:**能整理目录树、但不能上传内容,且删掉自己的名字绝不伤基视图**。`grovefs view fork` 就是这一档——把基视图的目录项**复制**成一份独立元数据树(SQLite 是 `<meta-dir>/meta/<name>.db`;PG 是新的 `grovefs_v_<hex>` schema;FDB 是新子空间),保留 inode 与硬链接,内容字节一个不拷、经跨视图账本共享;定义仅一行 `fork <基视图>`:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --meta-dir D fork --name V --base B` | V=新 fork 名,B=本地 SQLite 物理基视图名 | 建 `D/meta/V.db`(复制 B 的目录项)并写 `D/meta/views/V.layout`(内容仅 `fork B`);此后两边各自演化,基视图之后新增的名字不会注入已建 fork |
| `… fork --name V --base B --empty`(MCP:`fork_view` 传 `seed:false`) | `--empty`=只建根、不复制目录项 | 仍写 `fork B` 定义、挂载语义完全一致,但树里只有根行;随后用「接枝」(`graft`,下一小节)按需装入挑选过的子集——字节依旧零复制 |

```bash
$ grovefs view --meta-dir $BASE fork --name lab --base team
forked view 'lab' from base 'team': 42 entries, 37 shared object(s)
note: mounts of 'lab' get an independent tree — structure is writable, content is refused with EROFS (upload bytes belong to base 'team')
note: `grovefs view rm --name lab` deletes this fork's own tree only; shared objects stay while any view (including 'team') still holds them
$ grovefs view --meta-dir $BASE list
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
$ grovefs --home $BASE mount --view lab -m /mnt/lab &
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
$ mkdir /mnt/lab/docs/2026/review      # 组织目录树:放行
$ mv /mnt/lab/docs/2026/a.md /mnt/lab/docs/2026/review/a.md   # 改名:放行(只改 fork 自己的元数据)
$ echo x > /mnt/lab/docs/2026/new.md   # 上传字节:拒绝
bash: /mnt/lab/docs/2026/new.md: Read-only file system
$ grovefs view --meta-dir $BASE rm --name lab
removed fork view 'lab' of base 'team' (its own metadata tree is gone; content bytes stay while any view — including 'team' — still holds a name for them)
```

语义与守卫:`view fork` 拒绝 `name == base`、拒绝目标名已有定义或已有物理 db、拒绝基名本身有定义(派生/别名/fork 不能当基)、拒绝基名在该 store 里没有物理树(三档同一句话:SQLite 找 `meta/<base>.db`、PG 找 `grovefs_v_<hex>`、FDB 找子空间);`alias` 拒绝指向 fork(fork 是树身份,不是公开名——给它发 key 用 `apikey create --view lab`),`import` 拒绝 fork 目标。**结构面可写、内容面只读**:`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/空对象 `create` 放行,`write`/`truncate` 一律 `EROFS`;本地(`--home`)与 `meta-server` 两条路径同判(后者靠 `M_OPEN` 回包第 3 个标志字节)。删除只摘 fork 自己的 `node` 行:共享对象由跨视图账本判定,只有**最后一个持有视图**摘名后才真正回收;`view rm --name <fork>` 删定义 + 自己那棵 `<name>.db`(+WAL/SHM)并 `refs rebuild` 重算账本,基视图与字节分毫不动。诚实边界:内容写门是**客户端门**,绕过 FUSE 直连 data-server 的 raw 协议客户端仍能写字节——硬边界仍是服务端 token+API key(§6)。可见性:`view list` 同时列出定义槽(派生/别名/fork)与**裸基视图**(无定义的物理树);`apikey list --view V` 只列能开 V 的 key(纯别名也匹配绑定到真实名的 key),并汇总未绑定 key 数。e2e 见 `fork` 套件(70 条断言,含 `--empty` 空种子与 `view graft` 接枝)。

#### 定制 fork 视图:从建到删的七个动作

fork 能定制的只有两样:**树**(复制来的目录项,可任意整理)和**名字**(视图名、挂载点、key 绑定);内容字节永远只读、与基视图共享。按一次真实整理走一遍:

| # | 动作 | 命令 | 结果 / 判据 |
| --- | --- | --- | --- |
| 1 | 选基与命名 | `view list` 确认基是**裸基视图**(没有派生/别名/fork 定义) | fork 复制的是**该 store 里的物理树**(SQLite 文件行 / PG schema / FDB 子空间);派生名、别名、另一个 fork 都会被拒;名字不能等于基名,也不能撞已有 db |
| 2 | 建树 | `grovefs view --meta-dir D fork --name F --base B` | 100,002 条实测 **0.65s**;产出 `meta/F.db`(**12 MB**)+ `meta/views/F.layout`(13 B,内容只有 `fork B`) |
| 3 | 挂载 | `grovefs --home D mount --view F -m /mnt/F` | 日志出现 `fork view 'F' of base 'B': independent tree — structure writable, content read-only (EROFS)` |
| 4 | 整理骨架 | `mkdir`/`mv`/`ln -s`/`rm`/`chmod`/`chown`/`touch`(空文件) | 只改 `meta/F.db`;`B` 的目录项与字节分毫不动 |
| 5 | 决定怎么「藏」 | 在 fork 里 `rm`(永久,改树) 或挂载时叠 `--layout`(只影响本次挂载,不改树) | 删行后审计上看不出「藏过」;`--layout` 可让同一个 fork 在不同客户端呈现不同形状 |
| 6 | 发凭据 | `grovefs apikey --meta-dir D create --label team-a --view F [--ro]` | key 只对 F 生效;即使 rw,内容写仍 `EROFS`(只有结构可写) |
| 7 | 换代 / 删除 | 新快照 fork 到 `F2` → 切挂载 → `view rm --name F` | `rm` 删 F 自己的树 + 定义并 `refs rebuild`;共享字节按账本回收(§2.3) |

一次完整的整理(第 4–5 步)可以照抄:

```bash
# 4) 重排骨架:归档、分组、软链、清理
mkdir -p /mnt/F/archive/2025
mv /mnt/F/raw/*.log /mnt/F/archive/2025/     # 改名/移动:只动 fork 的 node 行
ln -s archive/2025 /mnt/F/latest             # 符号链接:放行(目标文本不改写)
rm -rf /mnt/F/tmp                            # 删名字:只影响 F,基树照旧
# 5) 只想改「这次挂载看到什么」而不动 fork 的树:叠一层客户端规则
printf 'hide /tmp\nonly /archive\n' > hide-tmp.layout
grovefs --home D mount --view F -m /mnt/F2 --layout hide-tmp.layout
```

**能做到 / 做不到(一张表划清)**:

| 想做的事 | fork 上的做法 | 边界 |
| --- | --- | --- |
| 复制一份可自由整理的目录册 | `view fork` | 只复制目录项;字节共享,不复制 |
| 隐藏某些名字 | 在 fork 里 `rm` | 基树不受影响;但「藏过」这件事审计上看不出来 |
| 让不同团队看到不同形状 | 同一 fork 多挂载点各配 `--layout`;或再 fork 一份 | `--layout` 是客户端显示层,**不是安全边界** |
| 跟随基树的新增/改名 | **不支持**;重新 fork(蓝绿换名) | fork 是**时间点快照**(§2.4) |
| 在 fork 里上传新文件 | `touch` 空文件放行;写内容 `EROFS` | 内容门是客户端门,硬边界仍是服务端 token/key |
| 基视图在 PG / FDB 上 | `view fork` 同样可用 | PG 跨 schema `INSERT … SELECT`、FDB 复制 subspace;字节经共享账本,与 SQLite 同契约(`forksrv` 三档实证) |
| 把 fork 当公开名/别名 | 不行,`alias` 拒绝指向 fork | fork 是树身份;要公开名用 `apikey create --view F` |
| 迁到另一台机器 | `view export --view F` + `import --as-fork B` | 字节另行 `rsync blocks/`(§3.4.2) |
| 删掉 fork 连带回收字节 | `view rm --name F` | 只有账本归零的字节才真正回收;`--base` 对 fork 会被拒 |

**四个常见坑**:① `view rm --name F --base` 会被「名字被定义治理」拒掉——fork 不是基视图,删它**只能**不带 `--base`;② 重建快照会丢掉 fork 侧的全部整理,所以换代用新名字 `F2` 再切挂载点,不要原地 `rm` 后重建;③ 每个 fork 都是一份完整目录册(100k 条目 12 MB),N 个视角就是 N × 12 MB,不是几十字节;④ 别手改 `meta/F.db`——账本 `refs.db` 只在 `fork`/`rm`/`import` 时成套更新,手改出的 `data_ref` 没有 holder 记录,回收判定与 `view export` 的引用计数都会与实际不符。

**关于「基视图的身份」:视图的身份就是名字**(SQLite 是 `meta/<名>.db` 的文件名,PG 是 schema 名),**没有独立 UUID**。`views/F.layout` 里的 `fork B` 只是一个名字引用,用于 `view list` 展示、`view rm --base` 的依赖守卫和挂载提示;挂载 fork 时读的**只有它自己的 `meta/F.db`**,运行期不回查基树。由此:

- 基树在文件层被改名/移走,已建的 fork **照常挂载、内容不变**(它是自包含快照);变的是 `fork B` 这行定义指向了一个不存在的名字,`view list` 不再列出 B 的树。
- 受支持的「改对外名」方式是**别名轮换**(`view alias --retire <旧名>`):真实名不动,fork/派生/别名照常;不要手 `mv meta/<基>.db`。
- 依赖守卫按名字工作:只要还有 `fork B`/`base B`/`alias B` 引用 B,`view rm --name B --base` 就拒删。
- 跨视图账本 `refs.db` 是**按对象计数**(`holders(data_ref, holders)`)、与视图名无关,改名不会算错引用;真出了偏差用 `grovefs refs --meta-dir D rebuild` 重算。

#### 选择性接枝:只装你要的那部分,并顺手改名(`graft`)

整份 fork 是「全量快照」;而真正交付一份整理好的视图时,常见的是**只要基视图里的一小部分,而且要改名**(`data/4f2c-9a10.fits` → `M31_2025.fits`)。两个动作组合即可,仍然**不复制字节**:

1. **空 fork**:`fork_view` 传 `seed:false`(CLI 形态 `view fork --empty`)——只建出 `meta/F.db` 的根,不搬 10 万行目录项。
2. **接枝**:把一份**计划**(纯文本,一行一个条目)喂给 `graft_view`(CLI 形态 `view graft --map PLAN`,文件或 `-` 读 stdin),按需把基视图的条目接进 F 的任意目录,并逐条改名。

**目标可以是 fork,也可以是裸基视图**——判据只有一条:**它得拥有自己的 `node` 表**。派生透镜和纯别名只是别人的显示层,会被按角色拒掉(拒绝原文见下文)。

计划格式(外部程序生成最方便,也可直接管道喂):

```text
# 只有「整行以 # 开头」才是注释;行尾 # 不是注释(会被当成文件名的一部分!)
src/data/4f2c-9a10.fits	by_target/M31_2025.fits
src/docs/readme.md	src/docs/README.md
src/calib
```

四条语义(每条都对应一次实测):

| 写法 | 落点 | 说明 |
| --- | --- | --- |
| `源`(无 TAB) | 目标视图里的**同一个完整相对路径** | **不是 basename**:`src/docs/readme.md` 不会变成 `/readme.md`,而是在目标里重建 `src/docs/` 两级父目录 |
| `源<TAB>目标` | 就是「目标」 | **改名的唯一手段**:TAB 右边 basename 不同就是改名,可带目录(缺的父目录自动建,0755) |
| `--dest D` | 上面两种落点**都**再套一层 `D/` | 给每一行加前缀(**有 TAB 的行也加**);只影响「放哪」,不改名字 |
| `--skip-existing` | 已存在的同名条目**原地跳过** | 已存在的目录**合并**、同名文件**跳过**,其余照搬;不带它时任何冲突都**拒掉整份计划** |

- 路径都相对各自视图的根:前导/尾随 `/` 与 `.` 可省,**`..` 直接拒**(计划是数据,不许越界)。
- **源是目录 → 连整棵子树一起搬**(名字保持);源是文件/符号链接 → 只搬它自己。
- 计数里的 `entries` **包含自动补建的父目录**:计划 4 行也可能回显 `13 entries (8 directories)`(见下文实测),不是虚报。
- 整份计划一个事务落地:任何一行出错(路径打错、目标已存在、目标角色不对)**一行都不落**。
- 字节零复制:接进来的名字与源视图指向**同一个对象**,`refs.db` 多记一个 holder;两边各自删名字,最后一个持有者才真回收(§2.3)。硬链接关系也保留:源里共用 inode 的两个名字接进来仍是同一个 inode(`nlink=2`)。

#### 接枝实跑:五种形态 + 拒绝原文(回显逐字)

准备一棵基视图(用 `import` 造出一个 `/src` 前缀)和一棵空 fork:

```text
$ grovefs import --meta-dir /tmp/gv --view team --source /tmp/gv/src
created BASE view 'team' (target was absent); imported 6 object(s)
imported into view 'team' under /src: 6 dir(s), 6 object(s) (6 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed

$ grovefs view --meta-dir /tmp/gv fork --name lab --base team --empty
forked view 'lab' from base 'team' as an EMPTY tree: root only, nothing copied
note: place the subset you want with `grovefs view graft --name lab --src team --map plan.tsv` (or the `graft_view` tool, same implementation) — a directory source brings its subtree; each destination basename is the rename
note: 'lab' has the same fork semantics — structure writable, content writes (write/truncate) refused with EROFS, bytes shared through the cross-view ledger
```

**形态①:改名 + 搬子树 + 默认 `--src`(取 fork 定义里的基名)**

```text
$ cat plan.tsv
# 注释行:整行以 # 开头才被忽略(行尾 # 不是注释)
src/docs/2026/a.md	notes/A.md
src/docs/2026/readme.md	src/docs/2026/README.md
src/calib
src/data/4f2c.fits	by_target/M31.fits

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv
grafted 13 entries (8 directories) from 'team' into 'lab' at /: 5 shared object(s), 0 skipped
note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
```

4 行计划为何是 13 条?——**父目录是替你补建的**:`/src`、`/src/docs`、`/src/docs/2026`(3 个)、`/notes`(1)、`/by_target`(1)、`/src/calib` 整棵子树(`calib`+`x`+`y` = 3)= **8 个目录**;文件 5 个(`A.md`、`README.md`、`c1.txt`、`c2.txt`、`M31.fits`)。用 MCP 的 `list_view_tree` 可直接核对(路径是元数据,不碰字节):

```text
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"doc","version":"1"},"capabilities":{}}}' \
             '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"lab","depth":9}}}' \
  | grovefs mcp --meta-dir /tmp/gv
 /by_target/M31.fits
 /notes/A.md
 /src/calib/x/c1.txt
 /src/calib/y/c2.txt
 /src/docs/2026/README.md          # 只列文件;dirs=8 files=5
```

注意 `/src/docs/2026/b.md` 不在里面——计划没点它,接枝**只搬点名的条目**(这正是它与整份 fork 的区别)。

**形态②:重跑冲突 → `--skip-existing` 幂等合并**

```text
$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv
error: 'A.md' already exists in the destination — rename it in the plan or pass --skip-existing
$ echo $?
1

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --map /tmp/gv/plan.tsv --skip-existing
grafted 0 entries (0 directories) from 'team' into 'lab' at /: 0 shared object(s), 5 skipped
```

→ 布置脚本可以**无脑重跑**:冲突只跳那一条,其余照搬,退出码 0。

**形态③:`--dest` 是前缀(有 TAB 的行也加),而「无 TAB 行保留完整相对路径」**

```text
$ cat plan2.tsv
src/docs/2026/b.md
src/docs/2026/b.md	custom/kept.md

$ grovefs view --meta-dir /tmp/gv graft --name lab --src team --dest staging --map /tmp/gv/plan2.tsv
grafted 7 entries (5 directories) from 'team' into 'lab' at /staging: 1 shared object(s), 0 skipped

$ grovefs mcp ... list_view_tree {"view":"lab"}        # 实测落点
 /staging
 /staging/custom
 /staging/custom/kept.md
 /staging/src
 /staging/src/docs
 /staging/src/docs/2026
 /staging/src/docs/2026/b.md
```

两行都吃了 `--dest staging`:第 1 行(无 TAB)落点 = `staging/` + **源完整路径** `src/docs/2026/b.md`;第 2 行(有 TAB)落点 = `staging/` + 计划里写的 `custom/kept.md`。**想让同一份数据换个名字,只能靠 TAB 列**。

**形态④ `--map -`:外部程序生成计划,整份管道喂**

```text
$ printf 'src/data/4f2c.fits\tvia_stdin.fits\n' | grovefs view --meta-dir /tmp/gv graft --name lab --src team --map -
grafted 1 entry (0 directories) from 'team' into 'lab' at /: 1 shared object(s), 0 skipped
```

生成的脚本可以直接 `... | grovefs view graft --name F --map -`,不必先落盘。

**形态⑤:目标是裸基视图(反向接枝)——必须显式 `--src`**

基视图没有可推断的「来源基名」,省略 `--src` 会被直接拒;给了 `--src lab` 就能把 fork 里整理好的名字搬回基视图:

```text
$ printf 'src/docs/2026/b.md\n' | grovefs view --meta-dir /tmp/gv graft --name team --map -
error: 'team' is a base view — pass --src to name the view its entries come from

$ printf 'notes/A.md\tfrom_lab/A.md\n' | grovefs view --meta-dir /tmp/gv graft --name team --src lab --map -
grafted 2 entries (1 directory) from 'lab' into 'team' at /: 1 shared object(s), 0 skipped
note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
note: the destination is a base view — content stays writable there; re-running the same plan with --skip-existing merges into what is already there
```

注意最后一条 note 会**根据目标类型变**:目标 `fork` 说内容只读(EROFS),目标 `base` 说内容可写——同一个工具不会对你号错脉。

**拒绝原文(全部实测,拿去对日志用)**

| 情形 | 拒绝原文(逐字) |
| --- | --- |
| 目标已存在同名(未加 `--skip-existing`) | `error: 'A.md' already exists in the destination — rename it in the plan or pass --skip-existing` |
| 目标名不存在 | `error: no tree for graft destination 'ghost' in /tmp/gv (local files) — create it first (…)` |
| 目标/源是派生透镜 | `error: graft destination 'lens' is a derived lens: it renders another tree read-only and owns no nodes — use a base view or a fork` |
| 目标/源是纯别名 | `error: graft destination 'pub' is an alias of 'team' — use 'team' (an alias has no tree of its own)` |
| 计划为空(只有注释/空行) | `error: empty graft plan — one '<src>' or '<src>TAB<dst>' line per entry (blank lines and '#' comments are ignored)` |
| 源路径在当前源视图里不存在 | `error: 'src/docs/nope.md' does not exist in view 'team'`(整份计划**一行不落**) |

**同一个能力的 MCP 入口**(`graft_view` 的 `map` 直接传计划文本,`\t` 是 TAB):

```text
[graft_view] {"name":"lab2","map":"src/docs/2026/a.md\tby_target/M31_a.md\nsrc/calib"}
← grafted 8 entries (5 directories) from 'team' into 'lab2' at /: 3 shared object(s), 0 skipped
  note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
  note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
```

**能/不能**

| 想做的事 | 能不能 | 边界 |
| --- | --- | --- |
| 接不同源视图(甚至另一个 fork)的条目 | **能** | 一次调用一个 `--src`;多个源就多次调用,各自原子 |
| 把同一对象接成多个名字 | **能** | 硬链接语义:同一 `data_ref`,账本多记 holder |
| 靠接枝上传/修改字节 | **不能** | 目标若是 fork,内容仍 `EROFS`;直接接进基视图则基视图照常可写 |
| 拿派生/别名当源或目标 | **不能** | 它们没有自己的 `node` 表,按角色拒(上表原文) |
| PG 后端 | **不能**(与 fork 同因) | 权威树在 PG,没有可读的 `meta/<名>.db` |

CLI(`grovefs view graft --name F --map PLAN|-`)与 MCP(`graft_view`)共用同一实现:参数语义与拒绝文案逐字节相同,只有入口不同。

### 3.4.2 单视图元数据导出/导入:换机器、换系统迁移(`grovefs view export / import`)

GroveFS 的元数据**不绑定硬件**:`node` 行只存 `data_ref`(对象 key),对象路径是 key 的纯函数(`blocks/<FNV1a(key)%256 两hex>/<key>`),没有绝对路径、设备号、主机名或宿主 inode;GroveFS 自己的 `ino` 也在 DB 里。所以整库/整目录搬到另一台机器可直接用——前提是**元数据与内容字节一起走**。想按视图、跨机器、可选择性迁移时,用 `view export` 把一棵树写成宿主无关 JSONL,再在目标存储根 `view import`:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --meta-dir D export --view V --out F [--force]` | D=源存储根,V=**物理基视图或 fork** 名,F=输出文件 | 写表头(format/version/kind/定义原文/节点与 blob 计数)+每行一个 node(id/inode/名字/is_dir/data_ref/link_target/mode/uid/gid/三个时间);**不含字节**;`--force` 才覆盖已存在的 F;挂载中也可导出(只读快照) |
| `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]` | D=目标存储根,V=新基视图名,F=上面导出的文件,B=目标机上已存在的裸基视图 | 在 `D/meta/V.db` 重建整棵树(保留 id/inode/硬链接/权限/属主/符号链接),完成后重建 `refs.db` 账本;已有树必须 `--replace`;`--as-fork B` 额外写入 `fork B` 定义(恢复“结构可写、内容 EROFS”) |

```bash
# 源机(挂载中也可;停写后更稳)
$ grovefs view --meta-dir /data/src export --view team --out /tmp/team.jsonl
exported view 'team' metadata: 42 node(s), 37 blob reference(s) -> /tmp/team.jsonl
note: metadata only — copy the store's `blocks/` tree (the whole data root) as well
# 只拷内容字节(元数据已在上面的 JSONL 里)
$ rsync -a -H /data/src/blocks/ /data/dst/blocks/
# 目标机
$ grovefs view --meta-dir /data/dst import --view team --in /tmp/team.jsonl
imported BASE view 'team' from /tmp/team.jsonl: 42 node(s), 37 blob reference(s); cross-view ledger adjusted
$ grovefs --home /data/dst mount --view team -m /mnt/team     # 硬链接仍是同一 inode,权限/时间原样
```

**同一对命令也跨后端**（SQLite ⇄ FDB，不需要第三个工具）：一棵 SQLite 树导出的 JSONL 正是一个 subspace 导入的输入，反向同理——`view --meta-store fdb://… import` 在集群里**开出这个名字的 subspace**（`view create` 之外唯一的建树入口），`view --meta-store fdb://… export` 把一个 subspace 写成同一格式的 JSONL。字节仍各走各的对象存储（`rsync -a` 数据根，或让 data-server 直接服务同一批对象）：

```bash
S=/data/sqlite-store; C=/data/cluster; CL=fdb:///path/to/fdb.cluster
grovefs view --meta-dir $S export --view team --out /tmp/team.jsonl
rsync -a -H $S/blocks/ $C/blocks/                        # 内容字节另走,元数据已在 JSONL 里
grovefs view --meta-store $CL --meta-dir $C import --view team --in /tmp/team.jsonl
# 回滚方向:同一个集群 dump 再搬回一棵 SQLite 树
grovefs view --meta-store $CL --meta-dir $C export --view team --out /tmp/team2.jsonl
grovefs view --meta-dir $S import --view team_rolledback --in /tmp/team2.jsonl
```

往返不重写任何行:导入后从 subspace 再导出的 dump 与源 dump **逐行相同**(`diff` 判据),所以 id/ino/硬链接/mode/属主/三个时间/symlink 文本都是原值带过去的,而不是被重新分配;导入时两个分配器水位抬到导出最大值(PG 档 `setval` 的同位动作),迁移后新建的节点 id 严格大于导入侧的最大 id,不会撞号。共享字节账本(FDB 是集群里的 `refs` subspace)在同一事务里随导入/`--replace` 更新,失败则整体不生效——一棵树要么原样、要么完整是新树(这条是单事务语义;`--meta-store fdb://…` 且 dump 装不下一个事务时改为自动分批、根行最后写,见 §6.3.4)。迁移窗口内**源存储不被写入**,原树仍在,回滚就是同一对命令反向跑一遍。PG 档没有这条通道:它的权威树在 schema 里,走 `pg_dump`/逻辑复制(§15.9)。

e2e:`fdb` 套件第 12 节(SQLite→subspace→SQLite 全链,含逐行 `diff`、水位抬升、守卫与源不动)、`migrate` 套件(`MIGRATE_BACKEND=fdb` 档)、`import` 套件(`IMPORT_BACKEND=fdb` 档,把这条拒绝之外的替代路线端到端跑通)。

**导入的原子性**:先在 `D/meta/.V.import-<pid>-<时间>` 建临时树,全部行与校验通过、WAL 合并后才 `rename` 到 `D/meta/V.db`——坏文件/悬挂父节点/计数不符/失败的 `--replace` 都不会破坏旧树(旧树要么原样、要么完整换成新树,没有中间态)。

**守卫**:虚拟名(别名/派生)没有自己的树,`export` 直接拒绝并提示导出其物理基;输出文件已存在需 `--force`;导入目标已有定义槽先 `view rm`;表头 format/version 不符、根节点不是 `id=1` 的目录、悬挂父节点、表头计数与正文不符均整单拒绝;`--as-fork` 要求基视图在**目标机上**已存在、且基名本身没有定义。

**迁移的拓扑约束(数据分布,不是硬件绑定)**:

- 多盘 `--data-dir`:对象按 `FNV1a(key) % root 数量` 选盘,目标机 root **数量与顺序要一致**;也可合并成单 root(把各盘 `blocks/` 合并)。
- 跨 server 条带(`s<N>:...`)：第 i 块在 `servers[i % N]`，目标必须保留 **N 个 data-server 及顺序**；台数不符直接被拒（报 `laid out across N servers but this mount has M; refusing to remap`），**顺序不符会被 stat 检出**：非空条带对象的 `{uuid}#0` 必在成员 0，落在别的成员上就报“服务器对、顺序不对”而不是出一个 0（否则整棵树会被读成一堆空文件）；`read` 路径本身仍只按分块定位，不做这层回溯（不会读到别人的数据，对象名带全局 `#i`）。**换机器 / 换 IP 本身不是约束**：成员按下标寻址，在 `-o data=` 里**原位替换**那一项就能自动识别，元数据不改、已写对象不需 `rebal`（轮转放置 `i % N` 只决定新写入对象落哪台）。
- HA `epoch`(每个视图的 `ctrl` 表):整目录拷贝会带过去,迁移后旧主必须停;`view export/import` 与 `view fork` 有意不复制 `ctrl` 与 `locks`。
- PG 后端不能用文件拷贝,整库级备份用 `pg_dump`/逻辑复制;但 `view export/import` 三档通用——PG 的树在 schema 里,导出走 REPEATABLE READ 只读快照、导入单事务重建(§15.9,`xfer` 套件 PG 档实证)。
- `uid/gid` 是数值元数据(advisory),按数字保留;symlink 目标文本是用户数据,原样保留。
- 整目录冷拷时先停所有挂载与 `meta-server`;不能停机就 `sqlite3 <db> ".backup"` / `VACUUM INTO`,**不要裸拷运行中的 DB**(WAL)。
- **`st_ino` 随库走、迁机器不变**(`node.ino` 是 GroveFS 自己的编号),但 **`st_dev` 是 FUSE 每次挂载新分配的**:按 `(dev, ino)` 二元组去重的备份/同步工具在重挂或迁库后要重建映射,只按 ino 查(`find -inum`、`stat -c %i`)不受影响。
- **拷完怎么确认没漏**:对象名是 UUID 而非内容哈希,读侧没有校验环节,漏拷只能主动比——树上引用的键数(`sqlite3 D/meta/V.db 'SELECT COUNT(DISTINCT data_ref) FROM node WHERE data_ref IS NOT NULL;'`)对 `grovefs gc --meta-dir D --data-dir R` 输出里的 `matched`,`matched < refs` 就是有键没有对象。用 `rsync -a` 保 mtime(`gc --older-than` 靠它判新对象宽限),用过 `import` 的存储必带 `-H`(导入对象与源文件是硬链接)。

**多个基视图**:可建任意多个基视图(`init --view A` / `view create --view A`),各自一棵 `meta/A.db`,树、字节、权限、删除互不影响;`view list` 会列出全部裸基视图。`view export/import` 按视图进行,迁移时可以只搬其中一棵,也可以给不同基视图绑定不同 key(`apikey create --view A`)。

e2e 见 `migrate` 套件(SQLite 档 74 条断言;`MIGRATE_BACKEND=fdb` 把同一套断言对着集群复跑,81 条)。断言覆盖:挂载中导出/宿主无关性/虚拟名与覆盖守卫/跨绝对路径导入/硬链接与权限保真/原子替换/损坏输入拒绝/`--as-fork` 账本共享/多基视图独立(FDB 档「两个存储根」按 subspace 边界改判;`fork`/`--as-fork` 三档同契约,见 §15.9)。

**规模实测**(单机、默认 IO,`bash .build-bin/perf_migrate_scale.sh` 可复跑):迁移一棵 5052 节点、5000 个对象的视图——`export` 0.19s 产出 1.16 MB JSONL(是视图库的 1.16 倍,约 230 B/节点);`import` 到新存储路径 0.56s(含账本重建);挂载后 0.23s 枚举完 5000 个文件,内容逐字节一致、权限与属主保留。同一批文件当初注册进源视图(零拷贝导入)花了 83s——那部分逐对象的存储开销完全不在导出/导入路径上。**迁移代价由命名空间大小决定,不由数据量决定**;字节另走 `rsync -a -H blocks/`。

### 3.4.3 删整个基视图:连字节一起回收(`grovefs view rm --base`)

不带 `--base` 的 `view rm` 只删**定义槽**(派生/别名/fork),从不动数据;要连一棵**物理基视图**从头到尾删掉——元数据树消失、跨视图账本重建、**无任何其他视图仍持有的内容字节从 `blocks/` 被回收**——用 `--base` 把这件事显式说出来(避免误删):

| 命令 | 效果 |
| --- | --- |
| `grovefs view --meta-dir D rm --name V --base [--data-dir R]` | 删 `D/meta/V.db`(+WAL/SHM)→ `refs rebuild` 按现存树重算 holder → 对 V 树里**无其它视图持有**的 blob 逐个 `backend.remove`;`--data-dir` 指向真正存放 `blocks/` 的根(默认元数据根 `D`,与 `import --data-dir` 同一约定,用于 meta-server/data-server 分家部署);回显如实给出 freed/held/not-found/failed 计数 |

```
$ grovefs view --meta-dir .home rm --name alice --base
removed base view 'alice': 2 node(s) gone, 2 blob reference(s) dropped, 2 object(s) freed from .home/blocks, 0 still held by other views
```

**四重守卫**(任一命中即拒,树分毫不删):① 该名字被定义治理(是派生/别名/fork 的定义槽)——先跑不带 `--base` 的 `view rm` 删定义;② 仍有别名/透镜/fork **定义**经此基解析(否则删基会让那棵挂载悬空),报 `view 'lens1' is governed by a definition over base 'alice' — remove it first`;③ 本机 `/proc/mounts` 里有 `grovefs:<V>` 活挂载,报 `view 'alice' is mounted here at …  — unmount it first`(**其他机器**对同一 meta-server 的挂载本机看不到,须自行先停);④ **PG 档走同一条命令**(`view --meta-store postgres://… rm --base`):基树是一个 schema,`remove_view` 在一个事务里逐 key 减 `grovefs_refs.holders`(归零即 DELETE)、再 `DROP SCHEMA … CASCADE`,提交之后才按返回的 `unbooked_refs` 逐个回收 `blocks/` 字节——没有 SQLite 档“先删 db 再重建账本”的中间窗口。**绕过 CLI 手工 `DROP SCHEMA` 不算删除**:账本是无 view 列的全局表,裸 drop 只删树不销账;更要紧的是该视图订过哪些对象**只有那个 schema 知道**,ref 清单随 schema 一起消失,字节从此没有记录可回收(§15.8 的 E 段实测)。

**最后一个命名者死才释放**:fork 的 blob 引用来自基视图自己的 key、账本 hold>1,删基时那些仍被 fork 命名的字节**一个都不能动**;只有当基与所有依赖定义都消失、账本再无持有者时字节才真正回收。条带 key `s<N>:<stripe>:<uuid>` 活在 data-server 上,普通本地根无法回收——诚实计入 failed 并跳过,绝不半删。**MCP 边界**:`remove_view` 是控制面 only(等价不带 `--base` 的 `view rm`),删整基视图只经 CLI——接口从不碰数据字节。

e2e 见 `viewrm` 套件(42 条断言:种树/挂载守卫/多重守卫/共享字节删失/最后命名者死后回收/分家根 `--data-dir`/删后重挂空树语义);PG 档的同一序列见 `.bneck/pg_view_teardown.sh`(44 条断言:真 FUSE 挂载写入 → schema/账本/字节三方对账 → 事务性删除 → 二次删除被拒 → out-of-band drop 后 `refs rebuild` 的两种方向 → 残留恰好等于不可回收的那部分)。

### 3.5 智能体定制数据列表:MCP 接口(`grovefs mcp`)

视图治理(§3.2/§3.4/§3.4.1)同时是一套 **MCP(Model Context Protocol)工具**。`grovefs mcp` 在 stdio 上说换行分隔的 JSON-RPC 2.0(实现 2025-06-18 协议的常用方法面:生命周期 `initialize`/`ping`、工具 `tools/list`/`tools/call`、资源 `resources/list`/`resources/templates/list`/`resources/read`、提示 `prompts/list`/`prompts/get`、补全 `completion/complete`、日志 `logging/setLevel` 与服务端 `notifications/message`;客户端通知一律静默不作答),任何 MCP 客户端(Claude Desktop、IDE 智能体等)把它当子进程拉起,智能体即可**通过接口自动定制数据列表**:查视图、**列出某视图真实呈现的目录清单**(派生视图经其透镜渲染,还能不落盘地预览一副待发布规则会长成啥样)、用内联规则文本发布只读派生透镜(四动词同 `--layout`,规则以文本随行、不落文件)、轮换公开名、删定义、**发布前把整副定义走完全部治理守卫做干跑校验(不写盘)**。与 `grovefs view` 子命令共用同一套库函数(`grovefs::mcp`)——校验、拒绝文案、落盘动作逐字节相同,一个契约、两个入口,智能体做不出 CLI 做不出的事,也绕不过任何守卫。**探查只读**:`list_view_tree`/`get_view` 以只读方式打开本地 `<meta-dir>/meta/<view>.db`、经与挂载同一副 `LayoutMeta` 透镜渲染目录清单(不挂载、不读对象字节;远端/PG 后端的树不在此遍历);回包里的 `size` 是元数据字段而非字节数(目录 `4096`、文件 `0`、软链为 target 长度),需真实字节请挂载后 `stat`。**控制面 only**:不挂载、不写数据。默认信任能启动此进程的调用方(= 已持有存储根的操作系统级权限),跨网鉴权仍由服务端 token / API key 负责;当它被 front 给多个不同调用方时,可用 `grovefs mcp --require-api-key` **可选地**为接口上 API key 门(见 §3.5.1)。下面全部回显取自真实运行:

```
# 1) 存储机:建一个物理视图(智能体稍后要在它的树上定制列表)
$ grovefs --home $HOME_DIR init --view team --manifest team.manifest
initialized BASE view 'team' at /data/xy/grovefs/.demomcp/home/meta/team.db
applied seed manifest .demomcp/team.manifest

# 2) MCP 客户端配置里登记 stdio 服务器(以通用 mcpServers 写法为例):
#    "grovefs": { "command": "grovefs", "args": ["mcp", "--meta-dir", "/data/xy/grovefs/.demomcp/home"] }
$ grovefs mcp --meta-dir $HOME_DIR       # 客户端实际这样拉起:stdin 进帧、stdout 出帧

# 3) 智能体侧 tools/call 与工具结果文本(原样回显):
[list_views]   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{
  "db_path": "/data/xy/grovefs/.demomcp/home",
  "defined": [],
  "physical": [
    "team"
  ]
}
[preview_rules] "rules": "bog /x"                    # 先发后落:非法动词在发布前被抓住
bad layout rules: line 1: unknown layout verb 'bog'
[define_derived_view] {"name":"release","base":"team","rules":"# public release: no private photos\nhide /photos/private"}
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: mounts of 'release' are read-only (virtual shape) — write to base view 'team'
[define_derived_view] {"name":"team","base":"team",...}                     # 名与基同名
isError → derived view name must differ from its base      # 与 CLI 逐字节同文案(治理失败按 MCP 惯例回在结果内)
[list_views] 再次                                    # 结构化 JSON 回执,字段: name/kind/real_view/规则计数
view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)   # CLI 对照:`grovefs view list` 同文案

# 4) 用户侧:直接挂智能体定的名(无 --layout、无本地规则文件)
$ grovefs --home $HOME_DIR mount --view release -m /mnt/release &
derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ ls /mnt/release
docs  photos

# 5) 探查与预览(只读元数据,不挂载、不碰数据字节):
[list_view_tree] {"view":"team"}              # 先摸清真实路径:智能体据此写规则而非猜
{"kind":"physical","rendered_from":"team","dir_count":4,"file_count":3,"truncated":false,"entries":[
  {"path":"/docs","kind":"dir"},{"path":"/docs/2026","kind":"dir"},{"path":"/docs/2026/b.md","kind":"file"},
  {"path":"/docs/a.md","kind":"file"},{"path":"/photos","kind":"dir"},{"path":"/photos/private","kind":"dir"},
  {"path":"/photos/private/x.jpg","kind":"file"}]}
[list_view_tree] {"view":"release"}           # 经已发布透镜渲染:显示路径 /docs /docs/2026 /docs/2026/b.md /docs/a.md /photos(/photos/private 被规则剪除,与挂载所见一致)
[list_view_tree] {"base":"team","rules":"only /docs"}   # 不落盘预览一副待发布规则:仅 /docs 子树呈现,不写任何 .layout
[validate_definition] {"name":"release","base":"team","rules":"hide /photos/private"}   # 发布前干跑:把 name+base+rules 走完整套治理守卫,不写盘,回显确切判定
{"accepted":true,"dry_run":true,"kind":"derived","hide":1,"only":0,"reloc":0,"would_replace_existing_definition":false,"base_view_db_present":true,"note":""}
[get_view] {"name":"release"}                 # 读回单个透镜原始规则,供“读—改—写”迭代
{"kind":"derived","base":"team","hide":1,"only":0,"reloc":0,"definition":"base team\nhide /photos/private","mount":"read-only (virtual shape; writes go to the base view)"}
```

十个工具与参数:治理八个——`list_views`(可选 `db_path`)、`define_derived_view`(`name`/`base`/`rules`,后者为内联规则文本)、`alias_view`(`name`/`real`/可选 `retire` 原子轮换)、`fork_view`(`name`/`base`,可选 `seed`——`false` 只建一个空树根,把“拷不拷目录项”交给调用方;把基视图目录项复制成独立元数据树——结构可写、内容 EROFS)、`graft_view`(`name`/`map` 计划文本,可选 `src`(缺省取 fork 定义的基名)/`dest`/`skip_existing`——把别的视图里点名的条目(源为目录则连整棵子树)接进目标视图的任意位置,逐条改名,字节零复制、整份计划一个事务),`remove_view`(`name`,删 fork 时连同其自己的树)、`preview_rules`(`rules`,干跑校验动词语法不写盘)、`validate_definition`(`name`/`base`/`rules`,发布前把整副定义走完 define 的全部治理守卫——非法名、名与基同名、遮蔽物理 db、覆盖纯别名、规则语法——拒绝时回与真实发布逐字节相同的文案、接受时回 JSON 判定,绝不写盘);只读探查两个——`list_view_tree`(`view` 或 `base`+`rules` 模拟,可选 `path`/`depth`/`max`/`db_path`;渲染挂载实际所见的路径清单,可经已发布透镜或不落盘模拟一副规则)、`get_view`(`name`,返回单个定义的原始 `definition` 文本与计数)。同一套治理再以 MCP 其余常用方法面二次暴露,复用**完全相同的渲染器**,故资源/提示/补全与工具结果逐字节一致:**资源** `resources/list`/`resources/templates/list`/`resources/read` 把视图与手册作为可附着上下文——`grovefs://guide` 原样回分节操作手册、`grovefs://views` 回与 `list_views` 相同的快照、`grovefs://view/<name>/definition`(原样治理文本)与 `/tree`(挂载实见的清单,模板带 `{?path,depth,max}`,与 `list_view_tree` 的参数同义——大树可一次只附一个子树,被界剪断时 `truncated:true`);**提示** `prompts/list`/`prompts/get` 提供两副参数化治理剧本(`reshape-view` 取 `base`/`goal`、`rotate-alias` 取 `real`/`new`/`old`);**补全** `completion/complete` 按 `ref` 角色感知(`reshape-view.base` / `rotate-alias.real` 只给物理视图,`rotate-alias.new`/`old` 只给别名,其余给全部视图名);**日志** `logging/setLevel` 设会话阈值后,服务端在每次应答调用后按严重度(成功 `info`、失败 `warning`)推送 `notifications/message`,`data` 带方法与被操作对象(如 `tools/call list_views ok`)。面向运行时的元数据:每个工具带 `title` 与 `annotations`(`readOnlyHint` 标出 `list_views`/`preview_rules`/`validate_definition`/`list_view_tree`/`get_view`,`destructiveHint` 只给 `alias_view`/`remove_view`);四个回 JSON 的工具带 `outputSchema`,结果同时给 `structuredContent`;`initialize` 的 `instructions` 分七节(Discover/Reshape/Curate/Validate before publishing/Golden order/Beyond tools/Mechanics),客户端注入上下文即可照做。协议细节补一条:按 `2025-06-18`/`2025-03-26`/`2024-11-05` 协商,不认识的版本回 `2025-06-18`;`arguments` 也接受 JSON 字符串;缺参附 `(tool 'x' requires: …)`、工具名打错附 `did you mean '…'?` 与 `available:` 清单。语义:全部治理守卫照常适用(派生挂载只读、别名期间真实名拒挂、`--retire` 先写后删、fork 结构写放行/内容 EROFS;改定义热生效仍只在 `meta-server` 入口,本地挂载挂载时解析一次)。协议细节:通知(无 id)一律不作答;非法帧回 `-32700` 后继续服务;缺参与未知工具按 MCP 惯例回 `isError` 而非协议错误。`db_path` 默认取 `grovefs mcp --meta-dir`(回退 `--home`),也可逐调用覆盖。e2e 见 `mcp` 套件(77 条断言:握手能力面声明与协议协商/发现(`instructions` 逐个点名十工具且分节、`title`+`annotations`+`outputSchema`)/内联规则换形/路径探查与形状预览/发布前干跑校验/别名轮换/空 fork `seed:false` + `graft_view` 接枝/接枝守卫面 11 条拒绝原文(自接·空计划·`..`·根路径·基视图缺 `src`·目标或源为派生/别名·无名树·非法名·重名整批回滚·`skip_existing` 跳过)/CLI 同文案/资源枚举与读取(含 `grovefs://guide`、`grovefs://views`、树查询参数下钻与 `max` 截断)/提示实例化/按 `ref` 角色感知的视图名补全/`structuredContent` 与文本一致/未知工具建议与缺参提示/日志阈值→通知且 `data` 带工具名/协议错误路径)。智能体侧的接线手册（英文，便于国际化部署）已随仓库沉淀为一个项目 skill(`skills/grovefs-mcp/`):`SKILL.md`(拉起方式、十工具参数表、黄金工作流、逐个确切拒绝文案表、硬边界)、`reference.md`(完整方法面/鉴权门矩阵/错误码/落盘效果/CLI 对照)、`examples.md`(真实回显),并附两个脚本:`scripts/mcp_call.py`(零依赖 stdio JSON-RPC 客户端,回包分开 stdout/stderr,退出码 0 成功 / 2 协议错误 / 3 工具拒绝 / 4 传输问题)与 `scripts/smoke.sh`(在 `mktemp -d` 的临时存储根上跑完全链路并逐条断言,不碰真实数据)。

### 3.5.1 给接口和挂载点上把锁:API key 认证、scope 分区与调用审计(`grovefs apikey`)

同一个服务端常常要面对**多个不同调用方**——智能体运行时把一个 `grovefs mcp` 服务 front 出去,多台客户机经 LAN 挂同一个 `meta-server`——此时需要可撤销的凭据与归属审计。`grovefs apikey` 铸造 API key(**只存 SHA-256 摘要**,明文仅在 `create` 时打印一次,故 `api_keys.db` 泄露也换不出可用凭据),并给每个 key 打 **scope**(`--for admin` 是顶替数据库密码的治理端口凭据,见 §3.5.2)与 **owner**(`--owner <账号>`,`--for admin` 必填):`--for mcp`(只开治理接口)、`--for mount`(只开 `--require-api-key` 的 meta/data 服务端)、`--for admin`(只开治理端口 `meta-server --control-listen`,同时放行 mount+mcp)、`--for any`(**默认**,仅 mount+mcp——`any` 不会升格为 `admin`,scope 概念之前铸造的所有旧 key 都是这一档,故老部署一律无需改动)。两处入口共用同一份注册表(`<meta-dir>/meta/api_keys.db`),靠 scope 隔离:挂载凭据重放不到治理 API,智能体 key 也开不了数据面。挂载 key 还可带**两个访问控制维度**:`--view V` 把凭据**绑定到单一物理视图** V——用它 `M_OPEN` 别的视图在握手处即被 `E_AUTH` 拒(省略 `--view` 的是系统级 key,可开任意视图);`--ro` 标为**只读**——服务端在 meta 面对其逐写意图 op 拒 `E_NOTPERM`、data 面内容写拒 `EROFS`,并在 `M_OPEN` 回包提示客户端只读挂载(「只读」由服务端强制,不靠客户端自觉 `-o ro`);只读/读写**可原地切换**——`apikey set-ro --id <ID> --rw` 把只读 key 切回读写、不带 `--rw`(或带 `--ro`)切回只读,活挂载在下一次秒级复验 tick 就按新值放行/拒绝,无需重铸、无需重连(唯一例外:握手时已落内核只读的挂载要解除 OS 层caps仍需重挂,详见 3.5.1 的 set-ro 段;且切换只对经服务端的挂载成立,本机 `--home` 挂载无服务端凭据门、不受影响)。**治理面门**:`grovefs mcp --require-api-key` 据此对所有**读写治理状态的方法**设门——`tools/list`、`tools/call`、`resources/list`、`resources/templates/list`、`resources/read`、`completion/complete`(`initialize` / `ping` / `prompts/*` / `logging/*` 始终开放,好让客户端完成握手、取静态剧本、并控制服务端详谈程度)。**挂载面门**:`meta-server --require-api-key` 卡 `M_OPEN`、`data-server --require-api-key` 卡 `OP_OPEN`,客户端以 `mount --api-key <ID:SECRET>` 呈现(不便写命令行时可导出 `GROVEFS_API_KEY='<ID:SECRET>'`,密钥随即不落 argv);**开启顺序很重要**: 服务端**不写** `--require-api-key` 就完全**不校验** key(客户端带一个瞎编的 key 也照挂,实测 `MOUNTED`);写了则是 **fail-closed**——`api_keys.db` 里没有可用 key 时**所有人(包括你)都进不来**,所以先把 key 铸好、再带 flag 重启;**审计面**:每一次 `tools/call`、`resources/read`,以及每一次挂载连接(含被拒的)都追加进同一张审计表(时间、key、平面 method、工具 tool、成败、详情),`grovefs apikey calls` 回读。撤销只把行标记为 revoked(历史调用仍保留归属),撤销后的 key 立即鉴不上,**且已建立的挂载也会被封停**(见下)。存储是一处独立的本地 SQLite(`<meta-dir>/meta/api_keys.db`):控制面状态。下列回显取自真实运行:

```
# 1) 铸造一个 key(明文只打印一次;scope 默认 any)
$ grovefs apikey --meta-dir $HOME_DIR create --label claude-desktop
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    06fc0287c1464bd1b5a8ec1d4c1c9c31
  label: claude-desktop
  scope: any
  key:   06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0451c46889d8e...(64 hex secret)

Present it to a `--require-api-key` server as
  grovefs mount --meta-server ... --data-servers ... --api-key '06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0...(64 hex secret)' -m /mnt/x
(or export GROVEFS_API_KEY='...' so it stays out of the command line).
Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount
credential out of the governance API (and vice versa).
Revoke later with: grovefs apikey --meta-dir $HOME_DIR revoke --id 06fc0287c1464bd1b5a8ec1d4c1c9c31

# 2) 铸造一个只能挂载的 key;scope 写错直接拒绝(打错一个词不会静默放宽权限)
$ grovefs apikey --meta-dir $HOME_DIR create --label laptop-alice --for mount
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646   scope: mount   key: aac1f51a...:a5d5450a...
$ grovefs apikey --meta-dir $HOME_DIR create --label x --for both
error: unknown api key scope 'both' (expected any|mcp|mount|admin)      # exit=1

# 2b) 视图绑定 + 只读的 key(访问控制维度)
$ grovefs apikey --meta-dir $HOME_DIR create --label reader --for mount --view alice --ro
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…(64 hex secret)     # 只能挂 view alice,写一律被服务端拒
$ grovefs mount --view bob -m /mnt/b --meta-server ... --data-servers ... --api-key "$READER_KEY"   # 挂别的视图
error: permission denied: this api key is bound to view 'alice', not 'bob'    # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key
bash: /mnt/a/should_fail: 只读文件系统                                               # 据握手标志落内核只读;服务端另逐写拒 E_NOTPERM/EROFS

# 2c) 原地切换只读/读写(热生效,不重铸不重连)
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id 7c0e --rw
apikey: id='7c0e…' (label 'reader') is now rw (read-write)
  a LIVE mount presenting this key picks up the new verdict on its next op (within the 1s re-check window) — no reconnect;
  only a mount the handshake placed kernel read-only needs a remount to lift the OS-level cap.
$ grovefs apikey --meta-dir $HOME_DIR set-ro --id 7c0e            # 切回只读(默认方向)
apikey: id='7c0e…' (label 'reader') is now ro (read-only)
# 前缀歧义/已撤销/未知 id 与 revoke 同样处理:歧义前缀报错、revoked 行不回活、未知 id 报错(exit=1)。

# 3) 列表带 scope/view/rw 列,绝不落密文;哈希也不落盘(库内只有摘要)
$ grovefs apikey --meta-dir $HOME_DIR list
#   id                                 label            scope   view          rw   created                   last_used                 revoked
1   296ef5839f5146078764ee5d16be6ef7   agent            mcp     -             rw   2026-10-27T06:57:59...      -                         -
2   aac1f51a0d5641ddaa8dbb83dc1cf646   laptop-alice     mount   -             rw   2026-10-27T06:57:59...      2026-10-27T06:58:02...    -
3   06fc0287c1464bd1b5a8ec1d4c1c9c31   claude-desktop   any     <any>         rw   2026-10-27T06:57:59...      -                         -

# 3b) 按视图过滤:只列能开 lab 的 key(纯别名也匹配绑定到真实名的 key),并提示系统级 key 数
$ grovefs apikey --meta-dir $HOME_DIR list --view lab
#   id                                 label            scope   view         rw   created                   last_used                 revoked
1   0cfaffd5fe2e4486af8ed1f282222dec   lab-agent        any     lab          rw   2026-10-27T15:31:33...      -                         -
1 bound key(s) for view 'lab' of 2 total

# 4) 服务端开门时自报门槛;不开门的默认路径行为与从前逐字节一致
$ grovefs meta-server --meta-dir $HOME_DIR --listen 0.0.0.0:50110 --require-api-key
meta-server listening on 0.0.0.0:50110 (base $HOME_DIR, role Primary)
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op
$ grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir $HOME_DIR
data-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live connections on the next op

# 5) 客户端拿各种坏凭据去挂:拒因逐字节的原文回到本地 stderr(退出码 1,不留脏挂载)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ...                      # 不带 key
error: no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "aac1f51a...:deadbeef"
error: authentication failed: invalid api key secret
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "00000000...:whatever"
error: authentication failed: unknown api key id
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "no-colon-here"
error: malformed api key (expected '<id>:<secret>')
$ grovefs mount --view alice -m /mnt/a --meta-server ... --data-servers ... --api-key "$MCP_KEY"   # 拿治理 key 来挂
error: authentication failed: this api key is scoped to 'mcp', not 'mount'
#   反方向同样被拒:把 mount key 塞进 MCP initialize 的 apiKey,tools/list 回 -32001,
#   且这条跨入口尝试同样被审计成 scope 拒因。

# 6) 挂载连接与治理调用同表审计(method 分平面、tool 分握手)
$ grovefs apikey --meta-dir $HOME_DIR calls --limit 6
last 6 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-10-27T06:58:02.176356991+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   meta         M_OPEN                 yes  view='alice' uid=1005
2026-10-27T06:58:00.180250185+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   data         OP_OPEN                yes
2026-10-27T06:58:00.169508131+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'
2026-10-27T06:58:00.164775080+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: invalid api key secret

# 7) 撤销一行输出说清两面契约;历史调用仍保留归属
$ grovefs apikey --meta-dir $HOME_DIR revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646
apikey: revoked id='aac1f51a0d5641ddaa8dbb83dc1cf646' (label 'laptop-alice'); it can no longer authenticate, past calls keep attribution
  a LIVE mount presenting this key is refused on its next op (within the 1s re-check window) and unmounts itself;
  a `grovefs mcp` session in flight keeps its identity until it reconnects.

# 8) 封停活挂载:实测撤销后一次写的遭遇(无需运维介入)
$ printf 'late' > /mnt/a/new.txt
bash: /mnt/a/new.txt: 权限不够        # EACCES,来自服务端的拒因
$ cat /mnt/a/grovefs-mount.log        # 挂载自己把原因打出来并卸载
using remote metadata service at 10.0.0.5:50110
using remote data service at 10.0.0.5:50100
grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view
received termination signal, unmounting cleanly...
unmounted view 'alice' at /mnt/a
$ ls /mnt/a                            # 干净的空目录——不是 "Transport endpoint is not connected"
#   同期另一个 key 的挂载照旧读写(逐调用方撤销,不像轮换一份共享 token 那样断全部人);
#   拿被撤销的 key 重挂则 fail-closed:"authentication failed: this api key has been revoked"。

# 9) 治理面门的形状:门只卡状态方法面——不带 apiKey 的 initialize 照常应答,随后 tools/list 被拒
$ grovefs mcp --meta-dir $HOME_DIR --require-api-key      # 客户端 initialize 不带 apiKey
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize 应答(门只卡状态方法面)
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}

# 10) 客户端 initialize 携带 "apiKey":"<id>:<secret>"(且 scope 不是 mount)→ 该会话鉴权通过,工具面放行:
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
```

五个子命令:`create --label L [--for any|mcp|mount|admin] [--owner ACCOUNT] [--view V] [--ro]`(铸造并一次性打印 `<id>:<secret>`;`--view`/`--ro` 只对 mount/any scope 有意义,`--owner` 对 admin 必填)、`list [--view V]`(id/owner/label/scope/view/rw/created/last_used/revoked,无密文;带 `--view` 时只列能开该视图的 key,纯别名也匹配绑定到真实名的 key)、`revoke --id ID`(支持精确 id 或唯一前缀)、`set-ro --id ID [--rw|--ro]`(原地翻转活跃 key 的只读标志:默认与 `--ro` 置只读、`--rw` 恢复读写;只动写权限不改存活/鉴权,经服务端建立的已建立挂载 ≤1s 热生效免重连——本机 `--home` 挂载无服务端凭据门,不受切换影响;revoked id 拒翻)、`calls --limit N`(默认 20,倒序回读审计)。无法自定义握手的 MCP 客户端,可由运维方导出 `GROVEFS_MCP_API_KEY='<id>:<secret>'` **预鉴权整个服务进程**(`grovefs mcp` 启动即以该 key 建立身份,客户端 initialize 无须再带);挂载侧对应的环境变量是 `GROVEFS_API_KEY`(不传 `--api-key` 时生效,让密钥不进命令行)。`data-server` 的 key 库路径由 `--meta-dir <元数据根>` 指定,**默认取本节点首个 `--data-dir`**——同机部署无需额外参数,数据盘与元数据根分开部署时必须指对(指到一个没铸造过 key 的目录时 fail-closed,一个也不放)。诚实边界:这把 key 认证的是**入口的调用方**——它让某个调用方能(或不能)发治理写命令、能(或不能)开某棵视图树,并把每次准入归咎到具体 key,但不改变治理语义本身;视图绑定/只读维度由服务端强制;活连接复证是 **op 驱动**的(有流量才查),故撤销的封停边界是“该挂载撤销后的首个服务端往返”——一次完全由内核缓存回答的读不需要往返,因此不会被打断(下一次往返即封);`--token` 与 `--require-api-key` 可同时存在,两者都得过。不开 `--require-api-key` 时,服务行为与不设门完全一致(只是照样落审计)。e2e 见 `apikey` 套件(33 条断言:铸造/列表/哈希不落盘/无 key 拒·错 secret 拒·对 key 放行/调用归属/撤销即失效且行保留/短 id 前缀撤销/env 预鉴权/`list --view` 过滤/默认不设门回归/`set-ro` 原地 ro↔rw 翻转·翻转后仍鉴权;`APIKEY_BACKEND=fdb` 把同一套对着集群复跑,36 条——多出的三条正是「凭据落在集群」本身:本地根下不出现 `api_keys.db`、一个从未铸造过 key 的 home 仍能列出该 key、本地根里搜不到明文)与 `mountkey` 套件(76 条断言:scope 铸造与 typo 拒绝/六类拒因入协议/准入与 env 回退(argv 不泄密)/连接审计到具体 key 与视图/scope 双向隔离/撤销封停活挂载且无脏 endpoint/其他 key 不受波及/token 与 key 双门 AND/数据面独立门/故障转移携带凭据/不设门默认路径无回归/视图绑定 key 跨视图拒挂与审计归属/只读 key 服务端强制写拒/`set-ro` 在同一活挂载 rw→ro 拒写·ro→rw 放行·免重连)。

### 3.5.2 任意机器管同一份元数据:远程治理面(`--control-listen` / `--control-server`)

治理命令(`view create/define/alias/list/rm`、`fork`/`graft`、`export`/`import`、`apikey`、`refs rebuild`、`gc`)必须能读写**元数据存储本身**。SQLite 后端的树是 `<meta-dir>/meta/<view>.db`,只有持盘主机上的进程够得着;PG 后端虽然任何能连库的机器都能管,但代价是把 `postgres://user:pw@…` 发给每台管理机——一个共享密码,既说不清是谁在改,也没法只撤一个人。

远程治理面把这两件事拆开:`meta-server --control-listen HOST:PORT` 再开一个**只跑治理动词** 的监听,治理命令用 `--control-server HOST:PORT --api-key '<id>:<secret>'` 指过来,由那个本来就持有存储的进程代做真实操作。管理机上没有数据库 URI、不必能访问 `<meta-dir>`,SQLite 与 PG 走同一条路。凭据是**账号密钥**(§3.5.1 的 `--for admin --owner <账号>`):一账号可持多把,逐把铸造、逐把撤销,每次调用落到审计。

启动(持盘主机;治理口与挂载口是两个端口,可分别设防火墙):

```bash
$ grovefs meta-server --meta-dir /srv/gfs --listen 0.0.0.0:50110 --control-listen 0.0.0.0:50111
meta-server listening on 0.0.0.0:50110 (base /srv/gfs, role Primary)
meta-server: metadata backend sqlite — one db file per view under <base>/meta/
meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition (`grovefs view create --view <name>`, or `--home <dir> init --view <name>`); no empty tree is ever auto-created
meta-server: CONTROL plane listening on 0.0.0.0:50111 (admin-scope api keys only; every call is audited as method=control — mint with `grovefs apikey create --for admin --owner <account>` on this host)
```

第一个管理员 key 在持盘主机上铸造(`apikey` 直接读本地库;治理口不给自己铸管理员——否则这道门就白设了):

```bash
$ grovefs apikey --meta-dir /srv/gfs create --label ops-alice --for admin --owner alice
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    6e0f9e874f3c4bdca09274f0ac40828b
  owner: alice
  label: ops-alice
  scope: admin
  view:  <any>
  rw:    rw (read-write)
  key:   6e0f9e874f3c4bdca09274f0ac40828b:8516e6eaf85045178490e81d995d92e6227c5bb0204b4b9d845d357c06eb6229

Admin scope also governs a `meta-server --control-listen` from any machine, with
  grovefs view --control-server HOST:PORT --api-key '<id>:<secret>' list
  grovefs apikey --control-server HOST:PORT --api-key '<id>:<secret>' create --label k2 --for mount
```

任意一台管理机(密钥进 env,不进 argv):

```bash
$ export GROVEFS_CONTROL_KEY='6e0f9e874f3c4bdca09274f0ac40828b:8516e6eaf85045178490e81d995d92e6227c5bb0204b4b9d845d357c06eb6229'
$ grovefs view --control-server 10.0.0.5:50111 list
no alias/fork/lens definitions in remote control plane at 10.0.0.5:50111 (backend sqlite) — the 1 base view(s) below each map to their own tree
base view 'alice' -> own tree meta/alice.db (read-write; deletes free a shared object only when no view holds it)
view keys: `grovefs apikey --control-server 10.0.0.5:50111 --api-key <admin-key> list [--view NAME]` (add --view to see only that view's keys)

$ grovefs view --control-server 10.0.0.5:50111 create --view bob
created BASE view 'bob' at /srv/gfs/meta/bob.db (via 10.0.0.5:50111)
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.

$ grovefs view --control-server 10.0.0.5:50111 rm --name bob --base
removed base view 'bob': 1 node(s) gone, 0 blob reference(s) dropped, 0 still held by other views; 0 object(s) freed from /srv/gfs/blocks, 0 not found under that data root (they live on another store?), 0 could not be freed (striped keys move through a data-server)

$ grovefs refs --control-server 10.0.0.5:50111 rebuild
refs ledger rebuilt at /srv/gfs/meta/refs.db: 0 holder entries across the surviving views
  run it QUIESCED: a booking committed between the server's read pass and write pass would be overwritten with the pre-booking count
```

审计与本地同一张表:`method` 列写 `control`、`tool` 列写动词、`detail` 带账号;鉴权失败的尝试以空 `key_id` 落行,所以「谁拿错凭据试过」也看得见。

```bash
$ grovefs apikey --control-server 10.0.0.5:50111 calls --limit 4
last 4 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-11-11T13:35:53.830004726+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      info                   yes  account=alice
2026-11-11T13:35:51.145091131+00:00 -                                  control      info                   no   authentication failed: this api key is scoped to 'mount', not 'admin'
2026-11-11T13:35:43.825879139+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      apikey.list            yes  account=alice
2026-11-11T13:35:43.820049671+00:00 6e0f9e874f3c4bdca09274f0ac40828b   control      refs.rebuild           yes  account=alice
```

拒因原文(退出码 1):

```bash
$ grovefs view --control-server 10.0.0.5:50111 list                     # 不带 key
error: --control-server 10.0.0.5:50111 needs an admin account key: pass --api-key '<id>:<secret>' (or export GROVEFS_CONTROL_KEY). The first one is minted ON THE SERVER HOST with `grovefs apikey --meta-dir <server-meta-dir> create --for admin --owner <account>`.
$ grovefs view --control-server 10.0.0.5:50111 --api-key "$MOUNT_KEY" list
error: authentication failed: this api key is scoped to 'mount', not 'admin'
$ grovefs view --control-server 10.0.0.5:50111 --meta-dir /tmp list      # 本地 flag 与远程并用
error: --control-server 10.0.0.5:50111 cannot be combined with --meta-dir/--meta-store: a remote control plane governs the store ITS server owns, so a local path here would describe a different store. Drop the local flags — the account key alone decides what is governed.
$ grovefs view --control-server 10.0.0.5:50111 scan          # 仍在本地的一档
error: `view scan` is not a governance op: it moves or inspects TREE BYTES or cluster state on this machine, so it cannot run through --control-server 10.0.0.5:50111. Governance over the wire covers view create/define/alias/list/rm (including --base), fork/graft, export/import, apikey and refs; run this one on the server host with --meta-dir/--meta-store.
```

覆盖范围按动词划分(三档后端一致,见下方“三档后端同一条路”):

| 经治理口(任意机器) | 必须在持盘主机(`--meta-dir` / `--meta-store`) |
| --- | --- |
| `view create`(含 `--manifest` 文本) / `define` / `alias` / `list` / `rm`(删定义、`--base` 删整棵树) | `view scan`(清点本机 `meta/` 文件或集群子空间) |
| `view fork`(整树复制，含 `--empty`) / `graft`(挑选子集 + 改名/`--dest`) | `view lease`(本机部署的写者租约：读出/释放/围栏) |
| `view export` / `import`(分片上下行；本机只有 `--out` 与输入文件) | `mount` / `umount`(挂载点在本机) |
| `apikey create` / `list` / `revoke` / `set-ro` / `calls` | `gc --data-dir R`(就地扫本机 `blocks/` 的 SQLite 专用形态) |
| `refs rebuild` | `du --meta-dir`(本机树文件) |
| `gc --data-servers … [--apply]`(服务端读自己的树，字节向 data-server 枚举) | |

语义与边界:

- **同一套守卫**:治理口背后是同一个 `ControlStore`,名字非法、已存在、被别名/透镜/fork 治理等拒因与本地逐字节相同,只是执行进程换了。
- **只认 admin**:`mount`/`mcp`/`any` 三种 scope 的 key 在该端口一律拒(`any` 不会升格成管理员);没有匿名模式,`api_keys.db` 为空时所有人都进不来。
- **撤销即生效**:每次调用现查现鉴,`apikey revoke` 之后下一个动词就回 `this api key has been revoked`。
- **无会话**:帧协议一请求一响应,没有可延长的租约;密钥只在帧里出现,服务端只比对 SHA-256 摘要。
- **端口即边界**:治理口只对内网网段开,挂载口照旧走 `--listen`;`--control-listen` 绑定失败会让 `meta-server` 启动即失败,不会带病起来。
- **三档后端同一条路**:`fork`/`graft`/`export`/`import`/`gc` 由持有存储的 `meta-server` 执行,后端是 SQLite 文件、PG `grovefs_v_<hex>` schema 还是 FDB 子空间都不改变命令形状;PG/FDB 部署下服务端本来就是唯一能碰 schema/子空间的进程,管理机连 `postgres://` 连接串或集群文件都不需要(也就没有共享数据库密码外流)。
- **dump 只走元数据**:`export` 由服务端渲染到 `<meta-dir>/meta/.xfer/` 暂存区、按**行对齐分片**回传(永不切行),`import` 反向分片推上去再单事务安装;跨机 import 只搬元数据,字节本体要另外搬数据根——CLI 每次都打印这条提醒。
- **e2e**:`forksrv`(fork/graft 三档)、`xfer`(export/import 三档 × 本地/远程)、`gcremote`(gc 三档)三份套件逐条断言「管理机无落盘 + 账本精确 + 跨档一致」。
- **缺省不变**:不给 `--control-listen` 的 `meta-server` 与本文其他章节完全一致;治理命令不带 `--control-server` 就仍然直接改本机 / 本 URI 的存储。

e2e 见 `ctlplane` 套件(启动横幅/首个 admin key 必须在主机铸造/远程 create·define·alias·list·rm·refs·apikey 全套且管理机无落盘/第二个 home 仍为空(证明树只在服务端)/`--for any`·`--for mount` 与垃圾 key 被拒/远程撤销后立即失效/不带 key 与本地 flag 混用两类拒绝/`scan`·`lease` 两个非治理动词按名拒绝/远程删树后守卫仍生效/PG 档同套断言);fork/graft 与 export/import 的远程路径分别由 `forksrv`、`xfer` 套件覆盖(三档后端 × 本地/远程)。

## 4. 生产拓扑:中心存储 + 多客户机 LAN 挂载

真实场景:数据放中心集群,多台客户机经 LAN 挂载,同一用户跨机器看到**同一份权威视图**。
这需要拆开两个服务:

```bash
# ===== 中心节点 =====
# 数据服务:把物理对象存到本节点的硬盘目录
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster

# 元数据服务:托管每用户权威视图(多客户机共享同一棵,不再各存各的副本)
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110
#   --meta-dir 就是元数据根:视图库在其 meta/ 下。视图名必须显式创建——先建树再挂:
$Y view --meta-dir /data/cluster create --view alice
#   (不建就挂会被 M_OPEN 拒:view 'alice' does not exist on this meta-server …)

# ===== 每台客户机(远程挂载不需要 --home,本地什么都不落) =====
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

- `--data-servers <host:port>`:文件 IO 走网络到 data-server,不碰客户机本地盘。
- `--meta-server <host:port>`:视图树由中心 meta-server 提供,**多台客户机挂同一用户 = 共享同一棵权威树**。
- 客户机本地**不留视图副本、也不落盘**：给了 `--data-servers`/`--meta-server` 的远程挂载**不需要 `--home`**（传了也不写任何东西）——权威数据在 meta-server，字节在 data-server，访问控制全部发生在服务端 `M_OPEN`/逐 op,客户端不查本地库。

> **视图名不存在时直接拒绝——这是唯一行为,没有开关。**
>
> **规则**:名字必须已经存在——一棵物理树,或一条治理定义(派生/别名/fork)——否则 `M_OPEN` 直接拒绝,且**什么都不写**。拒因走**带 payload 的 `E_DB`**(不是空包),所以客户端打印的是原因而不是裸 `Input/output error`,服务端日志里也有同一句(实测原文,SQLite):
>
> ```text
> meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition …; no empty tree is ever auto-created
> $ grovefs mount --view typo1 --meta-server 10.0.0.5:50110 --data-servers 10.0.0.5:50100 -m /mnt/t
> error: view 'typo1' does not exist on this meta-server (sqlite): no tree and no definition —
>   views are created explicitly, so unknown names are refused instead of becoming an empty tree;
>   create it first: `grovefs view --meta-dir /data/cluster create --view typo1` …
> meta: M_OPEN view='typo1' refused: …                          # 服务端日志
> $ ls /data/cluster/meta | grep typo1                          # 空:没有 typo1.db,也没有 -wal/-shm
> ```
>
> **没有任何开关**:未知视图名一律拒绝,没有任何参数或环境变量能改变这条规则。
>
> 开了 `--require-api-key` 时这条拒绝同时进审计(`grovefs apikey calls`):`… M_OPEN view='typo1' refused: …`。已存在的基视图、派生视图、纯别名、fork 照挂;PG 后端探的是 PG schema 本身(`information_schema.schemata`),所以拒绝时**不会建 schema**。
>
> **建新视图一律先建后挂**:SQLite `grovefs view --meta-dir <meta-dir> create --view <name>`(或 `--home <meta-dir> init --view`、`view import` 播种);PG `grovefs view --meta-store postgres://… --meta-dir <meta-dir> create --view <name>` 显式建 schema(或从 `pg_dump` 恢复)。
>
> **正交的另一把锁是把 key 绑到视图**：`apikey create --view team` 的 key 去挂别的名字，在**绑定检查**处就被拒（比存在性检查更早），实测 `permission denied: this api key is bound to view 'team', not 'ghost3'`，同样不建树。分工：存在性检查管「这个名字在不在」，绑定 key 管「这个调用方能开哪一棵」。
>
> 清理误建的空树（先 `umount` 所有该名字的挂载）：裸基视图（无定义的物理树）用 `grovefs view --meta-dir D rm --name <base> --base` 一条命令删——它删 `meta/<base>.db`(+WAL/SHM)、`refs rebuild` 重算账本、并回收无其他视图持有的字节（§3.4.3）。手工等价路径：`rm -f meta/<name>.db meta/<name>.db-wal meta/<name>.db-shm` 后 `grovefs refs --meta-dir D rebuild`；PG 后端是**同一条命令**加 `--meta-store`（`view --meta-store postgres://… rm --name <base> --base`）——销账（逐 key 减 `grovefs_refs.holders`）与 `DROP SCHEMA … CASCADE` 在同一事务里完成，提交后按删除前读到的 `data_ref` 回收字节（§15.8）。绕过 CLI 手工 `DROP SCHEMA` 不算删除：ref 清单随 schema 一起消失，账本里的行从此没人能回收（`refs --meta-store postgres://… rebuild` 只会把多计的行删掉，字节已经定位不到了）。

### 4.1 多机共享到底需要 PG 吗？——上手选型

> **直接回答：不需要。** 多机 LAN 共享一棵树的能力来自「同一个 `meta-server` + 同一个 `--view`」，与后端存的是 SQLite 还是 PG **无关**。默认 embedded SQLite(`rusqlite bundled` 编译进二进制、零外部依赖)已经足够支撑多客户端挂载、写串行化、实时失效推送。PG 只是 meta-server 的**可选后端**,纯服务端配置(`--meta-store postgres://…` 在 meta-server 启动时给,客户端无此选项、不感知)。两者是**二选一**的替代关系、不叠加:同一棵权威树只落在其中一种后端上——用 SQLite 就不需要装 PG;换 PG 后视图树只进 PG schema,服务端不再创建/读写任何 `<meta-dir>/meta/<view>.db`(留在目录里的是换库前的旧副本);控制面同样跟随后端——`grovefs view`/`apikey`/`mcp`/`data-server` 也接 `--meta-store postgres://…`(或 env `GROVEFS_META_STORE`),key 与审计进 `grovefs_control.api_keys`/`mcp_calls`、视图定义进 `grovefs_control.view_defs`,PG 部署下本地不落任何控制面文件(`control` 套件实证)。仅当你想把权威树的持久化/HA/运维**交给成熟数据库**时才上 PG。

| 拓扑 | 后端 | 适用场景 | 命令 |
| --- | --- | --- | --- |
| 单机本地 | SQLite (embedded) | 开发、测试、单用户 | `mount --view alice --mountpoint /mnt/alice` |
| **中心节点 + 多客户端 LAN** | **SQLite (默认)** | **小/中型团队共享、零外部服务依赖** | `meta-server --listen 0.0.0.0:50110` + 客户端 `mount --meta-server ip:port --view alice` |
| 中心 + 多客户端 LAN | SQLite + HA (`--role`+`--peer`+`--witness`) | 单 meta-server 挂后自动 failover、双写防护 | 加 `--role primary --peer …` 等 |
| 中心 + 多客户端 LAN | **PostgreSQL** (`--meta-store postgres://…`) | 想把 HA/备份/PITR/百万节点索引调优**交给 PG**,且已有 PG 运维能力 | `meta-server --meta-store postgres://user:pw@host:5432/db`(不能同时加 `--peer`/`--witness`) |

> **常见误解纠正**：
> - 「PG 是客户端跟服务端都要装」→ 错,**只在服务端**(meta-server 进程连 PG),客户端只装 `grovefs` 二进制、只说 GroveFS 的 TCP 帧协议。
> - 「上了 PG 写并发会更高」→ **部分对**：默认 SQLite 仍单写者；但 `--meta-store postgres://…` 会把写改为 SERIALIZABLE 写连接池（至多 8 连接，HopsFS 路线），不相交行上的并发写并行提交、冲突由 SSI 透明重试（`pg_ssi`/`pg_parallel` 套件实测）。并行仅限单个 PG 集群内，不跨集群分区元数据权威；跨集群扩展仍交 PG 自身复制/分片。
> - 「共享一棵树必须先有 PG」→ 错,默认 SQLite 就支持,`metasrv`/`freshness`/`locks` 三个套件全部纯 SQLite。

### 跨客户端一致性:两种模式

| 模式 | 命令 | 行为 |
|------|------|------|
| **实时失效推送(默认,推荐)** | 仅 `--meta-server` | 一客户端写成功后,meta-server 经事件总线向同用户其它订阅者广播失效(`inval_entry`+`inval_inode`),对端下次访问**即时可见**,无需轮询。 |
| **`--no-cache` 强一致兜底** | 加 `--no-cache` | 关闭内核属性/dentry 缓存并 `FOPEN_DIRECT_IO` 直读,写立即可见。代价:零客户端缓存,元数据/数据往返更多。适合对新鲜度要求极端严苛的场景。 |

> **默认即新鲜**:非 `--no-cache` 的推送失效模型已保证「A 写成功 → B 下次访问即时可见」,无需轮询、无需 `--no-cache`。这由 `.build-bin/test_freshness.sh` 端到端证明(顺序追加 100 行、截断、反向读三类场景均在 <500ms 内对端可见)。`--no-cache` **不是**保新鲜的唯一通道,只是在 writeback 脏页 flush 与远端失效交错的极端竞争窗口下,选择完全旁路内核缓存的更保守兜底。

> **分布式 advisory 锁**:`flock` / `fcntl F_SETLK/F_GETLK` 经 meta-server 的 `locks` 表(SQLite)集中仲裁,**跨进程、跨机生效**;主备切换后锁状态随快照复制存活。锁的回收有两条路径:①**持锁进程崩溃**——内核关闭其 fd 触发 FUSE `release`(携带匹配的 `lock_owner`),锁行被**即时删除**,远小于 lease;②**mount daemon 整机崩溃**——无 `release` 也无续约,锁在**一个短 lease(默认 15s)内**过期被扫。存活持有者由 mount daemon 的**续约心跳**(每 lease/3 bump `expires_at`)持续续期,**永不超时**。非合作应用(cp/dd)仍不阻塞,与 NFS/SMB advisory 语义一致。由 `.build-bin/test_locks.sh`(段 4 崩溃即时回收 / 段 4b 续约跨过 lease)证明。**伸缩性边界**:等锁(F_SETLK 带阻塞 / `flock -w`)在挂载进程里同步轮询(100ms 一次,上限 10 分钟),会占住一个 FUSE worker 线程;默认 `--threads 4` 下同一挂载点若同时有 ~4 个进程在等锁,该挂载的其他请求会排队——等锁多的场景请调大 `--threads`,事件驱动唤醒在演进计划里。

#### 4.1.1 三档的硬指标：写并行只是最容易被看见的一条

| 指标 | SQLite（默认） | PostgreSQL | FoundationDB（实验档） |
| --- | --- | --- | --- |
| 写模型 | 单写者（`BEGIN IMMEDIATE` 互斥）；读走 WAL 可并行 | 每视图 8 条 SERIALIZABLE 写连接 + 4 条只读连接，不相交行并行提交，冲突由 SSI 透明重试 | 乐观并发（OCC）提交、冲突重试；但**同一视图仍然只有一个写者**，靠部署级 lease 围栏 |
| 写吞吐（本仓库实测） | **767–1,109 ops/s**（N=1 1,109 / N=4 811 / N=8 767；并 发写仍单写者串行，也随挂载数略降） | 单视图平台值 **~4.6k–5.2k ops/s**（N=4 峰值 5,235；N≥8 因 SSI 重试略降；写语句缓存后 N=1/4/8 为 3,048–3,118 / 4,784–5,202 / 4,518–4,680，§6.2） | **171–498 ops/s**（真四机 `triple`，同场 PG 1,886–4,798；单机三进程集群上 133–1,601）；G3 性能 门 4,600 未过 |
| 单事务上限 | 无硬闸门（只受内存/磁盘约束） | 无硬闸门 | **10 MB**：超出的整树写自动按 70% 预算分块提交（带引用每批约 5,400 行；单事务能写多少行的实测边界见 §6.3.4） |
| 可用性 | 原生 HA：`--role primary/standby` + `--peer` + `--witness`，快照转发；单写者，切换有窗口 | 交给 PG 自身（流复制 / PITR），GroveFS 不再管 | 三副本 + 故障域：`kill -9` 损失 0s、整机 `virsh destroy` 0–1s、幸存者 4s 重组日志集；`fdbbackup` 时间点恢复 |
| 视图规模 | 每视图一个 `meta/<view>.db` + 一个引擎实例，约 10 + (读池−1)×~1.3 个 fd/视图（本地挂载读池 = `--threads`，默认 4 → 约 14 个；`GROVEFS_SQLITE_READ_POOL` 可调），65536 下约四千视图 | 每视图 12 条连接（默认 8 写 + 4 读，`GROVEFS_PG_*_POOL` 可调）；`--engine-idle-secs` 驱逐后按「最近 N 分钟活跃视图数」计 | 无连接模型：一个 database handle + 每视图键前缀，设计目标万级热视图 |

结论很清楚：**SQLite 与 PG 的差距是单写者模型本身（同一时刻只有一个写事务），实测约 4–6 倍**；反过来 FDB 也没有写并行优势（每视图一个写者），它的价值在别处。

> 历史口径更正：本节旧版把 SQLite 写成「45–58 ops/s，被 fsync ~17.5ms 封顶」。那个 fsync 不在视图库上，而在跨视图账本：`ATTACH refs.db` 后切 `journal_mode=WAL` 会把**该附加库自己的** `synchronous` 重置回 FULL，于是每次命名操作都在 `refs.db-wal` 上付一次 17ms 级 fsync（strace：300 次 create → 328 次 fsync，其中 301 次落在 `refs.db-wal`）。`attach_refs_ledger` 现在把 main 的 `synchronous` 镜像到账本（单元测试 `refs_ledger_mirrors_the_view_synchronous_mode` 钉住），单线程顺序 create 从 19.7ms → 2.14ms/op，上表并发值随之抬升。

远端挂载（`meta-server`）下，一次 create 曾要三个元数据往返：内核的 `LOOKUP`、`M_CREATE`，以及紧随其后、只为填满 FUSE 回复的 `M_GETATTR`。现在 `M_CREATE` / `M_SYMLINK` 把刚提交的那一行内联进回包（`MetaProvider::create_node_with_attr`；本地引擎走默认实现，只是多读一行、不多走一个往返），客户端不再补这次 `M_GETATTR`：同一远端挂载点 1500 次顺序 create，`--stats` 的 `remeta.rpc` 每 create **3.003 → 2.003**，**0.457–0.494ms → 0.360–0.407ms/op**（+24%，三次复测）。属性就来自刚写入的那一行本身，mode/uid/gid/nlink/链接目标由 `create_and_symlink_replies_carry_the_row_in_one_round_trip` 与 `.build-bin/probe_create_attr.sh` 钉住；老服务端回包不带行时客户端自动回落一次 `M_GETATTR`，新旧混跑不破。

#### 4.1.2 能取代的是功能面，取代不了的是形态

**功能面上 SQLite 是超集**：22 项治理能力（见 §10.1 矩阵）里，SQLite 除 `view scan`/`view lease` 两项 FDB 专属外全部可用——`fork`/`graft`、`export`/`import --as-fork`、`refs rebuild`、`rm --base`、远程 `gc`、`du`、`apikey`+审计，连**原 生 HA 也只有 SQLite 有**（PG/FDB 都拒绝）。并且 SQLite 有两件 FDB 做不到的事：没有 10 MB 事务闸门、零外部依赖单二进制。（FDB 那 10 MB 已不再是「搬不动大树」的理由——`import`/`fork`/`graft` 超出即自动分批，见 §6.3.4——只是同一份树会分布在多个事务里，而不是一个。）

**取代不了的是这四条**（都在“形态”而非“功能”上）：

| 维度 | SQLite 的真实状态 | 为什么 FDB 绕不过 |
| --- | --- | --- |
| 可用性归存储层 | 主备 + witness + 快照转发：单写者、切换有窗口、依赖两个外部进程与网络 | 副本与故障域在存储层，进程/整机失效由集群自己重组 |
| 视图规模 | 一视图一文件一引擎，fd/内存线性涨，没有“驱逐”之外的招 | 无连接模型，万级热视图才够它的立项门槛 |
| 多机共享写 | 一个 `--meta-dir` 只能有一个写者（文件锁）；把 `.db` 放共享文件系统不成立 | 多个部署可并行服务**不同**视图（lease 按部署围栏），同一视图仍单写者 |
| 元数据独立扩容 | 元数据绑死在 meta-server 那台机器的盘上 | 元数据集群独立加节点，与字节（data-server/对象存储）解耦 |

#### 4.1.3 后端独有 / 半独有命令

“独有”指只有一档能跑；“半独有”指三档同名但落点或形态不同。细节与拒绝原文见 §15.9。

| 命令 | SQLite | PG | FDB | 说明 |
| --- | --- | --- | --- | --- |
| `grovefs import --view V --source DIR` | 可用 | 拒绝 | 拒绝 | 顶层 import 是“把盘上目录纳管”，靠同盘硬链接零拷贝；远程库没有可纳管的本地盘，拒绝并指向 `view import` |
| `gc --meta-dir D --data-dir R [--apply]` | 可用 | 拒绝 | 拒绝（本地形态） | 离线扫孤儿要同时看见库与对象目录；远程部署改用 `gc --control-server … --data-servers …` |
| `meta-server --role primary/standby --peer … [--witness …]` | 可用 | 拒绝 | 拒绝 | 原生 HA 是 SQLite 专有；PG 用流复制/PITR，FDB 用集群自身与 `fdbbackup` |
| `view --meta-store fdb://… scan` | 拒绝 | 拒绝 | **可用** | 列集群里有哪些树子空间与 key 数；只能在持有集群访问的主机上跑（`--control-server` 也拒绝） |
| `view --meta-store fdb://… lease [--release\|--force]` | 拒绝 | 拒绝 | **可用** | 部署级写者围栏：列出/释放/强夺；同上必须直连集群 |
| `view define / alias / list` | 定义落 `meta/views/*.layout` | 定义落 `grovefs_control.view_defs` | 定义落集群 `ctl` 子空间 | 三档各落自己的控制面存储，同一条纪律；FDB 见 §6.3.5 |
| `apikey … / calls` | `meta/api_keys.db` | `grovefs_control` | 集群 `ctl` 子空间 | 能力三档都 OK，只是落点不同 |
| `refs rebuild` / `rm --base` / `du` | `--meta-dir` | `--meta-store postgres://…` | `--meta-store fdb://…`（`du` 也可 `--meta-server`） | 同名不同壳；`du` 对不存在的视图名三档回同一句 `no tree for view …` |

#### 4.1.4 怎么选：三条线，不要按“功能有没有”选

- **单中心 + 活跃视图几百 + 元数据几 GB 内** → **SQLite**（默认）。最省事、事务最大、单二进制；主备 HA 按需加。
- **要 8 路并行写 / 把备份与 PITR 交给数据库 / 已有 PG 运维** → **PostgreSQL**。连接墙已由 `--engine-idle-secs` 驱逐消解，写并行是它相对 SQLite 的唯一硬优势（~4.6k vs ~50）。
- **要存储层自愈 + 万级热视图 + 元数据独立扩容** → **FoundationDB**（实验档）。 代价是 10 MB 事务要分片（已自动，见 §6.3.4）、生产最小 5 节点、G3 性能门未过。

```
你的部署形态？
├─ 单机 / 单中心，活跃视图 < 千级，能接受主备切换 ─────► SQLite（默认）
├─ 要多写并行，或想把 HA / PITR 外包给数据库 ───────────► PostgreSQL
└─ 要存储层自动愈合，且视图上万 / 元数据要独立扩容 ────► FoundationDB（实验档）
```

---

## 5. 如何处理数据(重点)

### 5.0 视图元数据解剖(一屏速查)

一个存储根(`<meta-dir>` 或本地 `--home`)里到底存了什么、挂一个名字时怎么被解析:

```text
<meta-dir>/
├─ blocks/<2hex>/<key>      内容字节,一文件一对象(key=UUID,按哈希扇出 256 桶)
└─ meta/
   ├─ <view>.db             每个物理视图一棵权威树(SQLite WAL)
   │     node(id INTEGER PRIMARY KEY, ino, parent_id, name, is_dir, data_ref,
   │          link_target, mode, uid, gid, mtime, atime, ctime,
   │          UNIQUE(parent_id, name))   ← 树的全部:名字、类型、指向 blocks 的 key、POSIX 属性
   │     ctrl               (k,v) 键值表:今天只有 HA 的 epoch(0/缺行=没启用 HA)
   │     locks              POSIX/BSD 锁表(租约到期自动回收,release 是持有者终止信号)
   ├─ refs.db               跨视图共享对象持有者账本(GC 判「最后一个名字」的唯一依据)
   ├─ views/*.layout        派生/别名/fork 定义(派生=`base <B>` 行+规则行;别名=单行 `alias <R>`;fork=单行 `fork <B>`(三种行可写在文件任意位置,注释/空行跳过))
   └─ api_keys.db           API key SHA-256 摘要 + 调用审计(SQLite 后端的控制面;PG 后端进 grovefs_control)
```

- **逐字段释义**:`node` 的 13 个字段(含 `ino` 与 `id` 的分工、`mode` 只存权限位、时间是秒)、`ctrl.epoch`、`locks` 的 9 个字段(租约与 `sid` 续约)、`refs.holders` 的计数口径、以及 4 个索引各自支撑的读路径,见网页版 `docs.html` §8.1.1(含真实 `sqlite3` 查询回显);本节只给一屏速查。
- **名字解析链**:`mount --view X` → `M_OPEN(X)` → 服务端查 `meta/views/X.layout`:
  含 `base <B>` 行(派生) → 打开物理树 `meta/<B>.db`,把规则行套成 `LayoutMeta` 透镜,只读挂载;
  含 `alias <R>` 行 → 不套规则,名字直接落到真实树 `meta/<R>.db`,全树可写、真实名拒挂;
  含 `fork <B>` 行 → 直接打开它自己的 `meta/X.db`(独立树);
  无定义 → X 就是物理视图,直开 `meta/X.db`。客户端本地不需要任何规则文件。
- **同一棵树的两种后端**:SQLite(默认,一 view 一 `.db`)与 PG(`meta-server --meta-store postgres://…`,
  一 view 一 schema `grovefs_v_<hex(view)>`,账本为共享表 `grovefs_refs.holders`)是**二选一**替代关系;
  控制面同样跟随后端:`--meta-store postgres://…` 下 key/审计/视图定义全部进 `grovefs_control` schema(`api_keys`/`mcp_calls`/`view_defs`),本地不落任何控制面文件;
  CLI `view`/`apikey`/`mcp`/`data-server` 用 `--meta-store`(或 env `GROVEFS_META_STORE`)接同一份系统记录。
- **备份闭环**:`blocks/` + `meta/*.db` + `meta/refs.db`(可用 `grovefs refs --meta-dir D rebuild` 重算)
  + `meta/views/*.layout` + (按需)`meta/api_keys.db`——PG 后端下一个 `pg_dump` 同时覆盖 `grovefs_v_*` 与 `grovefs_control`,本地只剩 `blocks/` + env 要备——§5.4 给完整清单与停服/热备份两种做法。
- **定制↔存储的对应**:四种形态的完整对照(命令→生成文件/表、参数、常见坑)见 §3.0;§5.0 的 `.layout` 就是「如何定制视图」的落盘形态(规则四动词见 README
  「视图三问」与 §3.1/§3.2);fork 是唯一会新建物理树的定制方式(`view fork` 拷 `node` 行、字节共享入账本)。

### 5.1 物理布局

- **单对象**:`blocks/<2hex>/<key>`,一个文件 = 一个对象文件。对象按 key 哈希自动扇出进 256 个桶子目录(桶名是 key 哈希的前两位 hex),单层即止、不建深层树——单目录条目数不随总量增长,千万级对象下每个桶仅数万条,`ls`/`rsync`/备份工具不退化。桶名是 key 的纯函数,启动时预建 256 个,热路径不做 mkdir。
- **key 命名**:视图写入时生成 UUID 作为对象 key,存进视图 `data_ref` 字段。
- **条带化(大文件跨多 data-server)**:`data_ref` 编码为 `s<N>:<stripe_bytes>:<uuid>`,
  第 i 块 key = `{uuid}#{i}`,落在 `servers[i % N]`——**是列表下标,不是地址**。**无 `s` 前缀的 key = 单对象**(未启用条带的存储形态)。

### 5.2 多 data-server 条带(聚合容量/带宽)

```bash
# 挂载时给逗号分隔的多个 data-server 地址 → 自动启用确定性条带
$Y mount --view alice \
     --data-servers 10.0.0.5:50100,10.0.0.6:50100 --meta-server 10.0.0.5:50110 -m /mnt/alice
```

- 文件 ≥ 16MiB 才分块;小文件自然只落 chunk0(单节点)。
- **价值**:把一个大文件摊到多节点的**聚合容量**上。注意:单条已饱和的链路(loopback/单网卡)上不会有吞吐增益,须用真正独立的链路才能测出收益。
- 单地址 `--data-servers host:port` 就是未启用条带:整对象落那一台,key 不带 `s` 前缀。

### 5.3 删除 / GC

- `unlink`(删除文件)后,视图解除引用,后端据 key 尽力删全部分块;条带 remove 会展开删所有 chunk,**幂等**(残留块可被 GC 清)。
- **跨视图共享对象的引用账本 `refs`**:一个 `data_ref` 命名唯一一个底层对象,却可能被多个物理视图同时挂着名字(`view fork` 与 `view export/import --as-fork` 复制同一批 `data_ref`、派生视图/别名共享基视图字节;而 `grovefs import` 把同一源文件导进两个视图时各得一个**独立 key**——两处 `blocks/` 都是源文件的硬链接,字节仍是同一 inode、零额外占用,但账本按 key 分别计数)。视图内的引用计数看不见别的视图,单看它会误删他视图仍在引用的对象。故记账落在 `<base>/meta/refs.db`(PG 后端落共享表 `grovefs_refs.holders`):每个视图 ATTACH 该账本,holder 计数与节点增删进**同一个 IMMEDIATE 事务**,跨文件原子提交。物理删除的准确规则是「**该视图内此 key 的最后一个名字被摘除、且全局账本再无其它视图持有**」——只有此时才 `backend.remove`。视图 db 被 out-of-band 删除(`rm meta/x.db`、新 `--home`)只会让账本**高估**、对象多留不误删;三档都有这条命令,干的都是「读树重算、不读账本」:`grovefs refs --meta-dir D rebuild`(重写 `meta/refs.db`)、`refs --meta-store postgres://… rebuild`(重写 `grovefs_refs.holders`)、`refs --meta-store fdb://… rebuild`(重写集群里的账本子空间);方向始终是「多留字节」而不是少算,见 §15.8。账本里没有行的键即该视图私有(空账本不需要任何初始化步骤)。

### 5.4 备份与迁移

- **元数据无硬件绑定**:对象位置 = `blocks/<FNV1a(key)%256 两hex>/<key>`,由 `data_ref` 纯函数推出;`node` 行不存绝对路径/设备号/主机名,`ino` 也是 GroveFS 自有编号。换机器/换系统可直接用,只要元数据与字节一起走。
- **要迁的清单**:`blocks/`(字节)、`meta/<view>.db`(每视图树)、`meta/refs.db`(跨视图账本,可用 `grovefs refs rebuild` 重算)、`meta/views/*.layout`(派生/别名/fork 定义)、`meta/api_keys.db`(用过 key 才需要)。以上是 SQLite 后端清单;PG 后端下树、账本、定义、key、审计全在库内,随 `pg_dump`/`pg_restore` 迁移,本地只需搬 `blocks/` + env。
- **整目录冷拷**:停掉所有挂载与 `meta-server` 后 `rsync -a -H` 整个存储根;**勿裸拷运行中的 SQLite**(WAL),不能停机就 `sqlite3 <db> ".backup"` / `VACUUM INTO`。
- **单视图导出/导入**:`grovefs view --meta-dir D export --view V --out F` → 目标机拷 `blocks/` 后 `grovefs view --meta-dir D2 import --view V --in F`;保留 id/inode/硬链接/权限/属主/符号链接,原子替换(失败不伤旧树),`--as-fork B` 可恢复 fork 语义。详见 §3.4.2。
- **拓扑边界**:多盘 `--data-dir` 的数量与顺序要一致(等盘克隆原位顶替不影响定位,增删/调序才重映射;或合并成单 root);跨 server 条带 `s<N>:` 要保留 N 个 data-server 及顺序;整目录拷会把 HA `epoch` 带过去,旧主必须先停;PG 后端用 `pg_dump`/逻辑复制。
- **多基视图**:任意多个基视图各自一棵 `meta/<name>.db`,可只迁其中一棵,key 也可按视图分别绑定。
- **HA** 模式下另可由 meta-server 用备份 API 复制(见 5.5)。

### 5.5 HA 主备(meta-server)

```bash
# 主:读写,并把快照周期推送给备
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110
# 备:只读服务,主心跳丢失后自动提升
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主(可选 RPO=0):加 --sync-repl,写返回前同步等备落盘 ACK
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110 --sync-repl
# 真 fencing(可选,opt-in):加 --witness <逗号分隔的 data-server 仲裁者列表,推荐奇数 N>,多数派授权
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --peer 10.0.0.6:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
```

- 两个节点必须用**各自独立的 `--meta-dir`**(各存一份视图副本,节点间靠快照复制)。
- 客户端 `--meta-server` 支持故障转移列表:`--meta-server 10.0.0.5:50110,10.0.0.6:50111`——主挂了自动切到备继续读写。
- **`--sync-repl`(可选,opt-in,默认关闭)**:启用后每个写 op 在返回客户端前,同步推快照并等备 fsync + 原子 rename 的 ACK,把 **RPO 从 ~500ms 降到 0**。代价:写延迟多一个控制通道 RTT + 一次 `VACUUM INTO` fsync。备不可达/超时时**诚实降级为 async 并打 warn**(单 op 的 RPO 回落到 REPL_INTERVAL,不谎报 0)。由 `.build-bin/test_ha.sh` 段 7(RPO=0)/段 8(分区降级)证明。
- **`--witness`(可选,opt-in,默认关闭)**:指定一个或多个主备都能访问的 `data-server`(`host:port`,逗号分隔列表,推荐奇数 N;N=1 只要求 1 个仲裁者)作**独立仲裁者集合**。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予(meta 侧对全列表并发 fan-out、按多数派判决 `witness_quorum`;两个多数派必交于≥一个仲裁者,同值碰撞天然只有一主)。每个 data-server 维护一个单调 `fence` token(`OP_FENCE` 用 `fetch_max` CAS 折入并回传**折入前的值 `prev`**):claim 成功当且仅当 `prev < 我的 epoch`(我把 token 顶高了);`prev >= 我的 epoch` 说明有等于或更高的 epoch 已存在(对端胜出,或同值碰撞)→ 被 fence。用 `prev` 而非折后的 max,正是为了**区分同值碰撞**——仲裁者恢复时两半同时以 candidate 抢占,先到的赢、后到的看到 `prev == candidate` 被挡,绝不双主。在任 primary 每 tick 向多数派续一份**有界租约(lease,默认 3s)**(多数报 `prev > 我`则自降级);每个仲裁者**拒发重叠租约**——同一时刻至多一份活租约,故**活租约本身即独占权威的证明**,写请求由 primary 的**本地租约有效性**直接放行(不再每写一个仲裁 RTT,仅每续租一次,摊薄到整窗),被 fence 的过期主在租约到点后的下一次复证即拒;待提升 standby 也**非重叠**——须待前任租约到期才可抢占(`prev >= candidate` 同值碰撞/对端更高,或前任租约尚活则拒绝/延后)。元数据面复证拦不住内容字节(`write()` 先落 backend、再 debounced touch meta),故**数据面同遭门控**:客户端把实时 epoch 贴到每个写意图数据帧(create/write/truncate/remove/fsync),作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`,过期主无法绕过元数据面静默写入内容(被封帧向权威重取 epoch 并自动重试一次);且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化(claim 取独占锁会排在途写排空,而 witness 与存储同节点),故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合(注:数据面门控仅对 --witness 列表内的 data-server 生效,即 quorum 会顶高它们的 token;故应把 witness 与客户端实际写入的内容盘同节点部署,列表外的内容盘 token 恒为 0、数据面 gate 不生效,仅剩元数据面 fence 兜底)。**FAIL-SAFE**:一旦配置了 witness 却**组不到租约多数派且连续超过宽限(=PROMOTE_TIMEOUT 3s)**,节点无法证明独占权威,便**放弃写权限**——primary 自降级为 standby、standby 拒绝提升(**写冻结**,宁可牺牲可用性也不冒双写)。各 data-server 独立把 token 持久化到 `<root>/.fence_token`,**重启不会把 epoch 清零**、放过期 primary 复活。**相比单仲裁的可用性红利**:宕**少数**仲裁者仍能组多数续租、不冻结写;少数盘丢也不复活过期主(多数仍保单调 token);唯**失去多数**才 fail-safe 冻结。由此把双写窗口收敛到**租约安全余量内**(而非 peer-only 的不封顶):不重叠租约使对端只能在我方租约到点后取 token,而我方提前余量停止信任缓存租约;瞬时仲裁错误不误伤单写,持续不可达仍由宽限(=PROMOTE_TIMEOUT 3s)兼底。`test_ha.sh` 段 9(过期 primary 重启被 witness 挡成 standby + token 持久化)、段 10(杀单 witness→primary 自降级 & standby 拒绝盲升→恢复 witness→恰好一主)、段 11(3 仲裁者宕 1 仍续租不冻、宕 2 破多数才 fail-safe)证明。
- **诚实标注**:
  - 默认(async)**RPO ≈ 500ms**(主每 ~500ms 推一次快照),极端故障下最后 <500ms 内的写可能丢失;加 `--sync-repl` 可把这批写的 RPO 降到 0。
  - fencing 需要**仲裁者**:严格两节点、**不配 `--witness`** 时无法防脑裂(两半互断会各自认为自己是主 → 不封顶双写),这是 CAP/共识的硬边界,须部署第三方仲裁(witness 或第三节点)才能收敛。**配了 `--witness` 即 fail-safe**:仲裁者**多数派**不可达时冻结写而非盲写/盲升;primary 持一份不重叠的有界**租约**(仲裁者拒发重叠租约,多数活租约即独占证明),写由本地租约校验放行、每续租一次向全列表 fan-out,并对内容数据帧按 token 门控,把双写窗口收敛到**租约安全余量内**且保证收敛到单主。**多数派仲裁的可用性红利**:`--witness` 取逗号分隔的 N 个独立仲裁者(推荐奇数),权威需多数派 `N/2+1`——**宕少数仲裁者仍不冻结写**(仅失去多数才停写)、少数盘丢也不复活过期主,比单仲裁消除了 witness 这个单点可用性依赖(N=1 即单仲裁者、witness 本身成为可用性依赖:它挂→主备都停写)。

---

## 6. 安全:准入(token / API key)与视图访问控制

**权限栈一屏速查**(细节按行内指针展开):

```text
层  机制                       谁强制             粒度与语义                              详见
1  token(共享口令)            服务端握手         集群准入,不匹配 E_AUTH;uid 自报属 advisory   §6
2  --view 名                  服务端路由         树选择,不是身份:同 view 即同树               §6/§0
3  API key(--require-api-key) 服务端逐 op        逐调用方:scope 分平面(mcp|mount|any)、      §3.5.1/§6.1
                                                  --view 绑定拒开它树、--ro 拒写、
                                                  可撤销(活挂载 ≤1s 封停)、全量审计
✗  显示治理(派生/别名/fork)  ——                 不是安全边界:只改呈现,可见即可读、          §3.2-§3.4.1
                                                  可读即可写回(fork 内容除外)
```

中心 server 若裸监听 LAN,任何同网段主机都能按 key 读写对象 / 打开他人视图。加共享口令:

```bash
# 两个 server 用同一 token
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token s3cr3t
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token s3cr3t

# 客户端挂载带同一 token(同时覆盖数据面 OP_OPEN 与元数据面 M_OPEN)
$Y mount --view alice \
     --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token s3cr3t -m /mnt/alice
```

- token 不匹配 → 连接时直接 permission denied,不重试。
- 空 token(默认)= LAN-trusted 无鉴权,仅限可信内网。

**但 `--token` 是一份共享密钥**:拿到它的主机都能挂,审计上也分不出「是谁挂的」,要踢掉一个调用方只能轮换 token 并断全部人。需要**逐调用方可撤销凭据**时,把同一份 `grovefs apikey` 注册表当挂载面的门用(铸造/scope/审计/撤销的完整用法见 §3.5.1):

```bash
# 服务端:把 key 门与 token 叠加(两道都得过),或只上 key 门
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --require-api-key --meta-dir /data/cluster
# 客户端:呈递一个 `--for mount`(或 `--for any`)的 key
$Y mount --view alice --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 \
     --api-key '<id>:<secret>' -m /mnt/alice          # 或 export GROVEFS_API_KEY 不进 argv
```

与 token 的两处关键差异:① **可逐调用方撤销且封停活连接**——服务端每秒复证一次(有流量才查),被撤销的 key 在该挂载撤销后的首个服务端往返上被拒(E_AUTH/EACCES 携拒因原文),挂载打印原因后**自卸载**(不会留下 stale endpoint),而其他 key 的挂载照旧服务;② **可审计**——每次连(含被拒)落 `apikey calls`(method=meta/data,tool=M_OPEN/OP_OPEN,detail 记 view 与 uid)。两道门都是**入口门**;再叠加 key 的 `--view`/`--ro` 两个访问控制维度后,「谁能碰哪棵树、读写还是只读」已由凭据自身携带并在服务端逐 op 强制。

### 6.1 共享 view 与逐调用方差异化权限（视图绑定 / 只读 key）

**默认行为下 `--view` 不隔离 uid**：任何 uid 只要 `mount --meta-server <addr> --view alice` 都落到 alice 同一棵权威树上,看到彼此写入。这就是「多人共享一个 view」开箱可用的形态——元数据集中、内容集中、实时失效推送保证收敛。

> 若需在同一棵共享 tree 里区分职责（例如只读审阅者、或限制某调用方只能碰某棵视图），给每个角色铸一把**视图绑定 / 只读 API key**：`grovefs apikey create --for mount --view V [--ro]`——可撤销、可归属，`--view` 让 key 只能开那一棵物理视图（开别的在 `M_OPEN` 处 `E_AUTH` 拒），`--ro` 让服务端逐写意图 op 强制拒（详见 §3.5.1）。

整体逻辑流（隔离单位 = view 名，不是客户端）：

```text
客户端 A · uid=1000   mount --view alice ─┐
客户端 B · uid=2000   mount --view alice ─┤ 同一个 --view → 落进同一棵树；
客户端 C · uid=1000   mount --view bob   ─┘ 一个客户端也可同时挂多个 view（多对多）
        │
        ▼ TCP 帧 M_OPEN(view, token, uid, api_key)   （客户端从不开 db 文件 / 不持 PG 连接串）
┌─ meta-server（单一权威写者）—— 按序过门，再路由
│  ① token 校验     不匹配 → E_AUTH（集群准入，唯一确定性门；uid 自报属 advisory）
│  ①' api-key 门    服务端 --require-api-key 时：scope 非 mount/any、缺失/无效/已撤销 → E_AUTH（带拒因原文）
│                   key 若 --view 绑定：开其它视图同样在 M_OPEN 处 E_AUTH 拒
│                   活连接每秒复证一次（有流量才查），撤销即断开该挂载并使其自卸载
│  ② ro 写门        key 若 --ro：每个写意图 op 直接 E_NOTPERM（data 面内容写 EROFS），M_OPEN 回包告知只读挂载
│  ③ view 名 → 引擎缓存  首次 M_OPEN 打开已存在的树，之后同 view 命中同一实例
└──┬──────────────────────────────────
   ▼ view=alice                        ▼ view=bob
  alice 权威树（A、B 共享，写串行化）    bob 权威树（仅 C，与 alice 互不可见）
   ├─ SQLite（默认）：       一 view = 一个独立 db 文件  <meta-dir>/meta/alice.db · bob.db
   └─ PostgreSQL（--meta-store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62
```

> 数据库里独立存储的单位是「视图」而非「每个客户端」：SQLite 一 view 一个 `.db` 文件、PG 一 view 一个 schema（均非单张表）；数据字节走 data-server → 存进其 `--data-dir` 下的 `blocks/<key>`，树里只存 `data_ref` 引用。

客户端上报的 uid 默认 `geteuid()`，可 `mount --run-as-uid N` 显式覆盖——它只作为条目的属主记进锁仲裁与审计，属 **advisory**（TCP 客户端可伪造），**从不作为访问控制门**。确定性靠 `--token`/`--require-api-key`（准入），「谁能碰哪棵树、读写还是只读」靠 key 的 `--view`/`--ro` 绑定在服务端逐 op 强制。两个必须知道的行为边界：① **加锁类 op（`flock`/`F_SETLK`，含锁续约）归类为写意图**（它们要写中心 `locks` 表），只读 key（`--ro`）对它们的请求一律被拒（`E_NOTPERM`，客户端 FUSE 侧落 `EPERM`，区别于 HA 备机的 `E_ROFS` 切换信号）——只读角色若需共享锁做读协调，得用一把读写 key；② **撤销是 op 驱动**的（服务端每秒在流量上复证一次），被撤销/降级的 key 在该挂载撤销后的首个服务端往返即被拒，完全由内核缓存回答的读不需往返、故不被打断（下一次往返即封）。

### 6.2 PostgreSQL 元数据后端（可选）

`meta-server` 默认将权威 view 树存在 embedded SQLite（一 view 一 `<meta-dir>/meta/<view>.db`）；可将它切为 PostgreSQL：

```bash
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store postgres://user:pw@10.0.0.9:5432/grovefs
```

客户端挂载命令完全一致(`--meta-server host:port --view alice`),线协议不变。服务端内部将每个 view 映射到一个 `grovefs_v_<hex(view)>` schema（hex 保证 identifier 安全、注入无关）,根节点固定 id=1,`ROOT_INO` 与 SQLite 行为一致。建表为幂等 `CREATE ... IF NOT EXISTS`,无需预迁移脚本。rename/unlink/lock 等需多语句原子性的 op 走显式事务。

**注意**：
- **与自研 HA 不兼容**：`--meta-store postgres://…` + 任一 `--role standby` / `--peer` / `--witness` 会在启动时 fail-fast（`VACUUM INTO` 快照机制仅适用于 SQLite）。PG 高可用请交给 **PostgreSQL 自身的流复制 / logical replication / PITR**——本来就比文件快照更强。
- **删视图是一条命令**：`view --meta-store postgres://… rm --name V --base --data-dir R` 在**一个事务**里逐 key 减 `grovefs_refs.holders`（归零即删除）再 `DROP SCHEMA … CASCADE`，提交后回收无人持有的 `blocks/` 对象；`refs --meta-store … rebuild` 在静止时按存活树重算整本账。PG 的 DDL 是事务性的，所以这一档不存在 SQLite 档“删 db → 重建账本”的中间态（§15.8）。
- **空闲视图自动释放连接**：每个被挂载过的视图在 meta-server 里常驻一套引擎池（默认 8 写 + 4 读连接，`GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL` 可调）。`meta-server --engine-idle-secs N`（默认 300，`0` 关闭）在视图的最后一个挂载会话结束且静默 N 秒后回收引擎，连接随之关闭；下次访问自动重开，客户端无感（`evict` 套件实证：6 视图 76 连接 → 卸载回收后 4）。于是 PG 侧规划公式从「视图总数 × 12 ≤ max_connections」变为「**最近 N 分钟活跃视图数** × (写池+读池) + 突发溢出 ≤ max_connections」——视图总数不再被连接数封顶。挂载中的视图永不被回收（会话计数钉住引擎，哪怕无人读写）。
- **写路径也缓存 prepared statement**：读路径每连接缓存 ≤16 条（上面 §8），写路径对等——每个写连接缓存 ≤32 条（`WRITE_STMT_CACHE_CAP`，`INSERT`/按名 `lookup`/`DELETE`/`SET ino`/四种 `setattr`/`rename`/锁 op/三条账本语句）。`tokio-postgres`/`postgres` 0.19 **没有** `prepare_cached`，所以缓存是自建的：`WriteConn` 持有 `HashMap<String, Statement>`，`WriteTx` 用固有方法遮蔽 `Transaction::query/execute`，**事务体内的 SQL 一字不改**即命中缓存；预编译语句在 `COMMIT`/`ROLLBACK` 后仍存活（实测），只有 DDL 改了结果类型才会报 `0A000`，此时驱逐该条并回落一次 unnamed。实测（`bench_pg_conc.sh`，N∈{1,4,8}、T=8、6s、默认 `sync_commit=on`，两轮）：FUSE 台架写吞吐 **3,048–3,118 / 4,784–5,202 / 4,518–4,680 ops/s**；缓存是代码内置的（`WRITE_STMT_CACHE_CAP`，没有开关），对照组是同台架、同一 op 序列、无缓存构建的读数 **2,556–2,577 / 4,131–4,200 / 3,350–3,497**，单连接一次事务 577–670µs → 316–330µs，即 N=1 +18–21%、N=4 +15–24%、N=8 **+30–36%**，叠加 `GROVEFS_PG_SYNC_COMMIT=off` 只在 N=4 稳定有益（5,225 → 6,142，+18%；N=1/N=8 在抖动内，见 §8）；争用用例 180/180 单一赢家，`test_pg.sh` 24/24。
- **多个 PG 实例分片（原型，不是承诺面）**：`--meta-store` 接受逗号分隔的多个 URI，`postgres://…/db1,postgres://…/db2`。每个视图的 `grovefs_v_<hex>` schema 落在**恰好一个**实例上，选择是视图名的纯函数（FNV-1a 哈希取模），所以 `view create`、任意一台 meta-server、以及跨视图操作（graft/export/rm by name）**不需要注册表**就能指向同一处；`PgEngine::open` 是唯一的路由入口，`view list` 跨实例合并，控制面（`grovefs_control` 的定义/key/审计）钉在**第一个**实例上。单 URI 原样透传，一实例部署行为逐字节不变。

  边界（刻意如此，别当成还差一个开关）：① **一个分片 = 一份独立的「元数据 + 内容」域**——`grovefs_refs.holders` 这本跨视图账本在每个数据库里各有一本（SQL 事务不能跨库），所以共享同一批 blob 的两个实例会各自记账、互相不知道对方还持有，因此每个分片必须配自己的 data-server；② `refs rebuild` 因此**按实例运行**（多 URI 会被明确拒绝并给出逐实例的命令），`gc` 同理；③ fork/graft 的源与目标必须在同一实例（一条 `INSERT … SELECT` 跨不了库），不同实例会给出点名两个落点的错误而不是连错服务器；④ 改实例列表会**重映射**视图——重分片是一次数据搬迁，不是改个 flag。

  取证（`.build-bin/probe_pg_shard.sh`，两库 `grovefs` + `grovefs_shard2`、8 个视图）：s1–s8 的 schema 在 db1/db2 上「一个且仅一个」（`db1 + db2 == 1` 逐视图），4/4 分散；**同一个 meta-server** 服务两侧视图（db2 的 s7 与 db1 的 s8 各写 200 个 4KiB 文件，2.67–2.87ms/op），行数落在被路由的 schema；删空 db1 分片的全部名字后 db1 账本用到的 200 行归零而 db2 的账本总数一字未动（跨分片不串账）；`refs --meta-store "a,b" rebuild` 被拒绝并提示逐实例重跑。路由函数另有 6 条单测（稳定性与分散度、单 URI 透传、控制面首实例、fork/graft 同实例约束、空实例跳空）。
- 为什么不提供 Redis：K/V+TTL 与视图树不同构（事务/行锁/多列索引都是自然需要,硬塞只会同实例重建一个更弱的 PG）。
- 分场景的可执行命令（单机验证 / 生产 systemd / 巡检 / 备份恢复 / 删视图 / 能力边界）见 **§15.3–§15.9**。

---

### 6.3 FoundationDB 元数据后端（实验性，不构成支持声明）

> **这一档的状态：功能面全部判过，容量面判为未通过，因此不进 CANON、不进承诺面。**
> 支持的元数据后端仍是嵌入式 SQLite 与 PostgreSQL（§6.2）；元数据面的写并行只在单个 PG 集群内，跨集群分区元数据权威不在本轮承诺面；
> 口径出处是决策门 G3 的实测（2026-11-09，同机同场同 op 序列、RF=3 三副本）：FoundationDB 臂
> **24 个单元里最好一条 1601 ops/s**（`ssd` 引擎，多数单元 133–998），把盘从提交路径整个拿掉
> （`memory` 引擎）也只有 **1642 ops/s**，
> 而同一次运行里 PostgreSQL 臂达到 **4100–4900 ops/s**——门槛是"不劣于 PG 平台值 ~4.6k ops/s"，
> 实测最好一条仍差 2.9×、多数单元差 5–15×。且这台机器没有 SSD 介质（两块阵列卡后 `queue/rotational=1`），
> 计划里的 SATA/NVMe 档位无法在此复现。但归因数据提醒一句：光换盘不够——把盘移出提交路径
> 也只到 1642 ops/s，剩下约 3× 在实现侧。归因、复跑配方与转正条件都在
> `docs/fdb-keyspace.md` §14 v1.8/§15；SSD 介质重跑是必要前提，不是充分条件。
>
> 下面是这一档**当前真实的行为**（实验用途）：`meta-server` 也能把权威 view 树放进 FoundationDB——
> 一 view 一 subspace（键编码与事务粒度的逐项设计见 `docs/fdb-keyspace.md`）：

```bash
# 这一臂是 Cargo 可选 feature，默认关闭：不链 libfdb_c 的构建完全不进这段代码
# 客户端库不在系统路径时给一个目录即可（无需装包，见 §11.10 第 1 步）；
# 绑定已预生成（`vendor/foundationdb-sys`），所以构建期也不需 libclang
FDB_CLIENT_LIB_PATH=/path/to/lib cargo build --release --features fdb

$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///etc/foundationdb/fdb.cluster --token $TOK
```

- **依赖面**：这一臂**不依赖任何系统安装包**，运行与构建两面都是（指的是 `libfdb_c` 与 FDB 制品这一面；`mount` 对 setuid `fusermount` 助手的需求与后端无关，§1/§11.0）。运行：`libfdb_c.so` 是一个被 sha256 校验过的官方制品（§11.10 第 1 步），构建时 `build.rs` 把 `$ORIGIN/lib`、`$ORIGIN/../lib` 与那个目录一并写进 ELF 的 RUNPATH，所以二进制自己找得到它，**不需要 `LD_LIBRARY_PATH`、不需要 `ldconfig`**；把 `grovefs` 与 `lib/libfdb_c.so` 放进同一个目录（或 `dist/` 那种子在上一级的布局）就是一份自足的投放。构建：`foundationdb-sys` 默认会在构建期跑 bindgen（那就得装 libclang），本仓库用 `[patch.crates-io]` 把它换成 `vendor/foundationdb-sys`——同一份代码，只是把绑定预生成后签进仓库（来源、sha256、刷新与回退步骤见 `vendor/foundationdb-sys/VENDOR.md`）；因此 `--features fdb` 只需要一个 `libfdb_c.so` 所在的目录，不需要任何包管理器动作。
- **投放面**：上面两面仓库里已有一份现成的实体可验——`dist/grovefs-fdb-experimental-linux-x86_64/`（主二进制 + `lib/libfdb_c.so` + 官方 7.1.67 的 `fdbserver`/`fdbcli`/`fdbmonitor` + `README.md`/`BUILDINFO.txt`/`SHA256SUMS.txt`，外层 `…tar.gz.sha256` 固定打包件指纹），目标机只需 glibc >= 2.31、内核 FUSE，以及非 root 挂载时发行版自带的 setuid 助手 `fusermount`/`fusermount3`（有 `CAP_SYS_ADMIN` 时 `fuser` 直连 `mount(2)`、不需助手，已在 user namespace 里实测；与 §1/§11.0 同一句话：需要的是 libfuse 的用户态工具，不是开发包；这一条与后端无关，SQLite/PG 档同样如此）。它**仍是实验档的取证产物而不是发布版**（无 tag；二进制已在 clean 工作树上重编，实现内容对应提交 `ed87cbc`，但 G3 性能门仍未过），端到端 `env -i` 的证明脚本是 `.bneck/pkg_verify.sh`，逐项坐标见 `docs/fdb-keyspace.md` §14 v2.2–v2.9（v2.9 记的是包内二进制换到 `ed87cbc` 那一版的尺寸与指纹，v2.3 补上了 root/`CAP_SYS_ADMIN` 那一格与“构建也不需 `pkg-config`/libfuse 开发包”的实测）。
- **URI 形态**：`fdb://<集群文件路径>` 或 `fdb://?cluster=<路径>`；两者都给空时回退 `$GROVEFS_FDB_CLUSTER_FILE`，仍为空则拒绝启动（不猜默认路径）。集群文件是 FDB 自己的寻址凭证，grovefs 不解析其内容。
- **客户端零变化**：`mount --meta-server host:port --view NAME` 一字不变，线协议不变。只有 meta-server 需要能链接 `libfdb_c`；客户机不装 FDB、不持集群文件、不感知后端。
- **树是显式建的**：`grovefs view --meta-store fdb://… create --view NAME` 开出一个 subspace（写根行与两个分配器键）。`view create --manifest` 在此档**拒绝**——manifest 种的是 SQLite 文件；要内容就挂载后直接写文件。
- **迁移与回滚走同一对命令**：`view export` / `view import` 在此档可用，且与 SQLite 档产出**同一种**宿主无关 JSONL——一个 subspace 能被导出，一份 dump 能在 `view create` 之外开出一个 subspace，导入后从集群再导出的 dump 与源 dump 逐行相同（id/ino/硬链接/权限/属主/时间一律保留原值），两个分配器水位抬到导出最大值。回滚到 SQLite 就是同一份 dump 反向导入，迁移窗口内源存储不被写入、原树保留。字节不在这条通道上：它们归 data-server 的数据根（§3.4.2 有整条命令序列）。
- **控制面随元数据后端进集群**：view 定义/别名落集群 `ctl` 子空间的 `CtlDefTable`（`view_defs`/`view_def_ver`），API key 与审计落同一 `ctl` 子空间的 `CtlKeyTable`，与 `--meta-store postgres://…` 把控制面搬进 `grovefs_control` schema 同构（§6.2）。所以一个 FDB 集群是控制面的**唯一权威**：瞄准它的每台 meta-server/MCP/CLI 共享同一份定义与 key，治理命令（`view`/`apikey`/`mcp`）都需带 `--meta-store fdb://…`（或 `GROVEFS_META_STORE`）且**需要集群可达**，本地不落 `*.layout`/`api_keys.db`。`--meta-dir` 在此档只是部署身份（写者租约按它 fencing）兼 scratch/config，不再是控制面记录。热重载按集群写版本戳（对位 PG 的 `xmin`、SQLite 的 mtime）；存量本地 `.layout` 定义在首次以 `fdb://` 打开时一次性尽力上传（不覆盖集群已有定义）。
- **编码口径**：意图 = RecordLayer 式（分层键空间、记录整体入值、二级索引以独立键区间表达）；载体 = Rust `foundationdb` crate 的 tuple layer + subspace layer。官方 RecordLayer 是 Python/Java 侧的遗留绑定，本后端**不使用**它，也只如此表述——不得写成“用了 RecordLayer”。
- **`--engine-idle-secs` 在此档只管内存**：FDB 没有“每视图 12 条连接”可回收（一个集群文件共享一个 handle，socket 状态在客户端自己的网络线程里），驱逐释放的是进程内引擎（号段、分页游标、last-touch 戳）。挂载中的视图仍永不被回收，客户端无感（§12.1）。这三条都在 `fdb` 套件第 13 节实测：把阈值压到 3s，挂载期间静默 8s ⇒ 服务端日志里 0 条驱逐；卸载会话后 ⇒ `meta-server: evicted 1 idle engine(s) (>3s idle, no sessions)`；同一个视图重挂 ⇒ 读回同一个对象（重开对客户端透明）。PG 的连接数规划公式在此档不适用，容量看集群的 proxy/GRV 与 log 副本写吞吐。
- **高可用与备份归集群**：FoundationDB 自带三副本与协调层，快照用 `fdbbackup`。grovefs 自研主备（`--role standby` / `--peer` / `--witness`）与 `--meta-store fdb://…` 同时给会启动即 fail-fast，文案专指 FDB（§15.9）。
- **功能面与容量面的取证范围不同**：**功能**判据在两处集群都有读数：本机单节点（副本因子 1）跑过当前全部断言（含写者租约一节）与四套治理套件的 fdb 档；真四机跨故障域集群（`triple`、`fault_domains n1..n4`）跑过同一套断言并全绿（2026-11-10 实测 `pass=170 fail=0 SKIP=0`，那一轮之后套件又长出 scan / rm-base / 租约 / 备份恢复等新段），四套治理套件目前仍只有单机读数。本节开头那一组**吞吐**数字只有一个来源——决策门 G3 的同场对照（同一台机器、同一份 feature 构建的二进制、照抄平台值出处的 op 序列、`configure new triple ssd` 三副本）。因此这组数字回答的是「FDB 臂相对 PG 臂差多少」，不是可外推的部署容量。**副本失效、恢复与跨故障域拓扑行为已在真集群上另测**（2026-11-10，四台机器各有独立内核与自己的数据盘，`fault_domains n1..n4`）：进程失效（`pkill -9`）与整机失效（`virsh destroy` 掉一台）的可用损失分别是 0s 与 0–1s，且三台幸存者无需死机回来就能重组日志集与三副本；`fdbbackup`/`fdbrestore` 做到时间点恢复——快照版本之后写的键只能靠日志重放回来，停止版本之后写的键被回滚。取证入口 `.bneck/fdb3vm_up.sh`、`fdb3vm_fault.sh`、`fdb3vm_backup.sh`，逐条账目见 `docs/fdb-keyspace.md` §14 v2.4。**仍未测的只剩「部署容量」**：G3 未过（本机无 SSD/NVMe 介质），下面的数字只是同场对照、不是部署容量——直到 2026-11-12 才有真四机集群的同一口径对照（下一条）。
- **三副本真集群上的吞吐复测（2026-11-12）**：上一段那组 G3 数字来自**单机三进程**的 `triple`（用 locality key 伪造故障域）。这次换到**真四机集群**（`.bneck/fdb3vm_up.sh` 拉起的 4 个 zone、一份 VM 一个内核与自己的数据盘，`Redundancy mode ssd-2 / triple / Fault Tolerance 1 zones / Replication health Healthy`），用同一段 op 序列与同一份 feature 二进制做同场 PG/FDB 对照（`.bneck/bench_g3.sh` + `CLUSTER3=/data/grovefs/.fdb3vm/cluster3vm`，`N∈{1,4,8}`、`T=8`、6s、两轮）：

  | N | PG ops/s（两轮） | FDB ops/s（两轮） | FDB / PG |
  | --- | --- | --- | --- |
  | 1 | 1,886 / 2,141 | 459 / 171 | ~0.16 |
  | 4 | 4,502 / 4,798 | 498 / 234 | ~0.08 |
  | 8 | 3,872 / 4,583 | 306 / 351 | ~0.08 |

  判定 `G3_RED`（`fdb/gate=0.11`、`fdb/pg=0.10`）——与单机集群同一结论，换到真三副本**没有改变量级**。两个必须写明的口径事实：① **跳动很大**：单独用 `.build-bin/bench_fdb3.sh` 对同一个点跑 3 轮，`N=1` 得到 425 / 96 / 223、`N=4` 得到 141 / 375 / 450、`N=8` 得到 422 / 418 / 296（中位 223 / 375 / 418），也就是说单点差异可达 2–3×，**任何单次读数都不能当成容量**，要比就取多轮中位数并同时报散布；② FDB 臂的跑满时间明显超出时箱（8s 箱实测 13–19s），说明慢的那几轮里提交路径在排队，而不是驱动算错了。复跑配方：`bash .build-bin/bench_fdb3.sh`（内置 preflight，非 `triple` 或 `Replication health` 不健康就拒绝并报错，不会给出一个无从比对的数字；`RUNS=n` 取多轮，`N_LIST`/`T`/`RUN_SECS` 同 PG 台架）；带 PG 对照臂的版本是 `CLUSTER3=… bash .bneck/bench_g3.sh`（两臂交错、同一会话）。

#### 6.3.1 能力边界：三层各自负责什么

这一档有三层「高可用」相关的东西，责任方和失效域都不同，不能混成一句「FDB 档高可用」：

| 层 | 谁负责 | 这一版的实际行为 | 失效域 / 边界 |
| --- | --- | --- | --- |
| 元数据写者 | GroveFS meta-server + 一份 `--meta-dir` | **一个部署同时只有一个写者**：启动时在集群里取一行 writer 租约（键 `(root,"writer")/('lease', <canonical --meta-dir>)`，值 `(owner, expires_at_ms)`，owner = `host:pid:nonce`），续租周期 `max(TTL/3, 200ms)`；取不到就最多等 `GROVEFS_FDB_WRITER_WAIT_SECS`（默认 TTL+2s）再拒绝；运行中续租发现行已不属于自己就打印原因并 `exit 1`。TTL 默认 10s（`GROVEFS_FDB_WRITER_LEASE_SECS`） | 一个 `--meta-dir`。同一批子树被两个写者进程写是唯一不能正确的形态（定义已是集群共享的同一份，租约禁的是两个写权威而非各持定义），这正是租约禁止的 |
| FDB 集群 | FoundationDB 自己 | 节点数、副本因子、故障域全由集群的 `configure` 决定（`single`/`double`/`triple`/`three_data_hall`…），grovefs 既不参与也不改写拓扑；启动探针只读 `\xff\xff/status/json` 并报告当前拓扑 | 集群的协调层与日志副本。单节点集群能跑但没有冗余（唯一的 fdbserver 死掉时挂载点阻塞而不是报错，集群回来后同一个挂载点继续可用） |
| GroveFS 原生 HA | 本项目的 standby/peer/witness | 与 `--meta-store fdb://…` 组合时**启动即拒绝**：它搬的是 SQLite 快照、用 epoch 做主备切换，FDB 档的副本与恢复归集群 | 不适用 |

两个边界要单独说清，它们最容易被说错：

- **副本与故障域是集群的能力，不是 grovefs 的**。grovefs 不复制任何东西；`configure new triple` + 四个故障域的集群上，进程 `kill -9` 与整机掉电的可用损失实测为 0s 与 0–1s，三台幸存者无需等死机回来就重组日志集（取证入口 `.bneck/fdb3vm_up.sh` / `fdb3vm_fault.sh` / `fdb3vm_backup.sh`，账目见 `docs/fdb-keyspace.md` §14 v2.4）。同一份数据在单节点集群上就没有这层保护。
- **fencing 的粒度是「部署」，不是视图、也不是集群**：
  - 对「同一 `--meta-dir` 的第二个写者」是强制的：它在 `bind` **之前**被拒（端口从未监听），拒绝文案点名 `lease --release --deployment …` 出口；
  - 对「还活着的旧写者」是强制的：`view lease --release --deployment <dir> --force` 清掉活行后，旧持有者在下一次续租（≤ 一个续租周期，默认约 3.3s）打印 `meta-server: LOST the FDB writer lease …` 并以状态 1 退出，不会继续写；
  - 对「两个不同 `--meta-dir` 同时服务同一视图」**不强制**：租约按部署（`--meta-dir`）fencing、按设计不跨部署（控制面定义/key 则是集群共享的同一份），所以「一个视图只被一个部署服务」是运维约定（§6.3.3）；
  - 对**客户端**不强制：挂载点不携带租约或 epoch 令牌，围栏只保证旧服务端停止写，不保证旧客户端不重连到新服务端。

#### 6.3.2 部署与使用

最小可用部署是「一个 FDB 集群 + 一个 meta-server」：

```bash
# 1) 集群先起来，并且自报 available（单机开发集群的最短路径）
fdbcli -C /etc/foundationdb/fdb.cluster --exec 'configure new single ssd'
fdbcli -C /etc/foundationdb/fdb.cluster --exec 'status minimal'   # 期望 The database is available.

# 2) meta-server：启动探针在 bind 之前确认集群可用，再取写者租约
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///etc/foundationdb/fdb.cluster --token $TOK

# 3) 树是显式建的，客户端零变化
$Y view --meta-store fdb:///etc/foundationdb/fdb.cluster create --view alice
$Y --home $BASE mount --meta-server 127.0.0.1:50110 --view alice --mountpoint /mnt/alice
```

启动成功后 stderr 里有两行可核对（§17 的关键字表同列）：

```text
meta-server: fdb cluster up — redundancy single (ssd-2), tolerates 0 zone failure(s), 1 storage server(s), 1 coordinator(s)
meta-server: writer lease HELD for deployment /data/cluster (holder <host>:<pid>:<nonce>, renewed every 3s, expires 10s after the last renewal); …
```

集群不可用时**拒绝启动**而不是挂在那里等：探针有界（`GROVEFS_FDB_OPEN_TIMEOUT_SECS`，默认 10s；不可达集群实测 2.07s 返回），文案带上集群自己的解释并点名排障命令：

```text
--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (quorum_not_reachable: Unable to reach a quorum of coordinators.) — …
```

三个环境旋钮（进程级，meta-server 与 `view lease`/`scan` 读同一套）：

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `GROVEFS_FDB_WRITER_LEASE_SECS` | `10` | 写者租约 TTL。崩溃接管的上限就是一个 TTL；`0` 关闭租约（关掉的是防护本身，只用于测试或确定单实例的场合） |
| `GROVEFS_FDB_WRITER_WAIT_SECS` | TTL + 2 | 启动时等一个仍在被续租的行的上限， 超过即拒绝。设得比 TTL 小，会让「死进程的行还没过期」的重启被误拒 |
| `GROVEFS_FDB_OPEN_TIMEOUT_SECS` | `10` | 启动探针（读 `\xff\xff/status/json`）的超时 |

运维命令：

```bash
# 谁在写、还剩多久：STATE / DEPLOYMENT / HOLDER / EXPIRES
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster lease
# 清掉一个再也不会回来的部署（机器已毁）；活行要 --force 才清
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster \
        lease --release --deployment /data/cluster            # 过期行
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster \
        lease --release --deployment /data/cluster --force    # 活行 = 主动围栏
grovefs view --meta-store fdb:///etc/foundationdb/fdb.cluster scan   # 租约表是 KIND=lease 的一行，不是 corpse
```

多节点与多故障域：grovefs 侧**不需要任何额外配置**，拓扑全部在集群侧完成——每台机器一个 fdbserver、各自独立的数据目录，`fdbcli --exec 'configure new triple'` 之后用 `fault_domains` 把进程分配到不同故障域。meta-server 仍是一台（同一 `--meta-dir` 只能有一个写者）；多台 meta-server 只在「各自 `--meta-dir`、各自视图」的形态下成立，与集群的副本数无关。副本数、故障域与恢复策略改的是集群，不改 grovefs 的命令行。

#### 6.3.3 已知限制（当前版本）

- **meta-server 仍是单点**：租约保证不双写，不提供自动切换。进程死掉时，活挂载在服务中断期间的写立即 EIO（实测 1.5s），同址重启后同一个挂载点自动恢复；没有「第二个 meta-server 自动接管」的机制——租约只保证它不会在上一个还活着时启动。接管靠人（或编排工具）在同一 `--meta-dir` 上重启。
- **崩溃接管最多等一个 TTL**：`kill -9` 的进程不能自己还租，重启会等行过期（默认 TTL 10s；套件把 TTL 压到 2s 时四次实测 1640–1696ms 接管）。要更快的接管就调小 TTL，代价是调度抖动更容易让续租迟到——丢租约的进程直接退出，而不是降级继续写。
- **计划内重启同样要等一个 TTL**：`meta-server` 不捕 `SIGTERM`，`systemctl restart` 之后旧行仍要等到过期才会被新进程覆盖（最多一个 TTL；启动等 `GROVEFS_FDB_WRITER_WAIT_SECS`，默认 TTL+2，所以不会报错，只是慢）。要零等待就在旧进程已停后先 `lease --release --deployment <dir> --force` 再启动。
- **租约用各主机墙钟比较过期时间**：行里存的是 epoch 毫秒，读它的进程按本机时钟判断。主机之间时钟漂移接近 TTL 会让行提前或延后过期，部署要开 NTP。
- **「一个视图只被一个部署服务」靠约定**：租约按部署 fencing，不阻止两个不同 `--meta-dir` 打开同一视图并互相写（它们读的是集群共享的同一份定义，`view list` 一致；租约不跨部署）。
- **过期行不会自动回收**：进程被 `SIGTERM`/`SIGKILL` 收掉（meta-server 不捕信号，正常返回才走释放）或强制围栏留下的行一直列在 `view lease` 里（`STATE=expired`），直到同部署重启覆盖它、或被 `--release` 清掉；`view scan` 会把它计为一个 subspace，表尾标成 writer-lease subspace。
- **启动探针只回答「可用/不可用」**：不校验 schema 版本、不预热引擎，所以第一个请求仍可能撞上 OCC 冲突重试（errno 映射见 §12）。
- **`view lease` / `view scan` 需要集群可达**：不可达时命令失败并给出原因，不会打一张空表冒充「没有租约」。
- **客户端没有 fencing 令牌**：被围栏的是服务端进程；旧客户端重连到新服务端后照常工作，这是有意的（挂载点不该因为服务端换人而失效），但也意味着租约不是端到端的一致性屏障。
- **多机部署的真实形状（四机虚拟机实测）**：四台 guest 各跑一个 `fdbserver`（zone `n1..n4`），四个地址**同时都是 coordinator**（quorum 3）。好处是任意一台宕机集群仍可读写；代价是**每台既是协调者又是存储域**——“掉一台”同时掉一个协调者和一个故障域，`status` 里 `full_replicas` 从 4 降到 3 并保持 available，而不是掉一个副本（`.bneck/fdb3vm_lease.sh` 在持租约时 destroy 一台实测租约不误丢）。
- **缺协调者时管理命令明显变慢**：协调者不齐时 `fdbcli … status json` 每次约 5s（超时重试）才返回，`meta-server` 的启动探针（`GROVEFS_FDB_OPEN_TIMEOUT_SECS`，默认 10s）与 `view scan` 都按这个代价计；自动化脚本要给单条命令留 ≥15s 超时，否则会把“慢”误判成“挂”。
- **guest 上没有 fdbserver 的 systemd unit**：实验档的集群是手工 `setsid -f fdbserver -p <port> -C <cluster> -d <datadir> -L <logdir>` 拉起的，机器重启后不会自己回来（`systemctl list-units | grep fdb` 为空）。集群“在”但节点没回来时 `status` 只报副本不足而不报错，所以重启后必须逐台确认进程与 `status minimal`。
- **容量面仍未判过（G3）**：原因与数字在 §6.3 开头，这一档仍是实验档，不进 CANON。

#### 6.3.4 容量换算：一行多少字节、一个事务能写多少行

两个数不能混用，它们差三倍：

| 口径 | 每行引用一个不同对象 | 不引用对象 |
| --- | --- | --- |
| **落盘**（视图子空间的 key+value 逐键累加） | **292 B/行**（5 键/行：`n` 名、`d` 按 id、`h` 持有者、`r` 反索引、`i` ino 指针；根行与两个分配器另计） | **194 B/行**（4 键/行，没有 `r`） |
| **一个事务**（FDB 的 10 MB 事务状态；默认分批，见下） | **≤ 7,700 行** | **≤ 15,000 行** |

于是「10 MB 有多少行元数据」的答案是 **约 3.4 万行**（每行引用一个不同对象）或 **约 5.2 万行**（不引用），这是**落盘**口径——算一棵树在集群里占多大用这个。它**不是**一次 `view import`/`fork`/`graft` 能搬多少行：FDB 限制的是「事务状态」，除了写入的键值还含该事务**读到的范围**、账本计数与内存索引，实测是落盘字节的 2–3 倍。实测数据（单条命令全部可复跑，见本节末）：

- **落盘**：`t5000`（5,000 行、每行一个 `data_ref`）= 25,001 键 / 1,458,714 B ⇒ 292 B/行；`t12000nr`（12,000 行、无引用）= 48,002 键 / 2,326,965 B ⇒ 194 B/行；账本另计（`refs` 子空间每个不同对象一行、约 55 B，实测 1,001 键 / 55,055 B），所以「带引用」的真实占用是 292 + ~55 ≈ 350 B/行。
- **单事务边界**（`single ssd` 单节点，默认 10 MB 事务状态；超过就分批，这里是单事务一次能写多少行的实测边界）：`import` 带引用 **7,700 行提交、7,900 行 `2101`**；不带引用 **15,000 行提交、17,000 行 `2101`**；`graft` 6,000 行（6,000 个引用）**进空树提交、进 10,000 行的树 `2101`**——同一个计划成败只取决于目标树有多大，因为 `graft` 的同一事务还要扫目标树的整个命名空间（续 id/ino 分配器），**读集也计入这 10 MB**。
- **默认自动分批**：`import`/`fork`/`graft` 先按实测模型（`rows×625 B + 新账本对象×660 B + 事务内读到的行×240 B`）给整个计划定价，**装得下就仍是一个事务**（小 dump 的语义不变）；装不下就按 **70% 预算（7,000,000 B）**切分——带引用时每批 **5,447 行**（7,000,000 / 1,285），不带引用 11,200 行——每批一次提交。真集群实测（单机 `single ssd`）：**20,000 行带引用的 `import` 4 个事务、7.8s**（7,900 行就 `2101`）；**20,000 行的 `fork` 4 个事务、7.2s**；**8,001 个条目 `graft` 进 20,000 行的 fork 2 个事务**；**6,002 个条目 `graft` 进 28,001 行的 fork 2 个事务**（6,000 行进 10,000 行树即 `2101`）；`import --replace` 一份 20,000 行的树 4 个事务、9.6s。每批提交都打一行 `INFO fdb import/fork/graft: transaction committed … transaction=k of=K`。
- **分批仍然守住三件事**：① **根行最后写**——根行是「可挂载」的唯一判据，于是中途失败留下的是「没有任何挂载点能到达的键」（`view scan` 标成 `corpse`），而不是半棵能被客户端读到的树；报错给出「已完成 X of Y、第 k/K 个事务」并指明 `--replace` 重跑或 `view rm --name … --base` 清掉；`--replace` 对 corpse 本就是**续跑入口**（实测：`import` 在 2/4 个事务时被 `kill -9`，留下 54,472 键 / 10,894 条目的 corpse，`view scan` 标 `corpse`，同一条命令加 `--replace` 后 4 个事务写回 20,000 行、导出与源 dump 逐行相同，`refs rebuild` `0/0`）。② **账本不泄漏**——`put_row_tx` 只在「本视图首次给这个对象取名」时记一次账，所以分批与跨批硬链接天然正确；`--replace`/重跑的退订走 `unbook_refs`（256 个引用一个事务）再清前缀，而不是一个事务碰百万账本键（那本身就是同一面 10 MB 墙）。③ **硬链接跨批保住**——`graft` 的「源 ino → 目标 ino」映射作为上一批的**输出**传给下一批，跨批的两个名字仍指向同一个 inode（实测：6,002 条目 2 个事务、硬链接对分居两批，`ino` 相同）。实测五轮之后 `refs rebuild` 报 **`0 row(s) corrected, 0 row(s) deleted`**。
- **一个条目比一批装得下还大**：`graft` 把这种目录递归拆成「**只写它自己那一行**（保住 mode/uid/gid/时间戳——不这么做会被 `graft_ensure_dir` 以 0755 和当前 euid 重建）+ 每个子项一个单元」，于是 8,000 条目的目录可搬，而不是被拒绝。分批的 `graft` 若中途失败，**已提交的批次保留**，报错直接点名下一句该是 `--skip-existing` 续跑。

换算方法与复跑：`view scan` 给出每个子空间的键数与前缀，`fdbcli -C <cluster> --exec 'getrange "<prefix>" "<prefix>\xff" 200000'` 的逐行输出按「可打印字符 1 字节 + `\xNN` 3 字符」还原成字节数；行数与事务边界的实测配方是 `view import` 一份按行数生成的 dump（`.fdbtest/gen_dump.py`），二分到「提交」与 `2101` 的分界线。

#### 6.3.5 后续演进方向（未实现，按此顺序评估）

1. **接管自动化**：控制面在集群 `ctl` 子空间（定义/key/审计共享一份），租约可以上移到视图或集群粒度并配一个跟随者——发现行过期即接管，而不是等人重启。需要同时给客户端一个 fencing 令牌（或用 FDB 的 read version 做读一致性），否则「旧客户端 + 新服务端」仍是弱环。
2. **容量面转正**：G3 的判据与复跑配方在 `docs/fdb-keyspace.md` §14 v1.8/§15；必要条件是在 SSD/NVMe 介质上以 RF=3 重跑 `.bneck/bench_g3.sh` 达到 ≥4.6k（SATA）/ ≥9k（NVMe）ops/s，且同场 PG 控件不低于平台值。归因数据说明光换盘不够（`memory` 引擎 1642 ops/s），实现侧还有约 3× 的余量要处理。
3. **运维闭环收紧**：把「`fdbbackup` 拍快照 → `fdbrestore` 恢复 → 挂载校验」固化成 CLI（当前是 `.bneck/fdb_backup_mount.sh` 的配方）；为租约行增加显式回收（当前靠同部署重启覆盖或手工 `--release`）。

---

## 7. 只读挂载

```bash
$Y --home $BASE mount --view alice --mountpoint /mnt/alice_ro --ro
```
所有写操作(create/write/mkdir/unlink/rename/chmod/truncate 等)返回 `EROFS`(内核 `MS_RDONLY` + 文件系统侧守卫双保险)。适合分发/归档视图。

### 7.1 两处「看起来是 POSIX 但不是」的边界(实测确证)

- **权限位存储、显示、不强制**:`chmod`/`ls -l` 如实记录与回显 `node.mode`,但挂载准入**不看树内 mode**——同 uid 对 `chmod 444` 的文件追加写仍 `rc=0` 且字节落盘(`echo more >>` 后回读含新行;实测本机 `--home` 挂载)。真正的访问控制是:服务端 token/API key(§6)、`--ro` 挂载(上节)、`allow_other` 与否决定的挂载可见性、以及本机模式下 OS 对 `blocks/` 的文件权限。**不要把 `chmod` 当视图内的隔离手段**;跨 uid 读被拒是 FUSE 挂载可见性(未开 `allow_other` 时其他用户根本看不到挂载点),不是 mode 在起作用。
- **稀疏洞不省盘**:后端按完整对象存字节,`truncate -s 1M` 扩洞实际占满 1 MiB(`stat` 回显 `size=1048576 blocks=2048`,即 `st_blocks*512 == st_size`,无洞)。不要指望稀疏镜像文件(qcow2 等)少占网络/磁盘;需省空间先压缩/转置格式再导入。

两处均为当前版本设计取舍(对象存储模型不维护 extents,准入不走 POSIX mode 检查路径),列入遗留观察项(§5-1/§5-2),不影响数据完整性。

---

## 8. 性能旋钮

- `--threads N`(默认 4):FUSE 并发派发工作线程,每线程独立 `/dev/fuse` fd(`clone_fd`,Linux 4.5+)。`0` = 单线程模式(可作 A/B 对照)。运行内核低于 4.5 时自动退回单工作线程(不硬失败)。
- 客户端大 IO 自动分块并行:读 ≥1MiB 按 1MiB 切片并发,写 ≥4MiB 分片并发(零协议改动)。
- **小文件批量创建(组提交 + 读池跟 worker 数)**:同一父目录里的 `create` 被内核父 inode 锁串行化,所以「多线程建文件」的并行只可能落在**不同父目录**上(`cp -r`/`tar -x` 展开多层子树就是这种形状),而这正是唯一有并行收益的场景。这里削掉两个串行点:① **组提交**——`CREATE` 先入队,抢到 leader 标志的 worker 用一次 `BEGIN IMMEDIATE` 提交整队(`MetaProvider::create_nodes_batch`,SQLite 引擎实现;单个 create 自成 leader、批长 1,单线程不付额外延迟),批内重名只废那一条(自己回 EEXIST),不连带兄弟项;② **读池与派发同宽**——本地挂载把 SQLite 暖读池设成 `--threads` 的值(见下一条,`GROVEFS_SQLITE_READ_POOL` 仍可覆盖),否则十几个 worker 并发读时大部分要现场 `open` 一个连接。同一挂载点、每线程一个父目录、128 B 文件、`--threads 16`,创建速率(files/s,多轮复测取区间):T=1 **1.7k** / T=2 **3.0–3.2k** / T=4 **5.0–5.1k** / T=8 **5.4–5.9k** / T=16 **6.3–7.0k**(另一条形状——「只建不写」的 `touch`——见下一条,它不付 data 面那次对象创建)。两个串行点各自可复现地量出来:把读池显式压到一条(`GROVEFS_SQLITE_READ_POOL=1`)同一负载掉到 T=8 **约 1.4k** / T=16 **约 1.2k**,即超出的每个并发读都在现场 `open` 一条连接;把两个父目录并成一个(回到 VFS 串行)则各线程都在 ~1.9k files/s——**那不是文件系统慢,是 VFS 的形状**。计数证据(3200 文件、8 线程、`--stats`):`meta.create` 只提交 **1.0–1.1k 次**(平均批 ~3),`meta.setattr` 3200 次但 p50 只有 **50µs**——内核 `WRITEBACK_CACHE` 在 close 时回填 size 与 mtime,两者与库里已存的值相同时(同一长度、同一个 epoch 秒)既不 ftruncate、也不 touch、也不落库,所以这 3200 次里一次写事务都没有,`write.touch_mtime` p50 = 0(新建文件那一秒已记入,同秒首次写不再 touch;mtime 只有 epoch **秒**精度,同秒回写同一值只是放大)。这条路径上还有一项不属于串行点的成本已经消掉:CREATE/UNLINK/账本那几段固定 SQL 现在常驻连接(`prepare_cached`),不再每行现场解析——引擎级微基准(同一事务、同一批语句,N=1200)单条 create **83µs/行**、组 3 **38µs/行**、组 8 **25µs/行**;删除侧同理,单线程 `rm` 小文件 **3.9k 文件/s**(`meta.unlink` p50 135µs),并行删(每线程一个父目录)T=8 **4.8k 文件/s**。
- **一次 `create` 到底花在哪(SQLite 档,`--stats` 拆解)**：单机 release、同一挂载点、N=2000：`create` 端到端 **0.177ms/op**（5,660 ops/s，`meta.create` p50 86µs / mean 95µs）；同条件 `mkdir` **0.162ms**（p50 65µs）、`utime` **0.142ms**（`meta.setattr` p50 128µs）。**`create` 不碰数据面**：名字落库即返回，后端对象由首次 `write`/`truncate` 现开（各自的 `O_CREAT`），所以「只建不写」的创建（`touch`、空文件树 `cp -r`）既不付一次 open/close、也不占一个缓存 fd——`file − mkdir ≈ 15µs` 只剩 data_ref 与账本那一句 INSERT，`mkdir − utime ≈ 20µs` 是「INSERT 比一条 UPDATE 多」的那部分，剩下 ~100µs 是 FUSE 派发与内核 `LOOKUP`（`mkdir` 端到端 0.162ms 减其 `meta.create` p50 65µs，即 §8.2 的地板）。语义不变量不变：未写过的文件 `getattr`/`read` 都是 0 字节（对象不存在 **就是** 空文件）、`fsync` 成功、`unlink` 只删名字（账本里没有对象可回收），`du` 计 0 字节、`gc` 无孤儿可清。故障面也因此更干净：`data-server` 不可达时 `touch` 照样成功（名字只过元数据面），第一个真正跨数据面的 op 才报错——`fsync` 拿到 `EIO`（`resilience` 套件断言这两半；没 `fsync` 的写本就停在页缓存里，应用侧本来看不到这次故障）。同一二进制上「只建不写」的并发曲线（每线程一个父目录、`--threads 16`，files/s）：T=1 **3.3k** / T=2 **4.5k** / T=4 **7.7k** / T=8 **12.2–12.6k** / T=16 **13.3–13.9k**。**SQL 语句不是大头，卷上的提交与 fsync 才是**：同卷同 pragma 用 raw SQLite 打「一条 INSERT + 一条账本 INSERT + 一次 COMMIT」（`.build-bin/probe_wal_modes.sh`，每档 N=500，`/data` 是 xfs，libsqlite3 3.31.1）——`WAL+NORMAL` 每次提交 **p50 46µs / mean 404µs**（中位数里没有 fsync；均值与 `meta.create` 偶发的 59ms 尾刺都由周期 checkpoint 的 fsync 贡献，2,470 commits/s），`synchronous=OFF` **p50 30µs / mean 55µs**（18,159/s），`synchronous=FULL` **p50 16.7ms**（每次提交都 fsync，53/s），`journal_mode=DELETE+NORMAL` **p50 20.5ms**（37/s）；同一段代码放 tmpfs 上全部塌到 **~26µs**（37k/s）——fsync 就是那个数量级差。所以这一档**不提供 `synchronous=OFF` 这类降耐久旋钮**：它能快 3–7×，但代价是「提交返回后仍可能回滚」而不是「只丢最近几个提交」，与本节其余档位的耐久口径不一致。要更快就换介质或换后端（PG 写并行见 §6.2），而不是降耐久。两个脚本都可重跑：`.build-bin/probe_sqlite_create_breakdown.sh`、`.build-bin/probe_wal_modes.sh`。
- **元数据读并行**(SQLite 写不并行;PG 写并行见 §6.2):每个视图的 `MetaEngine` 除单条写连接外开一池暖读连接(**本地挂载 = `--threads`(默认 4);库内嵌/meta-server 默认 `READ_POOL_SIZE = 1`;`GROVEFS_SQLITE_READ_POOL` 覆盖,0–16,池空时按需开、用完归还**),`lookup`/`getattr`/`readdir` 与 nlink 解析走池,因此同视图并发读不必排在写锁后面;SQLite **写仍串行在同一权威连接上**(单写者语义不变),PG 写则另走 SERIALIZABLE 写池(§6.2)。池连接按 WAL 读事务读已提交数据,自己的写立刻可见(`cargo test` 内 `reads_through_the_pool_observe_committed_writes` 钉住这一点)。HA 备机/只读打开的引擎不预开池,首次读按需建立并归还,同样有界。PG 引擎(`PgEngine`)同样持一条对等的 `PG_READ_POOL_SIZE = 4` 只读连接池(`GROVEFS_PG_READ_POOL`,按 PG 事务读已提交数据,取不到只读连接时回退写连接以如实暴露故障),且每条读连接缓存自己的 prepared statement(每连接 ≤ 16 条:lookup/getattr/两个 readdir 页/锁探测/epoch 读),不再每次读都发 unnamed 语句让 PG 重解析重规划——同一挂载点 `--no-cache` 下连续 `stat` 实测 0.820ms → 0.528ms/op(−36% 延迟、+55% 吞吐,三次复测稳定);写侧 PG 另由 `PG_WRITE_POOL_SIZE = 8` 的 SERIALIZABLE 写池并行提交(§6.2)。
- **PG 提交耐久档**（`GROVEFS_PG_SYNC_COMMIT=off`，默认 `on`）:PG 写连接在会话级执行 `SET synchronous_commit = off`，COMMIT 返回时不再等本地 WAL 落盘。同台架对照（`bench_pg_conc.sh`，N∈{1,4,8}、T=8、6s，写语句缓存已在位）：**3,305 / 5,225 / 4,284 → 3,194 / 6,142 / 4,180 ops/s**——唯一稳定的收益是 **N=4（写池打满）的 +18%**，N=1/N=8 落在单点抖动内（−3% / −2%；更早一轮同样的格局是 N=4 独赢 +29%）。代价是**崩溃可丢最近若干已提交事务**，且配了 `synchronous_standby_names` 时也不再等备机确认——承诺 RPO=0 的部署必须留默认 `on`。SSI 隔离、跨视图账本、治理不变量与 `--sync-repl` 快照转发都不受影响（后者与 PG 后端本就互斥）；默认档下 `test_pg.sh` 与 `GROVEFS_PG_SYNC_COMMIT=off` 下各 24/24 通过。
- **目录列举分页 + READDIRPLUS**(不带开关即生效):`MetaProvider::readdir_page(parent, offset, limit)` 在 SQLite 与 PG 引擎里都下推成 SQL 页,FUSE 回调按 32→1024 几何块流式填回复缓冲,因此一个内核 READDIR/READDIRPLUS 请求付 `O(缓冲)` 而不是 `O(整棵目录)`;属性随目录项一次带回(内核 ≥4.6 的 `FUSE_DO_READDIRPLUS`,与上面能力位分属不同的 all-or-nothing 协商组,内核不支持则自然回退普通 READDIR)。三处关键实现:① **keyset 续读**——内核在「回复缓冲填满处」续读,偏移量与我们的页边界对不上,所以每 32 行记一个游标 `(parent, n) -> 第 n-1 行的名字`:任意续读先 `WHERE parent_id=? AND name>? ORDER BY name LIMIT n` 定位,再在内存丢掉不足 32 行的前缀;OFFSET 只作没有游标时(seekdir、游标被清)的回落,任何名字变更(create/unlink/rename/link)立即清空游标,绝不返回旧顺序。② **查询计划钉死**——列表 SQL 都带一个恒真的 `name>''`(名字不可能为空),它只负责把计划钉在 `UNIQUE(parent_id,name)` 上;否则 SQLite 会退到 `(parent_id,is_dir)` 前缀索引 + 临时 B 树排序,一次 OFFSET 页 13–55ms。③ **目录 nlink 走覆盖索引**——`getattr` 与 READDIRPLUS 的 `.`/`..` 要算目录 nlink(`2 + 子目录数`),靠 `idx_node_parent_kind(parent_id, is_dir)` 做纯索引扫描,100k 子项 **0.02ms**(逐行回表要 9.6ms/次,而且每个回复都要算)。实测(单机 release,同机同盘):含 10,002 个条目的目录 `ls -f` 基视图 **131ms**、100 规则派生 **105ms**,`ls -l` **162ms**;含 100,002 个条目的目录 `ls -f` 基视图 **1.19s**、fork **1.19s**、100 规则派生 **1.01s**(2s 内重读 **0.12s**)。5001 条目录 `ls -l` 本地 **2.56s**(`--no-readdirplus` 只留分页 5.22s);远端经 `M_READDIRP` 同目录 **2.29s**(只留分页 4.22s),输出均逐字节相同。
- **作用范围**(与上条同口径):只读连接池与 SQL 分页在两个存储引擎上都有——SQLite `MetaEngine` 与 PG `PgEngine`(前者按上一条口径定宽、后者 4 条只读连接且各自缓存 prepared statement,两者均可用 `GROVEFS_SQLITE_READ_POOL` / `GROVEFS_PG_READ_POOL` 调)。本地 `--home`(SQLite 或 PG)吃满分页与 PLUS;`meta-server` 侧并发 RPC 处理器吃各自后端读池,且 `M_READDIRP` 帧把分页下推到服务端,远端客户端每缓冲只取一页并协商 PLUS(同数据集远端实测 2.29s)。`--layout`/派生视图的 `LayoutMeta` 也覆写了 `readdir_page`:它把「按规则裁剪后的形状」**按父目录物化一份**(最多 4 个目录 / 40 万行,TTL 2s),每个内核缓冲只切一片——所以 100k + 100 规则首次 1.01s、2s 内重读 0.12s;超过 40 万行的大目录仍逐缓冲现算(正确,只是不缓存)。仍走 trait 默认实现(每缓冲一次全量列表、不协商 PLUS)的只剩没有覆写 `readdir_page` 的自定义 provider。元数据**写**并行只在 PG 后端(单集群内 SERIALIZABLE 写池,见 §6.2),SQLite 写仍串行;跨节点多权威仍未做,单权威下的**多 PG 实例分片**只有原型(`--meta-store a,b` 按视图路由,边界见 §6.2)。判据由 `cargo test` 内 `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` 与 `readdir_page_resumes_at_arbitrary_offsets_and_drops_stale_checkpoints` 钉住。
- `--no-readdirplus`:关闭 READDIRPLUS 协商,强制回退普通 READDIR+逐条 `LOOKUP`,供 A/B 基准排障(内核不认该能力时本来就是这个行为,此开关只是强制)。
- `--o allow_other` 等额外 fuser 选项透传给挂载。

### 8.1 分阶段延迟遥测(opt-in,排查瓶颈用)

挂载时加 `--stats` 可开启**分阶段延迟遥测**:它把一次读/写/元数据操作在各阶段的耗时分别记录进 HDR 直方图,离线聚合出 p50/p90/p99/max,用来**定位瓶颈到底在哪一段**(键解析 vs 数据后端 vs 远端 TCP 往返 vs 条带扇出 vs SQLite/PG 查询 vs SSI 重试…),而不是只看一个总墙钟。这是**白盒诊断设施,不是吞吐/可扩展性声明**——关掉时每个埋点仅一次 relaxed 原子读即跳过(不取时钟、不加锁、不分配),对热路径无可测影响。

```bash
# 卸载时把汇总报告打到 stderr(挂载进程 stderr)
$Y mount --view alice -m /mnt/alice --stats
# 额外周期性 + 退出时原子落盘到文件(CI/长时间采样)
$Y mount --view alice -m /mnt/alice --stats --stats-file /var/log/grovefs.stats --stats-interval 10
# 或不改命令行,用环境开关(GROVEFS_STATS=1)
GROVEFS_STATS=1 $Y mount --view alice -m /mnt/alice
```

- `--stats`:开启遥测(退出时打印报告;若同时给 `--stats-file` 再落盘一份)。
- `--stats-file PATH`:报告写入该文件(先写临时文件再 `rename`,并发 `cat` 不会读到半截);退出时也会刷新。
- `--stats-interval SECS`:>0 时起一个守护线程,每 SECS 秒重写一次 `--stats-file`(未给文件则跳过周期),便于不卸载也能采样。
- 隐藏控制节点 **`.grove-stats`**:仅在 `--stats` 打开时,根目录下出现这个只读合成文件,`cat` 它就是当前快照;卸载或未开启时**完全不出现**——它是纯客户端合成节点,从不写进元数据存储,因此不触碰单身份/无孤儿 `data_ref`/原子改名等治理不变量,也不给默认挂载增加可见项。

覆盖的阶段(`group` 分类):`op`(read/write 的 op_total——内核看到的端到端单 op 延迟,减去下面子阶段即 FUSE 回包/派发开销、key_resolve、backend_total、write.touch_mtime)、`data-io`(local/remote 的 read/write/len 往返、remote.pool_wait、remote.chunked、shard 的 read_total/write_total/server_op/split/concat)、`meta`(lookup/getattr/readdir/readdir_page/create/unlink/rename/setattr/statfs、db.sqlite_read/write_txn/finalize_count、db.pg_query/write_txn/pool_wait/ssi_retry、remeta.rpc)。非计时计数器:`sqlite read-pool lazy-opens`、`pg ssi-retries`、`remote reconnects`、`broadcast send timeouts`、`ha fencing demotions`、`ha quorum refusals`、`lock lease renewals`、`lock expired reclaims`、`gc objects freed`、`bytes read`、`bytes written`——用来给上面的耗时曲线**提供解释信号**(例如 pool_wait 尾部抬高→池太小;ssi_retry 计数上升→并发写冲突;remote reconnects 增长→后端在重启/漂移)。报告同时提供文本表与 CSV(`--stats-file` 内两段都写),便于离线绘图或 A/B 对拍。`bytes=` 列只在后端边界阶段(`read`/`write.backend_total`、`io.*`)累计,故 `op_total` 行显示 `bytes=0` 属预期——它表示“该阶段不计字节”而非“没读到数据”;时延分位数显示 `0` 表示低于直方图的 µs 分辨率(sub-µs),也不是漏采样。报告表头即把这两条口径写明。

> 注意口径:遥测数字是**单次挂载进程内**各阶段的实测分布,用于排障;它不等于、也不应用于论文里的吞吐/规模结论(本系统对吞吐/可扩展性刻意不作声明)。`errno 计数` 暂未纳入固定注册表(需扩 `Counter` 集与报告 schema),作为后续项。

### 8.2 读写/元数据加速(内核自适应 + 自动降级)

系统的加速改动都遵循**新内核走快路、老内核自动降级**的能力协商原则:FUSE 能力位在 `INIT` 阶段**逐位**申请(fuser 的 `add_capabilities` 每组 all-or-nothing,逐位申请使内核缺某一位时只掉那一位、不连累其余),内核不支持的位只是不生效,绝不影响其余路径;SQLite/PG 侧优化与内核版本无关。每次挂载(无论是否 `--stats`)都如实回显内核**实际授予**的能力位组合:

```
grovefs FUSE caps granted: ASYNC_READ=true BIG_WRITES=true PARALLEL_DIROPS=true POSIX_LOCKS=true FLOCK=true WRITEBACK=true ATOMIC_O_TRUNC=true AUTO_INVAL_DATA=true READDIRPLUS=true MAX_PAGES=true; max_write req 4 MiB
```

本机 5.4 与集群 6.8(Ubuntu 24.04)实测该行全部 `true`;内容只读(fork)挂载下 `WRITEBACK=false`,使被拒的写落在首个同步 `write()` 上。`ATOMIC_O_TRUNC=true` 时带 `O_TRUNC` 的打开在 `open` 内完成截断,省掉内核随后补发的一次 `FUSE_SETATTR` 往返(实测 `O_WRONLY|O_TRUNC` 打开从 5.4 的 131µs/op 降到 84µs/op);老内核不给该位时截断回落到 SETATTR 路径,两条路都能正确截断,不存在“申请了却当成给了”。多线程 `clone_fd` 依赖的 `FUSE_DEV_IOC_CLONE` 是 Linux 4.5+,运行内核低于该版本时挂载退回单工作线程并回显一行提示(而非硬失败)。

**元数据写尾延迟(P0)**:每个可写视图引擎起一条**后台 `wal_checkpoint(PASSIVE)` 线程**(每 1s 一次,每 ~30s 一次 `TRUNCATE` 收缩 `-wal` 文件),并把写连接的 `wal_autocheckpoint` 置 0——把原本内联在提交里的同步 checkpoint 停顿(实测曾贡献 `create` 单次 **125ms** 尾刺)移出写热路径。线程经 `Arc<StopSignal>` 随引擎 `Drop` 关停(不 join、不阻塞卸载),只读/备机引擎不启动。配合热读路径 `prepare_cached` 复用语句(避免每次重编译 SQL)。同一 3000 文件建目录负载 `--stats` 前后对比:

| 指标(µs) | 优化前 | 优化后 |
| --- | --- | --- |
| `meta.create` total | 1.64 s | **0.38 s**(↓4.3×) |
| `meta.create` p99 / max | 1151 / **125439** | **230 / 33535**(尾刺消除) |
| `meta.lookup` p50 | 39 | **17** |
| `meta.getattr` p50 | 66 | **36** |
| `db.sqlite_read` p50 | 44 | **21** |

**读缓存 TTL(P1)**:`--cache-ttl <秒>` 把内核 attr/dentry 缓存 TTL 从默认 2s 放大,削减对同一目录/文件的重复 `LOOKUP`/`GETATTR` 往返——适合读多或不可变树(如天文“数据不落地”的一次性流读)。安全边界:跨端新鲜度**仍由默认 `notify` 实时失效推送保证**,且新内核(≥3.15)协商到 `AUTO_INVAL_DATA` 后,size/mtime 变化会顺带丢弃对端陈旧页数据;`--no-cache` 恒把 TTL 钉为 0(完全旁路)。`run_all.sh` 的 `freshness`/`disp_fresh` 套件默认 TTL 未变下持续证明 <500ms 对端可见。**透镜自己那份物化列表也是缓存**:派生/`--layout` 挂载为把一次目录遍历从 O(n²) 压成 O(n),按父目录物化形状(4 目录 / 40 万行 / TTL 2s)切片供给,而对端改基树的事件不经过这副透镜,所以失效推送在丢内核条目的同时必须调 `flush_listing(坐标)`(客户端自备透镜走 `run_event_stream`,服务端渲染的派生视图走 `broadcast_derived_display`)——否则“推送失效”被缓存压回 TTL 量级。接口语义由 `tests/layout.rs::flush_listing_drops_the_materialized_shape` 钉住,端到端收敛由 `disp_fresh` 套件钉住(先断言“不 flush 就会 stale”,再断言 flush 后立即可见,避免空跑)。

**内容带宽整形(P1)**:`--bw-read <速率>` / `--bw-write <速率>` 给本次挂载的内容读、写各上一道客户端 GCRA 令牌槽——按实际交付字节铺开互不重叠的时间槽(读在回复前、写在落盘后),长跑速率精确等于上限,单个大于 1 秒额度的 op 最多提前 1 秒放行;超额 op 只睡到自己的时间槽,绝不丢字节也不截断,写侧延迟天然形成背压。语法:纯字节数或 `K`/`M`/`G` 后缀(1024 进制,如 `500000`、`50M`、`1G`),`0`/缺省=不限(热路径只多一次 `rate == 0` 分支)。回退:flag > `GROVEFS_BW_READ`/`GROVEFS_BW_WRITE` > 配置文件;非法速率在参数解析处即拒(静默忽略一个操作员以为已生效的上限更糟)。观测:`--stats` 报告的 `bytes read`/`bytes written` 计数可核对实际吞吐(整形开启时 `op_total` 含蓄意等待,`backend_total` 不含);回归 `bwlimit` 套件用 24MiB@8MiB/s 的下限断言钉住生效。**诚实边界**:这是客户端自限速而非隔离——绕开 FUSE 直连 data-server、或同一视图的多个挂载各自拿满额度;服务端无法把内容寻址的字节归到某棵视图上,故它永远不是集群级配额。元数据 op 不整形。

**大块 IO(P2,内核自适应)**:`max_write`/`max_readahead` 申请 4MiB,由 fuser 与内核按 `FUSE_MAX_MAX_PAGES` 各自截断——老内核(如 5.4)自动降到页上限,6.x+ 给到更大块=更少往返;`PARALLEL_DIROPS`(并发目录 op)与 `WRITEBACK_CACHE`(批量合并写)早已协商。大块读写已逼近底层文件系统地板,实测 FUSE 每 op 仅加 ~50–70µs。

> 诚实边界:上表是本机单进程 `--stats` 实测分布(存在 run-to-run 抖动,但 4.3× 的 create total 与 125ms→33ms 的尾刺收敛远超噪声,且机制可归因);这些是排障/调优观测,**不构成吞吐或可扩展性声明**。远程/PG 后端瓶颈是网络 RTT,须另用 `bench_remote.sh` 量测。

### 8.3 调优回路与旋钮生效层(方法论)

先找瓶颈再动旋钮;**每次只拧一个旋钮**,同负载对比 `--stats` 两窗数据再下结论。

**旋钮按生效层分,拧错层=白拧**:

| 生效层 | 旋钮 | 说明 |
| --- | --- | --- |
| 应用层(挂载进程自身) | `--threads N`(默认 4,每 worker 独立 `/dev/fuse` fd)、`--stats`/`--stats-interval`(纯观测零开销)、`--log-file` | 改 GroveFS 自己的并发与观测 |
| FUSE 协商层(受内核钳制) | `max_write`/`readahead`、`--cache-ttl`、`--no-cache` | 上限由内核 `FUSE_MAX_MAX_PAGES` 决定:老内核(如 5.4)申请 4MiB 也自然退化,不是 bug |
| 后端能力层(服务端配置) | `--data-servers A1,A2[,A3…]` 分节点、≥16MiB 自动条带、`--meta-store postgres://…` | 改容量与并行度(§5.2、§6.2) |
| 结构性(改代码) | `MetaProvider`/`StorageBackend` 换实现(§16.1) | 前层都不够用时才到这里 |

**最小回路**:① 定位现象——挂 `--stats` 跑一段真实负载读报告:`[io]` 看 backend_total/pool_wait 占比,`[meta]` 看每操作 p50/p99 与 ops/s,`[counters]` 看并行切片与 readdir 预取计数;② 判瓶颈——backend_total/pool_wait 占大头=网络 RTT 或后端;`meta.create` p50 高且 ops/s 上不去=SQLite 单写者串行;`dup>0` 占比高=内核缓存没吃到(考虑 `--cache-ttl`);③ 只拧一个旋钮,重启挂载生效(远程挂载 systemd `daemon-reload`+重启;本地挂载改 env/配置即可);④ 对比两窗,收益声称以同机同负载实测为准——手册里给的都是地板线,不是收益数字。

**已有实测地板(本机回环,跨机器须重测)**:大块读写 FUSE 自身开销每 op 仅加 ~50–70µs(§8.2);后台 WAL checkpoint 下 `lookup` p50 17µs、`getattr` p50 36µs(§10);远程链路瓶颈是网络 RTT,须用 `bench_remote.sh` 另量。低于地板的数字说明量的不是机制本身——跨机器对比前先跑 `dd` 走 ext4 基线,把磁盘本底扣掉。

---

## 9. 常用运维

```bash
$Y umount --mountpoint /mnt/alice        # 卸载(后台挂载进程随之退出)
fusermount -u /mnt/alice                 # 等价(前台挂载则 Ctrl-C 即卸载)
ps aux | grep grovefs                     # 查看在跑的 server / mount
```

> `mount` **默认转后台**:自我 re-exec 成独立会话进程,等挂载点在 `/proc/mounts` 真正生效才返回
> (脚本无需再 sleep 轮询),终端只打一行摘要(含 pid 与日志路径);后台进程的输出追加进
> `--log-file`(默认 `$TMPDIR/grovefs-mount-<挂载点>.log`,`--log-file /dev/null` 可完全静音);
> `grovefs umount -m MP` 卸载并终止它。`--foreground` 保留传统终端附着式(systemd `Type=exec`
> unit、调试、想看实时日志时用)。内核实现备注:session 仍走 `Session::notifier() + run()` 阻塞式,
> **不要**用 `spawn + BackgroundSession::join()` 当阻塞等待——`join()` 会立即卸载。
>
> 完整的部署教程见 **§11**,巡检/备份/升级/故障处理手册见 **§12**。

---

## 10. 验证与回归

全板一共 **34 段**:`build` → `unit` → **31 个套件** → `bench`。**前 33 段门控 GREEN,`bench` 只打吞吐参考值、永不门控**(只要纯功能回归就 `SKIP_BENCH=1`)。`pg`/`pg_parallel`/`control` 在无可达 PostgreSQL 的主机上自动 SKIP,不拉低 GREEN。`fdb` **不在这 34 段里**:它是实验档(§6.3),按名 `bash .build-bin/run_all.sh fdb` 才跑,跑成什么都不影响默认全板的颜色;显式跑时主机上无可达 FoundationDB 集群则整套 SKIP,置 `FDB_TEST=1` 时缺集群即判 FAIL(不允许把没跑过的后端算作通过)。

```bash
export LC_ALL=C LANG=C                                 # 仓库根目录下执行(PKG_CONFIG 不必设:无 crate 查 pkg-config,§1)
bash .build-bin/run_all.sh                # 全板 34 段(33 门控 + bench)
                                          #   build unit
                                          #   posix edge e2e remote errors resilience concurrent
                                          #   metasrv ha striping locks freshness disp_fresh
                                          #   layout fork viewrm gc bwlimit migrate import
                                          #   govcheck govtime grovesci mcp apikey mountkey
                                          #   config pg pg_parallel evict control
                                          #   bench(只打吞吐,不门控)
bash .build-bin/run_all.sh fdb            # 实验档 FoundationDB,不在 CANON(§6.3)
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 同上,去掉 bench
bash .build-bin/run_all.sh fork mcp       # 只跑指定套件(build 自动前置)
```

端到端演示脚本(视图隔离 + 实时失效推送)见 `.build-bin/test_metasrv.sh`。

### 10.1 三引擎完备性矩阵与跨机专项(不进默认全板)

默认全板的 `pg`/`pg_parallel`/`control` 只能回答“这一档能不能挂载读写”，回答不了“同一件事在三个后端上是不是都有出口”。后者由下面五份脚本给出，全部可复跑、全部自己清场：

| 脚本 | 量的是什么 | 最近一次实测 |
| --- | --- | --- |
| `bash .bneck/engine_matrix.sh` | 把**同一串治理序列**在 SQLite/PG/FDB 上各跑一遍，22 项能力并排成一张表：OK=能用、REFUSED=设计上不支持（引拒绝原文）、ERROR=缺口。跑的是**无 FUSE** 的纯治理面，所以 `graft` 的源是一份手写 seed dump（`data_ref` 为空，只量树搬运）；字节与账本那条路归 `forksrv`/`xfer` | 22 行全部 OK/REFUSED，**无 ERROR**；末尾 `post-teardown` 校验 `sqlite 树文件 0 / pg schema 与账本回到基线 / fdb 树子空间 0` → `TEARDOWN CLEAN` |
| `bash .bneck/pg_view_teardown.sh` | PG 基视图的事务性删除 + 账本双向修复，全程真 FUSE 挂载、真 data-server、真 PG | 44 pass / 0 fail（含 `schema dropped with 6 name(s), 4 object(s) no longer held by any view`、`3 row(s) deleted`、`2 row(s) corrected`、二次删除被拒） |
| `bash .build-bin/test_pg.sh` / `test_pg_parallel.sh` | PG 档 e2e（并发、驱逐、控制面）；末尾用 `view rm --base` 自清场 | 24 / 41 断言全过；跑完不留**本视图**的 schema 与账本行（共享集群上别的套件留下的孤儿不再让这条断言误红） |
| `bash .bneck/fdb3vm_lease.sh` | 四机虚拟机集群上的跨机租约：同 `--meta-dir` 拒绝 / 不同目录并行 / 持租约时 destroy 一台 / 持有者与观察者时钟差 TTL/2 | 59 pass / 0 fail |
| `bash .build-bin/test_fdb.sh`（在 guest 上跨机复跑） | FDB 档 275 断言（含 §10d fork/graft 复制子空间、§12 双向迁移、§16 租约、§13 驱逐语义） | 275 OK / 0 fail |

> **PG 跨机部署也已实测**（一次性隔离探针，不进默认全板）：把一份独立 `initdb` 的 PG 放在 HOST（`192.168.122.1:5439`，`listen_addresses` 仅该网段、不碰共享的 loopback `5433`），guest `gfs1`（`192.168.122.124`）跑 `meta-server --meta-store postgres://…@192.168.122.1:5439/grovefs` + `data-server` + 客户端 FUSE 挂载。证据：HOST 侧 `pg_stat_activity` 见 guest IP `192.168.122.124` 的 20 条常驻连接（视图 8 写+4 读暖池 + 控制面/账本），guest 挂载写入的 4 个 `node` 行（`/`、`hello.txt`、`sub`、`sub/n.txt`）与 2 个 `grovefs_refs.holders` 持有者全部落在 HOST 的 PG 里，写/读/`mkdir` 全过、字节留在 guest 的 `data-server`。即「元数据库在另一台机器」对 PG 成立，与 FDB 的四机集群同属跨机部署。探针脚本、隔离库与 guest 侧产物均已清场，共享 `5433`/`5434` 未受影响。

矩阵的结论（也是“完备性是否满足”的答案）——22 行，三档**零 ERROR**：

| 能力 | SQLite | PG | FDB |
| --- | --- | --- | --- |
| `view create`（建基树） | OK | OK | OK |
| `view define`（服务端派生透镜） | OK | OK | OK |
| `view alias`（纯别名） | OK | OK | OK |
| `view fork`（独立元数据树） | OK | OK | OK |
| `view list`（定义 + 裸基视图） | OK | OK | OK |
| `view export`（元数据 dump） | OK | OK | OK |
| `view import`（dump → 树） | OK | OK | OK |
| `view import --as-fork`（dump → fork，保留 fork 语义） | OK | OK | OK |
| `view import`（手写 seed dump，空 `data_ref`） | OK | OK | OK |
| `grovefs import`（目录→视图，零拷贝） | OK | REFUSED | REFUSED |
| `view graft`（挑选子树接到另一棵树） | OK | OK | OK |
| `refs rebuild`（跨视图账本重算） | OK | OK | OK |
| `gc --data-dir …`（就地扫本机 `blocks/`） | OK | REFUSED | REFUSED |
| `gc --data-servers …`（经 data-server 枚举孤儿，含 `--apply`） | OK | OK | OK |
| `du`（字节总量，树来自后端、字节问 data-server） | OK | OK | OK |
| `du --view <不存在的名字>`（no-tree 守卫） | REFUSED | REFUSED | REFUSED |
| `apikey list`（凭据登记册） | OK | OK | OK |
| `apikey calls`（MCP 调用审计） | OK | OK | OK |
| `view scan`（集群清点） | REFUSED | REFUSED | OK |
| `view lease`（写者租约行） | REFUSED | REFUSED | OK |
| 原生 HA（`--role standby`） | OK | REFUSED | REFUSED |
| `view rm --base`（连字节一起拆树） | OK | OK | OK |

`apikey`/`audit` 两行的“OK”是**同两条命令、三个落点**：SQLite 写 `<meta-dir>/meta/api_keys.db`，PG 写 `grovefs_control` schema，FDB 写集群的 `ctl` 子空间；`du`/`dumiss` 的树也分别来自这三处（字节统一向 data-server 问）。

> **这张表回答“能不能跑”，不回答“怎么写”**：同一件事在三档的命令形态、以及六处只有一档能做的事，见 §4.1.3；为什么选某一档（写吞吐 / 事务上限 / 可用性 / 规模）见 §4.1.1。

剩下的 REFUSED 分成两种形状，都是**设计边界**而非缺口，每条都能给出“为什么”和“替代做法”（§15.9 逐行写全）：

- **跑在本地数据根上的两个动词**：`grovefs import`（目录零拷贝入库）与 `gc --data-dir`（就地扫 `<data-dir>/blocks/`）天然依赖“树与字节在同一台机器”。PG/FDB 部署的字节归 data-server 所有，对应的形态是远程 `gc --data-servers`（三档均 OK），目录入库则走挂载点 `rsync`/`cp` 或“本地 import → `view export` → `view import`”三步。
- **后端专属的运维面**：`view scan`/`view lease` 清点的是 FDB 集群里的子空间与写者租约（SQLite/PG 没有这层概念），原生 HA 的快照转发是 SQLite 文件形态专有（PG/FDB 各有自己的 HA 工具链）。

`du --view <不存在的名字>` 的 REFUSED 不是缺口而是**本轮补上的对齐**：以前同一个拼错的名字，SQLite 报 `sqlite open …: No such file or directory`、PG 报 `pg query: db error`（`42P01` 被折成一句话）、FDB 干脆回 `0B`——与“真的空视图”无法区分。现在三档都是同一句（矩阵 `dumiss` 行钉住原文）：

```
error: no tree for view 'mxnone' in <store> — `grovefs view create --view mxnone` makes one,
       and `grovefs view list` shows the names that exist
```

三档共同拥有的是**控制面 + 树 + 账本 + 基视图拆除 + fork/graft + export/import + 远程 gc + du + apikey/审计** 这些事，也就是一个元数据引擎至少要有的那一部分（`forksrv`/`xfer`/`gcremote` 三份 e2e 逐档断言，其中 `xfer` 覆盖 `--as-fork`/`--replace`）。单事务能装多少行、10 MB 折合多少元数据行，见 §6.3.4——那是“容量”这一个维度的口径，与这里的“能力”维度正交。

> 矩阵要 `LD_LIBRARY_PATH` 能找到 `libfdb_c`（脚本会自己试 `.fdbbin/lib` 与 `target/fdb/release/lib`，都找不到时 fdb 一列会整列报库错误）。矩阵跑之前要确认本机没有残留的 grovefs 挂载/服务，否则 PG 列的 schema 计数会把并发运行的那个算进去。脚本自己 `unset GROVEFS_META_STORE`（每臂逐命令显式传 store），所以从任何 shell 跑都一样；它还需要 5433 的 PG 与 `fdb:///data/grovefs/.fdb3vm/cluster3vm`（`PGSTORE`/`FDBSTORE` 可覆盖）。跑完的 `/tmp/engmx/*.log` 留着逐格原文。

---

## 11. 手把手部署教程(生产环境,从零到验收)

> 本章按顺序走一遍真实部署:预检 → 构建安装 → 规划 → token → systemd 常驻 → 客户端挂载 → 防火墙 → 验收;
> §11.8 是 HA 主备拓扑,§11.9 是 PostgreSQL 后端拓扑。示例 IP/路径请按实际替换。

### 11.0 部署前预检(每台节点都跑一遍)

内核 ≥ 4.5 才能用多线程派发(`clone_fd` 依赖 `FUSE_DEV_IOC_CLONE`);低于 4.5 时 `mount` 自动退回单工作线程(并回显一行提示),仍能正常挂载读写,只是不并行。`/dev/fuse` 存在可读写、`fusermount`/`fusermount3`
在 PATH 里或可从绝对路径命中(**非 root 的挂载与卸载都走这个 setuid 助手**;运行时只需 libfuse 的用户态工具,**不需要** libfuse 开发包;有 `CAP_SYS_ADMIN` 时直连 `mount(2)`,不需助手)。
构建机额外需要 Rust 工具链(edition 2021)——**不需要 pkg-config、也不需要 libfuse 开发包**(实测 `PKG_CONFIG=/nonexistent cargo build --release` 全量通过,产物 `DT_NEEDED` 不含 fuse 库)。

```bash
uname -r                                   # 期望 >= 4.5
ls -l /dev/fuse                            # 存在且当前用户可读写
which fusermount fusermount3 || true       # 至少有一个
```

数据盘要求:`data-server --data-dir`(本地模式为 `--home`)所在文件系统要有真实容量——
文件的字节全部以对象形式落在其 `blocks/` 下,**不要**把数据根建在 tmpfs 上。

### 11.1 构建并安装二进制

```bash
cargo build --release                                    # 产物:target/release/grovefs
sudo install -m 755 target/release/grovefs /usr/local/bin/grovefs
grovefs --version
```

> 仓库自带的 `.build-bin/pkg-config` shim 是**历史遗留**(早期 fuser 走 libfuse 后端时才用),现行依赖图里没有 crate 查 pkg-config,挂不挂它都照样构得成。
> 每台节点(中心 + 客户机)装同一个二进制即可,子命令决定角色。

### 11.2 规划:节点、目录、端口

本教程拓扑(最常见形态:单中心节点 + 多客户机):

| 节点 | 角色 | 目录/端口 |
| --- | --- | --- |
| 中心 `10.0.0.5` | `data-server` | 数据根 `/data/cluster/blocks`,监听 `0.0.0.0:50100` |
| 中心 `10.0.0.5` | `meta-server` | `--meta-dir /data/cluster`(权威视图在 `/data/cluster/meta/<view>.db`),监听 `0.0.0.0:50110` |
| 客户机 `10.0.0.21…` | `mount` | 远程挂载**不需要 `--home`**(本地什么都不落盘),挂载点 `/mnt/<view>` |

两个 server 的 `--listen` 缺省值是 `127.0.0.1`(data 50052 / meta 50053),**生产必须显式给 `0.0.0.0:<port>`**,否则客户机连不上。

### 11.3 生成共享 token(强烈建议)

中心 server 裸监听 LAN 时,同网段任何主机都能按 key 读写对象/打开他人视图。生成一份随机口令,三方(两个 server + 所有客户端)一致:

```bash
sudo mkdir -p /etc/grovefs
sudo bash -c 'umask 077; echo "GROVE_TOKEN=$(openssl rand -hex 16)" > /etc/grovefs/grovefs.env'
sudo chmod 600 /etc/grovefs/grovefs.env
```

把同一个 `grovefs.env` 分发到每台节点(root-owned 0600)。token 一致即可;空 token = LAN-trusted 无鉴权,仅限完全可信内网。

### 11.4 中心节点:systemd 常驻两个服务

两个 server 都是前台阻塞式进程,`SIGTERM` 即干净退出;systemd `Type=simple` 直接托管。

```ini
# /etc/systemd/system/grove-data-server.service
[Unit]
Description=GroveFS data-server
After=local-fs.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/cluster --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/grove-meta-server.service
[Unit]
Description=GroveFS meta-server
After=local-fs.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-data-server grove-meta-server
systemctl status grove-data-server grove-meta-server    # 两个 active (running)
ss -lntp | grep -E '50100|50110'                        # 两个端口在听
```

> `meta-server` 的元数据根用子命令参数 `--meta-dir`(优先于全局 `--home`;`--home` 是全局默认根);`data-server` 的数据根是 `--data-dir`,两者互不相干,都不需要全局 `--home`。

### 11.5 客户机:挂载也交给 systemd(模板实例,一个 view 一个实例)

mount 单元带 `--foreground` 保持进程附着(`Type=exec` 要盯住服务进程本体,不能让它转后台即退);
内置 SIGTERM 处理,`systemctl stop` 会自动 `fusermount -u` 干净卸载,不留死挂载。

```ini
# /etc/systemd/system/grove-mount@.service   —— %i = 视图名,挂载点固定 /mnt/%i
[Unit]
Description=GroveFS mount view %i
After=network-online.target grove-data-server.service
Wants=network-online.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
# 上次异常退出可能留个死挂载,先清掉(- 表示失败也继续)
ExecStartPre=-/usr/bin/fusermount -u /mnt/%i
ExecStart=/usr/local/bin/grovefs mount --view %i --foreground --data-servers 10.0.0.5:50100 --meta-server 10.0.0.5:50110 --token ${GROVE_TOKEN} -m /mnt/%i
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl start grove-mount@alice          # 挂 alice 视图到 /mnt/alice
sudo systemctl start grove-mount@bob            # 同一台机再挂 bob,互不可见
findmnt -T /mnt/alice                           # 源显示 grovefs:alice,FUSE 类型
```

注意两点(都与 uid 有关,见 §6.1):

- systemd 默认以 root 起挂载进程,上报给 meta-server 的 uid 是 **0**;如需以固定角色挂载,可在 ExecStart 加 `--run-as-uid N` 指定该挂载的属主槽位(记进锁仲裁与审计)。
- 需要**其他用户也能访问**挂载点时加 `--o allow_other`,并确认 `/etc/fuse.conf` 打开了 `user_allow_other`。

想改成**逐调用方可撤销凭据**(踢掉一台机不再需要轮换共享 token 并断全部人):两个 server 的 ExecStart 各加 `--require-api-key`(data 侧另需 `--meta-dir` 指到元数据根,若它不等于首个 `--data-dir`),客户端把 `--token ${GROVE_TOKEN}` 换成 `--api-key ${GROVE_API_KEY}` 并把该 key 用 `grovefs apikey create --label <主机名> --for mount` 铸造后写进同一份 root-owned 0600 的 `grovefs.env`(密钥因而进不了 `ps` 可见的 unit 文本)。撤销后的真实行为(实测):该挂载在撤销后的首个服务端往返上打印服务端拒因(`grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view`)、**干净自卸载并以退出码 0 结束**——一次有意的收手而不是崩溃,所以 `Restart=on-failure` **不会**重拉,unit 落在 `inactive (dead)`(`systemctl status` 看起来像运维手动卸过,要辨“被踢”请看 journal 里那行拒因,或挂 `OnFailure=`/`ExecStopPost=` 告警);拿被撤销的 key 重挂则 fail-fast。若希望被踢后自动重试(例如运维会重新铸一把),改用 `Restart=always`;想保持一次就停则无需额外配置。其他主机的挂载全程不受波及——这正是逐调用方凭据相对共享 token 的全部价值。

### 11.6 防火墙:只对内网网段开两个服务端口

```bash
# ufw 示例(按实际网段改);两个 server 同为帧协议,端口不同
sudo ufw allow from 10.0.0.0/24 to any port 50100 proto tcp
sudo ufw allow from 10.0.0.0/24 to any port 50110 proto tcp
# firewalld 等价:
# sudo firewall-cmd --add-rich-rule='rule family=ipv4 source address=10.0.0.0/24 port port=50100 protocol=tcp accept' --permanent
# sudo firewall-cmd --add-rich-rule='rule family=ipv4 source address=10.0.0.0/24 port port=50110 protocol=tcp accept' --permanent && sudo firewall-cmd --reload
```

不要把 50100/50110 暴露公网;token 只是第二道防线,不是传输加密(帧协议不加密,TLS/隧道属未实现项)。

### 11.7 验收清单(逐条打勾,全部命令都是实命令)

```bash
# 1) 两个 server 存活且监听
systemctl is-active grove-data-server grove-meta-server && ss -lntp | grep -E '50100|50110'

# 2) 视图已挂上、打了 grovefs 标签
findmnt -T /mnt/alice          # 期望:FUSE / grovefs:alice

# 3) 新视图需先显式创建:名字不在时 M_OPEN 直接拒绝(无开关),
#    挂载本身从不建树。建完再挂:
#    grovefs view --meta-dir /data/cluster create --view carol
#    预建层级(--manifest)只在本地模式用,见 §3
sudo grovefs view --meta-dir /data/cluster create --view carol
sudo systemctl start grove-mount@carol && ls /mnt/carol

# 4) 跨客户端实时可见(两台客户机挂同一 --view,核心卖点)
#    客户机 A:
echo hi-$(date +%s) > /mnt/alice/from_a.txt
#    客户机 B(写完立刻,不等轮询):
cat /mnt/alice/from_a.txt      # 期望:即时看到同内容

# 5) 分布式锁跨机生效
#    A:flock -x /mnt/alice/lk -c 'sleep 30' &   # B 此时 flock 应阻塞,30s 后放行
#    B:flock -x /mnt/alice/lk -c 'echo got'      # 期望:等 A 退出后才打印

# 6) 字节确实落在中心,不碰客户机盘
ls /data/cluster/blocks | head       # 期望:256 个桶目录已在服务首启时建好,对象按 key 哈希落在其中增长(find /data/cluster/blocks -type f 看总量)

# 7) 数据服务重启对客户机透明(断→拉起,客户机 IO 自动重连)
sudo systemctl restart grove-data-server && cat /mnt/alice/from_a.txt

# 8) 卸载干净、无残留
sudo systemctl stop grove-mount@alice && findmnt -T /mnt/alice | grep -q grovefs && echo "残留!" || echo "已卸载"
```

全部通过即部署完成。想跑整套回归(33 个门控阶段)见 §10 与 §12.7。

### 11.8 HA 主备拓扑(在 §11.4 基础上改)

要点:两台 meta-server 用**各自独立的 `--meta-dir`**(快照复制,不是共享盘!`blocks/` 才是共享的,客户机 `--data-servers` 始终指向同一组 data-server):

```bash
# nodeA(10.0.0.5):primary
$Y meta-server --meta-dir /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --token $TOK
# nodeB(10.0.0.6):standby(同样写成 systemd unit,只改 --meta-dir/--listen/--role/--peer)
$Y meta-server --meta-dir /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --token $TOK
# 客户端把故障转移列表写进 --meta-server:
$Y mount --view alice --meta-server 10.0.0.5:50110,10.0.0.6:50111 --data-servers 10.0.0.5:50100 --token $TOK -m /mnt/alice
```

防双写需第三方仲裁:把 **3 个(奇数)data-server** 写进 `--witness` 列表,且这些 data-server 应与客户端实际写入的内容盘**同节点部署**(数据面门控只对列表内的 data-server 生效,见 §5.5)。验收:

```bash
sudo ssh nodeA systemctl stop grove-meta-server     # 杀主
journalctl -u grove-meta-server -n5 --since '-1min' # nodeB 期望出现:meta HA: promoted to primary at epoch N
# 客户机继续可写(切到备的地址列表自动故障转移)
sudo ssh nodeA systemctl start grove-meta-server    # 旧主回来 → 见更高 epoch 自动降级为 standby
```

RPO 与 fencing 的取舍、fail-safe 写冻结的预期行为见 §5.5;巡检要盯的日志关键字见 §12.1。

### 11.9 PostgreSQL 后端部署(只改 meta-server 一处)

```bash
# 1) 准备一个 PG(自建或发行版包均可),建库并给连接角色建 schema 的权限:
#    createdb grovefs;  psql -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grovefs;"
# 2) meta-server 启动参数加 --meta-store(与 --role standby/--peer/--witness 互斥,同时给会启动即 fail-fast):
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store postgres://grove:pw@10.0.0.9:5432/grovefs --token $TOK
```

客户端命令一字不变(见 §4.1:客户机不装 PG、不持连接串、不感知后端)。每个 view 的 `grovefs_v_<hex(view)>` schema 必须先用 `view --meta-store … create` 显式建好(挂载只打开已存在的 schema,不会就地创建),无需预迁移脚本;控制面随 `--meta-store` 一起进 PG:key 与审计落 `grovefs_control.api_keys`/`mcp_calls`、视图定义落 `grovefs_control.view_defs`,`grovefs view`/`apikey`/`mcp` 加同一 `--meta-store`(或 env `GROVEFS_META_STORE`)即接同一份记录,本地不落任何控制面文件(`control` 套件逐节实证)。备份/HA 改走 PG 自己的工具链(一次 `pg_dump` 同时覆盖树+锁+控制面;`pg_basebackup`/流复制/PITR),见 §12.3。为什么上/不上 PG 的诚实边界见 §4.1。**巡检 SQL、备份恢复、删视图与 PG 下的能力边界（含实测拒绝原文）见 §15.6–§15.9。**

### 11.10 FDB 实验档复跑配方（不是部署承诺，状态见 §6.3）

> 下面这串命令是**怎样把这一档跑起来**（用于实验与复测），不是推荐部署。容量面已判为未通过，
> 受支持的元数据后端仍是 SQLite 与 PostgreSQL（§11.9）：

```bash
# 0) 不构建、不装包的跳板：直接用仓库里那份自足投放包（§6.3 「投放面」）
#    dist/grovefs-fdb-experimental-linux-x86_64/：grovefs + lib/libfdb_c.so + fdb/{fdbserver,fdbcli,fdbmonitor}
#    下面 1)–5) 的每一步都能只靠包内二进制完成（包 README 有逐条命令）；本配方余下部分走源码树

# 1) 先让集群可达(单节点开发集群可用用户态目录,无需 root/系统包):
bash .build-bin/fdb_env.sh up            # 起 fdbserver、生成集群文件、导出 GROVEFS_FDB_CLUSTER_FILE
fdbcli -C "$GROVEFS_FDB_CLUSTER_FILE" --exec 'status minimal'   # → The database is available.

# 2) 装一个带 feature 的服务端二进制(客户端不需要)。libfdb_c 不必装包：
#    第 1 步那个用户态目录里有 .so，指给构建期即可（绑定已预生成，不需 libclang）
FDB_CLIENT_LIB_PATH=/path/to/lib cargo build --release --features fdb
#    跑起来不再需要 LD_LIBRARY_PATH——RUNPATH 里已写进 $ORIGIN/lib 与该目录

# 3) meta-server 只多一个参数(与 --role standby/--peer/--witness 互斥,同时给会启动即 fail-fast):
$Y meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 \
     --meta-store fdb:///path/to/fdb.cluster --token $TOK

# 4) 每个要用的视图先开一个 subspace(挂载不建树):
$Y view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster create --view team

# 5) 把一棵已有的 SQLite 树搬进集群(不新建工具;目标名不必先 view create,导入即开 subspace):
$Y view --meta-dir /data/sqlite-store export --view team --out /tmp/team.jsonl
rsync -a -H /data/sqlite-store/blocks/ /data/cluster/blocks/
$Y view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster import --view team --in /tmp/team.jsonl
```

确认到位看 meta-server 日志第二行横幅:`meta-server: metadata backend foundationdb — one subspace per view in FoundationDB (cluster file …); <base> holds no trees`。客户机命令与 §11.5 完全一致;控制面(视图定义/别名/API key/审计)都在集群 `ctl` 子空间,`fdbbackup` 一并覆盖(树+控制面同一次快照),`--meta-dir` 只是部署身份与 scratch/config、无权威记录要单独备。data-server 无参数变化,字节仍存 `blocks/` 对象。FDB 下的命令边界见 §15.9。第 4 步的 `view create` 与第 5 步的 `view import` 是此档仅有的两个建树入口(挂载不建树);第 5 步反向跑就是回滚。

---

## 12. 维护手册(巡检 / 备份恢复 / 升级 / 故障处理)

### 12.1 日常巡检

```bash
# 进程与端口
systemctl is-active grove-data-server grove-meta-server
# 所有 grove-mount@* 实例都在跑?
systemctl list-units 'grove-mount@*' --state=running

# 挂载点都在、标签对
findmnt -t fuse | grep grovefs

# 中心容量(对象直存于 blocks 下 256 个桶子目录,直接 du 整目录即可)
# 判异常要看斜率而不是看大小:每周同一天追一行带日期的读数
du -sh /data/cluster/blocks; df -h /data/cluster
echo "$(date +%F) $(du -sh /data/cluster/blocks | cut -f1)" >> /var/log/grovefs_capacity.log

# 昨夜到现在有没有降级/冻结/提升事件
journalctl -u grove-meta-server --since yesterday | grep -E 'meta HA|sync-repl|fenced|promotion'

# 元数据后端可达性：受支持档只看下面的日志横幅行（它能直接读出树在哪）。
# 元数据面是否在集群上:meta-server 日志的 backend 横幅行(下表)比任何本地目录都可靠
journalctl -u grove-meta-server | grep -E 'metadata backend'
```

日志关键字 → 含义 → 处置(字串以源码 eprintln 为准):

| 日志关键字 | 含义 | 处置 |
| --- | --- | --- |
| `meta sync-repl: ... degraded to async for this op` | `--sync-repl` 备不可达/ACK 超时,该笔写 RPO 回落 ~500ms(诚实降级,不谎报 0) | 查备机存活/网络;持续出现则要么修备要么接受 async RPO |
| `meta HA: promoted to primary at epoch N` | standby 提升成功 | 确认旧主已停/将降级,事后排查主为何失联 |
| `meta HA: demoted to standby (peer epoch N)` / `(a quorum ... holds ... > our ...)` | 发现对端/仲裁持有更高 epoch,自降级 | 正常收敛行为;若双侧频发查网络抖动 |
| `meta HA: cannot form lease quorum ... fail-safe self-demote` | witness 多数派不可达超宽限(3s),**写冻结**(fail-safe,宁停写不双写) | 预期保护而非故障:恢复仲裁节点可达即自动解冻 |
| `meta-server: evicted N idle engine(s) (>Xs idle, no sessions)` | 空闲引擎回收(`--engine-idle-secs`,§6.2):N 个无挂载会话且静默超阈的视图释放了引擎(PG 连接/文件句柄) | 正常回收噪声无需处置;若希望视图热不回落,缩短阈值或确认会话已断;`0` 则关闭驱逐(常驻模型) |
| `meta HA: promotion refused/deferred` | 提升被否(对端持更高 token 或前任租约尚活) | 正常:绝不双主;若两节点都拒升,查 witness 多数派 |
| `invalidation listener stopped: token rejected` | 客户端失效推送通道被 token 拒(中心 token 换了) | 用新 token 重启对应 grove-mount@ 实例 |
| `meta-server: metadata backend foundationdb — one subspace per view in FoundationDB (cluster file …); <base> holds no trees` | 树与控制面都在集群里,`--meta-dir` 只装日志与 scratch(§6.3) | 正常。排查"我的树跑哪去了"先看这一行:若它写的是 `one db file per view under <base>/meta/`,说明 `--meta-store` 没生效,这个服务端读的是 本地 SQLite 树 |
| `meta-server: fdb cluster up — redundancy …, N storage server(s), M coordinator(s)` | 启动探针读到的集群拓扑(读 `\xff\xff/status/json`,bind 之前打印) | 正常。与预期不符(单机开发集群显示 `single`、`0 zone failure(s)`)说明集群未按多副本配置;排查前先看这一行,不必先登集群 |
| `meta-server: writer lease HELD for deployment <dir> (holder …, renewed every Ns, expires Ns after the last renewal); …` | 该 `--meta-dir` 的写者租约已取到,续租周期与 TTL 同行列(§6.3.2) | 正常。同一 `--meta-dir` 再起一个实例会等一个 TTL 后拒绝(§15.9);`kill -9` 后重启会等旧行过期才接管 |
| `meta-server: writer lease DISABLED (GROVEFS_FDB_WRITER_LEASE_SECS=0); …` | 租约被人为关闭,不再阻止同目录的第二个写者 | 只用于测试或确定单实例的场合;生产部署撤掉该变量 |
| `meta-server: LOST the FDB writer lease for '…' (another server took it over); stopping instead of writing beside it` | 续租发现行已属于别人(被 `--force` 围栏,或本进程停顿超过 TTL 后被接管) | 进程以 1 退出是有意为之,不是崩溃;确认没有两个 server 共用同一目录后就地重启 |
| `meta-server: writer lease released for '…'` | 正常关闭时先还租约再关网络 | 正常噪声,无需处置 |
| `--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (…)` | 启动探针有界失败,服务端拒绝启动而不是挂起 | 查集群:`fdbcli -C <集群文件> --exec 'status minimal'`;协调器不足看 `quorum_not_reachable` |
| `names the FoundationDB backend, but this binary was built without \`--features fdb\`` | 服务端二进制不含该 feature(未链 `libfdb_c`),却给了 `fdb://…` | 预期拒绝而非降级:用 `--features fdb` 重建,或去掉 `--meta-store` 回到默认 SQLite(§6.3) |
| `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness` | 想用 grovefs 自研主备服务 FDB 部署 | 撤掉 HA 参数:副本与恢复由集群负责(`fdbbackup`),§6.3/§15.9 |
| `fdb:// uri names no cluster file` | URI 里无路径且 `$GROVEFS_FDB_CLUSTER_FILE` 也未设 | 给 `fdb://<集群文件>` 或 `fdb://?cluster=<路径>`(§6.3) |
| `meta-server: evicted N idle engine(s) (>Xs idle, no sessions)`（FDB 部署） | 回收的是**进程内**引擎(号段/游标/戳),无连接可关 | 正常回收噪声;此档该阈值只是内存上限(§6.3) |

### 12.2 日志与可观测

所有进程向前台 stderr 写日志;`RUST_LOG` 控制 tracing 等级(默认 `warn`)。后台挂载(默认形态)的全部输出落在其 `--log-file`/`$TMPDIR/grovefs-mount-<挂载点>.log`,前台挂载(--foreground/systemd 单元)才进终端或 journal:

```bash
RUST_LOG=info   # meta-server 常规运维建议级别;debug 仅在排障时短开(帧级日志量大)
journalctl -u grove-meta-server -f        # systemd 下实时追日志
```

没有集中 metrics 接口(未实现);健康判断靠:进程 active + 端口在听 + `findmnt` 在挂 + 上面那张关键字表无新增异常行。

### 12.3 备份与恢复

要备的东西(缺一不可互相配齐):

| 件 | 位置 | 方式 |
| --- | --- | --- |
| 数据对象 | data-server `--data-dir` 下的 `blocks/`(本地模式为 `<home>/blocks/`) | 普通目录,`rsync -a`/文件系统快照直接备;若用过 `grovefs import`(§3.3),部分对象是与源目录共享的硬链接——用 `rsync -a -H` 或 `cp -a` 保留链接避免内容翻倍,且源目录本身也应进同一备份 |
| 权威视图 | `<meta-dir>/meta/<view>.db`(含 WAL) | 见下三种流程;**勿在有写活动时裸拷** |
| 跨视图账本 | `<meta-dir>/meta/refs.db` | 随 `meta/` 一起拷;丢失/替换后可用 `grovefs refs --meta-dir D rebuild` 按现存视图重算(只会高估不误删) |
| API key 注册表 | SQLite:`<meta-dir>/meta/api_keys.db`(含 WAL);PG:`grovefs_control.api_keys`/`mcp_calls`;FDB:集群 `ctl` 子空间 | SQLite 同上,冷拷;PG 随 `pg_dump` 进库;FDB 随 `fdbbackup` 进集群,均无独立文件要备 |
| 视图定义(派生/别名) | SQLite:`<meta-dir>/meta/views/*.layout`;PG:`grovefs_control.view_defs`;FDB:集群 `ctl` 子空间(`CtlDefTable`) | SQLite 普通文本文件,需一并拷(HA 备机也需手工同步,§3.2/§3.4);PG 随库复制、FDB 随 `fdbbackup` 复制,均无手工同步 |
| token | `/etc/grovefs/grovefs.env` | 单独加密保存(丢失 = 所有客户端挂不上) |
| API key 注册表与审计 | SQLite:`<meta-dir>/meta/api_keys.db`(含 `-wal`;用过 `apikey create` 才存在)/ PG:`grovefs_control.api_keys`+`mcp_calls`(随 `pg_dump` 走)/ FDB:集群 `ctl` 子空间(随 `fdbbackup` 走) | SQLite 随 `meta/` 一起**冷拷**(先停写)；开了 `--require-api-key` 时它是准入的唯一依据——丢了 = 一切 key 鉴不上、历史审计断档。**切勿单独删除 `-wal`**(会连带删掉尚未 checkpoint 的 key 行与审计行)；分盘部署时 `data-server --meta-dir` 必须指到放着这份库的那个元数据根（它不复制、只读那一份），指错即 fail-closed；PG/FDB 部署下给 `data-server` 同一个 `--meta-store` URI 即可,无本地库可指 |

**单机/本地模式**:先 `umount` 停写(或停 meta-server),再 `cp -a`/`rsync` 整个 `--home`。恢复:拷回原位 → 重挂 → `ls`/`cat` 抽查。二进制本身不在备份范围——重装 `grovefs` 即可,数据全在 `--home`。

**SQLite + 自研 HA**:standby 每 ~500ms 收到的是 `VACUUM INTO` 完整快照(在线备份语义),但它是**复制体不是归档备份**(仍建议对主/备任一侧做定期异地快照)。节点修复流程:

- 备机坏了→修好后直接 `systemctl start` 对应 standby meta-server:它从主继续收快照追平,不需要手工灌数据。
- 主机坏了→备自动提升(见 §12.1 日志);旧主修好后 `start`:发现更高 epoch **自动降级为 standby**,不会复活成双主(配了 `--witness` 时,即使旧主带更高 epoch 重启也会被仲裁挡在 standby,`test_ha.sh` 段 9 证明)。
- 两半都丢且要重建:从最近一份归档快照恢复 `<meta-dir>/meta/`,epoch 从头开始的前提是**所有 data-server 也同时换新**(否则残留 `.fence_token` 会挡住写——这是 fail-safe 的正确行为,不是损坏)。

**PostgreSQL 后端**:视图树备份完全交给 PG 工具链(`pg_dump`/`pg_basebackup` + WAL 归档/PITR);GroveFS 侧只备 `blocks/` + env 文件——key、审计与视图定义都已在 `grovefs_control` schema 里,一次 `pg_dump` 同时覆盖树+控制面(`control` 套件实证),没有本地要单独冷拷的控制面文件。`--meta-store` 模式下自研快照备份 API 有意返回错误(设计如此)。可直接照抄的 dump/恢复/演练命令（含 DBA 建库与恢复后核对项）见 **§15.7**。

**迁到新机器/换系统**:元数据不绑定硬件(只存对象 key,路径是 key 的纯函数),整目录冷拷即可用;要按视图选择性迁移用 `view export/import`(详见 §3.4.2)。三个拓扑边界:多盘 `--data-dir` 的数量/顺序要一致(等盘克隆原位顶替可,增删/调序会重映射;或合并成单 root);条带 `s<N>:` 要保留 N 个 data-server 及顺序;整目录拷会把 HA `epoch` 带过去,旧主必须先停。PG 后端用 `pg_dump`/逻辑复制。

### 12.4 升级与滚动变更

客户端二进制升级(单台无中断):`systemctl stop grove-mount@alice` → 替换 `/usr/local/bin/grovefs` → `start`。(stop 会干净卸载;直接 kill 二进制不行——死挂载要 `fusermount -u` 手工清。)

中心 server 升级(无 HA 拓扑,接受秒级 IO 暂停):替换二进制 → `systemctl restart grove-data-server grove-meta-server`。客户机挂载进程**不需要**跟着重启——远程链路对 server 重启透明(resilience 套件证明:连接池整体丢弃后有限退避重连);只有客户端侧协议/功能需要新版时才重挂。

HA 拓扑滚动升级(写不中断,切换瞬间除外):

```bash
# 1) 先升级并拉起 standby(追平快照)
# 2) stop primary → standby 在 ≤3s(PROMOTE_TIMEOUT)内提升,客户端自动切地址
# 3) 升级旧主并以 standby 角色拉起 → 见高 epoch 自动降级,回到初始形态
```

> 版本一致要求:线协议是自定义帧且**不带版本号**——兼容完全取决于 `M_OPEN` 确认包的能力字节宽度与解析宽严。该包固定携带三个能力字节(只读 / 可分页 / 内容只读)。新客户端严格校验三字节齐全,缺任一(对端只发两个字节的早期代)直接报人话错误 `meta-server handshake ack is missing the capability flags — client and server builds must match`,挂载不成立、绝不猜测默认。实测矩阵:三个字节都齐的同代部署(任意方向的 新↔旧 3 字节版)握手一致、读写都通;唯一需提醒的窗口是“两字节时代的旧客户端连当代服务端”——它把确认包按 `get(i).unwrap_or(0)` 宽容解析,会忽略第三个字节(内容只读),因而旧客户端挂载 **fork 视图**时不会在 FUSE 层给出内容只读的 `EROFS`(服务端不 gate 字节流,见 §7)。因此升级顺序:先把所有存储端与服务端升到三字节代,再不要拿两字节代客户端去挂 fork 视图。

> **切换窗口的写重试要求**:stop primary 后到 standby 提升完成(≤3s `PROMOTE_TIMEOUT`)的窗口内,在途/首次的写往返可能失败(`EIO`,或经服务端门禁路径的 `EROFS`/`EPERM`)——这是转接窗口内旧连接失效的正常表现,实测为状态相关(同一操作有时 `rc=0`,耗时 ~0.8s)。调用方对切换窗口内的首次写应带重试(应用级重跑即可,幂等的追加除外——重试前确认上一次未落盘);写不中断的承诺只对切换完成后的新主成立。

### 12.5 故障处理速查表

| 症状 | 第一反应 | 解释 |
| --- | --- | --- |
| `ls` 报 `Transport endpoint is not connected` | `fusermount -u /mnt/x`(或 `grovefs umount -m /mnt/x`)后重启对应 mount 服务 | 挂载 daemon 死了/被 kill,留下死挂载点 |
| 挂载即报 `Permission denied` | 看错误里的**拒因原文**(随握手逐字节回传):`meta-server rejected the access token` / `no api key presented` / `unknown api key id` / `invalid api key secret` / `malformed api key` / `scoped to 'mcp', not 'mount'` / `this api key has been revoked` | `OP_OPEN`/`M_OPEN` 握手不过不重试,fail-fast;前者比对 token,后者查 `grovefs apikey list` 的 scope/revoked 列与 `apikey calls` 的归属行 |
| 挂载用着用着自己卸了(日志 `access refused by the server … — unmounting this view`) | `grovefs apikey calls` 看那把 key 是否被 revoke/过一分钟前刚被撤销 | 预期行为:服务端每秒复证活连接,撤销后首个往返即封停该挂载(§6),避免留一个还能读的服务端已不认的视图 |
| 写全部 `EPERM`、读正常 | `grovefs apikey list` 看该 key 是否 `--ro`(只读) | 只读 key 逐 op 服务端强制拒 `EPERM`、即时生效(不缓存);**这是权限不是故障**,故意区别于 `EROFS`。另一种来源:旧/改造客户端挂着派生(虚拟)视图名写(服务端派生只读门禁同样回 `EPERM`,§3.2) |
| 挂基名被拒 `"governed by an alias"`(本地与远端回显同一段文案,§3.4) | `grovefs view list` 查该真实名是否被纯别名治理 | 预期行为:别名是其唯一挂载入口;要恢复基名直挂,`grovefs view rm --name <别名>`(仅影响新挂载) |
| 远程挂载写全部 `EROFS` | `journalctl -u grove-meta-server` 看 `fenced/写冻结/promotion refused` 或角色;`findmnt` 看挂载行是否带 `ro` | 三种正常情形:① 该节点是 standby(只读等提升);② witness 多数派不可达触发 fail-safe 写冻结——修仲裁可达性,宁停写不双写;③ 挂的是**派生(虚拟)视图名**(只读契约,§3.2)——改挂基名写 |
| `flock` 看起来被占住、持有者机器已整机崩溃 | 等 ≤15s(`LOCK_LEASE_SECS`)lease 过期自动扫 | 无 `release` 信号时 lease 是唯一兜底;正常进程崩溃是即时回收 |
| data-server 重启后客户机 IO 卡一下又好了 | 无需动作 | 连接池整体丢弃后有限退避重连(resilience 证明) |
| 非 root 用户看不到挂载点/`allow_other` 不生效 | 查 `/etc/fuse.conf` 的 `user_allow_other`;查 fusermount SUID 位 | FUSE 惯例限制,与 GroveFS 无关 |
| `--meta-store postgres://…` 加了 `--peer` 启动即退 | 去掉 HA 参数 | PG 与自研快照 HA 互斥是设计决定(§6.2),高可用用 PG 流复制 |
| 大文件没看到条带分块 | 确认文件 ≥16MiB 且 `--data-servers` 给了逗号分隔多地址 | 小文件自然只落 chunk0,是设计不是 bug |
| 挂载点磁盘写满 | 先扩 `blocks/` 所在盘；事后用 §12.6 的三条判据查“是业务量还是没回收” | 容量治理没有 magic:对象只增删不压缩,长期靠 `du` 斜率 + 物理/逻辑比值定位 |

### 12.6 容量与回收的实话

- `unlink`(最后一个链接消失)时后端据 key 尽力删除对象/全部分块,幂等。**没有任何存活视图树命名、账本也不再记账的页**(孤儿)由离线命令 `grovefs gc` 审计/回收:默认只扫描,列出最大的孤儿、账本仍持有的键(先 `refs rebuild`)和被跳过的非对象项;`--apply` 才真删,且只删比 `--older-than`(默认 `24h`)更老的页,永不删账本仍持有的键,并要求本存储没有活挂载、本机没有 grovefs 进程(`--apply` 必须离线;硬链接 `import` 会继承源文件旧 mtime,所以年龄门是硬约束而不是仅供参考)。

  ```bash
  grovefs gc --meta-dir D                        # 只扫描:孤儿 / 账本持有 / 跳过项
  grovefs gc --meta-dir D --data-dir /data      # meta 与 data 分家时指向真正存 blocks/ 的根
  grovefs gc --meta-dir D --apply                # 回收 24h 以上的孤儿(离线)
  grovefs gc --meta-dir D --apply --older-than 0 --json   # 立即回收 + 机器可读
  ```

  守卫:任何视图树读不开就整体中止(跳过一棵树会把它的对象全误判成孤儿);账本仍持有的键只报告不删;`--data-dir` 与所有树零交集时拒绝(防 `--meta-dir`/`--data-dir` 配错,`--force` 可越过);PG 部署直接拒绝(树是 schema,字节归 data-server)。e2e 见 `gc` 套件(41 条断言:干净库/out-of-band 删树→held→rebuild→孤儿→grace→真删/非对象不删/本库挂载拒与他库同名不拦/错根防呆/PG 与非法时长拒绝/`--json` 字段)。

  **三档后端通用的是远程形态**:服务端读自己的树(SQLite/PG/FDB),字节向 data-server 枚举——操作员机器上既不需要 `meta/` 也不需要 `blocks/`,PG/FDB 部署下的孤儿回收就走它:

  ```bash
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100,10.0.0.5:50101            # 只扫描
  grovefs gc --control-server 10.0.0.5:50111 --api-key "$ADMIN_KEY" \
             --data-servers 10.0.0.5:50100 --apply --older-than 24h   # 真删(离线)
  ```

  与本地形态同一套语义(`--older-than`/`--force`/`--json`/错根防呆都在服务端判),但两处“缺失”必须明说而不是假定掉:*静默证明只认服务端自己的会话*——`--apply` 拒绝的是“本 server 还服务着活会话”或“这些视图在本机还挂着”,回 `gc --apply refused: this deployment is in use — <view> (N live session(s) on this server); …; stop those mounts first (reclaim must run offline, and a mount on another machine is not visible from here, so stop it too)`;另一台机器上的挂载它看不见,所以那句话后半句是给操作员的作业而不是保证。`--data-dir` 与 `--control-server` 互斥(字节不在这台机器上)。e2e 见 `gcremote` 套件(66 条断言,三档:匹配/孤儿计数、宽限期经数据面生效、会话静默证明、错根防呆、`--json` 形状与本地/远程 flag 组合拒因)。
- 把 `du` 变成可判的三条：① **记斜率而不是记大小**（每周同一天追一行带日期读数，斜率对不上业务增量才叫异常）；② **除一遍逻辑量**——`blocks/` 是物理字节，挂载里看到的是逻辑字节，两者同量级才正常。逻辑量用 `grovefs du`（一次批量取全部对象字节，万级文件几十毫秒；在挂载里 `find` 走的是逐文件 stat，慢两个数量级）：

  ```bash
  grovefs du --home H --view V --total          # 本地 SQLite 部署：一个参数就够
  grovefs du --view V --meta-server M --data-servers A1,A2 --total   # 条带部署：给全 data-server 列表
  find /mnt/da -type f -printf '%s\n' | awk '{t+=$1} END{printf "logical %.2f GiB\n", t/1073741824}'
  ```

  字节来源判定不了时 `du` 拒绝出数而不是出一版 0：`--data-servers` > `--data-dir` > 元数据根，树来自 meta/PG 而两个地址都没给即报错；地址列表与对象 `s<N>` 布局台数不符（含同一棵树里有扩容前后的两代）也直接按方向报出该给哪几个地址，列表台数对但**顺序反了**同样报错不出数（否则整树会被量成一堆 0）。**只有确认缺失的对象**才计入 `warning: N of M object(s) are not in the data source and count as 0 bytes`；超时、拒连、读失败一律报错不出数，因为没人回答的字节不是 0 字节。

  ③ **比值 > 1 只有两个成因**：该删的没删（去视图里确认文件真 unlink 了），或 import/fork 的共享字节还被别的视图持着（`grovefs view --meta-dir D list` 看还有谁在引用，归零才物理回收，§5.3）。
- 最常见的第三种其实是事故:**绕过挂载直接 `rm meta/<view>.db` 不会触发后端删除**,对象会永远留在 `blocks/` 里(引用账本只会高估不会误删,所以它只占空间、不丢数据);回收账本可用 `grovefs refs --meta-dir D rebuild` 归位。归位后再跑一次 `grovefs gc --meta-dir D`,这些页会从 `ledger-held` 变成孤儿,`--apply` 才会回收;不重建则 `gc` 一直保守地不删。
- SQLite 视图库的回收在引擎内部处理(锁表事务回收、hardlink refcount GC);PG 模式下交给 VACUUM。

**对象丢失 / 数据源不对时,各命令会不会出声**(运维速查;完整回显与成因见 docs.html §11.5):

| 情形 | 命令 / 路径 | 会出声吗 |
| --- | --- | --- |
| 整个对象不在选中的根里 | `du --data-dir`/`--data-servers` | **会**：stderr `warning: N of M object(s) are not in the data source and count as 0 bytes` |
| 数据源根本没给 | `du` | **会（拒统计）**：`no data source to measure: pass --data-servers … or --data-dir …` |
| 地址列表台数/顺序与 `s<N>` 布局不符 | `du --data-servers` | **会（拒统计）**：探测前按方向拒绝，绝不把布局错配说成对象丢失 |
| 条带对象**中间分块**真丢 | `du` | **会（判损坏）**：stderr `warning: N of M object(s) are damaged striped objects and are excluded from the total (their true size is unknown)`，损坏字节不计入总数 |
| 条带对象**首分块**真丢（后续分块仍在） | `du` | **会（判损坏）**：同上进 `damaged` 告警（`missing its leading chunk #0 …`），不再给错位极小值 |
| 条带对象**中间分块**真丢，经挂载读取 | 挂载内 `read`/`stat` | **不会（已知缺陷）**：`stat size` 截短到首个缺失分块的偏移（如 40 MiB 对象报 16 MiB），整读 rc=0 且内容纯为缺口前缀，缺口后存活分块亦不可见，不报 `EIO`；仅 `du` 判损坏 |
| 读一个对象已丢的文件（重挂后/fd 缓存已清） | 挂载内 `read`/`stat` | **不会（已知缺陷）**：`stat size=0`、整读 rc=0 空内容，不报 `EIO` |
| 对象被外部 `mv`/`rm`，但挂载已建立 | 挂载内旧 fd | **不会（且不可观察）**：data-server 按 key 缓存旧 fd，需清缓存/重启后才可测 |
| `view rm --base` 时数据根打不开 | `view rm --base` | **会**：`error: cannot open data root …` + 离线 `gc` 回收提示（分开陈述“树已删”与“字节未收”） |
| 条带对象全在远端、本机无 `blocks/` | `gc --meta-dir D`（审计） | **会（不误报为空）**：`objects: 0 local block(s) — N striped layout(s) … use \`grovefs du\` for their bytes` |

> 一句话选型：要“发现字节丢了”用 `du`（整对象不在数据源、列表不符、条带分块损坏都会出声；但挂载读丢失仍静默——属未实现项）；要“回收无人引用的页”用离线 `gc`；要“把账本归位”用 `refs rebuild`。挂载读本身不担当完整性校验。

### 12.7 变更后的回归验证

任何升级/配置变更后,在有构建环境的机器上跑一遍全板(33 个门控阶段),或只跑关心的面:

```bash
bash .build-bin/run_all.sh                    # 全板(build 自动前置)
SKIP_BENCH=1 bash .build-bin/run_all.sh       # 纯功能回归，跳过吞吐基准
bash .build-bin/run_all.sh metasrv ha locks   # 只跑指定套件(build 自动补)
```

GREEN 判定:所有套件 PASS(`pg` 在无 PG 主机上自动 SKIP,不拉低 GREEN;`bench` 仅供参考、永不门控结果)。

---

---

## 13. 完整案例实操:一台存储机 + 一读写一只读客户端(手把手,每个参数都解释)

> 目标拓扑(固定 IP,照抄可用):
>
> ```
>                      存储服务器 192.168.1.10
>         ┌───────────────────────────────────┐
>         │ data-server :50100   字节存 /data/blocks/       │
>         │ meta-server :50110   权威树存 /data/meta/*.db  │
>         │          (默认 embedded SQLite3,无需安装任何数据库)    │
>         └─────────┬─────────────────┬────────┘
>             ▲ 读写                   ▲ 只读
>  客户端A 192.168.1.11            客户端B 192.168.1.12
>  挂载到 /mnt/da(读写)           挂载到 /mnt/db(只读)
>         两台挂同一个 --view share → 同一棵权威树,A 写 B 秒可见
> ```
>
> 两台客户端挂的是**同一个视图 `share`**(共享同一棵树),只是 B 只读。如果 A/B 要看**互不可见**的两棵树,把 B 的 `--view` 换成另一个名字即可(§2 视图隔离)。

### 13.0 关于「数据库用 sqlite3」——先消除一个误解

GroveFS 的元数据引擎**默认就是 SQLite**,而且是用 `rusqlite` 的 `bundled` 特性把 SQLite **源码编译进了 `grovefs` 二进制**——它是进程内引擎,不是一个需要安装的数据库服务。所以:

- **不需要**在 192.168.1.10 上安装 `sqlite3` 命令行工具或任何数据库软件;
- **不要**给 `meta-server` 传 `--meta-store postgres://…`(传了才切 PG)——不传 = 本案例要的 SQLite;
- 权威树的物理形态就是普通文件 `/data/meta/share.db`(+同名 `-wal`/`-shm` 伴生文件,WAL 模式正常现象);
- 有写活动时**不要**用 sqlite3 CLI 去打开/复制这个文件(会撞上 WAL  checkpoint);要查看内容先停写,备份流程见 §12.3。

### 13.1 第一步:三台机器都装同一个二进制

在**构建机**(或存储机本身)编译,然后拷到每台机器:

```bash
# 构建机(只需 Rust 工具链;无需 pkg-config 与 libfuse 开发包,详见 §1)
cargo build --release
# 分发到三台机器并安装
for H in 192.168.1.10 192.168.1.11 192.168.1.12; do
  scp target/release/grovefs $H:/tmp/ && ssh $H 'sudo install -m 755 /tmp/grovefs /usr/local/bin/grovefs'
done
# 每台验证
grovefs --version
```

**意义**:GroveFS 是单二进制多角色——同一个 `grovefs` 程序,子命令决定它当存储端(`data-server`/`meta-server`)还是客户端(`mount`)。三台必须同版本(握手强制校验能力字节,混版直接报错)。二进制静态链入了 SQLite,客户机**除了 `fusermount` 没有任何额外依赖** 。

### 13.2 第二步:存储机(192.168.1.10)建目录与 token

```bash
sudo mkdir -p /data                             # 数据基目录;blocks/ 与 meta/ 都在服务首次启动时自动建
sudo mkdir -p /etc/grovefs                      # 配置目录
sudo bash -c 'umask 077; echo "GROVE_TOKEN=$(openssl rand -hex 16)" > /etc/grovefs/grovefs.env'
cat /etc/grovefs/grovefs.env                    # 记下这串 token,下一步 scp 给两台客户端
```

**意义与目录布局**:`/data` 这个盘将来长这样——

```
/data/
├─ blocks/            ← 文件的真实字节,一个文件=一个 UUID 命名的对象(或条带分块),按 key 哈希扇出在 256 个桶子目录里
└─ meta/              ← 每个视图一个 SQLite 库
   └─ share.db        ← 视图 share 的权威目录树(目录项、权限、data_ref 指针)
```

「视图(树形目录)与对象(字节)分离」是 GroveFS 的根本设计:应用看到的层级树存在 `meta/*.db`,字节永远在 `blocks/`，二者靠目录项里的 `data_ref` 指针关联。token 则是集群准入口令:三台机器必须同一份,后文所有 `--token` 都取自它。

```bash
# 把同一份 env 分发到两台客户端(也可手工重复 13.2 生成后三处一致——用 scp 最保)
scp /etc/grovefs/grovefs.env 192.168.1.11:/tmp/ && ssh 192.168.1.11 'sudo mkdir -p /etc/grovefs && sudo mv /tmp/grovefs.env /etc/grovefs/ && sudo chmod 600 /etc/grovefs/grovefs.env'
scp /etc/grovefs/grovefs.env 192.168.1.12:/tmp/ && ssh 192.168.1.12 'sudo mkdir -p /etc/grovefs && sudo mv /tmp/grovefs.env /etc/grovefs/ && sudo chmod 600 /etc/grovefs/grovefs.env'
```

### 13.3 第三步:存储机手工起两个服务(先验后常驻)

开两个 SSH 窗口各跑一个(前台方便看报错,验完再换 systemd):

```bash
# 窗口 1:数据服务。TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token "$TOK"
# 窗口 2:元数据服务
grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token "$TOK"
```

逐参数解释——`data-server`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--listen` | `0.0.0.0:50100` | 监听地址。**缺省是 `127.0.0.1:50052`(只接本机)**——要接两台客户端必须显式写 `0.0.0.0`;端口号本身自定,客户端 `--data-servers` 指对即可(50100/50110 是本文档约定值) |
| `--data-dir` | `/data` | 数据基目录:服务会在其下自动建 `blocks/` 子目录与 256 个桶,文件字节以 UUID 为 key 存入 `blocks/<2hex>/<key>`。**可传逗号分隔多路径(`/data,/mnt/d2,...`)= 机内多盘自动放置**:整对象按 key 哈希选定一条盘(客户端与 `data_ref` 全不知情,statfs 自动聚合容量)。但根列表对同一存储须**固定**——放置是哈希 mod 根数,列表与对象本身都不记录盘序号,故坏盘用等尺寸克隆盘**原位顶替**(同路径、同一位置、根数不变)照常定位;而增删盘或调换 `--data-dir` 顺序才会重映射所有 key、使旧对象按新布局不可达(放置层不报错、只静默 ENOENT);扩容宜一次性配齐多根,或在 OS 层用 LVM/mdadm 合盘后只挂单路径 `--data-dir`。server 只认这个布局,基目录指错 = 数据"丢"(其实是没去找) |
| `--token` | grovefs.env 里那串 | 数据面握手(`OP_OPEN`)口令。客户端不带对同一串 → 连接即 `PermissionDenied`,不重试。不给 = LAN-trusted 无鉴权,仅限全可信内网 |
| (可加 `--require-api-key`) | — | 把数据面准入改成**逐调用方可撤销的 API key**(客户端 `mount --api-key`):只收 `grovefs apikey create --for mount`(或 `any`)铸造的活 key,每次连(含被拒)落审计,且每秒复证活连接——撤销会断掉已建立的连接并使其自卸载。与 `--token` 同时给出则两道都得过;不给 = 行为与从前一致(只是依旧落审计)。key 库路径由 `--meta-dir <元数据根>` 指定,**默认取首个 `--data-dir`**(分盘部署时必须指对,指到没铸造过 key 的目录 = 一个也不放) |

逐参数解释——`meta-server`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--meta-dir` | `/data` | **元数据根**(子命令参数):view 库落在 `<meta-dir>/meta/`(即 `/data/meta/share.db`)。优先于全局 `--home`——后者是默认根,同时给出时以 `--meta-dir` 为准 |
| `--listen` | `0.0.0.0:50110` | 同上,缺省 `127.0.0.1:50053` 只接本机;元数据面单独占一个端口,与数据面 50100 互不串扰 |
| `--token` | 同一串 | 元数据面握手(`M_OPEN`)口令,与数据面相互独立校验、建议用同一份便于管理 |
| (可加 `--require-api-key`) | — | 元数据面改按 `M_OPEN` 里的 API key 放行(`mount --api-key ID:SECRET`,或客户端 env `GROVEFS_API_KEY`):读的是同一份注册表(SQLite:`<meta-dir>/meta/api_keys.db`;PG:`grovefs_control.api_keys`)——与 `grovefs mcp --require-api-key` / `grovefs apikey --meta-dir` **同一份注册表**,靠 scope 分入口;撤销能在首个往返内封停已建立的挂载(拒因随帧回传,挂载自卸)。详见 §6 / §3.5.1 |
| (不传 `--meta-store`) | — | **这就是“数据库用 sqlite3”**——不传即 embedded SQLite,一 view 一 `.db`;传 `postgres://…` 才会切 PG(且与 HA 互斥,见 §11.9) |
| (不传 `--role/--peer/--witness`) | — | 单权威节点形态:本案例不做主备/仲裁,`192.168.1.10` 宕机 = 元数据面暂停(磁盘数据不丢)。要 HA 按 §11.8 扩 |

验证(第四台机器或本机另开窗口):

```bash
ss -lntp | grep -E '50100|50110'      # 两行都在,进程名 grovefs
```

两个前台窗口应分别打出一行 banner——`data-server listening on 0.0.0.0:50100 (host <存储机主机名>, 1 data root(s): /data)` 和 `meta-server listening on 0.0.0.0:50110 (base /data, role Primary)`;data 行回显主机名与根列表,是因为各节点根路径常常同名——翻日志时先认准 host 再认路径。长什么样的完整终端见 §13.11 实录①。

### 13.4 第四步:存储机 systemd 常驻 + 防火墙

确认手工能起后,Ctrl-C 停掉两个前台进程,交给 systemd(unit 全文见 §11.4,路径换成本案例的):

```ini
# /etc/systemd/system/grove-data-server.service
[Unit]
Description=GroveFS data-server
After=local-fs.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target

# /etc/systemd/system/grove-meta-server.service
[Unit]
Description=GroveFS meta-server
After=local-fs.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-data-server grove-meta-server   # enable=开机 自起,now=立刻起

# 建应用要挂的视图:视图只能显式创建,名字不存在就挂会被 M_OPEN 拒
sudo grovefs view --meta-dir /data create --view share
```

**unit 里每个设置的意义**:`EnvironmentFile` 让 token 不进 unit 文件也不进 ps 命令行(命令行传口令会被 `ps aux` 同机任何用户看到);`Type` 不写即 simple——两个 server 本就是前台阻塞式,systemd 直接盯进程存活;`Restart=on-failure` 崩溃自动拉起(OOM/段错误等非零退出),配合 §12.1 的日志巡检看有没有反复拉起;`After=local-fs.target` 保证 /data 所在盘挂载完成后才起服务。

防火墙(存储机执行,只对内网段开两个服务端口):

```bash
sudo ufw allow from 192.168.1.0/24 to any port 50100 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 50110 proto tcp
# firewalld 等价:
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50100 protocol=tcp accept"
# sudo firewall-cmd --permanent --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 port port=50110 protocol=tcp accept" && sudo firewall-cmd --reload
```

### 13.5 第五步:客户端 A(192.168.1.11)读写挂载

先预检 + 挂载点目录:

```bash
ls -l /dev/fuse && which fusermount fusermount3   # §11.0 三件套
sudo mkdir -p /mnt/da
```

手工挂一把(`mount` 默认转后台,等挂载点就绪才返回;失败时后台的错误明细会回打到终端):

```bash
TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
sudo grovefs mount --view share \
     --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token "$TOK" -m /mnt/da
# 看到 "grovefs: mounted view 'share' at /mnt/da (background pid …)" 即成功;另开窗口:
echo hello-$(date +%s) | sudo tee /mnt/da/probe.txt && sudo ls -l /mnt/da
# 验证完 sudo grovefs umount -m /mnt/da 卸载(后台进程随之退出),下一步换 systemd
```

逐参数解释——`mount`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--home`(客户机**不需要传**) | — | 远程挂载不在本地落盘任何数据,此参数与数据面/元数据面都无关——只有本地模式(`init`/无 `--meta-server` 的 `mount`)才用它定位权威库;访问元数据根的命令(`meta-server`/`apikey`)各自用 `--meta-dir` 命名,`--home` 仅作回退 |
| `--view` | `share` | 选树标签:挂哪棵权威视图。与 Linux 用户名无关(§2);A/B 用同一个名字才会共享同一棵树。`share` 必须先在中心 `grovefs view --meta-dir /data create --view share` 建好(不建就挂即拒) |
| `--data-servers` | `192.168.1.10:50100` | 数据面地址:文件字节走自定义 TCP 帧到这个 `data-server`,不碰本机盘(非 NFS)。逗号分隔多地址才启用跨机条带,单地址无条带 |
| `--meta-server` | `192.168.1.10:50110` | 元数据面地址:目录树由中心 `meta-server` 提供——这是“多机共享一棵树”的根;同时它也订阅失效推送(谁写了什么,这台机下次访问即时重取)。逗号分隔多个 = 故障转移列表(§11.8) |
| `--token` | grovefs.env 那串 | 同时递给数据面 `OP_OPEN` 与元数据面 `M_OPEN` 两处握手;不匹配 = 挂载命令直接报权限错,fail-fast |
| (可加 `--api-key`) | — | 服务端开了 `--require-api-key` 时递上的 per-caller 凭据 `<id>:<secret>`(`grovefs apikey create --for mount` 铸造);也可导出 `GROVEFS_API_KEY` 让密钥不进 argv。缺失/无效/已撤销/只给 mcp scope = 挂载 fail-fast 并打印具体拒因;被撤销的 key 封停**正在用**的挂载(§6) |
| `-m` / `--mountpoint` | `/mnt/da` | 挂载点。**注意它是选项,必须带 `-m`/`--mountpoint`,不是写在行尾的位置参数**(实测:裸写报错 `unexpected argument`)。目录不存在会被自动创建 |
| (不加 `--ro`) | — | 读写模式(默认) |
| (不加 `--no-cache`) | — | 默认靠 meta-server 的实时失效推送保新鲜(§4),不需要 `--no-cache` 的零缓存代价;只有要极端旁路内核缓存时才加 |
| (不加 `--threads`) | — | 缺省 4 个 FUSE 工作线程并发派发,每线程独立 `/dev/fuse` fd;IO 密集可调大,`0`=单线程仅供 A/B 对照 |

验完手工后常驻,用 systemd(本案例挂载点不叫 `/mnt/<view>`,所以用固定 unit 而非 §11.5 的模板实例):

```ini
# /etc/systemd/system/grove-mount.service
[Unit]
Description=GroveFS mount share -> /mnt/da
After=network-online.target
Wants=network-online.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStartPre=-/usr/bin/fusermount -u /mnt/da      # 清掉上次异常退出可能留的死挂载;- 表示失败也继续
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token ${GROVE_TOKEN} -m /mnt/da
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now grove-mount
findmnt -T /mnt/da        # 期望:fstype fuse[.passthrough]，源 grovefs:share
```

> 若挂载点要让**其他用户也能访问**,ExecStart 加 `--o allow_other`,并在存储机之外、这台客户端的 `/etc/fuse.conf` 打开 `user_allow_other`(FUSE 惯例限制,与 GroveFS 无关)。

### 13.6 第六步:客户端 B(192.168.1.12)只读挂载

与 A 几乎同一条命令,只改两处:`-m /mnt/db` 和加 `--ro`:

```bash
sudo mkdir -p /mnt/db
TOK=$(grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2)
sudo grovefs mount --view share \
     --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token "$TOK" --ro -m /mnt/db
```

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--ro` | (布尔开关) | 只读挂载。双保险:内核 `MS_RDONLY` + 文件系统侧守卫,所有写意图操作(create/write/mkdir/unlink/rename/chmod/truncate…)一律 `EROFS`。数据面/元数据面命令一字不变,只是这台客户端声明自己不发写 |

常驻 unit——B 机上**完整内容**如下(不是简写,照抄可用;路径 `/usr/bin/fusermount` 若你机上是 `fusermount3`,把 ExecStartPre 里的命令名换掉即可):

```ini
# /etc/systemd/system/grove-mount.service   ← 客户端 B(192.168.1.12)
[Unit]
Description=GroveFS mount share -> /mnt/db (read-only)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStartPre=-/usr/bin/fusermount -u /mnt/db
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token ${GROVE_TOKEN} --ro -m /mnt/db
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-mount
findmnt -T /mnt/db        # 期望:源 grovefs:share,挂载选项列里带 ro
```

**验收(两台一起验,这是本案例的核心卖点)**:

```bash
# A(192.168.1.11):
echo hi-$(date +%s) | sudo tee /mnt/da/from_a.txt
# B(192.168.1.12)——写完立刻,不等轮询:
sudo cat /mnt/db/from_a.txt          # 期望:即时看到同内容(实时失效推送)
sudo touch /mnt/db/x                 # 期望:Read-only file system (EROFS)
```

> **诚实边界——`--ro` 是客户端侧声明,不是服务端强制**:`--ro` 拦住的是这台机器上的正常写路径;但如果 B 机上的人自己重新 `grovefs mount` 一次不带 `--ro`(他拿得到 token),服务端并不会认出“这台机该只读”——token 是集群准入、view 是选树,都不区分主机。要**服务端硬只读**,按 13.7 用一把 `--view` 绑定的 `--ro` key(服务端逐 op 强制,不依赖客户端自觉带 `--ro`)。

### 13.7 可选加固:服务端硬只读（视图绑定 / 只读 API key）

思路:给两台的挂载各配一把**视图绑定的 key**——A 用读写 key、B 用只读 key（`--ro`），中心服务端逐 op 强制，只读不再依赖客户端自觉带 `--ro`。存储机上铸造（`--meta-dir /data` 与 meta-server 指同一个元数据根，`--for mount` 使其只开门挂载面）：

```bash
sudo grovefs apikey --meta-dir /data create --label A-rw --for mount --view share          # A:读写 key
sudo grovefs apikey --meta-dir /data create --label B-ro --for mount --view share --ro     # B:只读 key
sudo grovefs apikey --meta-dir /data list                                                  # 确认两行（view=share，rw/ro）
```

然后 meta-server / data-server 都加 `--require-api-key`，两台客户端的 ExecStart 用 `--api-key` 呈递各自的 key 并重启服务（改后两行全文，直接替换原 ExecStart）：

```ini
# 服务端(存储机)两服务都开门:meta-server ... --require-api-key;data-server ... --require-api-key --meta-dir /data

# 客户端 A 的 /etc/systemd/system/grove-mount.service 改成:
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --api-key ${GROVE_A_KEY} -m /mnt/da

# 客户端 B 的改成(--ro key 已由服务端强制,客户端 --ro 可留作双保险):
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --api-key ${GROVE_B_KEY} --ro -m /mnt/db
```

```bash
# 两台客户端各自改完后:
sudo systemctl daemon-reload && sudo systemctl restart grove-mount
```

| 参数 | 意义 |
| --- | --- |
| `apikey create --for mount --view share [--ro]` | 铸一把只能开 `share` 视图的挂载凭据；`--ro` 令其只读。**key 绑定视图后，拿它开别的视图在 `M_OPEN` 处 `E_AUTH` 拒** |
| `--ro`（key 侧） | 只读 key:读放行，每个写意图 op 在服务端**逐条动态重查**、回 `E_NOTPERM`（客户端看到 Operation not permitted；data 面内容写 `EROFS`）——不是故障，是鉴权拒绝 |
| `--api-key <ID:SECRET>`（或 env `GROVEFS_API_KEY`） | 挂载时在 `M_OPEN`/`OP_OPEN` 呈递这把 key（env 让密钥不进 argv）。撤销即断开该挂载并使其自卸载，其他 key 不受波及 |

验证:B 机即使去掉客户端 `--ro` 重挂，只要用的是 `--ro` key，写仍被服务端拒；`apikey revoke` 掉 A 的 key 后，A 上存活挂载的首个服务端往返即被拒（逐 op 复证）。

### 13.8 第七步:案例专属验收清单 + 回退

```bash
# ① 存储机:两服务 active 且开机自起
ssh 192.168.1.10 'systemctl is-enabled grove-data-server grove-meta-server && systemctl is-active grove-data-server grove-meta-server'
# ② A 写 → B 秒读(失效推送);B 写 → EROFS/EPERM。见 13.6 验段
# ③ 字节确实落在存储机,不碰客户端盘
ssh 192.168.1.10 'find /data/blocks -type f | head; sqlite3 --version 2>/dev/null || echo "无需 sqlite3,正常"'
#    注:对象按 key 哈希分布在 blocks/ 的 256 个桶里,用 find 看对象;share.db 是 SQLite 库文件,勿在有写活动时直接打开/拷贝
# ④ 中心重启后客户端自愈
ssh 192.168.1.10 'sudo systemctl restart grove-data-server'   # 之后回 A 继续读写,自动重连
# ⑤ 干净卸载与重挂(A 机同理)
ssh 192.168.1.12 'sudo systemctl stop grove-mount && findmnt -T /mnt/db | grep -q grovefs && echo 残留 || echo 已卸载'
```

全绿即部署完成。回退也简单:两台客户端 `systemctl stop grove-mount` → 存储机 `systemctl disable --now grove-data-server grove-meta-server`;磁盘上 `/data` 里的东西不删就是完整备份(§12.3),想彻底重来直接格式化数据盘(确认已备份后)。

日常维护(巡检关键字/备份/升级/故障表)全部适用 **§12**,IP 对应关系:存储机=192.168.1.10,写端=192.168.1.11,只读端=192.168.1.12。

### 13.9 每台机器的文件与命令总表(跟着这张表做,零回头)

**存储机 192.168.1.10 要建的文件(共 3 个):**

| 文件 | 内容在哪节 |
| --- | --- |
| `/etc/grovefs/grovefs.env` | §13.2 第三条命令生成 |
| `/etc/systemd/system/grove-data-server.service` | §13.4 第一个 ini 块全文 |
| `/etc/systemd/system/grove-meta-server.service` | §13.4 第二个 ini 块全文 |

执行顺序(存储机):`mkdir`(§13.2)→ 手工起两服务验证(§13.3)→ Ctrl-C → 写两个 unit → `daemon-reload` + `enable --now`(§13.4)→ 防火墙两条(§13.4)。可选加固再加一步:视图绑定 / 只读 key 两条(§13.7)。

**客户端 A 192.168.1.11 / 客户端 B 192.168.1.12 各要建的文件(共 2 个,两台同构):**

| 文件 | 内容在哪节 |
| --- | --- |
| `/etc/grovefs/grovefs.env` | §13.2 末尾 scp 分发(不用重建,和存储机同一份) |
| `/etc/systemd/system/grove-mount.service` | A 用 §13.5 的 ini 全文;B 用 §13.6 的 ini 全文 |

执行顺序(每台客户端):预检三件套 + `mkdir /mnt/xx`(§13.5/13.6)→ 手工挂一把验证(§13.5/13.6)→ Ctrl-C → 写 unit → `daemon-reload` + `enable --now` → `findmnt` 确认。若做了 §13.7:ExecStart 换成 §13.7 的对应行后再次 `daemon-reload + restart`。

### 13.10 本案例参数小抄(一张表看全部)

| 机器 | 命令 | 作何用 |
| --- | --- | --- |
| 存储机 | `grovefs data-server --listen 0.0.0.0:50100 --data-dir /data --token $TOK` | 字节存储面:`--listen` 接全网卡、`--data-dir` 基目录(自动建其下 `blocks/`)、`--token` 数据面口令 |
| 存储机 | `grovefs meta-server --meta-dir /data --listen 0.0.0.0:50110 --token $TOK` | 元数据面(默认 SQLite):`--meta-dir` 指元数据根(视图库在其 `meta/` 下)、不传 `--meta-store`=SQLite、不传 `--role/--peer`=单节点 |
| 存储机 | `grovefs view --meta-dir /data create --view share` | 显式建基视图:`share` 不建就挂会在 `M_OPEN` 被拒;库落在 `/data/meta/share.db` |
| 客户端A | `grovefs mount --view share --data-servers 192.168.1.10:50100 --meta-server 192.168.1.10:50110 --token $TOK --run-as-uid 1001 -m /mnt/da` | 读写挂载:`--view` 选树、`-m` 必填不可当位置参数、不写 `--no-cache` 靠默认推送保新鲜 |
| 客户端B | 同 A 改 `--ro --run-as-uid 1002 -m /mnt/db` | 只读挂载:`--ro` 拦本机正常写路径;配合中心对该客户端的 `--ro` key = 服务端硬只读 |
| 存储机 | `grovefs apikey --meta-dir /data create --for mount --view share [--ro]` | 视图绑定 / 只读 key:key 只能开 `share` 视图(开别的 `M_OPEN` 处 `E_AUTH` 拒);`--ro` 令每个写意图 op 服务端拒(`E_NOTPERM`→`EPERM`)、`revoke` 即时生效 |

### 13.11 全程终端实录(真实实测输出,逐字未改)

拍摄条件:grovefs 0.1.0 @ Ubuntu 20.04,**单机三合一等价拓扑**——命令里的 `127.0.0.1:5010x` 对应三机案例的 `192.168.1.10:5010x`,`/tmp/grove_case/data` 对应 `/data`,`/tmp/grove_case/mnt_da|mnt_db` 对应 `/mnt/da|/mnt/db`。每段都是真实终端回显(含中文报错);token 是一次性随机值已作废。重跑脚本:仓库 `.build-bin/run_case_demo.sh`。

**① 存储机:取 token、前台起两服务、确认监听**

```console
$ grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2
690987478d9cb1a4fee062db6a3d04c4
$ ./grovefs data-server --listen 127.0.0.1:50100 --data-dir /tmp/grove_case/data --token "$TOK"
data-server listening on 127.0.0.1:50100 (host <本机主机名>, 1 data root(s): /tmp/grove_case/data)        # 三机上用 0.0.0.0:50100,这行会显示 0.0.0.0 与存储机主机名
$ ./grovefs meta-server --meta-dir /tmp/grove_case/data --listen 127.0.0.1:50110 --token "$TOK"
meta-server listening on 127.0.0.1:50110 (base /tmp/grove_case/data, role Primary)
$ ss -lntp | grep -E "5010|5011"
LISTEN    0         128              127.0.0.1:50110            0.0.0.0:*        users:(("grovefs",pid=1733230,fd=4))
LISTEN    0         128              127.0.0.1:50100            0.0.0.0:*        users:(("grovefs",pid=1733225,fd=3))
$ ls /tmp/grove_case/data                        # 两服务启动时自动把骨架建好
blocks
meta
$ ls /tmp/grove_case/data/blocks | head -3     # 启动时已建好 256 个桶(两位哈希名)
00
01
02
$ find /tmp/grove_case/data/blocks -type f | wc -l
0                                                # 还没写文件,桶里没对象是对的
$ ./grovefs view --meta-dir /tmp/grove_case/data create --view share
created BASE view 'share' at /tmp/grove_case/data/meta/share.db
  deletes in a base view free the underlying object once no other view references it;
  reshape it with `view define` or publish it under a name with `view alias`.
```

**② 客户端 A:读写挂载 + 写文件**

```console
$ ./grovefs mount --view share --data-servers 127.0.0.1:50100 --meta-server 127.0.0.1:50110 --token "$TOK" -m /tmp/grove_case/mnt_da
using remote metadata service at 127.0.0.1:50110
using remote data service at 127.0.0.1:50100
mounting view 'share' at /tmp/grove_case/mnt_da (fully remote: no local store)   # 远程挂载不需要 --home
$ findmnt -T /tmp/grove_case/mnt_da
TARGET                 SOURCE        FSTYPE OPTIONS
/tmp/grove_case/mnt_da grovefs:share fuse   rw,nosuid,nodev,relatime,user_id=1005,group_id=1005
$ echo hi-1700000000 > /tmp/grove_case/mnt_da/from_a.txt
$ cat /tmp/grove_case/mnt_da/from_a.txt
hi-1700000000
$ mkdir /tmp/grove_case/mnt_da/docs && echo ok > /tmp/grove_case/mnt_da/docs/n1.md
$ ls -lR /tmp/grove_case/mnt_da
/tmp/grove_case/mnt_da:
总用量 5
drwx------ 2 fs fs 4096 10月 17 11:12 docs
-rw------- 1 fs fs   14 10月 17 11:12 from_a.txt

/tmp/grove_case/mnt_da/docs:
总用量 1
-rw------- 1 fs fs 3 10月 17 11:12 n1.md
```

写完后回**存储机**看副作用——两个文件恰好两个 UUID 对象,元数据全在 share.db(+WAL 伴生文件):

```console
$ ls /tmp/grove_case/data/blocks
b85d5e05-722d-4980-a0e0-e15b9e094de0
fa513481-830f-4719-b5a7-156e1ace1c96
$ ls /tmp/grove_case/data/meta
share.db  share.db-shm  share.db-wal
```

**③ 客户端 B:只读挂载,读得到、写不进**

```console
$ ./grovefs mount --view share --data-servers 127.0.0.1:50100 --meta-server 127.0.0.1:50110 --token "$TOK" --ro -m /tmp/grove_case/mnt_db
using remote metadata service at 127.0.0.1:50110
using remote data service at 127.0.0.1:50100
mounting view 'share' at /tmp/grove_case/mnt_db (fully remote: no local store, read-only)
$ findmnt -T /tmp/grove_case/mnt_db                # 注意 OPTIONS 里是 ro
TARGET                 SOURCE        FSTYPE OPTIONS
/tmp/grove_case/mnt_db grovefs:share fuse   ro,nosuid,nodev,relatime,user_id=1005,group_id=1005
$ cat /tmp/grove_case/mnt_db/from_a.txt           # A 刚写的,这边直接能读
hi-1700000000
$ ls /tmp/grove_case/mnt_db
docs
from_a.txt
$ touch /tmp/grove_case/mnt_db/x
touch: 无法创建 '/tmp/grove_case/mnt_db/x': 只读文件系统
$ echo bad > /tmp/grove_case/mnt_db/new.txt
bash: /tmp/grove_case/mnt_db/new.txt: 只读文件系统
$ mkdir /tmp/grove_case/mnt_db/zz
mkdir: 无法创建目录 “/tmp/grove_case/mnt_db/zz”: 只读文件系统
```

**④ 跨客户端即时可见(实时失效推送,不是轮询)**

```console
# 客户端 A 上：
$ echo "round2-$(date +%s)" > /tmp/grove_case/mnt_da/round2.txt
# 客户端 B 上立即 cat，没刷任何东西：
$ cat /tmp/grove_case/mnt_db/round2.txt
round2-1792235543
```

**⑤ 存储机:视图绑定 / 只读 key 加固 + 干净收尾**

把只读从"客户端自觉带 `--ro`"升级为"服务端逐 op 强制":给 A、B 各铸一把绑定 `share` 视图的 key(`--meta-dir` 与 meta-server 同名,指同一个元数据根;B 的那把再加 `--ro`)。命令与 §13.7 同一套,只是把元数据根指向本案例的 `/tmp/grove_case/data`:

```bash
# A:读写 key(只能开 share)
./grovefs apikey --meta-dir /tmp/grove_case/data create --label A-rw --for mount --view share
# B:只读 key(只能开 share,且写一律被服务端拒)
./grovefs apikey --meta-dir /tmp/grove_case/data create --label B-ro --for mount --view share --ro
# 确认两行(view=share,一份 rw 一份 ro;secret 只在 create 时打印一次,list 里只有哈希)
./grovefs apikey --meta-dir /tmp/grove_case/data list
# 撤销即时生效:被撤销 key 的存活挂载在下一次服务端往返即被拒并自卸载
./grovefs apikey --meta-dir /tmp/grove_case/data revoke --id <A-rw 的 id>
```

客户端用 `--api-key '<id>:<secret>'`（或 `export GROVEFS_API_KEY`）呈递这把 key 后重挂;B 换成 `--ro` key 后,即使去掉客户端 `--ro` 重挂,写仍被服务端拒（`E_NOTPERM`→`EPERM`）。服务端需给 meta-server / data-server 加 `--require-api-key`（见 §13.7）。收尾卸载：

```console
$ fusermount -u /tmp/grove_case/mnt_da; fusermount -u /tmp/grove_case/mnt_db
A umount ok
B umount ok
```

> 实录没拍的三件 systemd 相关事(`enable --now` 后开机自起、崩溃自动拉起、重启后 token 从 env 注入)属标准 systemd 行为,本沙箱无 systemd 未实测——在真机上按 §13.8 验收清单①④⑤跑一遍即验。

---

## 14. 进阶:数据目录定制 与 客户端显示方式

> 回答两个高频问题:① 服务端盘很多/数据量很大,数据目录怎么定置、存量数据怎么进来;② 同一批数据,不同客户端能不能看到不同的组织形状。

### 14.1 服务端的数据目录:两个各自独立的旋钮

| 旋钮 | 存什么 | 能放哪 |
| --- | --- | --- |
| `data-server --data-dir DIR` | 数据基目录:启动时自动建 `<root>/blocks/` 与 256 个桶,字节按 key 哈希存入 `blocks/<2hex>/<key>`;可逗号分隔多路径做多盘自动放置(整对象按 key 哈希选盘;根列表须固定,增删盘会重映射所有 key) | 任意路径/任意挂载点,不存在会自动建 |
| `meta-server --meta-dir DIR` | 权威视图树 `<meta-dir>/meta/<view>.db`(控制面另有 `api_keys.db`/`refs.db`;SQLite 后端形态,PG 后端下两者全进 `grovefs_control`) | 任意路径,与对象盘可不同盘 |

大数据量放独立大盘的例子(192.168.1.10 上 /data 只是小系统盘,对象盘另挂 /mnt/bigdisk):

```bash
# /etc/fstab 或手工挂好大盘后,把 unit 的 --data-dir 改掉并声明挂载依赖:
# ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --data-dir /mnt/bigdisk --token ${GROVE_TOKEN}
# [Unit] 加两行,保证大盘挂载完成才起服务(否则服务会在空盘上开局):
#   Requires=mnt-bigdisk.mount
#   After=mnt-bigdisk.mount
sudo systemctl daemon-reload && sudo systemctl restart grove-data-server
```

**搬迁已有对象目录**(安全,因为 key 只是 UUID、不含路径信息):

```bash
# ① 客户端停写(建议 systemctl stop grove-mount)② 停两服务 ③ 整体 rsync ④ 改 --data-dir ⑤ 拉起
rsync -a /data/blocks/ /mnt/bigdisk/blocks/
```

> **例外——条带文件的搬迁**:条带布局自描述在 `data_ref`(`s<N>:<stripe>:<uuid>`)里,但 chunk i 落在哪台是由客户端 `--data-servers` 列表的**顺序**(`servers[i%N]`)决定的——多地址列表迁移时,列表顺序与台数一变,已写条带文件就找不回分块。单地址 `--data-dir` 搬迁无此问题。

**多块盘/容量扩展**：两条正路① 一条 data-server 挂多盘：`--data-dir /data,/mnt/d2,...` 逗号分隔多存储根,整对象按 key 哈希自动放置到其中一条盘,`df`(statfs)自动聚合全盘容量——但根列表对同一存储须**固定**(放置是哈希 mod 根数,增删盘会重映射所有 key、使旧对象按新布局不可达),故宜一次性配齐多根;② 起多台 data-server + 客户端 `--data-servers a,b,c` 条带(≥16MiB 才分块,小文件只落一台;链路未饱和才有带宽收益,见 §5.2 诚实标注)。也可在 OS 层用 LVM/mdadm 把多盘合成一个文件系统再挂给单路径 `--data-dir`。没有 per-directory 配额与存储分层(未实现)。

> **换盘不影响定位,增删/调序才影响**:落盘位置是 key 的纯函数(哈希 mod 根数)现场重算,`--data-dir` 列表与对象本身都不记录"第几块盘"。故坏盘用等尺寸克隆盘**原位顶替**(同路径、`--data-dir` 里同一位置、根数不变)后数据照常定位——盘内容与其内部 256 桶扇出都是按 key 寻址的整块拷贝,无需重建任何索引。反之一旦**增、删、或调换 `--data-dir` 顺序**,取模基数变、所有 key 重映射,旧对象按新布局寻址即不可达;放置层不做 remap 守卫,只会静默 ENOENT(跨 server 条带则在 `--data-servers` 台数/顺序对不上时直接拒挂并报 `refusing to remap`)。运维上把 `--data-dir` 列表当作不可变配置:等盘克隆原位替换可,原地扩容/重排不可,需扩容走"全量重灌到新固定布局"。

### 14.2 存量大量数据怎么进来:两条正路,首选零拷贝 `grovefs import`

**首选——原地登记不搬家**(同文件系统,§3.3):字节零复制、秒级完成(只建链接+目录项,不读内容)、时间戳/权限保真:

```bash
# 在存储机上(能同时触到 meta 根与 blocks 根的机器)一条命令登记整棵树
grovefs import --meta-dir /data/cluster --view legacy --source /data/legacy --prefix /legacy
# 跨设备的源?import 自动退化为流式拷贝并如实计数(仍可用,只是不再零字节)
```

**备选——经 FUSE 写路径灌入**(需要重组/清洗数据、PG 后端、或想让大文件享受 ≥16MiB 跨 server 条带时):

```bash
# 在任一台已挂载的客户端(或存储机自己也挂一份)分批灌入
rsync -a /srv/legacy/ /mnt/da/imported/           # 目录层级、权限随写自动成型
# 单文件 ≥4MiB 自动分片并发写;配了多地址 --data-servers 且 ≥16MiB 自动条带,导入无需特殊调优
# 导完抽验:中心对象目录在增长,视图树里能看到层级
ls /data/blocks | head && find /mnt/da/imported -maxdepth 2 | head
```

两条路的取舍:import 零拷贝但导入对象是**单文件对象**(不走条带),且靠 TTL 而非事件推送向在线挂载收敛;rsync 灌入多一份字节但新写大文件可条带、新文件即时推送可见。注意(均为实现事实,不吹导入速度):经 FUSE 写入的吞吐上限在帧链路与中心盘,TB 级存量建议低峰分批;每百万级文件会让 `meta/<view>.db` 相应增长(一行节点+目录项),属正常。

### 14.3 客户端的“显示方式”由哪些机制决定

| 机制 | 效果 | 适用 |
| --- | --- | --- |
| **不同 `--view` = 不同树** | 两棵完全独立的层级,互不可见;想要不同组织形状最彻底的方式 | A/B 看根本不同的东西 |
| **同 `--view` = 同一棵树** | 字节一份,所有挂载点看到完全相同的权威树(实时同步);B 只读用 `--ro` 或只读 key(§13) | A/B 共享同一批数据 |
| **`mount --layout` 显示透镜(§3.1)** | 同一棵树,每个挂载点自行 hide/only/map/link——部分显示、重组路径,字节不动、其它挂载无感 | 定制显示形状,不需两份数据 |
| **服务端派生视图(§3.2)** | 同一套透镜规则**命名后集中存在服务端**(`meta/views/`),客户端只 `mount --view <派生名>`,同名跨机同形、可按名配只读 key(§13.7);**派生(虚拟)视图挂载一律只读**(写走基名) | 团队共享显示契约、免分发规则文件;只读分发形状 |
| **服务端纯别名(§3.4)** | 公开名 1:1 映射物理视图(`alias <真实名>`,无规则):别名挂载**全树可写**,真实名从此**不是挂载入口**(猜出来也挂不进) | 名字治理:对外名自由轮换、存储身份不外露(泄露时 `--retire` 一条命令原子轮换:旧名失效、真实名全程拒挂);与形状治理(派生)正交互补 |
| **服务端 fork 视图(§3.4.1)** | `view fork` 把基视图的目录项**复制成独立元数据树**(保留 ino/硬链接、字节零复制):结构可写(mkdir/rename/mv/rm)、内容只读(`EROFS`);删除只删本视图的 `meta/<名>.db`,基视图与字节不受影响 | 可自由整理树形但不伤基视图的“组织沙箱”;内容仍共享一份、不复制字节 |
| **树内多入口:硬链接 + 软链接** | 同一份字节在同一棵树里摆出多种组织(按年份/按类型/快捷入口) | 同 view 内的“多视图目录” |

**硬链接 = 一份字节、多个名字**(本案例最实用的招式):

```bash
cd /mnt/da
mkdir -p by_year/2026 by_type/docs
ln imported/report.pdf by_year/2026/report.pdf    # 硬链接:两个目录项指同一个对象,字节只存一份
ln -s by_year recent                               # 软链接:目录级快捷入口(只是一个路径字符串)
rm imported/report.pdf                             # 安全:硬链接还在,对象不会被删
ls by_year/2026/ by_type/  recent/                 # 从任一个入口打开都是同一份数据,B 机同样可见
```

GC 语义(源码保证):对象只在**最后一个引用它的目录项**消失时才从 `blocks/` 删除;refcount 读写在同一事务内完成,并发硬链接不会造成误删或泄漏。软链接反之——它不占引用,指向的文件被真删它就悬空;也**不要拿软链接指向另一个 view 的路径**(在谁的挂载点下,另一棵树根本不可见)。

### 14.4 “B 只想看到一部分”的现有路径与未实现清单

现有四种可落地做法:

1. **`mount --layout` 显示透镜(§3.1),或其服务端集中定义版——派生视图(§3.2)**:B 的挂载加 `only /by_public` 或 `hide /photos/private`(本地规则),或由管理员 `grovefs view define` 后 B 直接 `mount --view <派生名>`(免分发规则文件、同名跨机同形、可按名配只读 key)——同一棵权威树,B 的挂载点只呈现被允许的形状,**字节零复制**;但两者都是显示定制**不是硬安全边界**(B 不带透镜/直挂基名仍见全树,拦住他的是 token / key)。区别在写权限:**派生(虚拟)视图挂载一律只读**(形状由服务端治理,写只能走基名),而客户端自备的 `--layout` 透镜仍可写(直写真实条目)——B 需要可写的形状就用 `--layout`,只要只读分发形状就用派生视图。
2. **同 view + 目录约定**:只读端 B 用软链接/习惯入口只进 `by_public/` 这类发布目录——简单,但是**约定不是强制**(B 能 `cd` 到任何地方)。
3. **拆两个 view + 灌两遍**:`share_pub` 只放可公开内容(导入时用 `grovefs import --source` 指子目录/或 rsync 加 `--include/--exclude` 筛),B 挂这个 view——**硬隔离、真只读(配只读 key)**,代价是公开部分字节多一份(可写共享对象仍无此功能)。若要真安全隔离选这条。
4. **fork 视图(§3.4.1)**:`view fork --name L --base R` 把 R 的目录项复制成 L 的**独立元数据树**——字节零复制(对象共享、跨视图账本记账),B 可随意 `mkdir`/`mv`/`rm` 组织自己的树形,但**内容只读**(上传/改内容 `EROFS`);删 L 只删它自己的元数据,R 与字节毫发无损。适用“要一份可自由整理、互不打扰的副本树,且不往里写新内容”。

**未实现,别在当前版本上期待**(诚实标注):

- 子树挂载(`--subpath`/`--prefix`):把挂载点限定到视图内某个目录——没有(`--layout only/hide` 是显示裁剪,不是路径 chroot);
- 跨 view **可写**硬链接/共享同一对象——没有,`ln` 只在树内,分视图写数据即两份字节;跨视图共享字节只有**内容只读**的 fork 视图(§3.4.1,结构可整理、上传/改内容 `EROFS`);
- 导入后源目录的**持续自动同步**(watcher/定时重扫)——没有,`grovefs import` 是手动幂等增量命令,重跑才补新文件;rename/替换写类外部更新也靠重跑跟进(§3.3);
- **远程直连导入**——`grovefs import` 只收 `--meta-dir`/`--data-dir`(本地路径),**不接受 `--meta-server`/`--data-servers` 远程地址**,没有"在别的机器上把目录灌进远端集群"的入口。原因:import 直接写权威 `meta/<view>.db` 与 `blocks/`(零拷贝=源与 blocks 同盘硬链接),远程协议里没有对应的"批量提交整棵树"操作。跨机批量进存量走 §14.2 备选路(已挂载点 `rsync -a` 经 FUSE 写路径灌入,大文件还能条带),或在存储机上执行 import;
- **splice/zero-copy 读回**——本机 `LocalBackend` 的大块读回没有零拷贝通道。原因:当前依赖的 fuser 0.18 回复面只有 `writev`(把用户态缓冲拷进 `/dev/fuse`),`ReplyData` 只收 `&[u8]`,无 `splice`/`sendfile`/`vmsplice` 入口——要做必须 fork fuser 自己接管管道。实测收益上限不够覆盖这份复杂度与风险:warm 64MiB 读 1318MB/s vs 同盘 ext4 原生 1402MB/s(差距 ~6%),cold 1GiB 读 2.02s ≈ ext4 2.00s(差距 ~1%,受盘速主导),且远程/条带后端完全不受益;叠加页缓存一致性与 `--no-cache` 强一致挂载的正确性风险。故不做,保留为负空间。
- per-directory 强制权限——目录级权限未实现(`--layout` 可做目录级显示裁剪,但如上述不构成强制隔离)。

**文档口径**(README / USAGE / docs.html 共用一条规矩):只陈述当前版本的行为——不写已移除的参数、环境变量或开关,不做版本前后对比与历史基线数字,不写「新增了 / 现已支持 / 与旧版一致 / 退回旧行为」这类变更叙述(演变史归 git 提交记录)。当前版本内的真实回退语义、上面这份未实现清单不在此列,照常保留。

---

## 15. 场景案例集（按用途挑一条，命令改地址即可照抄）

前面各节按**机制**讲，本节按**你要干的事**给最短可执行序列。约定：

- `G=grovefs`（换成你的二进制路径，如 `/usr/local/bin/grovefs`）；地址统一写 `10.0.0.10`，端口 `50100`（数据）/`50110`（元数据）/`5432`（PG）——换成你的即可；
- PG 各节在本机 PostgreSQL 12 上**逐条实跑**过（建库、起服务、挂载、巡检 SQL、`pg_dump` + 新库恢复、`view rm --base` 的事务性销账 + `DROP SCHEMA` + 字节回收、`refs rebuild` 的两个方向（整条链由 `.bneck/pg_view_teardown.sh` 的 44 条断言在真 FUSE 挂载上跑通）、以及下文引用的每一句拒绝原文），跨挂载 `rsync -aH` 也在两个本地挂载之间验证过硬链接与增量；实测端口 5433，正文写标准 5432，差别只在端口；
- 每条场景末尾都有一行**验收命令**，跑出预期结果再往下走，别攒到最后一起查；
- 机制细节不在这里重复，表里给交叉引用。

### 15.0 场景导航

| 我要做的事 | 看这节 | 要 PG 吗 |
| --- | --- | --- |
| 一台机器上 5 分钟看效果 | §15.1 | 否 |
| 一台存储机 + 多台客户机（LAN 共享） | §15.2 | 否 |
| 先在小机器上验证 PG 后端能不能用 | §15.3 | 是 |
| 生产中心直接上 PG（systemd + 最小权限） | §15.4 | 是 |
| 已有 SQLite 中心，想换成 PG（**会看到空树**，先读再动） | §15.5 | 是 |
| PG 日常巡检：有哪些视图、多少节点、谁在连 | §15.6 | 是 |
| PG 备份 / 恢复 / 演练 | §15.7 | 是 |
| PG 下删视图、清残留 schema、清孤儿字节 | §15.8 | 是 |
| PG 下哪些命令不能用（以及替代做法） | §15.9 | 是 |
| 已有目录零拷贝入库 | §15.10 | 否 |
| 读写 / 只读 / 视图绑定分权 | §15.11 | 否 |
| 多 data-server 条带扩容 | §15.12 | 否 |
| 派生视图 / 别名 / fork 三件套怎么选 | §15.13 | 否 |
| 智能体（MCP）接入与审计 | §15.14 | 否 |
| 长命令瘦身：配置文件 + 环境变量 | §15.15 | 否 |
| 报错对症表 | §15.16 | — |

### 15.1 场景：单机 5 分钟看效果（无服务进程）

```bash
G=grovefs
export GROVEFS_HOME=/data/demo        # 之后所有命令都不用再写 --home
$G init --view team                   # 建一棵物理视图树(meta/team.db)
$G mount --view team -m /mnt/team     # 默认转后台;前台调试加 --foreground
mkdir -p /mnt/team/docs && echo hello > /mnt/team/docs/a.txt
cat /mnt/team/docs/a.txt              # hello
ls /data/demo/blocks | head           # 字节真正落在这里(按 key 命名)
$G umount -m /mnt/team                # 收尾(别用 umount 命令,它不会清理后台进程)
```

验收：`cat` 输出 `hello`，`blocks/` 下出现至少一个对象文件。单机模式没有 token、没有网络端口，FUSE 挂载点权限就是 OS 权限。

### 15.2 场景：一台存储机 + 多台客户机（SQLite 中心，最小可用）

存储机 `10.0.0.10`（数据与元数据同盘，`/data/cluster` 下 `blocks/` 与 `meta/` 并列）：

```bash
G=grovefs; TOK=$(head -c 24 /dev/urandom | base64)   # 两端用同一个值
export GROVEFS_TOKEN=$TOK                           # 服务端/客户端都读它,省掉 --token
$G init --view team --home /data/cluster            # 1) 先建基视图,再起服务
$G data-server --listen 0.0.0.0:50100 --data-dir /data/cluster &
$G meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 &
```

任一客户机（客户端**不装数据库、不落盘元数据**）：

```bash
export GROVEFS_TOKEN=<同一串>
G=grovefs
$G mount --view team -m /mnt/team \
   --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110        # 读写
$G mount --view team -m /mnt/team_ro \
   --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 --ro   # 本机只读
```

验收：A 机 `echo x > /mnt/team/t.txt`，B 机 `ls /mnt/team_ro` 在 2 秒内看到（TTL 默认 ≤2s，无需重挂）。要**服务端硬只读**而不只是本机 `--ro`，看 §15.11。

> **视图名必须先建后挂**:视图名必须先 `view create` / `init` / `view import` 出来才允许挂载,拼错的名字收到可读拒绝而不会静默长出一棵空树(这是唯一行为,没有开关;细节见 §4)。

### 15.3 场景：先用 PG 后端跑通（单机验证，比 §15.2 只多一个参数）

```bash
# 0) 中心机上准备角色与库(客户端什么都不用装;已存在则这两条各自跳过)
sudo -u postgres psql -c "CREATE ROLE grove LOGIN PASSWORD 'grovepw';"
sudo -u postgres createdb grovefs
sudo -u postgres psql -d grovefs -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grove;"
#    只需 CREATE(建 schema/表)+CONNECT,不需要 SUPERUSER,不需要预建任何表

# 1) 起服务:meta-server 只多一个 --meta-store
G=grovefs; TOK=s3cr3t-change-me
export GROVEFS_TOKEN=$TOK
export GROVEFS_META_DIR=/data/cluster                      # 省掉两处 --meta-dir
$G data-server --listen 0.0.0.0:50100 --data-dir /data/cluster &
$G meta-server --listen 0.0.0.0:50110 \
   --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs &

# 2) 显式建视图:PG 下就是一个 schema(挂载不会建)
$G view --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs create --view demo

# 3) 客户端:挂载命令与 SQLite 后端一字不差(线协议不变)
$G mount --view demo --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 -m /mnt/demo
mkdir -p /mnt/demo/docs && echo hi > /mnt/demo/docs/a.txt
```

**看 PG 里到底建了什么**（本机实测输出在括号里）：

```bash
sudo -u postgres psql -d grovefs -c '\dn'
#   grovefs_refs        ← 跨视图引用账本(共享表 grovefs_refs.holders)
#   grovefs_v_64656d6f  ← 视图 demo 的 schema
#                        (名字 = grovefs_v_ + 视图名的 UTF-8 字节的十六进制)
printf %s demo | od -An -tx1 | tr -d ' \n'; echo   # 64656d6f,自己算一下对得上
sudo -u postgres psql -d grovefs -c '\dt grovefs_v_64656d6f.*'
#   node / ctrl / locks 三张表(节点、HA 纪元、锁)
```

验收：`\dn` 里出现 `grovefs_v_64656d6f` 与 `grovefs_refs`；`cat /mnt/demo/docs/a.txt` 输出 `hi`；`ls /data/cluster/meta` 里**没有** `demo.db`（权威树在 PG，不在文件里）。

> 三个必须知道的行为：① 每个视图的 schema 必须由 `view --meta-store postgres://… create` **显式建好**（挂载不会建）；在 PG 后端上跑不带 `--meta-store` 的 `init`/`view create` 只会在本地造一个用不到的 `<view>.db`；② 视图名超过 **25 字节**会被拒（PG 标识符 63 字符上限，hex 编码后翻倍，中文约 8 个字），客户端只会看到 `Input/output error` + `background mount exited`，真实原因在 meta-server 日志里：`view name '...' incompatible with PG schema naming (empty, or longer than 25 bytes)`；③ `--meta-store` 与 `--role standby`/`--peer`/`--witness` 同时给会启动即失败（见 §15.9）。

### 15.4 场景：生产中心用 PG（systemd + 最小权限 + 连接串不落 argv）

中心机（PG 与 GroveFS 同机或分机都可，客户端不感知）：

```bash
# 1) 一次性:角色、库、最小权限(角色/库已存在则跳过)
sudo -u postgres psql -c "CREATE ROLE grove LOGIN PASSWORD 'grovepw';"
sudo -u postgres createdb grovefs
sudo -u postgres psql -d grovefs -c "GRANT CREATE,CONNECT ON DATABASE grovefs TO grove;"

# 2) 连接串只放 root 可读文件,不进 systemd 单元(argv 在 ps 里全网可见)
sudo install -d -m 700 /etc/grovefs
sudo sh -c 'cat > /etc/grovefs/meta.env' <<'EOF'
GROVEFS_TOKEN=换成你的32字节随机串
GROVEFS_META_DIR=/data/cluster
GROVEFS_META_STORE=postgres://grove:grovepw@10.0.0.9:5432/grovefs
EOF
sudo chmod 600 /etc/grovefs/meta.env
```

`/etc/systemd/system/grovefs-meta.service` 只改 `ExecStart` 一行（其余照 §11.4）：

```ini
[Service]
EnvironmentFile=/etc/grovefs/meta.env
ExecStart=/usr/local/bin/grovefs meta-server --listen 0.0.0.0:50110 \
          --meta-store ${GROVEFS_META_STORE}
```

> `GROVEFS_META_STORE` 有双重身份：它既是 systemd `${VAR}` 展开的来源，也是 GroveFS 自己的 `--meta-store` env 回退（`view`/`apikey`/`mcp`/`meta-server`/`data-server`/`du` 未给 flag 时读它，源码 `resolve_store()`）——`ExecStart` 里显式带 `${GROVEFS_META_STORE}` 与省略该参数走 env 回退，取到的是同一份值。若你的 systemd 版本不展开，直接把连接串写进单元并 `chmod 600` 该单元；**不要**为了省事把它塞进 shell 历史。

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now grovefs-meta grovefs-data
systemctl status grovefs-meta --no-pager | head -5
journalctl -u grovefs-meta -n 20 --no-pager | grep -i "store\|postgres\|listen"
```

验收：`journalctl` 里出现监听行且无 `connect`/`auth` 报错；`sudo -u postgres psql -d grovefs -c '\dn'` 在第一个客户端挂载后多出 `grovefs_v_<hex>`。备份/高可用交给 PG 自己（§15.7、§15.9）。

### 15.5 场景：已有 SQLite 中心，想换成 PG（**先读警告**）

> **切过去会看到空树。** PG 与 SQLite 是两套元数据存储，彼此不读取：`meta-server --meta-store postgres://…` 启动后，某个视图第一次被打开时建的是**空 schema**（只有根节点）。`meta/*.db` 与 `blocks/` 都还在原地，但新中心不认它们。这不是 bug，是后端边界；当前版本**没有**把 SQLite 元数据导进 PG 的工具（`view export` 只从本地 `.db` 读，PG 侧没有对应导入命令；`import` 只写本地 SQLite）。

因此只有三条路，按代价从小到大：

**路 A：不切。** 你想要的若是“运维统一/高可用”，SQLite 中心配 §5.5 的主备已经够用；PG 换来的是**别人运维数据库的经验**与 PG 自己的流复制/PITR，不是性能（§4.1 有选型表）。新项目要 PG 就**第一天起**用 PG，迁移成本为零。

**路 B：重放（元数据重建，字节多一份）。** 新旧两个中心并存，用挂载把数据搬过去：

```bash
# 旧中心(SQLite)与新中心(PG)同时在线,视图名可以相同
G=grovefs
$G mount --view team -m /mnt/old --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110 --token $OLD_TOK
$G mount --view team -m /mnt/new --data-servers 10.0.0.20:50100 --meta-server 10.0.0.20:50110 --token $NEW_TOK
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 目录项与内容都按新 key 重新登记
rsync -aH --info=progress2 /mnt/old/ /mnt/new/        # 再跑一遍是增量的(只补新文件)
```

- 对象的 key 是**随机 UUID**（不是内容哈希），所以数据会**在新中心再落一份**，临时容量按旧数据量 × 1 准备；
- 搬完核对：两侧 `find | wc -l` 与关键文件 `sha256sum`；确认无误后旧中心下线；
- 旧 `blocks/` 里的字节在新中心**没有引用**，不会被自动回收（GC 是引用驱动的），要空间就按 §12.6 的取舍手工清理；
- 先在一个小视图上把路 B 走完整一遍再上生产，别拿主视图试手。

**路 C：冷改库（不提供、不承诺）。** 把 `meta/<view>.db` 的 `node` 行手工灌进 `grovefs_v_<hex>`——表结构对得上（`id/parent_id/name/is_dir/data_ref/…`），但 `ctrl` 的 epoch、锁 `locks`、序列、账本的一致性全靠自己保证。没有官方脚本，出问题无法支持，**不建议**。

### 15.6 场景：PG 日常巡检（用 psql，不要用 sqlite3）

```bash
psql "postgres://grove@10.0.0.9:5432/grovefs" -c "..."    # 下文 SQL 都写在这个位置
```

```sql
-- 1) 有哪些视图(一个视图一个 schema,另加共享账本 grovefs_refs)
select nspname from pg_namespace where nspname like 'grovefs_v_%' order by 1;

-- 2) 一条 SQL 看全:每个视图的节点数(实测可用,不需要挨个算 hex)
select n.nspname,
       (xpath('/row/c/text()',
         query_to_xml(format('select count(*) as c from %I.node', n.nspname),
                      false, true, '')))[1]::text::bigint as nodes
from pg_namespace n where n.nspname like 'grovefs_v_%' order by 1;

-- 3) 连接数:活跃视图各自持有 8 写 + 4 读池(共 12 条);空闲超 --engine-idle-secs(默认 300s)且无挂载会话的视图会被回收、连接随之释放
--    → 规划 PG max_connections 按「最近 N 分钟活跃视图数 × 12 + 突发溢出 + 运维连接」留量,不是视图总数;卸载潮后本查询应自己回落
select state, count(*) from pg_stat_activity where datname='grovefs' group by 1 order by 2 desc;

-- 4) 提交/回滚比:40001/40P01 冲突会在客户端透明重试,PG 侧体现为回滚计数
select datname, xact_commit, xact_rollback,
       round(100.0 * xact_rollback / nullif(xact_commit + xact_rollback, 0), 2) as rollback_pct
from pg_stat_database where datname = 'grovefs';

-- 5) 谁在跑长查询(卡住会拖住对应视图的写)
select pid, now() - query_start as dur, state, left(query, 60) as q
from pg_stat_activity where datname='grovefs' and state <> 'idle'
order by dur desc limit 5;

-- 6) 体积 top10(单视图三张表:node/ctrl/locks,大的一定是 node)
select schemaname || '.' || relname as tbl, pg_size_pretty(pg_total_relation_size(relid)) as size
from pg_statio_user_tables order by pg_total_relation_size(relid) desc limit 10;
```

验收：第 2 条行数 = 当前视图数，`nodes` 与客户端 `find /mnt/<view> | wc -l` 同量级（客户端多算挂载点自身）。

> 与 SQLite 巡检（§12.1）的差别：PG 下**没有** `meta/<view>.db` 可查，`grovefs view list` 也看不到 PG 里的树（§15.9）；`pg_stat_database` 的 `rollback_pct` 长期 > 10% 且写入变慢时，先看是不是同一个目录被多客户端高频 `rename`/`unlink`（行锁热点），而不是去调 GroveFS。

### 15.7 场景：PG 备份 / 恢复 / 演练

一份完整视图 = **PG 里的元数据** + **`blocks/` 里的字节**，两者要同时点备份；只备一半恢复出来是“有名字没内容”或“有对象没名字”。

```bash
DAY=$(date +%F); BK=/backup/grovefs-$DAY; install -d "$BK"

# 1) 元数据:只导 GroveFS 的 schema(表数 = 视图数 × 3 + 1;
#    实测 1 个视图 + 账本 = 4 张表,约 10KB)
pg_dump -Fc -n 'grovefs_v_*' -n grovefs_refs -f "$BK/meta.dump" grovefs

# 2) 字节:与 1) 同一停写窗口内同步(rsync 有增量,第二次起很快)
rsync -aH --delete-after /data/cluster/blocks/ "$BK/blocks/"

# 3) 恢复到新库(演练不要动生产库)。建库与恢复要 DBA 身份:
#    GroveFS 的运维角色只有 CREATE+CONNECT,没有 CREATEDB
sudo -u postgres createdb grovefs_restore
sudo -u postgres pg_restore -d grovefs_restore "$BK/meta.dump"

# 4) 核对恢复结果(实测:表数 3N+1、节点行数一致、data_ref 与账本都在)
sudo -u postgres psql -d grovefs_restore -c '\dt grovefs_v_*.*'
sudo -u postgres psql -d grovefs_restore -tAc 'select count(*) from grovefs_v_<hex>.node'
sudo -u postgres psql -d grovefs_restore -tAc 'select count(*) from grovefs_refs.holders'

# 5) 演练:临时端口起 meta-server 指向恢复库,用只读挂载验收
G=grovefs
export GROVEFS_TOKEN=$TOK
$G meta-server --meta-dir /tmp/restore-meta --listen 127.0.0.1:50210 \
   --meta-store postgres://grove:grovepw@127.0.0.1:5432/grovefs_restore &
$G data-server --data-dir /tmp/restore-blocks --listen 127.0.0.1:50200 &   # 先把 $BK/blocks 拷成 /tmp/restore-blocks/blocks
$G mount --view team -m /mnt/restore --data-servers 127.0.0.1:50200 --meta-server 127.0.0.1:50210 --ro
find /mnt/restore -type f | wc -l        # 与生产侧同量级即可
$G umount -m /mnt/restore
```

- `pg_dump -n 'grovefs_v_*'` 的通配在 PG ≥ 10 可用（本机 PG 12 实测 7 张表）；漏了 `grovefs_refs` 只会让账本回退到“多留字节”的保守行为，不会误删；
- **高可用不要叠 GroveFS 的 `--role`**：PG 下用 PG 自己的流复制或 PITR（§15.9 有 fail-fast 原文）；
- 恢复后客户端的挂载点必须先全部卸掉再切库，重挂即可；不要把恢复库直接改成生产库名跑在线流量。

### 15.8 场景：PG 下删视图、清残留 schema、归位账本

一条命令做完三件事：**销账**（`grovefs_refs.holders` 归零行删除）+ **删树**（`DROP SCHEMA … CASCADE`）+ **回收字节**（无人再持有的 `blocks/` 对象 `backend.remove`）。

```bash
G=grovefs
# 1) 先把挂载全部下线(在线客户端遇到 schema 消失会 EIO)
$G umount -m /mnt/demo

# 2) 删整棵基视图:销账与 DROP 在同一事务里,不会出现“树没了账还在”
$G view --meta-dir /data/cluster --meta-store postgres://grove:pw@10.0.0.9:5432/grovefs \
   rm --name demo --base --data-dir /data/cluster
#   removed base view 'demo' from postgres://…: schema dropped with 6 name(s), 4 object(s) no longer held by any view
#   4 object(s) freed from /data/cluster/blocks, 0 not found under that data root (wrong --data-dir? …), 0 could not be freed
#   note: new mounts of 'demo' are now refused — `view create` starts over; the definitions this meta-dir holds were not touched

# 3) 归位账本(只在静止时跑):按存活树重算,删掉没有任何树认领的行
$G refs --meta-dir /data/cluster --meta-store postgres://… rebuild
#   refs ledger rebuilt in postgres://…: 3 tree(s), 12 content key(s), 12 holder unit(s); 2 row(s) corrected, 1 row(s) deleted
```

- **为什么能原子**：PG 的 DDL 是事务性的，所以“逐 key 减一格 holder（归零即 `DELETE`，并记下这个 ref）→ `DROP SCHEMA … CASCADE`”可以在**一个事务**里提交，崩溃也不会留下“树没了、账还在”的中间态；相较 SQLite 档的“先删 `<view>.db`，再 `refs rebuild` 重算整本账”，这里不需要事后重建；
- **字节在这一步回收**：删完 `node` 行，`blocks/` 里的对象就是孤儿；CLI 拿删除前读到的 `DISTINCT data_ref` 逐个 `backend.remove`，**只有账本归零的**才删（仍被别名/fork/其它视图命名的计入 `still held by other views`）。`--data-dir` 指向真正存 `blocks/` 的那台，点错只会如实报 `not found`，不会假称已回收；条带 key（`s<N>:<stripe>:<uuid>`）在 data-server 上，普通本地根回收不了，计入 `could not be freed` 并跳过；
- **删不掉的情况**：名字本身还被定义治理（先用不带 `--base` 的 `view rm` 删定义）、仍有定义经此基解析、本机还挂着——与 SQLite 同一套守卫，文案逐字相同（§3.4.3）；
- **二次删除会失败**：报 `no schema grovefs_v_<hex> in the database — 'demo' has no base tree here (a governed name is removed with \`view rm\` WITHOUT --base; …)`，退出码非 0。把“什么都没做”报成成功，会让打字错误静默通过；
- **绕过 CLI 手工 `DROP SCHEMA … CASCADE` 的代价**：账本是无 view 列的全局表，裸 drop 只删树不销账，该视图订过的每个对象留下一行；更要紧的是**它订过哪些对象只有那个 schema 知道**，ref 清单随 schema 一起消失——下面第 3 步能把账本归位（多计的行删掉），但**归不了字节**，那些对象从此没有记录可回收。`.bneck/pg_view_teardown.sh` 的 E/H 段就是为这件事写的：drop 前后各记一次，最后断言“剩下的正好是它订过的那些”并写明原因；
- **`refs rebuild` 的方向**：枚举 `grovefs_v_*` schema → `view_from_schema_name` 过滤出本编码的视图名 → 逐树 `SELECT DISTINCT data_ref` → 按“还有几棵树命名它”重写 `grovefs_refs.holders`。**一棵树读失败就整体中止**（少算会走向误删，多算只多留字节）；健康库上是 no-op（`0 row(s) corrected, 0 row(s) deleted`）；读树不读账本，所以它既能删掉越界删树留下的过计，也能把丢失的记账（`holders` 行被误删）从树恢复回来——两个方向都在 `pg_view_teardown.sh` 的 E/F 段实测；
- **`pg_drop_views.sh` 已降级为 out-of-band 巡检器**：CLI 能做的事它不再抢着做，只在“手上没有 grovefs 二进制”“要清 CLI 删不到的残留（没有任何定义引用、`view list` 也看不到的孤儿 schema）”“想在不启动服务的机器上核账本”时用（`--list` 列 schema 与解码出的视图名，不传 `--yes` 一律干跑）；
- **`gc` 在 PG 档仍拒绝**：它只审计本地 page 文件，PG 下树是 schema、字节归 data-server，没有等价命令（§15.9）——字节回收只在上面第 2 步发生，所以“先删树再想回收”本来就是错的顺序。

- **`grovefs_refs` 不要随手 DROP**：它是跨视图删除安全的账本，丢了只会让系统保守地多留字节（不会误删对象）；要从丢失状态恢复，用上面第 3 步的 `refs rebuild`。没有任何定义引用、CLI 删不到的残留 schema，用 `bash .bneck/pg_drop_views.sh --list` 先看清楚，再 `--yes` 清（干跑默认）；
- **删完确认**：§15.6 的第 1 条 SQL 里那个 schema 消失、客户端不再能挂载该视图名；账本里 `select count(*) from grovefs_refs.holders` 归零。

> 套件：`.build-bin/test_pg.sh`（24 条）与 `.build-bin/test_pg_parallel.sh`（41 条）末尾都用 `view rm --base` 自清场，跑完库里 0 schema / 0 账本行（此前每跑一次就漏一个 schema）；`.bneck/pg_view_teardown.sh`（44 条）把上面整条链在真 FUSE 挂载上从头跑到尾。三条都是“跑完不留痕”，TEARDOWN 不再是人工步骤。

### 15.9 场景：非 SQLite 后端下的能力边界（哪些命令不能用、替代做法）

PG 与 FDB 各一表；每行的“实测原文/行为”都在本机当前构建上跑过：

> **运维机只需账号密钥**：下表里的 `--meta-store postgres://…` 写法适用于「在持库主机上执行、或执行者本身就是 DBA」的场合。要在**别的机器**上管同一份元数据，服务端开 `--control-listen`，管理机改用 `--control-server HOST:PORT --api-key <admin key>`，由 `meta-server` 代做同样的读写（§3.5.2）；FDB 档同理。

#### PG 档

| 操作 | PG 后端下 | 失败原文 / 实际行为 | 替代做法 |
| --- | --- | --- | --- |
| `init` / `view create`（不带 `--meta-store`） | 不建 PG 树 | 只会在本地造一个用不到的 `meta/<view>.db` | 加 `--meta-store postgres://…`：`view --meta-store … create --view V` 显式建 schema；挂载从不建树 |
| `view fork` / `view graft` | **可用** | 一次 `INSERT … SELECT` 把源 schema 的全部行复制进新建的 `grovefs_v_<hex>` 目标（同一条事务，id/ino/硬链接/mode/owner/时间全保留），`graft` 则在同事务里挑选子集并重发 id；回显 `forked view 'lab' from base 'alice': 6 entries, 3 shared object(s)`，字节零拷贝，共享账本里每个被命名的对象多一个 holder（`forksrv` 套件 PG 档实证） | 不需要代替做法；只想看裁剪后的树仍可用派生视图（§15.13）或别名（§3.4） |
| `view export` / `view import` | **可用** | 导出走 REPEATABLE READ 只读快照：把 schema 写成与 SQLite 档逐行同形的宿主无关 JSONL（表头 + 每节点一行）；导入在单事务里 DELETE → 逐行 INSERT → 结构/计数校验 → `setval` 抬高序列 → 按旧→新修正账本，失败整体回滚（新建的 schema 一并 `DROP`）。目标已有树需 `--replace`，`--as-fork` 写 `fork <base>` 定义，坏 dump 不留半成品（`xfer` 套件 PG 档实证） | 整库级备份仍用 `pg_dump -n grovefs_v_<hex>`（§15.7）；两件事不冲突 |
| `view list` | 可用(加 `--meta-store`) | `view --meta-store postgres://… list` 读 `grovefs_control.view_defs` + `grovefs_v_*` schema;不带 `--meta-store` 只看本地文件(输出 `no view definitions under …/meta/views`) | 同左,或 §15.6 巡检 SQL |
| `view rm --base`（连字节删整个基视图） | **可用**（`view --meta-store postgres://… rm --name V --base --data-dir R`） | 在一个事务里逐 key 减 `grovefs_refs.holders`（归零即 `DELETE`）后 `DROP SCHEMA grovefs_v_<hex> CASCADE`，提交后拿删除前读到的 `DISTINCT data_ref` 逐个 `backend.remove`。回显分两行陈述：`schema dropped with 6 name(s), 4 object(s) no longer held by any view` / `4 object(s) freed from …/blocks, 0 not found …, 0 could not be freed`。二次删除报 `no schema grovefs_v_<hex> in the database — 'V' has no base tree here …` 并以非 0 退出 | 守卫与 SQLite 同一套（受治名/依赖定义/本机活挂载）；`--data-dir` 指向真正存 `blocks/` 的根；§15.8 |
| `import`（零拷贝） | 不适用 | 会在本地造 `<view>.db`，元数据不进 PG | 挂载 + `rsync`（§15.5 路 B 同法） |
| `refs rebuild` | **可用**（`refs --meta-store postgres://… rebuild`） | 重算的是**库里**的共享账本（不是本地 `meta/refs.db`）：枚举全部 `grovefs_v_*` schema → 逐树 `SELECT DISTINCT data_ref` → 按“还有几棵树命名它”重写 `grovefs_refs.holders`，删掉没有任何树认领的行。健康库是 no-op（`0 row(s) corrected, 0 row(s) deleted`）；越界删过 schema 时把过计的行删掉（实测 `0 tree(s) … 3 row(s) deleted`），`holders` 行被误删时从树恢复（实测 `2 row(s) corrected`）。**一棵树读失败就整体中止**（少算走向误删） | 只在**静止**时跑；常态由服务端事务维护，不需要定期跑。逐字配方见 §15.8 |
| `du`（字节总量） | **可用**（`du --meta-store postgres://… --view V --data-servers HOST:PORT,…`） | 先查 `information_schema` 确认 `grovefs_v_<hex>` 存在，再 `SELECT … FROM <schema>.node` 读树，字节向 data-server 批量 stat（条带存储的地址列表长度必须与写入时一致，否则拒绝而不归零）；名字不存在时三档同一句 `no tree for view 'V' in postgres://… — \`grovefs view create --view V\` makes one …`，不会把“拼错的名字”报成 0 B | 同 SQLite（§8.4） |
| `apikey` | 可用(加 `--meta-store`) | key 与审计进 `grovefs_control.api_keys`/`mcp_calls`,本地不建 `api_keys.db`(`control` 套件实证) | 同 SQLite(§15.11) |
| `mcp` | 可用(加 `--meta-store`) | 治理与只读探查十工具全走 `grovefs_control`(`list_view_tree` 也能走 PG 树);`fork_view`/`graft_view` 与 CLI 同一实现，在 PG 上就是跨 schema 复制 | 同左;巡检亦可 psql(§15.6) |
| HA：`--role standby` / `--peer` / `--witness` | 启动即失败 | `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG 流复制 / 逻辑复制 / PITR |
| 视图名 > 25 字节 | 拒绝 | 服务端日志：`incompatible with PG schema naming (empty, or longer than 25 bytes)`；客户端只看到 `Input/output error` | 名字控制在 25 字节内（中文约 8 字） |
| 连接池 | 默认值 + env 可调 | 每视图默认 8 写 + 4 读(共 12 条)，`GROVEFS_PG_WRITE_POOL`/`GROVEFS_PG_READ_POOL`（各 0–64）可调；挂载会话钉住引擎，卸载且静默 `meta-server --engine-idle-secs`（默认 300s）后回收、下次访问自动重开（`evict` 套件实证） | 按「**活跃视图数** × (写池+读池) + 突发溢出」规划 `max_connections`；写热点先查锁争用，不是加池 |

#### FDB 档

| 操作 | FDB 后端下 | 失败原文 / 实际行为 | 替代做法 |
| --- | --- | --- | --- |
| `view --meta-store fdb://… create --view V` | **建树的唯一方式** | 开出一个 subspace（写根行与两个分配器键）；不带 `--meta-store` 只会造一个服务端读不到的本地 `meta/V.db` | 建完挂载写内容 |
| `view create --manifest` | 拒绝（本地形态） | `` `view create --manifest` seeds a SQLite db file; under `--meta-store fdb://…` mount the view and write the files instead ``；**但 `--control-server` 下可用**：manifest 以文本发到服务端，由服务端按自己的后端 seed | 挂载后直接写文件，或改用远程治理口（§3.5.2） |
| `view fork` / `view graft` | **可用** | 在集群里新建一个子空间并把源子空间整段复制进去（装不下一个事务就分批、根行最后写，id/ino/硬链接/mode/owner/时间全保留）；回显 `forked view 'lab' from base 'alice': 6 entries, 3 shared object(s)`，字节零拷贝，`refs` 账本给每个被命名的对象多记一个 holder；`graft` 换 id/ino 写入挑选子集（同样按预算分批，`--skip-existing` 续跑，`forksrv` 套件 FDB 档实证） | 不需要代替做法；只想换个名字看同一棵树仍用别名/派生视图 |
| `view export` / `view import`（单视图元数据搬运） | **可用**（跨后端与同后端的迁移通道，§3.4.2） | 导出把一个 subspace 写成与 SQLite 档逐行同形的宿主无关 JSONL（表头 + 每节点一行，无绝对路径/主机名/`blocks/` 路径）；导入在集群里**开出这个名字的 subspace**（`view create` 之外唯一的建树入口），保留 id/ino/硬链接/mode/uid/gid/时间/symlink 文本，同事务更新共享字节账本，并把两个分配器水位抬到导出最大值（对位 PG 的 `setval`）；目标已有树需 `--replace`，坏 dump 整体拒绝、不留半成品；目标若是 **corpse**（有键、无根行）默认拒绝并给两个出口：`view rm --name V --base`，或 `--replace`；带 `--replace` 时**收养**它——先按 256 个引用一个事务退订、再清前缀，然后照常分批写入，这正是分批 `import` 中断后的续跑路径（实测：54,472 键 / 10,894 条目的 corpse 被 `--replace` 收掉并重写 20,000 行，`refs rebuild` 报 `0 corrected / 0 deleted`）；导出对同一形状也拒绝（按根行区分 corpse 与从未建制，两者给不同出口）；`--as-fork` 在此档可用（写 `fork <base>` 定义 + 树，定义写失败则回滚整个子空间）；`--control-server` 下同一条路，只是 dump 分片流过治理口 | 字节另走对象存储：`rsync -a` 数据根，或让 data-server 服务同一批对象（§14.2） |
| `view rm --base`（连字节删整个基视图） | **可用**（`view --meta-store fdb://… rm --name V --base --data-dir R`） | 删的是**子空间**而不是 db 文件：清空该视图的全部键 → 按幸存子树重算共享 `refs` 账本 → 对已无人持有的对象走 data-server 的 `remove`。回显把两件事分开陈述，不叠成一个数：`removed base view 'V' from fdb://…: 3505 key(s) gone, 701 node(s), 1 object(s) no longer held by any view` / `0 object(s) freed from …/blocks, 1 not found under that data root (wrong --data-dir? …), 0 could not be freed` / `note: new mounts of 'V' are now refused — \`view create\` starts over; the cluster's view definitions were not touched (\`view scan\` confirms the subspace is gone)`。删除**不碰**集群 `ctl` 里的视图定义、别名与 API key | 残留清扫与验收：删完用下方 `view scan` 确认子空间已消失；`--data-dir` 指向真正存 `blocks/` 的那台（meta/data 分家时点错只会如实报 not-found，不会假称已回收） |
| `view scan`（集群侧清点） | **可用**（`view --meta-store fdb://… scan`） | 列的是**集群里的物理子空间**，不是 `view list` 那份治理目录（定义 + 基名）：`SUBSPACE / KEYS / ENTRIES / KIND / PREFIX`，`KIND` 区分 `view`（有根行的活树）、`corpse`（有键但无根行，挂不上）、`ledger`（共享账本）、`lease`（写者租约表，不属于任何视图）；表尾给出汇总与 PREFIX 十六进制（即 `fdbcli clearrange` 的参数） | 清残留按这个表来：`KIND=corpse` 或没人认领的 `view` 都可以用上一行的 `rm --base` 收掉；`ledger` 随最后一次无人持有自动消失；`lease` 行用 `view lease --release` 清，不要当 corpse 删 |
| `view lease`（集群侧写者租约） | **可用**（`view --meta-store fdb://… lease`） | 列出 `STATE / DEPLOYMENT / HOLDER / EXPIRES`：`HELD` = 仍在续租的写者，`expired` = 行还在但持有者已不再续租；`EXPIRES` 给出 `in/ago Ns`。带 `--deployment <该 server 的 --meta-dir>` 锁定一个部署；不带 `--release` 只读。表尾说明「一个部署一个写者」与清理方法 | 清理僵死行：`lease --release --deployment <dir>`（过期行）；加 `--force` 清活行 = 主动围栏：旧持有者在下一次续租时打印 `LOST the FDB writer lease` 并以状态 1 退出，不会继续写 |
| 启动时集群不可用 | 拒绝启动（bind 之前） | `--meta-store fdb://…: the cluster reports its database as UNAVAILABLE (<集群自己的解释>) — a meta-server cannot serve a mount from it; check fdbcli -C <cluster file> --exec 'status minimal'`；探针有界（`GROVEFS_FDB_OPEN_TIMEOUT_SECS`，默认 10s） | 先把集群拉起来并确认 `status minimal` 报 available；探针超时可调（§6.3.2） |
| 同一 `--meta-dir` 的第二个 meta-server | 等 `GROVEFS_FDB_WRITER_WAIT_SECS`（默认 TTL+2s）后拒绝（bind 之前） | `deployment '<dir>' already has a live meta-server writer (holder '<host:pid:nonce>', lease expires in Ns): one meta-server per --meta-dir may write this cluster — stop that server, or wait for its lease to expire (10s, GROVEFS_FDB_WRITER_LEASE_SECS), or clear a stale row with … lease --release --deployment … --force` | 停旧实例；崩溃场景等 TTL（上限 10s）自行接管；确定旧实例不会回来就用 `--release --force` 围栏（`fdb` 套件第 16 节实测） |
| `view create`（名字已被残留占用） | 拒绝，并点名出口 | `a subspace named 'V' already holds keys in fdb://… but has no root row (a corpse — nothing can mount it) — clear it first with \`grovefs view --meta-store fdb://… rm --name V --base\`` | 按提示先 `rm --base` 清残骸再建；健康子空间（含从备份恢复回来的）在这里只报「已存在」，不会被当成残骸 |
| `import`（零拷贝） | 拒绝 | ``import operates on a local SQLite view db; under a FoundationDB deployment (--meta-store fdb://…, or GROVEFS_META_STORE) a base tree is a subspace of the cluster …`` | 挂载 + `rsync`/`cp` 走挂载点（§14.2）；已有目录批量入库走三步：本地 `import` 暂存 → `view export` → `view --meta-store fdb://… import`（§3.4.2） |
| `gc --data-dir`（就地形态） | 拒绝（用远程形态） | `` `gc` audits LOCAL page files under <data-dir>/blocks; a FoundationDB deployment's trees are subspaces in the cluster and its bytes are owned by the data servers — reclaim there with the data-server tooling (and back the cluster up with `fdbbackup`), not this command ``；**远程形态三档通用**：`gc --control-server … --data-servers … [--apply]`，服务端读自己的树、字节向 data-server 枚举 | 用 §3.5.2 的远程 `gc`；集群备份用 `fdbbackup`，两件事分工不同 |
| `refs rebuild` | **可用**（`refs --meta-store fdb://… rebuild`） | 重算的是**集群里的**共享账本（不是本地 `meta/refs.db`）：扫所有存活子空间，按 `data_ref → 还有几个视图命名它` 重写 `refs`，并删掉没有任何树认领的行；健康集群上是 no-op（`0 row(s) corrected, 0 row(s) deleted`），`1 row(s) deleted` 就是清掉了一条孤儿记账（fdb 档实测） | 只在**静止**时跑（读一遍与写一遍之间若有新记账会被旧值覆盖）；常态仍由服务端事务维护，不需要定期跑 |
| `view list` / `view define` / `view alias` / `mcp` | 可用（加 `--meta-store fdb://…`），**要求集群可达** | 视图定义在集群 `ctl` 子空间（`CtlDefTable`，MCP 治理面读写的就是它）；瞄准同一集群的每个节点共享同一份记录；不带 `--meta-store` 只看本地文件（输出 `no view definitions …`），服务端读不到 | 同 PG（§15.11/§15.13/§15.14）；定义随 `fdbbackup` 一起备，无本地控制面文件要单独拷 |
| `apikey` / MCP 调用审计 | 可用；带 `--meta-store fdb://…` 时**落集群的 `ctl` 子空间** | 不带 store 时落本地 `meta/api_keys.db`；带 store 时 key 与审计行都在 `ctl`（`CtlKeyTable`，与 SQLite 档逐条一致，`apikey` 套件 fdb 档实证），此时集群不可达就用不了——与视图定义同落 `ctl`、同一条纪律 | 同 PG（§15.11） |
| `du` | 可用 | 走 `M_TREE_DUMP`（经 meta-server），或 `du --meta-store fdb://…` 直连集群一次只读快照扫子空间；两种形态的总额都与挂载所见字节一致（`fdb` 套件实证）。名字不存在时先判根行（`subspace_exists`）再扫，回同一句 `no tree for view 'V' in fdb://… — …`，不会用 0 B 冒充空树 | 同 SQLite |
| 挂载（`mount --meta-server`） | 可用 | 视图未 `view create` 时按名拒绝，不自建空树 | 先建后挂（§4） |
| HA：`--role standby` / `--peer` / `--witness` | 启动即失败 | `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness: GroveFS-native HA snapshot shipping is SQLite-only.` 后接 FDB 专属建议行 | 副本与恢复交给集群（`fdbbackup`）；单 meta-server 实例即可服务全部 subspace |
| 视图名 > 25 字节 / 非法字符 | 拒绝 | `view name is N bytes; the backends all cap it at 25 so no view is openable on one engine and unrepresentable on another` / `view name 'X' is not usable as an FDB subspace key` | 名字控制在 25 字节内（中文约 8 字），与 PG 同一口径 |
| 不带 `--features fdb` 的二进制收到 `fdb://…` | 启动即失败 | `… names the FoundationDB backend, but this binary was built without `--features fdb` … an fdb:// uri is never served by SQLite` | 用带 feature 的构建（§6.3），或去掉 `--meta-store` |

#### FDB 档的四件运维实测（备份恢复 / 集群整面消失 / 超大目录 / 元数据进程死掉）

表格之外，四件只能靠集群和活挂载才能回答的事，都有可执行配方与实测读数（均在单节点 dev 集群、SATA 盘上测得；不构成可用性承诺，R3 实验档不变）：

1. **备份与恢复是一个闭环，但恢复要求目标为空**。`fdbbackup start/status` 拍快照，`fdbrestore start -r <cluster>` 回灌；实测 12 步 48 条断言全绿（`.bneck/fdb_backup_mount.sh`）。三条硬约束：目标集群必须**没有同名子空间**（逐 range 检查，报 `restore_destination_not_empty`，`-k` 不解锁）；它只提交任务，需要目标集群里有 backup_agent 才会真正推进；**`fdbbackup` 备集群里的全部键——树与控制面（视图定义/别名/API key/审计都在 `ctl` 子空间）同一次快照一并覆盖；它不备 data-server 上的 `blocks/` 字节**——所以一份可恢复的部署要两样一起备：`fdbbackup` + 数据根（`--meta-dir` 只是部署身份与 scratch/config，无权威记录要单独备）。恢复完直接挂载读回内容、`view scan` 能清点回同名子空间，那才叫闭环。

2. **元数据集群整面消失时，挂载点不报错，它阻塞**。`kill -9` 掉唯一 fdbserver（单节点集群，没有 failover 可躲），8–90 秒窗口内：挂载点始终在 mount 表里（读 `/proc/self/mountinfo`，不是 `findmnt`——后者自己会 stat 目标而挂住）、FUSE 进程不死；挂载点上的 `cat`/`ls`/`create+write`/新 `dd` **全部 parked**，没有一个返回 EIO——客户端在等一个可用版本，外面套着内核的 FUSE 等待。集群回来后（实测 ~1.6s）parked 调用在 5–110ms 内被答复，**同一个挂载点直接可用，不需要重挂**；取决于事务是否已过期，恢复后**第一次** readdir/写可能报一次错（实测 rc=2），**重试一次即成功**。最要紧的一条：写入方被告知成功的文件，恢复后字节一个不少（已确认的 6/8 个 64 KiB 文件全部到位，sha256 不变）；数据半场独立，48 条断言见 `.bneck/fdb_fault_mount.sh`。

3. **两万条的单目录，列举与删除后重读逐名对齐**。经挂载点逐个 create（每个一次事务）建 20,000 个文件：153.4s（7.7ms/条）；`ls -1` 3.8s、`ls -U` 3.7s（约 5.2k 名/秒）、`find` 3.7s、`du -s` 13.2s；两次列举的行数**和集合**都与预期全名单逐名相等（既不重名也不缺口，页边界 0/31/32/33/63/64/65/1023/1024/1025/中点/末尾逐个 stat 均可解析）；从中间删 500 个后重读为 19,500 条且集合精确等于幸存集，`view scan` 报 19,501 entries（根 + 19,500）。同一套判据在 6,000 条上复跑通过。入口 `.bneck/fdb_bigdir_paging.sh`（`N=…` 可调）。

4. **元数据进程死掉是另一个故障面：写立刻报 EIO，不会 park**。`kill -9` 掉 **meta-server**（fdbserver 还活着）后，同一挂载点上的写**立即**返回 `Input/output error`（实测 1509ms 返回，客户端退避几次后放弃），文件不创建；把 meta-server 在原地址拉起来，同一挂载点直接恢复（`ls`/`cat`/新写全部成功，**重启前写入的文件字节完好**），**不需要重挂**。同段的 `--ro` 是**挂载策略**而不是视图属性：同一视图在另一个 rw 挂载上照常可写，ro 侧 `create`/`mkdir`/`unlink` 全部拒绝且理由是 `EROFS`。与第 2 条的区别在于**谁在等**：fdbserver 消失时客户端会 park 等一个可用版本（有恢复可能），meta-server 消失时请求在客户端一侧就失败了，没有可等的东西——所以两条不能写成一句话。`test_fdb.sh` 第 15 节 23 条断言把这三件事固化（`--ro` / `kill -9` 重启恢复 / 中断期 EIO）。

### 15.10 场景：已有目录零拷贝入库（SQLite 后端）

```bash
G=grovefs
export GROVEFS_HOME=/data/cluster
$G import --view legacy --source /data/legacy --dry-run          # 先看会进多少,不建库
$G import --view legacy --source /data/legacy                     # 同盘:硬链接,零额外字节
$G import --view legacy --source /data/legacy --prefix /legacy    # 挂到视图内的子目录
$G import --view legacy --source /data/legacy                     # 重跑=增量,只补新文件
```

- meta-server 部署下在**存储机**执行：`$G import --meta-dir /data/cluster --view legacy --source /data/legacy`；已挂载客户端 ≤2s 收敛，不用重挂；
- 源目录与 `blocks/` 不同文件系统时自动退化为流式拷贝（输出里 `hardlinked` 会变 `copied`）；
- 源目录里的后续外部改动（rename/替换）**不会自动同步**，靠重跑 `import` 跟进（§14.2）；PG 后端不要用这条命令（§15.9）。

验收：`$G import … --dry-run` 的目录/文件数 = 实际导入后 `find /mnt/<view>/<prefix> | wc -l` 对应部分；源文件 `ls -i` 与 `blocks/` 下对象 inode 相同（硬链接成立）。

### 15.11 场景：读写 / 只读 / 视图绑定分权

```bash
G=grovefs; export GROVEFS_TOKEN=$TOK
# 1) 服务端开门(必须重启才生效)
$G meta-server --meta-dir /data/cluster --listen 0.0.0.0:50110 --require-api-key &

# 2) 铸三把 key(密文只在创建时打印一次)
$G apikey --meta-dir /data/cluster create --label alice-rw --for mount --view team
$G apikey --meta-dir /data/cluster create --label bob-ro   --for mount --view team --ro
$G apikey --meta-dir /data/cluster create --label agent    --for mcp

# 3) 客户端:key 走环境变量,不进 argv(ps 可见)
export GROVEFS_API_KEY='<id>:<secret>'
$G mount --view team -m /mnt/team --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110

# 4) 看谁在用、用在哪儿、随时撤销
$G apikey --meta-dir /data/cluster list --view team
$G apikey --meta-dir /data/cluster calls --limit 20
$G apikey --meta-dir /data/cluster revoke --id <id 或其唯一前缀>
```

- 三层门的次序：`--token`（准入）→ `--require-api-key`（调用方）→ key 的 `--view`/`--ro`（服务端逐 op 强制）；
- 只读 key 的写请求被服务端拒（客户端 `EPERM`）；`flock`/`F_SETLK` 也算写意图，只读角色要共享锁就用一把读写 key；
- 撤销是 **op 驱动**的：撤销后该挂载的下一次服务端往返即封，完全由内核缓存回答的读不打断；
- 派生视图本来就是只读挂载（§15.13），不需要再配只读 key 做形状保护——但**看不到全树**这件事仍然只靠 key/视图绑定，不靠透镜。

### 15.12 场景：多 data-server 条带扩容

```bash
G=grovefs
export GROVEFS_TOKEN=$TOK
# 每台存储机各自起 data-server --data-dir 自己的盘(端口可同)
for h in 10.0.0.11 10.0.0.12 10.0.0.13; do
  ssh $h "grovefs data-server --listen 0.0.0.0:50100 --data-dir /data/blocks &"
done
# 客户端一条逗号列表启用确定性条带
$G mount --view big -m /mnt/big \
  --data-servers 10.0.0.11:50100,10.0.0.12:50100,10.0.0.13:50100 \
  --meta-server 10.0.0.10:50110
```

- 只有 ≥ 16MiB 的文件分块，小文件自然只落 chunk0；单条已饱和的链路不会因条带变快，要独立网卡/链路才测得出来；
- 条带布局编码在对象 key 里（`s<服务器数>:<条带大小>:<uuid>`），所以现在读旧对象仍按旧布局解析；**改变节点数只影响新对象**，旧数据不会自动重分布；
- 元数据仍然只有一份（单 meta-server 或 PG），条带只扩容**字节**的容量与带宽。

验收：`dd if=/dev/urandom of=/mnt/big/big.bin bs=1M count=64` 后，三台存储机 `du -sh /data/blocks` 都有可见增量（小文件只在一台上有）。

### 15.13 场景：派生视图 / 别名 / fork 三件套怎么选

| 你的需求 | 用哪个 | 一条命令 |
| --- | --- | --- |
| 同一棵树，不同客户端看到不同形状，**且只读分发** | 派生视图 `view define` | `$G view --meta-dir D define --name docs-only --base team --rules docs.rules` |
| 只是给同一棵树换个公开名字，**读写不变** | 纯别名 `view alias` | `$G view --meta-dir D alias --name public --real team` |
| 要一份能自由 `mkdir`/`mv`/`rm` 的独立目录树，**内容只读** | fork（**仅 SQLite**） | `$G view --meta-dir D fork --name lab --base team` |
| 客户端自己临时调形状，且要能写 | `mount --layout F` | `$G mount --view team --layout rules.txt -m /mnt/team` |

```bash
G=grovefs; D=/data/cluster
printf 'only /docs\n' > docs.rules
$G view --meta-dir $D define --name docs-only --base team --rules docs.rules
$G view --meta-dir $D list                      # 定义槽 + 裸基视图都会列出
$G mount --view docs-only -m /mnt/docs --data-servers 10.0.0.10:50100 --meta-server 10.0.0.10:50110
```

- 派生视图挂载一律**只读**（形状集中治理，写入走基名 `team`）；已挂载的客户端在服务端改规则后 ≤2s 热换形，不用重挂；
- 别名是 1:1 改名，**全树读写**；别名不可再当别名/fork 的基；
- fork 是“点即快照”：基视图之后新增的名字不会注入已建的 fork；fork/graft **三档同契约**（SQLite 拷 db、PG 跨 schema `INSERT…SELECT`、FDB 复制 subspace，`forksrv` 套件三档实证，§15.9）；内容写 `EROFS` 是**客户端门**，硬边界仍是 token/key。

### 15.14 场景：智能体（MCP）接入

```bash
G=grovefs; D=/data/cluster
# 1) 一把 mcp scope 的 key(或 both)
$G apikey --meta-dir $D create --label agent --for mcp

# 2) 手动试一次 stdio(客户端通常自己拉起,这步只用于验证)
GROVEFS_MCP_API_KEY='<id>:<secret>' $G mcp --meta-dir $D --require-api-key

# 3) 客户端配置(以 Claude Desktop 的 stdio 为例)
#   command: /usr/local/bin/grovefs
#   args:    ["mcp", "--meta-dir", "/data/cluster"]
#   env:     GROVEFS_MCP_API_KEY=<id>:<secret>
```

- 工具的写操作会改变**真实挂载点**看到的形状（派生视图/别名是服务端共享的），改前先用 `preview_rules` 预览、`validate_definition` 校验（见 `grovefs-mcp` skill）；
- 每次准入都审计到具体 key：`$G apikey --meta-dir $D calls --limit 20`；
- PG 后端下 MCP 只能看到本地 SQLite 视图（§15.9），别指望它管 PG 里的树。

### 15.15 场景：长命令瘦身（配置文件 + 环境变量）

```bash
install -d -m 700 ~/.config/grovefs
cat > ~/.config/grovefs/grovefs.conf <<'EOF'
# 只写 GROVEFS_* 键;已存在的同名环境变量优先,flag 优先级最高
GROVEFS_HOME=/data/demo
GROVEFS_TOKEN=s3cr3t
GROVEFS_META_DIR=/data/cluster
GROVEFS_META_SERVER=10.0.0.10:50110
GROVEFS_DATA_SERVERS=10.0.0.10:50100
EOF
chmod 600 ~/.config/grovefs/grovefs.conf

# 之后这就是完整命令:所有旋钮从文件读
G=grovefs
$G mount --view team -m /mnt/team
$G meta-server --listen 0.0.0.0:50110
```

- 优先级恒为 **flag > 活环境变量 > 配置文件 > 内置默认**；文件只在对应 env 未设置时生效；
- 查找顺序：`$GROVEFS_CONFIG` → `~/.config/grovefs/grovefs.conf`（第一个存在的生效）；
- 不进默认值的参数：`--view`/`-m`（每次挂载都不同，写进文件只会害你）；
- `--meta-store` 有 env 回退 `GROVEFS_META_STORE`（控制面/服务端/`du` 消费，见 §15.4）——多集群机器上这是最容易残留串值的全局键，切集群前 unset；
- 文件里放口令与放环境变量等价：`chmod 600`，别进 git；后台挂载会 re-exec 子进程，环境变量与已加载的值都继承。

### 15.16 场景：报错对症表

| 你看到 | 真实原因 | 去哪儿处理 |
| --- | --- | --- |
| 客户端 `Input/output error` + `background mount exited (exit status: 1)` | 服务端在 `M_OPEN` 里**解析不出**这棵视图（PG 下名字超 25 字节、元数据库损坏等）。注意：**名字不存在不属于这一类**，那会在握手处收到可读拒绝，不建树 | 看 meta-server 日志的最后一条 `M_OPEN view='…' resolve failed: …`；key 类拒绝不走 EIO，会直接回可读原因 |
| `error: view 'x' does not exist on this meta-server (sqlite/postgres): … views are created explicitly …` | 视图名还没有树/定义,服务端拒绝未创建的视图名(任何情况下都不会自建) | §4：先建后挂(SQLite `view create --view x` / `init` / `view import`；PG `view --meta-store postgres://… create --view x`) |
| 挂载**成功**了，但目录是空的 | 视图确实存在,但树里还没有内容——例如刚 `view create` 的空树,或历史上手工遗留的空树 | `ls meta/` 确认；建错了就按 §15.8 清掉重来 |
| `no physical base tree … cannot be forked` | 在 PG 后端下跑 `view fork` | §15.13 换派生视图/别名 |
| `no view definition '<v>' under …/views` | 在 PG 部署下漏了 `--meta-store`——治理动作落到了本地文件,PG 里根本没有这条定义 | 原命令补上 `--meta-store postgres://…`;真要删视图是 §15.8 的 `view --meta-store … rm --base`(一个事务里销账 + `DROP SCHEMA`) |
| `no metadata tree … lives in Postgres` | 在 PG 下跑 `view export` | 用 `pg_dump -n`（§15.7） |
| `--meta-store postgres://… cannot be combined with --role standby / --peer / --witness` | PG 与 GroveFS 原生 HA 不能叠 | §15.9：高可用改走 PG 流复制 |
| `--meta-store fdb://… cannot be combined with --role standby / --peer / --witness` | 同一条互斥，但 FDB 档的补救话术不同：复制与快照本来就在集群内（`fdbbackup`），原生 HA 在这里没有可交付的东西 | §6.3 / §15.9：去掉 HA 参数单实例起 meta-server |
| `... names the FoundationDB backend, but this binary was built without --features fdb (it does not link libfdb_c)` | 默认构建里没有 FDB 后端。**它不会降级成 SQLite 服务这条 uri**——服务端带着拒绝标记启动、`serve()` 直接失败，`du`/`view create`/MCP 各入口同样就地报错 | §6.3：`cargo build --release --features fdb`，或去掉 `--meta-store` 用默认 SQLite |
| `fdb:// uri names no cluster file: give fdb://<path> or set GROVEFS_FDB_CLUSTER_FILE` | scheme 写了但没给集群文件，env 回退也没设（协调器/TLS/machine_class 都在那个文件里，不在 URI 里） | §6.3：`fdb://<集群文件>` 或 `fdb://?cluster=<路径>`；两者都缺时读 `GROVEFS_FDB_CLUSTER_FILE` |
| `error: fdb open <路径>: No cluster file found in current directory or default location` | `fdb://` 指向的集群文件不存在/不可读（后半句是 `libfdb_c` 自己的原文，grovefs 只加 `fdb open <路径>:` 前缀） | 路径写错是这一类：在**执行命令的那台机器**上给真实存在的集群文件，或设 `GROVEFS_FDB_CLUSTER_FILE`（§6.3） |
| 命令**挂住不返回**：客户端只打到 `using remote metadata service at …` 就没了，退出只能靠外部超时；`meta-server` 那边横幅照打、端口照听、日志不再新增行 | **集群不可达在这一档不是报错，是无声等待**——`libfdb_c` 对协调器不可达按自己的策略持续重试，本后端没有给这类等待设上限（`docs/fdb-keyspace.md` §11 把 timeout/重试上限列为可后置项）。服务端启动**不探测集群**，所以"起来了"不等于"通" | 判据在集群侧：服务端机上 `fdbcli -C <集群文件> --exec "status minimal"`，`The database is unavailable` 就是这一类（几秒内给出，不像 grovefs 那样等）。恢复可达后卡住的那笔 op 自己走通，挂载不需重挂 |
| `serialization retry exhausted after 12 attempts (view 'x')` | 一笔 op 连撞 12 次仍未提交（OCC 中止，退避 200µs→25ms 用尽）。**PG 与 FDB 同一条 driver 文案**（`src/pg_store.rs:550`、`src/fdb_store.rs:1523`），FDB 档没有锁等待这一说，出现这行就是重试预算真耗尽 | 查争用面而不是查集群：同一目录内高频 `rename`/同名 `create` 风暴；PG 侧该串由 `pg_parallel` 套件在日志里实测，FDB 侧走的是同一常量与同一分支 |
| `view name is N bytes; the backends all cap it at 25 so no view is openable on one engine and unrepresentable on another` | 视图名超 25 字节。FDB 本身不需要这个上限，上限是从 PG 的 `schema_for` 跨过来的——为了让视图在三后端间可迁移（P8） | §15.9：换成 ≤25 字节的视图名 |
| `refs … rebuilt: 0 holder entries` | 跑的是**本地文件**账本重建（没带 `--meta-store`）：它按 `<meta-dir>/meta/*.db` 重算 `meta/refs.db`，在 PG/FDB 部署里那些文件并不存在，所以 0 行 | 后端部署要带 `--meta-store`：PG 报 `refs ledger rebuilt in postgres://…: N tree(s) …`，FDB 报同样形状的集群账本；账本内容看 `grovefs_refs.holders`（§15.6/§15.9） |
| 写操作返回 `EPERM` | 只读 key（含 `flock` 类）或派生视图 | §15.11 / §15.13 |
| 写 fork 内容返回 `EROFS` | fork 内容面只读 | 写基视图；结构整理继续在 fork 里做 |
| 挂载后 2 秒内别的客户端看不到变化 | 不是 bug：TTL ≤2s；若一直不同步，查两地是否连的**同一个** meta | §4 的“两种一致性模式” |
| `connection refused` / 握手超时 | 防火墙只开了其一，或 token 不一致 | §11.6、§11.7 的验收清单 |

---

## 16. 内部机制速览与扩展开发(改代码的读者)

> 以下内容逐条从**源码与协议常量**取证,不是概念科普。使用手册其余章节只用 CLI/协议行为,不依赖本节。

### 16.1 扩展点:三个 trait,一个装饰器

- **`MetaProvider`**(`src/meta.rs`,22 方法):元数据层全部契约——`lookup`/`getattr`/`readdir`/`create_node`/`rename`/`lock_op`/`fence_handle`/`fetch_fence_epoch`…带默认实现的扩展点四个:`readdir_page`/`supports_paged_readdir`/`display_targets`/`flush_listing`(后者是“你自己缓存了列表就要能被失效推送 flush”的那一半契约)。实现者六处：`MetaEngine`(本地 SQLite,每视图一库)、`PgEngine`(`src/pg_store.rs`,PG,每视图一 schema)、`FdbStore`(`src/fdb_store.rs`,FoundationDB,每视图一 subspace;只在 feature `fdb` 的构建里存在)、`MetaStore`(enum,运行期择 Sql/Pg/Fdb,meta-server 服务端侧)、`RemoteMeta`(客户端)、`LayoutMeta`(治理装饰器)。PG 与 FDB 都不是新的树引擎语义:`MetaEngine::open_sqlite`/`open_pg` 与 `MetaStore::open_fdb` 同一方法面换存储内核——所以每个功能都自动在三后端上跑,判据就是把同一套断言换参重跑(§10)。
- **`StorageBackend`**(`src/backend/mod.rs`,10 方法):数据对象 IO——`new_key`/`create`/`read`/`write`/`len`/`fsync`/`truncate`/`remove`/`import_at`/`statvfs`。实现者:`LocalBackend`/`ShardedBackend`/`PlacementBackend`/`RemoteBackend`。256 桶分片与条带化都在这层后面,FUSE 层不知道对象躺在哪块盘。
- **`LayoutMeta`**(`src/layout.rs`,治理装饰器):包住任意 `MetaProvider` 做"源路径↔显示路径"翻译,目录项不物化。`mount --layout`(客户端侧)与服务端派生视图两条入口汇到**同一个装饰器实现**——所见形状逐字节一致不是巧合,是同一份代码。

### 16.2 线协议速查(自定义二进制帧 over TCP)

定长头 + 变长体,全大端;**一连接一问一答**,并发靠客户端连接池与服务端每连接一线程——同步 trait 因此能被 fuser 工作线程直接驱动(`src/remote/proto.rs` 模块注释即设计契约):

| 项 | 值 |
| --- | --- |
| MAGIC | `0x5948_4653` = ASCII **"YHFS"** |
| MAX_FRAME | `1<<28` = **256 MiB**——超限直接断连,不拆包 |
| 数据面操作码 | `OP_CREATE=1` … `OP_FENCE=10`(create/read/write/len/truncate/remove/import_at/statvfs/open/fence) |
| 元数据面操作码 | `M_OPEN=16` … `M_READDIRP=37`(open/ping/lookup/getattr/…/readdirp 分页) |
| 写意图前缀 | 五帧 `OP_WRITE/TRUNCATE/REMOVE/IMPORT_AT/FENCE` 体内带 `[u64 epoch]` 前缀,供 `--witness`/备节点门控陈旧节点的重放(§5.5) |

### 16.3 接入新后端的最小路径

1. 实现对应 trait(元数据换 `MetaProvider`、字节换 `StorageBackend`),方法语义对照 SQLite `MetaEngine` / `LocalBackend` 的注释;**PG 式"换内核不换方法面"**是推荐的解法——功能自动全量可用;
2. 在工厂处挂类型:数据面参考 `backend::open_local`/`open_local_roots`(`src/backend/mod.rs`),元数据面参考 `MetaEngine::open_sqlite`/`open_pg`(`src/meta.rs`)与 `MetaStore::open_fdb`(`src/meta/engine.rs`)的接法; **元数据面接入新后端的完整实例见 `src/fdb_store.rs`**(feature `fdb`:一个视图一个 subspace、SERIALIZABLE 事务体 + OCC 重试环、无连接池故无 `checkout`/`requeue`,以及为什么这里不需要 `drop_cursors`);
3. 客户端侧如需新挂载类型,在 `main.rs` 的 mount 分发处接入;协议操作码尽量保持不 变——帧布局与操作码定型于 `src/remote/proto.rs`,改动必须两侧同步升级;
4. 回归:把 `.build-bin/run_all.sh` 的对应套件换参重跑,判据以脚本输出为准(§10)。FDB 档的换参入口是各脚本顶部的 `*_BACKEND=fdb` + `.build-bin/fdb_scaffold.sh`(它负责起停服务端、且**启动前要求端口空闲**——否则 `wait_port` 会采纳陌生遗留的服务端,让下一套件跑出与代码无关的红断言);语义等价另有**三档差分对拍** `tests/fdb_tree.rs` 兜底:同一条 op 序列分别跑过 `sqlite`/`postgres`/`foundationdb`,先逐 op 比对回包、再比 `dump_tree()` 的结构同型(行号按从根遍历重新编号,故比的是树而不是发号轨迹);某一档不可达就只比剩下的档,两档都没有就自行 SKIP,不拖红全板。

### 16.4 架构取舍与开发侧边界

为什么这样设计(逐条对应 §4.1、§5、§5.5、§10、§15.9):

- **单一权威序列化点**:一个视图的树只由一个 meta-server 进程写——消除多副本分叉,代价是该进程为性能与扩展瓶颈。接受,直到需求真实出现。
- **治理在呈现层,不在存储层**:透镜/别名/fork 都不搬字节,只改目录树呈现,所以"同一份数据不同形状"零拷贝。
- **token 与 key 分层**:token 管集群准入门,key 管逐调用方归属与撤销——泄露止损粒度不同,合并反而会逼小部署背上 key 运维。
- **async 复制 RPO≈500ms / `--sync-repl` RPO=0 / `--witness` 真 fencing**:一致性档位显式交给部署者选,代价是 fencing 能力分层(无 witness 时备只防脑裂不封旧主)。
- **条带 ≥16MiB 整块 round-robin**:够用且简单,避免元数据爆炸;不做任意粒度 erasure coding。
- **advisory 锁租约式**:无等待超时、无续期任务,靠 lease + 死亡即时回收兜底——简单换语义边界。
- **吞吐优先的耐久性档位**:SQLite 写连接 `wal_autocheckpoint=0` + 后台 checkpoint 线程把同步停顿移出写热路径(§10 实测 max 125ms→33ms);复制默认 async(RPO≈500ms),`--sync-repl` 显式换 RPO=0(代价一个控制通道 RTT + 一次 `VACUUM INTO` fsync,超时诚实降级不谎报,§5.5)——每一档都写明代价。
- **PG/FDB 后端不支持零拷贝 `import` 与原生 HA**:顶层 `import` 靠同盘硬链接、自研 HA 靠 SQLite 文件级快照,远程库没有可纳管的本地盘/快照路径,按名拒绝——诚实边界(§15.9),不是 bug。`fork`/`graft`/`view export`/`import --as-fork` 三档均可用(各在自己的存储里整树复制,`forksrv`/`xfer` 实证)。

开发参考:`src/`(主模块 10 个 + `remote/` + `backend/`)、协议常量集中于 `src/remote/proto.rs`、遥测在 `src/stats.rs`(报告分组与阶段名以代码为准)、全板回归入口 `.build-bin/run_all.sh`。本节内容在网页版 docs.html 「§18 内部机制与扩展开发 / §19 性能调优方法论」有对应的分层版。

---

## 附:命令速查

| 目的 | 命令 |
|------|------|
| 建视图 | `grovefs --home H init --view U [--manifest M]` |
| 本地挂载 | `grovefs --home H mount --view U -m MP [--ro] [--no-cache] [--threads N] [--run-as-uid N] [--layout F] [--foreground|--log-file F]` |
| 远程挂载 | `grovefs mount --view U -m MP --data-servers A[,A2] --meta-server M[,M_bak] [--token T] [--api-key ID:SECRET] [--run-as-uid N] [--layout F] [--foreground|--log-file F]` |
| 定义派生视图 | `grovefs view --meta-dir D define --name V --base B --rules F`(存于 `D/meta/views/V.layout`,`mount --view V` 即得形状；虚拟视图挂载只读，写走基名 `B`) |
| 定义纯别名 | `grovefs view --meta-dir D alias --name A --real R`(公开名 1:1 映射真实视图:`A` 全树可写,`R` 不可直挂,§3.4) |
| 创建 fork 视图 | `grovefs view --meta-dir D fork --name L --base R`(复制目录项成独立 `meta/L.db`,字节共享;结构可写、内容 `EROFS`,§3.4.1) |
| 导出/导入单视图元数据 | `grovefs view --meta-dir D export --view V --out F [--force]`(宿主无关 JSONL,不含字节) / `grovefs view --meta-dir D import --view V --in F [--replace] [--as-fork B]`(原子替换,字节另拷 `blocks/`,§3.4.2) |
| 泄露后轮换公开名 | `grovefs view --meta-dir D alias --name A2 --real R --retire A`(先写后删零窗口:`A` 失效、`R` 全程不可直挂,§3.4) |
| 列出/删除视图定义 | `grovefs view --meta-dir D list`(定义槽 + 裸基视图) / `grovefs view --meta-dir D rm --name V`(派生/别名只删定义不动数据;fork 连自己的 `meta/V.db` 一并删除并重建账本) |
| 删整个基视图(连字节回收) | `grovefs view --meta-dir D rm --name V --base [--data-dir R]`(删 `meta/V.db`+WAL/SHM、`refs rebuild` 重建账本、回收无其他视图持有的字节;受治名/依赖定义/本机活挂载均拒绝;PG 档加 `--meta-store postgres://…`，销账 + `DROP SCHEMA` 在同一事务里，§3.4.3) |
| 排查/回收孤儿对象(离线) | `grovefs gc --meta-dir D [--data-dir R] [--apply] [--older-than 24h] [--json]`(默认只扫描;`--apply` 要求本存储无活挂载、本机无 grovefs 进程,永不删账本仍持有的键,PG 部署拒绝,§12.6) |
| 导入存量目录 | `grovefs import --view V --source DIR [--prefix P] [--data-dir R] [--dry-run]`(同盘硬链接零拷贝,重跑增量;在存储机执行,§3.3) |
| 服务端用量统计 | `grovefs du --home H --view V [--data-servers A[,A2]\|--data-dir R] [--path P] [-d N] [-b] [--total]`(批量取对象字节汇总,不走逐文件 stat;字节来源 `--data-servers` > `--data-dir` > 元数据根,判不出来源直接拒绝,只有确认缺失才计入 warning,§12.6) |
| 卸载 | `grovefs umount -m MP` |
| 数据服务 | `grovefs data-server --listen IP:PORT --data-dir DIR [--token T] [--require-api-key [--meta-dir D]]` |
| 任意机器治理同一份元数据(用账号密钥代替数据库密码) | `grovefs view --control-server H --api-key K create\|define\|alias\|list\|rm`(`--meta-store` 换成 `--control-server` 同上) |
| 账号密钥(治理口) | `grovefs --home H apikey create --label L --for admin --owner ACCT`(首把在持盘主机铸造) / `apikey --control-server H --api-key K list\|create\|revoke\|set-ro\|calls` |
| 开远程治理口 | `grovefs meta-server --meta-dir D --listen IP:PORT --control-listen IP:PORT`(第二个端口只跑治理动词、只认 admin key、逐调用审计) |
| 元数据服务（PostgreSQL 后端） | `grovefs meta-server --meta-dir D --listen IP:PORT --meta-store postgres://user:pw@host:5432/db` |
| 铸造接入 key | `grovefs apikey --meta-dir D create --label L [--for any\|mcp\|mount\|admin] [--owner ACCT] [--view V] [--ro]`(明文只打印一次;`mount` = 只开挂载/数据面,`mcp` = 只开治理接口,`admin` = 只开治理端口且放行前两者、`--owner` 必填) |
| 列出/撤销/审计 key | `grovefs apikey --meta-dir D list [--view V]`(带 `--view` 时只列能开 V 的 key,纯别名也匹配绑定到真实名的 key) / `revoke --id ID`(精确 id 或唯一前缀;撤销会封停该 key 的**活挂载**) / `calls --limit N`(含被拒的挂载连接) |
