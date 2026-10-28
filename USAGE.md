# GroveFS 使用说明

> 用户视图 FUSE 文件系统:物理数据集中存放在硬盘,不同用户挂载各自看到独立的文件列表,
> 列表支持自定义文件夹层级,彻底脱离 NFS。本文件说明**如何使用**与**如何处理数据**。

---

## 0. 核心概念(先理解这三点)

| 概念 | 说明 |
|------|------|
| **视图(view)** | 每个用户一棵独立的目录树(含自定义层级),存放在一个 SQLite 库里:`<home>/meta/<view>.db`。不同用户互不可见。 |
| **数据对象(object)** | 文件的真实字节。一个文件 = `blocks/` 下的一个对象文件,以 UUID 命名(或条带化的多个分块);对象按 key 哈希自动分布在 256 个桶子目录里。视图里的目录项指向对象 key。 |
| **基目录(base / `--home`)** | **单机模式**:`blocks/`(数据)+ `meta/<view>.db`(视图)的本地根,供 `init` 与不带 `--meta`/`--data` 的挂载使用;默认 `$GROVEFS_HOME` 或 `~/.grovefs`。**服务/客户端模式**:客户端不需要它(远程挂载本地零落盘);存储机上的持库命令各自声明自己的根(`meta-server --db-path`、`apikey --db-path`、`data-server --root`,优先于 `--home`,后者仅作兼容回退)。 |

数据与视图**天然分离**:视图只是"目录树 + 指针",字节永远在 `blocks/`。

### 常用参数:先说它管什么,再看怎么写

> 下面命令里 `--home`、`--db-path` 这类是**参数写法**(照抄用);它们各自管的东西以**实际含义**为准:

| 它管什么(实际含义) | 命令行写法 |
| --- | --- |
| **本地存储根目录** — 单机模式下 grovefs 存放 `blocks/`(数据)与 `meta/`(视图)的总目录 | `--home <目录>` |
| **服务端元数据根** — `meta-server` 那台机器上放权威视图库、视图定义、API key 的总目录 | `--db-path <目录>` |
| **数据落盘位置** — `data-server` 实际写字节的存储根(默认同元数据根) | `--data-root <目录>` |
| **要被纳管的已有目录** — 本就躺在盘上、想变成一个视图的那批数据 | `--source <目录>` |
| **挂载点** — 本机用来访问这棵树的目录 | `-m <目录>`(即 `--mountpoint`) |
| **中心服务地址** — 客户端连过去的元数据 / 数据服务(主机:端口) | `--meta <地址>` / `--data <地址>` |
| **视图名** — 给一棵逻辑树起的任意标签(与 Linux 用户名无关) | `--view <名>` |
| **集群准入口令** — 服务端设的共享密钥,客户端挂载时递上(数据面与元数据面各设一份) | 服务端 `--token <串>` + 客户端 `--token <串>` |
| **可撤销的接入凭据** — 服务端改按 per-caller API key 放行(`grovefs apikey create --for mount` 铸造);可列表、可审计到具体调用方、可撤销,且撤销能封停**已建立**的挂载;`--view V` 把 key **绑定单一视图**(开别的视图握手即拒),`--ro` 服务端强制只读(逐写意图 op 拒) | 服务端 `--require-api-key` + 客户端 `mount --api-key <ID:SECRET>`(或 env `GROVEFS_API_KEY`) |
| **以指定身份挂载** — 锁仲裁与审计按此 uid 记属主 | `--as-uid <uid>` |
| **只读 / 零缓存强一致** — 挂载为只读 / 关闭客户端缓存 | `--ro` / `--strict` |
| **目录列举提速的回退开关** — 关掉 READDIRPLUS（随目录项一并带回属性），退回普通 READDIR + 逐条 `LOOKUP`，仅供 A/B 与兼容排障（只有 provider 能在自己存储里分页的挂载才协商 READDIRPLUS：本地 `--home` 的 SQLite/PG，以及对接能分页的 `meta-server` 的远端挂载；派生/透镜视图与老服务端本来就不协商） | `--no-readdirplus` |
| **分阶段延迟遥测** — opt-in 记录读/写/元数据各阶段耗时(离线聚合 p50/p90/p99/max 排障定位瓶颈；退出打印 + 周期落盘 + 隐藏节点 `.grove-stats`；非吞吐声明) | `--stats` / `--stats-file <路径>` / `--stats-interval <秒>` / `GROVEFS_STATS=1`(详见 §8.1) |
| **读缓存 TTL 调优** — 放大内核 attr/dentry 缓存 TTL(默认 2s),对读多/不可变树削减重复 LOOKUP/GETATTR;跨端新鲜度仍靠默认失效推送 + 新内核 `AUTO_INVAL_DATA` 兜底,`--strict` 强制归 0 | `--tune-cache <秒>`(详见 §8.2) |

### 关于 `--view` 与路径容量

- **`--view` 与 Linux 用户名无关**——只是视图命名空间的**任意标签**。二进制不调 `getpwnam`、不校验 uid、也不要求 `useradd`。`alice`、`team-a`、`u42`、`研究组` 都能当作用户名——只要它是目标文件系统的合法文件名。它的作用只有两个:① 确定 `<home>/meta/<view>.db` 库文件名;② 在 FUSE mount 上打 `grovefs:<view>` 标签供 `mount`/`findmnt` 展示。**鉴权来自「能访问 `--home` / `blocks/`」+ 服务端的 `--token` / `--require-api-key` 门**,不来自 Linux 用户。
- **一个用户一棵树、树内路径完全自主**——挂载后,在该 mountpoint 下可以 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意层级,与在自己 home 里操作无差异。不同用户隔离的粒度是「整棵视图树」,不是单个路径。manifest 只是预建骨架的便捷入口,不是路径数量的上限;挂载后随时可 `mkdir -p a/b/c/d/...`。

---

## 1. 环境准备

```bash
# 依赖:Linux + fuse3(fusermount)、pkg-config shim(已内置,无需 libfuse 开发包)
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config   # 构建时需要
export LC_ALL=C LANG=C
cargo build --release                                       # 产物:target/release/grovefs
```

可执行文件子命令:`init` / `mount` / `umount` / `data-server` / `meta-server`。

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

## 3. 用 manifest 预建自定义层级

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

要点(都有测试断言背书):透镜只作用于显示层,**真实条目保持真实 inode**,读写/锁/条带/GC 全部直通;经透镜 `rm` 删的是源条目;其它客户端、其它挂载完全不受影响(透镜不落盘、不改服务端状态)。它是**显示定制不是安全边界**——硬隔离请用独立 view + token / 视图绑定 key(§6.1)。限制:向纯合成目录创建返回 `EPERM`;symlink 目标文本不改写;共享 meta-server 下挂合成目录的条目失效推送可能不命中透镜坐标,建议配 `--strict`;非法规则文件挂载直接 fail-fast。

### 3.2 服务端派生视图:形状集中定义在服务端(`grovefs view`)

§3.1 的透镜规则由各客户端自备;想把同一套形状**命名后存到服务端**(团队共享一个显示契约、按名授权),用派生视图:`grovefs view define` 把规则写进 meta 根的 `<db-path>/meta/views/<name>.layout`(首行 `base <物理视图>`,余行就是 §3.1 的四个动词),客户端只需 `mount --view <派生名>`,**不需带本地规则文件**。派生(虚拟)视图**一律只读挂载**——形状集中治理、只分发不修改,写入只属于原(基名)视图;要可写的自定义形状用 §3.1 客户端自备的 `--layout` 透镜。同一个 `views/` 目录还放另外两种定义——**纯别名**(治理"叫什么",§3.4,`grovefs view alias`)与 **fork 视图**(独立元数据树:结构可写、内容只读,§3.4.1,`grovefs view fork`):

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --db-path D create --view V [--manifest F]` | V=新**基视图**(物理树)名,F=可选种子清单 | 显式创建一棵**物理树** `D/meta/V.db`(等价 `init --view V`);**只有基视图的删除才真正回收底层字节**(跨视图引用账本归零才物理 GC,§5.3);派生视图/别名只 reshape/rename,从不参与物理删除 |
| `grovefs view --db-path D define --name V --base B --rules F` | D=meta 根(同 meta-server 的 `--db-path`),V=派生名,B=物理视图名,F=规则文件 | 写入/替换 `D/meta/views/V.layout`;`V` 的挂载**只读**(写走 `B`);对 `meta-server`,已挂载连接 ≤2s 内**热换形**(免重挂);本地挂载下次挂载生效 |
| `grovefs view --db-path D list` | — | 列出全部视图定义:派生视图(base/规则计数)、纯别名(→ 真实视图)与 fork(→ 基视图),以及**未定义的裸基视图**(→ 自己的 `meta/<名>.db`),§3.4/§3.4.1 |
| `grovefs view --db-path D rm --name V` | V=派生名、别名或 fork 名 | 只删定义,**不动任何数据**;例外:`rm --name <fork>` 连同该 fork 自己的 `meta/<名>.db`(+WAL/SHM)一并删除并 `refs rebuild` 重算账本(基视图与字节不受影响);删别名后基名恢复可挂(仅新挂载) |

```bash
$ cat docsonly.rules
# show only the docs subtree
only /docs
$ grovefs view --db-path $BASE define --name docsonly --base team --rules docsonly.rules
defined derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: mounts of 'docsonly' are read-only (virtual shape) — write to base view 'team'
$ grovefs view --db-path $BASE define --name team --base team --rules docsonly.rules   # 名与基同名
Error: "derived view name must differ from its base"
$ grovefs view --db-path $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s)

$ grovefs --home $BASE mount --view docsonly -m /mnt/lens &   # 本地：直挂派生名，无 --layout
derived view 'docsonly' over base 'team': 0 hide, 1 only, 0 reloc rule(s)
note: derived (virtual) views mount read-only — write to base view 'team'
$ grovefs mount --view docsonly --meta 10.0.0.5:50110 --data 10.0.0.5:50100 -m /mnt/lens_r &   # 远程：同样只给名，M_OPEN 握手带只读标志
$ ls /mnt/lens                       # 只有 docs;基名 team 挂出来仍是全树，且可写
docs
$ echo x > /mnt/lens/docs/new.md     # 经虚拟视图的写一律拒绝
bash: /mnt/lens/docs/new.md: Read-only file system
```

语义与守卫(第 9 节 e2e 验证):define 拒绝 `name == base`、拒绝 base 本身是派生视图(不嵌套)、拒绝遮蔽物理 `<name>.db`、非法规则 fail-fast;**虚拟视图一律只读(硬契约)**——写只属于原(基名)视图,三层强制:`meta-server` 在 `M_OPEN` 握手回包里带派生标志,客户端直接落内核只读挂载(写/`flock` 写锁即时 `EROFS`、零往返);服务端另对派生名的写意图 op 加独立权威门禁(回 `E_NOTPERM`→`EPERM`,有意不用 `E_ROFS` 以免被误读为 HA 故障转移信号),旧/伪造客户端绕不过;本地(`--home`)派生挂载在挂载时同判强制 RO。**改定义热生效**——`meta-server` 每次分发 op 前检查定义文件 mtime,替换后已建立的挂载在客户端 TTL(≤2s)内自动切新形状、不需重挂(实测 ≤5s 断言,见第 9 节;换形后只读契约不变),`view rm` 则不热作用(已挂载者保持旧形状到重挂,避免正在用的树被抽掉);同名多客户端由服务端**同一份定义**渲染,虚拟 inode 跨机一致;共享 meta-server 下派生订阅者同时注册进自身名与 base 名两个桶、写双播,基侧写在非 `--strict` 下也能对派生挂载即时可见;要按名限制某调用方只能读这棵派生视图,铸一把 `--view docsonly --ro` 的 key(写永远走基名,派生挂载本就一律只读)。本地模式(`--home`)同机制:挂载不带 `--layout` 时自动解析 `<home>/meta/views/<view>.layout`(显式 `--layout` 优先,不叠两层透镜;热重载仅限 meta-server,本地挂载在挂载时解析一次)。注意:`grovefs view` 操作服务端本地文件,需在 meta-server 所在机器执行;HA 部署下 `meta/views/` 要随控制面库(`api_keys.db`/`refs.db`)手工同步到备机;它同样**不是安全边界**,直挂基名仍见全树(能拦住的是 token / key,不是形状)。

### 3.3 存量数据零拷贝导入(`grovefs import`):目录原地不动,客户端连接即得

服务端已经有一批现成数据(如 `/data/legacy`)想让客户端按视图获取时,不必先灌一遍:`grovefs import` 把**已存在的本地目录树**登记进视图,字节原地不动——同文件系统下对象就是源文件的硬链接(零额外字节、两路径同一 inode 双向实时可见),跨设备自动退化为流式拷贝:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs import --view V --source DIR` | V=目标视图(不存在则创建),DIR=已存在的源目录 | 整棵树镜像进视图(默认前缀 `/<DIR名>`):目录建节点、文件硬链接进 `blocks/` 并登记;mtime/atime/权限/属主保真 |
| `grovefs import ... --prefix P` | P=视图内目标目录(如 `/legacy`) | 镜像到指定前缀下,`P` 不存在自动建 |
| `grovefs import ... --db-path D [--data-root R]` | D=meta 根,R=存储根(默认 D) | meta-server 部署下在**存储机**执行,直写权威 `meta/V.db` 与 `R/blocks/`;已挂载客户端靠 TTL(≤2s)收敛 |
| `grovefs import ... --dry-run` | — | 只报告将导入多少目录/文件,**连视图 db 都不创建** |

```bash
$ grovefs --home /data/cluster import --view legacy --source /data/legacy --dry-run
dry run: view 'legacy' under /legacy would mirror 3 dir(s) and 2 file(s) (symlinks/special files and names already in the view are skipped on the real run)
$ grovefs --home /data/cluster import --view legacy --source /data/legacy
imported into view 'legacy' under /legacy: 3 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
$ grovefs mount --view legacy --meta 10.0.0.5:50110 --data 10.0.0.5:50100 -m /mnt/da &   # 客户端连接即得
$ echo "new" > /mnt/da/legacy/docs/quarter.txt   # 经挂载写 → 源路径同 inode 同步可见
$ cat /data/legacy/docs/quarter.txt
new
$ rm /mnt/da/legacy/photos/2026/y.jpg            # 视图里删名字 → 只摘 blocks/ 链接,源文件毫发无损
```

语义要点(`import` 套件 34 条断言):**重跑即增量**(已有名字跳过、新文件补登);**GC 只摘链接**(源路径永不因视图操作被删);导入后的条目与原生写入完全同权(读写/锁/快照/失效推送全适用),并能被 §3.2 派生视图 `map`/`link` 组成任意虚拟目录形状——该形状挂载**只读**(写走导入的原视图,经双播推送穿透到虚拟形状可见);`grovefs import` 也拒绝以派生名为目标(否则写出无人可读的影子 `<view>.db`)。注意三个实现事实:硬链接要求源与 `blocks/` 同盘(跨盘自动发拷贝并如实计数);外部工具以 **rename/替换写**更新源文件(vim 默认行为)时,视图侧看到的是旧 inode 的最后内容,需对新内容重跑 import;symlink/特殊文件有意跳过。PG 后端不支持直写(走 §14.2 挂载灌入)。

---

### 3.4 纯别名:名字治理(`grovefs view alias`)

§3.2 的派生视图治理"长什么样"(一个视图对外呈现哪棵树);**纯别名**治理"叫什么"——把存储真实名藏到服务端,对外只发公开名:名字就是别名,映射值才是真实名。定义文件同放 `<db-path>/meta/views/<name>.layout`,形态互斥:首行 `alias <真实视图>`、**不带任何规则行**。与派生视图的正反面:**别名可写**(它就是真实视图,全树原形),且**真实名从此不是挂载入口**——本地(`--home`)与 `meta-server` 都拒挂被治理的真实名(公开名与存储身份解耦,对外名字可以自由轮换,猜出真实名也挂不进来):

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --db-path D alias --name A --real R` | A=公开名,R=物理视图名 | 写入/替换 `D/meta/views/A.layout`(内容仅 `alias R`);挂 `A`=全树**可写**;`R` 从此**不可直挂**(本地与服务端均拒) |
| 挂载 | 无任何额外参数 | `mount --view A` 即得真实视图全树;远程在 `M_OPEN` 服务端解析(别名解析先于准入判定,别名与基名走同一道门) |
| `grovefs view --db-path D rm --name A` | — | 删别名,真实名对新挂载恢复可挂(已建立连接不受影响) |
| `grovefs view --db-path D alias --name A2 --real R --retire A` | A2=新公开名,A=被泄露的旧别名 | **原子轮换**:先写 `A2` 再删 `A`,真实名 `R` 全程被治理、**无裸露窗口**(不会出现两步之间 `R` 短暂可直挂的空档);旧名 `A` 立即失效(挂它只落到一个全新的空视图,触不到真实树);合法用户改用 `A2` 重挂即可 |

```
$ grovefs view --db-path $BASE alias --name teamwork --real team
defined pure alias 'teamwork' -> real view 'team'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --db-path $BASE list
view 'docsonly' -> base 'team': 0 hide, 1 only, 0 reloc rule(s) (read-only)
alias 'teamwork' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/work --meta 10.0.0.5:50241 --data 10.0.0.5:50240
mounting view 'teamwork' at /mnt/work (fully remote: no local store)
$ ls /mnt/work
docs photos
$ echo hello > /mnt/work/docs/2026/new.md   # 别名挂载可写,写入即真实树
$ ls /mnt_docs/docs/2026                    # 同一写经另一路派生名(docsonly)挂载穿透可见
a.md new.md
$ grovefs mount --view team -m /mnt/t --meta 10.0.0.5:50241 --data 10.0.0.5:50240
Error: Custom { kind: PermissionDenied, error: "view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)" }   # 直挂被治理的真实名:服务端拒挂(exit=1)
$ findmnt /mnt/work | tail -1
/mnt/work grovefs:teamwork fuse rw,nosuid,nodev,relatime   # 别名可写;派生名的挂载行是 ro
```

公开名泄露后**一条命令原子轮换**(旧名即刻失效、真实名全程不可直挂、授权不动):

```
$ grovefs view --db-path $BASE alias --name teamwork2 --real team --retire teamwork   # 'teamwork' 已泄露,换成 'teamwork2'
rotated public name for real view 'team': 'teamwork' -> 'teamwork2' (gapless — 'team' stayed governed throughout, 'teamwork' no longer resolves)
note: legitimate users just remount under 'teamwork2'
note: 'team' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
$ grovefs view --db-path $BASE list
alias 'teamwork2' -> real view 'team' (full tree, writable; 'team' not mountable by its own name)
$ grovefs mount --view teamwork -m /mnt/leak --meta 10.0.0.5:50241 --data 10.0.0.5:50240   # 拿旧泄露名再挂
$ ls /mnt/leak          # 空:旧名不再解析到真实树(命名空间按需新建,数据不外泄)
$ grovefs mount --view team -m /mnt/probe --meta 10.0.0.5:50241 --data 10.0.0.5:50240        # 猜真实名同样被拒(全程治理)
Error: Custom { kind: PermissionDenied, error: "view 'team' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)" }
```

守卫(`alias` 命令处 fail-fast,layout 套件第 10/11 节 26 条断言):拒绝 `name == real`、拒绝遮蔽物理 `<name>.db`、拒绝目标是派生视图(别名只映射物理视图)、拒绝**别名链**(`--real` 指向另一个别名)、一个真实视图同时只允许一个对外名(`--retire` 轮换在同一条命令内先写后删,不受此约束卡住);`--retire` 还拒绝指向派生视图或不存在的别名(不会误删形状定义)。本地(`--home`)同契约:挂载先解析 `views/<name>.layout`,别名换名后照常挂载并打印 `alias view 'A' resolves to real view 'R' (full tree, writable)`;直挂被治理的真实名回 `Error: "view 'R' is governed by an alias — mount the alias name instead"`;`grovefs import --view A` 拒绝以别名为目标(别名不是存储身份,写出无人可读的影子 db)。实现事实:别名解析单跳、每 `M_OPEN` 重扫 `views/` 目录(挂载罕见事件,小目录全读比维护失效协议便宜);轮换/`rm` 只影响**新**挂载(已建立的旧名连接持续到老连接断开,与只读门同语义);被轮换掉的旧名落到按需新建的空命名空间,触不到真实树,故泄露无害;`meta/views/` 是普通文件,HA 备机随控制面库(`api_keys.db`/`refs.db`)手工同步;`grovefs view`/`import` 在存储机执行(操作服务端本地文件)。拒因现在**逐字节入协议**(`M_OPEN` 应答的 data 段带服务端原文),故远端拒挂真实名的回显与本地完全一致——上面两处 `view 'team' is governed by an alias …` 就是真实运行输出;同样精确的还有 token 不符、api key 缺失/无效/超范围/已撤销、视图绑定 key 拒开其它视图这些拒因(对接**老服务端**时 data 段为空,客户端才退回通用文案)。**轮换是名字治理(泄露止损),不是安全边界**:硬隔离仍靠 token+key——轮换只让旧的公开名不再通向真实树,并不能撤销一个已持有 token 者的既有访问。

### 3.4.1 fork 视图:独立元数据树,结构可写、内容只读(`grovefs view fork`)

派生视图是只读的「显示透镜」,别名是全树可写的「公开名」;两者之间还缺一档真实需求:**能整理目录树、但不能上传内容,且删掉自己的名字绝不伤基视图**。`grovefs view fork` 就是这一档——把基视图的目录项**复制**成一份独立元数据树(`<db-path>/meta/<name>.db`,保留 inode 与硬链接),内容字节一个不拷、经跨视图账本共享;定义仅一行 `fork <基视图>`:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --db-path D fork --name V --base B` | V=新 fork 名,B=本地 SQLite 物理基视图名 | 建 `D/meta/V.db`(复制 B 的目录项)并写 `D/meta/views/V.layout`(内容仅 `fork B`);此后两边各自演化,基视图之后新增的名字不会注入已建 fork |

```bash
$ grovefs view --db-path $BASE fork --name lab --base team
forked view 'lab' from base 'team': 42 entries, 37 shared object(s)
note: mounts of 'lab' get an independent tree — structure is writable, content is refused with EROFS (upload bytes belong to base 'team')
note: `grovefs view rm --name lab` deletes this fork's own tree only; shared objects stay while any view (including 'team') still holds them
$ grovefs view --db-path $BASE list
fork 'lab' of base 'team' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
base view 'team' -> own tree meta/team.db (read-write; deletes free a shared object only when no view holds it)
$ grovefs --home $BASE mount --view lab -m /mnt/lab &
fork view 'lab' of base 'team': independent tree — structure writable, content read-only (EROFS)
$ mkdir /mnt/lab/docs/2026/review      # 组织目录树:放行
$ mv /mnt/lab/docs/2026/a.md /mnt/lab/docs/2026/review/a.md   # 改名:放行(只改 fork 自己的元数据)
$ echo x > /mnt/lab/docs/2026/new.md   # 上传字节:拒绝
bash: /mnt/lab/docs/2026/new.md: Read-only file system
$ grovefs view --db-path $BASE rm --name lab
removed fork view 'lab' of base 'team' (its own metadata tree is gone; content bytes stay while any view — including 'team' — still holds a name for them)
```

语义与守卫:`view fork` 拒绝 `name == base`、拒绝目标名已有定义或已有物理 db、拒绝基名本身有定义(派生/别名/fork 不能当基)、拒绝基视图没有本地 `meta/<base>.db`(PG 后端暂不支持 fork——权威树不在文件里);`alias` 拒绝指向 fork(fork 是树身份,不是公开名——给它发 key 用 `apikey create --view lab`),`import` 拒绝 fork 目标。**结构面可写、内容面只读**:`mkdir`/`rename`/`unlink`/`symlink`/`link`/`chmod`/空对象 `create` 放行,`write`/`truncate` 一律 `EROFS`;本地(`--home`)与 `meta-server` 两条路径同判(后者靠 `M_OPEN` 回包第 3 个标志字节)。删除只摘 fork 自己的 `node` 行:共享对象由跨视图账本判定,只有**最后一个持有视图**摘名后才真正回收;`view rm --name <fork>` 删定义 + 自己那棵 `<name>.db`(+WAL/SHM)并 `refs rebuild` 重算账本,基视图与字节分毫不动。诚实边界:内容写门是**客户端门**,绕过 FUSE 直连 data-server 的 raw 协议客户端仍能写字节——硬边界仍是服务端 token+API key(§6)。可见性:`view list` 同时列出定义槽(派生/别名/fork)与**裸基视图**(无定义的物理树);`apikey list --view V` 只列能开 V 的 key(纯别名也匹配绑定到真实名的 key),并汇总未绑定 key 数。e2e 见 `fork` 套件(55 条断言)。

### 3.4.2 单视图元数据导出/导入:换机器、换系统迁移(`grovefs view export / import`)

GroveFS 的元数据**不绑定硬件**:`node` 行只存 `data_ref`(对象 key),对象路径是 key 的纯函数(`blocks/<FNV1a(key)%256 两hex>/<key>`),没有绝对路径、设备号、主机名或宿主 inode;GroveFS 自己的 `ino` 也在 DB 里。所以整库/整目录搬到另一台机器可直接用——前提是**元数据与内容字节一起走**。想按视图、跨机器、可选择性迁移时,用 `view export` 把一棵树写成宿主无关 JSONL,再在目标存储根 `view import`:

| 命令 | 参数释义 | 效果 |
| --- | --- | --- |
| `grovefs view --db-path D export --view V --out F [--force]` | D=源存储根,V=**物理基视图或 fork** 名,F=输出文件 | 写表头(format/version/kind/定义原文/节点与 blob 计数)+每行一个 node(id/inode/名字/is_dir/data_ref/link_target/mode/uid/gid/三个时间);**不含字节**;`--force` 才覆盖已存在的 F;挂载中也可导出(只读快照) |
| `grovefs view --db-path D import --view V --in F [--replace] [--as-fork B]` | D=目标存储根,V=新基视图名,F=上面导出的文件,B=目标机上已存在的裸基视图 | 在 `D/meta/V.db` 重建整棵树(保留 id/inode/硬链接/权限/属主/符号链接),完成后重建 `refs.db` 账本;已有树必须 `--replace`;`--as-fork B` 额外写入 `fork B` 定义(恢复“结构可写、内容 EROFS”) |

```bash
# 源机(挂载中也可;停写后更稳)
$ grovefs view --db-path /data/src export --view team --out /tmp/team.jsonl
exported view 'team' metadata: 42 node(s), 37 blob reference(s) -> /tmp/team.jsonl
note: metadata only — copy the store's `blocks/` tree (the whole data root) as well
# 只拷内容字节(元数据已在上面的 JSONL 里)
$ rsync -a -H /data/src/blocks/ /data/dst/blocks/
# 目标机
$ grovefs view --db-path /data/dst import --view team --in /tmp/team.jsonl
imported view 'team' from /tmp/team.jsonl: 42 node(s), 37 blob reference(s); cross-view ledger rebuilt
$ grovefs --home /data/dst mount --view team -m /mnt/team     # 硬链接仍是同一 inode,权限/时间原样
```

**导入的原子性**:先在 `D/meta/.V.import-<pid>-<时间>` 建临时树,全部行与校验通过、WAL 合并后才 `rename` 到 `D/meta/V.db`——坏文件/悬挂父节点/计数不符/失败的 `--replace` 都不会破坏旧树(旧树要么原样、要么完整换成新树,没有中间态)。

**守卫**:虚拟名(别名/派生)没有自己的树,`export` 直接拒绝并提示导出其物理基;输出文件已存在需 `--force`;导入目标已有定义槽先 `view rm`;表头 format/version 不符、根节点不是 `id=1` 的目录、悬挂父节点、表头计数与正文不符均整单拒绝;`--as-fork` 要求基视图在**目标机上**已存在、且基名本身没有定义。

**迁移的拓扑约束(数据分布,不是硬件绑定)**:

- 多盘 `--root`:对象按 `FNV1a(key) % root 数量` 选盘,目标机 root **数量与顺序要一致**;也可合并成单 root(把各盘 `blocks/` 合并)。
- 跨 server 条带(`s<N>:...`):第 i 块在 `servers[i % N]`,目标必须保留 **N 个 data-server 及顺序**;数量不符会明确拒绝。
- HA `epoch`(每个视图的 `ctrl` 表):整目录拷贝会带过去,迁移后旧主必须停;`view export/import` 与 `view fork` 有意不复制 `ctrl` 与 `locks`。
- PG 后端不能用文件拷贝,用 `pg_dump`/逻辑复制;`view export/import` 目前只面向本地 SQLite 树。
- `uid/gid` 是数值元数据(advisory),按数字保留;symlink 目标文本是用户数据,原样保留。
- 整目录冷拷时先停所有挂载与 `meta-server`;不能停机就 `sqlite3 <db> ".backup"` / `VACUUM INTO`,**不要裸拷运行中的 DB**(WAL)。

**多个基视图**:可建任意多个基视图(`init --view A` / `view create --view A`),各自一棵 `meta/A.db`,树、字节、权限、删除互不影响;`view list` 会列出全部裸基视图。`view export/import` 按视图进行,迁移时可以只搬其中一棵,也可以给不同基视图绑定不同 key(`apikey create --view A`)。

e2e 见 `migrate` 套件(74 条断言:挂载中导出/宿主无关性/虚拟名与覆盖守卫/跨绝对路径导入/硬链接与权限保真/原子替换/损坏输入拒绝/`--as-fork` 账本共享/多基视图独立)。

**规模实测**(单机、默认 IO,`bash .build-bin/perf_migrate_scale.sh` 可复跑):迁移一棵 5052 节点、5000 个对象的视图——`export` 0.19s 产出 1.16 MB JSONL(是视图库的 1.16 倍,约 230 B/节点);`import` 到新存储路径 0.56s(含账本重建);挂载后 0.23s 枚举完 5000 个文件,内容逐字节一致、权限与属主保留。同一批文件当初注册进源视图(零拷贝导入)花了 83s——那部分逐对象的存储开销完全不在导出/导入路径上。**迁移代价由命名空间大小决定,不由数据量决定**;字节另走 `rsync -a -H blocks/`。

### 3.5 智能体定制数据列表:MCP 接口(`grovefs mcp`)

视图治理(§3.2/§3.4/§3.4.1)同时是一套 **MCP(Model Context Protocol)工具**。`grovefs mcp` 在 stdio 上说换行分隔的 JSON-RPC 2.0(实现 2025-06-18 协议的常用方法面:生命周期 `initialize`/`ping`、工具 `tools/list`/`tools/call`、资源 `resources/list`/`resources/templates/list`/`resources/read`、提示 `prompts/list`/`prompts/get`、补全 `completion/complete`、日志 `logging/setLevel` 与服务端 `notifications/message`;客户端通知一律静默不作答),任何 MCP 客户端(Claude Desktop、IDE 智能体等)把它当子进程拉起,智能体即可**通过接口自动定制数据列表**:查视图、**列出某视图真实呈现的目录清单**(派生视图经其透镜渲染,还能不落盘地预览一副待发布规则会长成啥样)、用内联规则文本发布只读派生透镜(四动词同 `--layout`,规则以文本随行、不落文件)、轮换公开名、删定义、**发布前把整副定义走完全部治理守卫做干跑校验(不写盘)**。与 `grovefs view` 子命令共用同一套库函数(`grovefs::mcp`)——校验、拒绝文案、落盘动作逐字节相同,一个契约、两个入口,智能体做不出 CLI 做不出的事,也绕不过任何守卫。**探查只读**:`list_view_tree`/`get_view` 以只读方式打开本地 `<db-path>/meta/<view>.db`、经与挂载同一副 `LayoutMeta` 透镜渲染目录清单(不挂载、不读对象字节;远端/PG 后端的树不在此遍历);回包里的 `size` 是元数据字段而非字节数(目录 `4096`、文件 `0`、软链为 target 长度),需真实字节请挂载后 `stat`。**控制面 only**:不挂载、不写数据。默认信任能启动此进程的调用方(= 已持有存储根的操作系统级权限),跨网鉴权仍由服务端 token / API key 负责;当它被 front 给多个不同调用方时,可用 `grovefs mcp --require-api-key` **可选地**为接口上 API key 门(见 §3.5.1)。下面全部回显取自真实运行:

```
# 1) 存储机:建一个物理视图(智能体稍后要在它的树上定制列表)
$ grovefs --home $HOME_DIR init --view team --manifest team.manifest
initialized view 'team' at /data/xy/grovefs/.demomcp/home/meta/team.db
applied seed manifest .demomcp/team.manifest

# 2) MCP 客户端配置里登记 stdio 服务器(以通用 mcpServers 写法为例):
#    "grovefs": { "command": "grovefs", "args": ["mcp", "--db-path", "/data/xy/grovefs/.demomcp/home"] }
$ grovefs mcp --db-path $HOME_DIR       # 客户端实际这样拉起:stdin 进帧、stdout 出帧

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

九个工具与参数:治理七个——`list_views`(可选 `db_path`)、`define_derived_view`(`name`/`base`/`rules`,后者为内联规则文本)、`alias_view`(`name`/`real`/可选 `retire` 原子轮换)、`fork_view`(`name`/`base`,把基视图目录项复制成独立元数据树——结构可写、内容 EROFS)、`remove_view`(`name`,删 fork 时连同其自己的树)、`preview_rules`(`rules`,干跑校验动词语法不写盘)、`validate_definition`(`name`/`base`/`rules`,发布前把整副定义走完 define 的全部治理守卫——非法名、名与基同名、遮蔽物理 db、覆盖纯别名、规则语法——拒绝时回与真实发布逐字节相同的文案、接受时回 JSON 判定,绝不写盘);只读探查两个——`list_view_tree`(`view` 或 `base`+`rules` 模拟,可选 `path`/`depth`/`max`/`db_path`;渲染挂载实际所见的路径清单,可经已发布透镜或不落盘模拟一副规则)、`get_view`(`name`,返回单个定义的原始 `definition` 文本与计数)。同一套治理再以 MCP 其余常用方法面二次暴露,复用**完全相同的渲染器**,故资源/提示/补全与工具结果逐字节一致:**资源** `resources/list`/`resources/templates/list`/`resources/read` 把某视图作为可附着上下文——`grovefs://view/<name>/definition`(原样治理文本)与 `/tree`(挂载实见的清单);**提示** `prompts/list`/`prompts/get` 提供两副参数化治理剧本(`reshape-view`、`rotate-alias`);**补全** `completion/complete` 用存储里的真实视图名对 `name`/`base`/`view`/`real` 参数做前缀补全;**日志** `logging/setLevel` 设会话阈值后,服务端在每次应答调用后按严重度(成功 `info`、失败 `warning`)推送 `notifications/message`。语义:全部治理守卫照常适用(派生挂载只读、别名期间真实名拒挂、`--retire` 先写后删、fork 结构写放行/内容 EROFS;改定义热生效仍只在 `meta-server` 入口,本地挂载挂载时解析一次)。协议细节:通知(无 id)一律不作答;非法帧回 `-32700` 后继续服务;缺参与未知工具按 MCP 惯例回 `isError` 而非协议错误。`db_path` 默认取 `grovefs mcp --db-path`(回退 `--home`),也可逐调用覆盖。e2e 见 `mcp` 套件(48 条断言:握手能力面声明/发现/内联规则换形/路径探查与形状预览/发布前干跑校验/别名轮换/CLI 同文案/资源枚举与读取/提示实例化/视图名补全/日志阈值→通知/协议错误路径)。智能体侧的接线手册（英文，便于国际化部署）已随仓库沉淀为一个项目 skill(`skills/grovefs-mcp/`):`SKILL.md`(拉起方式、九工具参数表、黄金工作流、逐个确切拒绝文案表、硬边界)、`reference.md`(完整方法面/鉴权门矩阵/错误码/落盘效果/CLI 对照)、`examples.md`(真实回显),并附两个脚本:`scripts/mcp_call.py`(零依赖 stdio JSON-RPC 客户端,回包分开 stdout/stderr,退出码 0 成功 / 2 协议错误 / 3 工具拒绝 / 4 传输问题)与 `scripts/smoke.sh`(在 `mktemp -d` 的临时存储根上跑完全链路并逐条断言,不碰真实数据)。

### 3.5.1 给接口和挂载点上把锁:API key 认证、scope 分区与调用审计(`grovefs apikey`)

同一个服务端常常要面对**多个不同调用方**——智能体运行时把一个 `grovefs mcp` 服务 front 出去,多台客户机经 LAN 挂同一个 `meta-server`——此时需要可撤销的凭据与归属审计。`grovefs apikey` 铸造 API key(**只存 SHA-256 摘要**,明文仅在 `create` 时打印一次,故 `api_keys.db` 泄露也换不出可用凭据),并给每个 key 打 **scope**:`--for mcp`(只开治理接口)、`--for mount`(只开 `--require-api-key` 的 meta/data 服务端)、`--for any`(**默认**,两者皆可——scope 概念之前铸造的所有旧 key 都是这一档,故老部署一律无需改动)。两处入口共用同一份注册表(`<db-path>/meta/api_keys.db`),靠 scope 隔离:挂载凭据重放不到治理 API,智能体 key 也开不了数据面。挂载 key 还可带**两个访问控制维度**:`--view V` 把凭据**绑定到单一物理视图** V——用它 `M_OPEN` 别的视图在握手处即被 `E_AUTH` 拒(省略 `--view` 的是系统级 key,可开任意视图);`--ro` 标为**只读**——服务端在 meta 面对其逐写意图 op 拒 `E_NOTPERM`、data 面内容写拒 `EROFS`,并在 `M_OPEN` 回包提示客户端只读挂载(「只读」由服务端强制,不靠客户端自觉 `-o ro`)。**治理面门**:`grovefs mcp --require-api-key` 据此对所有**读写治理状态的方法**设门——`tools/list`、`tools/call`、`resources/list`、`resources/templates/list`、`resources/read`、`completion/complete`(`initialize` / `ping` / `prompts/*` / `logging/*` 始终开放,好让客户端完成握手、取静态剧本、并控制服务端详谈程度)。**挂载面门**:`meta-server --require-api-key` 卡 `M_OPEN`、`data-server --require-api-key` 卡 `OP_OPEN`,客户端以 `mount --api-key <ID:SECRET>` 呈现(不便写命令行时可导出 `GROVEFS_API_KEY='<ID:SECRET>'`,密钥随即不落 argv);**审计面**:每一次 `tools/call`、`resources/read`,以及每一次挂载连接(含被拒的)都追加进同一张审计表(时间、key、平面 method、工具 tool、成败、详情),`grovefs apikey calls` 回读。撤销只把行标记为 revoked(历史调用仍保留归属),撤销后的 key 立即鉴不上,**且已建立的挂载也会被封停**(见下)。存储是一处独立的本地 SQLite(`<db-path>/meta/api_keys.db`):控制面状态。下列回显取自真实运行:

```
# 1) 铸造一个 key(明文只打印一次;scope 默认 any)
$ grovefs apikey --db-path $HOME_DIR create --label claude-desktop
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    06fc0287c1464bd1b5a8ec1d4c1c9c31
  label: claude-desktop
  scope: any
  key:   06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0451c46889d8e...(64 hex secret)

Present it to a `--require-api-key` server as
  grovefs mount --meta ... --data ... --api-key '06fc0287c1464bd1b5a8ec1d4c1c9c31:7ddadff0...(64 hex secret)' -m /mnt/x
(or export GROVEFS_API_KEY='...' so it stays out of the command line).
Scope it to one entrance with `--for mount` / `--for mcp` to keep a mount
credential out of the governance API (and vice versa).
Revoke later with: grovefs apikey --db-path $HOME_DIR revoke --id 06fc0287c1464bd1b5a8ec1d4c1c9c31

# 2) 铸造一个只能挂载的 key;scope 写错直接拒绝(打错一个词不会静默放宽权限)
$ grovefs apikey --db-path $HOME_DIR create --label laptop-alice --for mount
  id:    aac1f51a0d5641ddaa8dbb83dc1cf646   scope: mount   key: aac1f51a...:a5d5450a...
$ grovefs apikey --db-path $HOME_DIR create --label x --for both
Error: "unknown api key scope 'both' (expected any|mcp|mount)"      # exit=1

# 2b) 视图绑定 + 只读的 key(访问控制维度)
$ grovefs apikey --db-path $HOME_DIR create --label reader --for mount --view alice --ro
apikey created — shown ONCE (only its SHA-256 is stored; save it now):
  id:    7c0e...
  label: reader
  scope: mount
  view:  alice
  rw:    ro (read-only)
  key:   7c0e...:…(64 hex secret)     # 只能挂 view alice,写一律被服务端拒
$ grovefs mount --view bob -m /mnt/b --meta ... --data ... --api-key "$READER_KEY"   # 挂别的视图
Error: ... "permission denied: this api key is bound to view 'alice', not 'bob'"    # M_OPEN 处 E_AUTH
$ mkdir /mnt/a/should_fail                                                          # alice 的只读 key
bash: /mnt/a/should_fail: 只读文件系统                                               # 据握手标志落内核只读;服务端另逐写拒 E_NOTPERM/EROFS

# 3) 列表带 scope/view/rw 列,绝不落密文;哈希也不落盘(库内只有摘要)
$ grovefs apikey --db-path $HOME_DIR list
#   id                                 label            scope   view          rw   created                   last_used                 revoked
1   296ef5839f5146078764ee5d16be6ef7   agent            mcp     -             rw   2026-10-27T06:57:59...      -                         -
2   aac1f51a0d5641ddaa8dbb83dc1cf646   laptop-alice     mount   -             rw   2026-10-27T06:57:59...      2026-10-27T06:58:02...    -
3   06fc0287c1464bd1b5a8ec1d4c1c9c31   claude-desktop   any     <any>         rw   2026-10-27T06:57:59...      -                         -

# 3b) 按视图过滤:只列能开 lab 的 key(纯别名也匹配绑定到真实名的 key),并提示系统级 key 数
$ grovefs apikey --db-path $HOME_DIR list --view lab
#   id                                 label            scope   view         rw   created                   last_used                 revoked
1   0cfaffd5fe2e4486af8ed1f282222dec   lab-agent        any     lab          rw   2026-10-27T15:31:33...      -                         -
1 bound key(s) for view 'lab' of 2 total

# 4) 服务端开门时自报门槛;不开门的默认路径行为与从前逐字节一致
$ grovefs meta-server --db-path $HOME_DIR --listen 0.0.0.0:50110 --require-api-key
meta-server listening on 0.0.0.0:50110 (base $HOME_DIR, role Primary)
meta-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live mounts on their next op
$ grovefs data-server --listen 0.0.0.0:50100 --root /data/cluster --require-api-key --keys-from $HOME_DIR
data-server: api-key auth REQUIRED (present `mount --api-key`, minted by `grovefs apikey create --for mount|any`); a revoke closes live connections on the next op

# 5) 客户端拿各种坏凭据去挂:拒因逐字节的原文回到本地 stderr(退出码 1,不留脏挂载)
$ grovefs mount --view alice -m /mnt/a --meta ... --data ...                      # 不带 key
Error: Custom { kind: PermissionDenied, error: "no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)" }
$ grovefs mount --view alice -m /mnt/a --meta ... --data ... --api-key "aac1f51a...:deadbeef"
Error: Custom { kind: PermissionDenied, error: "authentication failed: invalid api key secret" }
$ grovefs mount --view alice -m /mnt/a --meta ... --data ... --api-key "00000000...:whatever"
Error: Custom { kind: PermissionDenied, error: "authentication failed: unknown api key id" }
$ grovefs mount --view alice -m /mnt/a --meta ... --data ... --api-key "no-colon-here"
Error: Custom { kind: PermissionDenied, error: "malformed api key (expected '<id>:<secret>')" }
$ grovefs mount --view alice -m /mnt/a --meta ... --data ... --api-key "$MCP_KEY"   # 拿治理 key 来挂
Error: Custom { kind: PermissionDenied, error: "authentication failed: this api key is scoped to 'mcp', not 'mount'" }
#   反方向同样被拒:把 mount key 塞进 MCP initialize 的 apiKey,tools/list 回 -32001,
#   且这条跨入口尝试同样被审计成 scope 拒因。

# 6) 挂载连接与治理调用同表审计(method 分平面、tool 分握手)
$ grovefs apikey --db-path $HOME_DIR calls --limit 6
last 6 call(s), newest first:
ts                        key_id                             method       tool                   ok   detail
2026-10-27T06:58:02.176356991+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   meta         M_OPEN                 yes  view='alice' uid=1005
2026-10-27T06:58:00.180250185+00:00 aac1f51a0d5641ddaa8dbb83dc1cf646   data         OP_OPEN                yes
2026-10-27T06:58:00.169508131+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: this api key is scoped to 'mcp', not 'mount'
2026-10-27T06:58:00.164775080+00:00 -                                  meta         M_OPEN                 no   view='alice': authentication failed: invalid api key secret

# 7) 撤销一行输出说清两面契约;历史调用仍保留归属
$ grovefs apikey --db-path $HOME_DIR revoke --id aac1f51a0d5641ddaa8dbb83dc1cf646
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
$ grovefs mcp --db-path $HOME_DIR --require-api-key      # 客户端 initialize 不带 apiKey
{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"grovefs"},...}}   # initialize 应答(门只卡状态方法面)
{"jsonrpc":"2.0","id":2,"error":{"code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"}}

# 10) 客户端 initialize 携带 "apiKey":"<id>:<secret>"(且 scope 不是 mount)→ 该会话鉴权通过,工具面放行:
[tools/call define_derived_view {"name":"release","base":"team","rules":"hide /photos/private"}]
defined derived view 'release' over base 'team': 1 hide, 0 only, 0 reloc rule(s)
```

四个子命令:`create --label L [--for any|mcp|mount] [--view V] [--ro]`(铸造并一次性打印 `<id>:<secret>`;`--view`/`--ro` 只对 mount/any scope 有意义)、`list [--view V]`(id/label/scope/view/rw/created/last_used/revoked,无密文;带 `--view` 时只列能开该视图的 key,纯别名也匹配绑定到真实名的 key)、`revoke --id ID`(支持精确 id 或唯一前缀)、`calls --limit N`(默认 20,倒序回读审计)。无法自定义握手的 MCP 客户端,可由运维方导出 `GROVEFS_MCP_API_KEY='<id>:<secret>'` **预鉴权整个服务进程**(`grovefs mcp` 启动即以该 key 建立身份,客户端 initialize 无须再带);挂载侧对应的环境变量是 `GROVEFS_API_KEY`(不传 `--api-key` 时生效,让密钥不进命令行)。`data-server` 的 key 库路径由 `--keys-from <元数据根>` 指定,**默认取本节点首个 `--root`**——同机部署无需额外参数,数据盘与元数据根分开部署时必须指对(指到一个没铸造过 key 的目录时 fail-closed,一个也不放)。诚实边界:这把 key 认证的是**入口的调用方**——它让某个调用方能(或不能)发治理写命令、能(或不能)开某棵视图树,并把每次准入归咎到具体 key,但不改变治理语义本身;视图绑定/只读维度由服务端强制;活连接复证是 **op 驱动**的(有流量才查),故撤销的封停边界是“该挂载撤销后的首个服务端往返”——一次完全由内核缓存回答的读不需要往返,因此不会被打断(下一次往返即封);`--token` 与 `--require-api-key` 可同时存在,两者都得过。不开 `--require-api-key` 时,服务行为与不设门完全一致(只是照样落审计)。e2e 见 `apikey` 套件(26 条断言:铸造/列表/哈希不落盘/无 key 拒·错 secret 拒·对 key 放行/调用归属/撤销即失效且行保留/短 id 前缀撤销/env 预鉴权/`list --view` 过滤/默认不设门回归)与 `mountkey` 套件(64 条断言:scope 铸造与 typo 拒绝/六类拒因入协议/准入与 env 回退(argv 不泄密)/连接审计到具体 key 与视图/scope 双向隔离/撤销封停活挂载且无脏 endpoint/其他 key 不受波及/token 与 key 双门 AND/数据面独立门/故障转移携带凭据/不设门默认路径无回归/视图绑定 key 跨视图拒挂与审计归属/只读 key 服务端强制写拒)。

## 4. 生产拓扑:中心存储 + 多客户机 LAN 挂载

真实场景:数据放中心集群,多台客户机经 LAN 挂载,同一用户跨机器看到**同一份权威视图**。
这需要拆开两个服务:

```bash
# ===== 中心节点 =====
# 数据服务:把物理对象存到本节点的硬盘目录
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster

# 元数据服务:托管每用户权威视图(多客户机共享同一棵,不再各存各的副本)
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110
#   --db-path 就是元数据根:每个 view 首次被打开时,按需在其 meta/ 下建权威库

# ===== 每台客户机(远程挂载不需要 --home,本地什么都不落) =====
$Y mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 -m /mnt/alice
```

- `--data <host:port>`:文件 IO 走网络到 data-server,不碰客户机本地盘。
- `--meta <host:port>`:视图树由中心 meta-server 提供,**多台客户机挂同一用户 = 共享同一棵权威树**。
- 客户机本地**不留视图副本、也不落盘**：给了 `--data`/`--meta` 的远程挂载**不需要 `--home`**（传了也不写任何东西）——权威数据在 meta-server，字节在 data-server，访问控制全部发生在服务端 `M_OPEN`/逐 op,客户端不查本地库。

### 4.1 多机共享到底需要 PG 吗？——上手选型

> **直接回答：不需要。** 多机 LAN 共享一棵树的能力来自「同一个 `meta-server` + 同一个 `--view`」，与后端存的是 SQLite 还是 PG **无关**。默认 embedded SQLite(`rusqlite bundled` 编译进二进制、零外部依赖)已经足够支撑多客户端挂载、写串行化、实时失效推送。PG 只是 meta-server 的**可选后端**,纯服务端配置(`--store postgres://…` 在 meta-server 启动时给,客户端无此选项、不感知)。两者是**二选一**的替代关系、不叠加:同一棵权威树只落在其中一种后端上——用 SQLite 就不需要装 PG;换 PG 后视图树只进 PG schema,服务端不再创建/读写任何 `<db-path>/meta/<view>.db`(留在目录里的是换库前的旧副本);唯二例外是凭据控制面 `api_keys.db` 与派生视图 `.layout` 定义,它们永远是本地 SQLite/文件,与后端选型无关。仅当你想把权威树的持久化/HA/运维**交给成熟数据库**时才上 PG。

| 拓扑 | 后端 | 适用场景 | 命令 |
| --- | --- | --- | --- |
| 单机本地 | SQLite (embedded) | 开发、测试、单用户 | `mount --view alice --mountpoint /mnt/alice` |
| **中心节点 + 多客户端 LAN** | **SQLite (默认)** | **小/中型团队共享、零外部服务依赖** | `meta-server --listen 0.0.0.0:50110` + 客户端 `mount --meta ip:port --view alice` |
| 中心 + 多客户端 LAN | SQLite + HA (`--role`+`--peer`+`--witness`) | 单 meta-server 挂后自动 failover、双写防护 | 加 `--role primary --peer …` 等 |
| 中心 + 多客户端 LAN | **PostgreSQL** (`--store postgres://…`) | 想把 HA/备份/PITR/百万节点索引调优**交给 PG**,且已有 PG 运维能力 | `meta-server --store postgres://user:pw@host:5432/db`(不能同时加 `--peer`/`--witness`) |

> **常见误解纠正**：
> - 「PG 是客户端跟服务端都要装」→ 错,**只在服务端**(meta-server 进程连 PG),客户端只装 `grovefs` 二进制、只说 GroveFS 的 TCP 帧协议。
> - 「上了 PG 写并发会更高」→ **部分对**：默认 SQLite 仍单写者；但 `--store postgres://…` 会把写改为 SERIALIZABLE 写连接池（至多 8 连接，HopsFS 路线），不相交行上的并发写并行提交、冲突由 SSI 透明重试（`pg_ssi`/`pg_parallel` 套件实测）。并行仅限单个 PG 集群内，不跨集群分区元数据权威；跨集群扩展仍交 PG 自身复制/分片。
> - 「共享一棵树必须先有 PG」→ 错,默认 SQLite 就支持,`metasrv`/`freshness`/`locks` 三个套件全部纯 SQLite。

### 跨客户端一致性:两种模式

| 模式 | 命令 | 行为 |
|------|------|------|
| **实时失效推送(默认,推荐)** | 仅 `--meta` | 一客户端写成功后,meta-server 经事件总线向同用户其它订阅者广播失效(`inval_entry`+`inval_inode`),对端下次访问**即时可见**,无需轮询。 |
| **`--strict` 强一致兜底** | 加 `--strict` | 关闭内核属性/dentry 缓存并 `FOPEN_DIRECT_IO` 直读,写立即可见。代价:零客户端缓存,元数据/数据往返更多。适合对新鲜度要求极端严苛的场景。 |

> **默认即新鲜**:非 `--strict` 的推送失效模型已保证「A 写成功 → B 下次访问即时可见」,无需轮询、无需 `--strict`。这由 `.build-bin/test_freshness.sh` 端到端证明(顺序追加 100 行、截断、反向读三类场景均在 <500ms 内对端可见)。`--strict` **不是**保新鲜的唯一通道,只是在 writeback 脏页 flush 与远端失效交错的极端竞争窗口下,选择完全旁路内核缓存的更保守兜底。

> **分布式 advisory 锁**:`flock` / `fcntl F_SETLK/F_GETLK` 经 meta-server 的 `locks` 表(SQLite)集中仲裁,**跨进程、跨机生效**;主备切换后锁状态随快照复制存活。锁的回收有两条路径:①**持锁进程崩溃**——内核关闭其 fd 触发 FUSE `release`(携带匹配的 `lock_owner`),锁行被**即时删除**,远小于 lease;②**mount daemon 整机崩溃**——无 `release` 也无续约,锁在**一个短 lease(默认 15s)内**过期被扫。存活持有者由 mount daemon 的**续约心跳**(每 lease/3 bump `expires_at`)持续续期,**永不超时**。非合作应用(cp/dd)仍不阻塞,与 NFS/SMB advisory 语义一致。由 `.build-bin/test_locks.sh`(段 4 崩溃即时回收 / 段 4b 续约跨过 lease)证明。**伸缩性边界**:等锁(F_SETLK 带阻塞 / `flock -w`)在挂载进程里同步轮询(100ms 一次,上限 10 分钟),会占住一个 FUSE worker 线程;默认 `--threads 4` 下同一挂载点若同时有 ~4 个进程在等锁,该挂载的其他请求会排队——等锁多的场景请调大 `--threads`,事件驱动唤醒在演进计划里。

---

## 5. 如何处理数据(重点)

### 5.1 物理布局

- **单对象**:`blocks/<2hex>/<key>`,一个文件 = 一个对象文件。对象按 key 哈希自动扇出进 256 个桶子目录(桶名是 key 哈希的前两位 hex),单层即止、不建深层树——单目录条目数不随总量增长,千万级对象下每个桶仅数万条,`ls`/`rsync`/备份工具不退化。旧扁平存储(`blocks/<key>`)读取自动回退,后台扫描线程惰性收敛进桶,无需人工迁移。
- **key 命名**:视图写入时生成 UUID 作为对象 key,存进视图 `data_ref` 字段。
- **条带化(大文件跨多 data-server)**:`data_ref` 编码为 `s<N>:<stripe_bytes>:<uuid>`,
  第 i 块 key = `{uuid}#{i}`,落在 `servers[i % N]`。**旧对象无 `s` 前缀 = 单对象**,天然向后兼容。

### 5.2 多 data-server 条带(聚合容量/带宽)

```bash
# 挂载时给逗号分隔的多个 data-server 地址 → 自动启用确定性条带
$Y mount --view alice \
     --data 10.0.0.5:50100,10.0.0.6:50100 --meta 10.0.0.5:50110 -m /mnt/alice
```

- 文件 ≥ 16MiB 才分块;小文件自然只落 chunk0(单节点)。
- **价值**:把一个大文件摊到多节点的**聚合容量**上。注意:单条已饱和的链路(loopback/单网卡)上不会有吞吐增益,须用真正独立的链路才能测出收益。
- 单地址 `--data host:port` 与旧行为完全一致。

### 5.3 删除 / GC

- `unlink`(删除文件)后,视图解除引用,后端据 key 尽力删全部分块;条带 remove 会展开删所有 chunk,**幂等**(残留块可被 GC 清)。
- **跨视图共享对象的引用账本 `refs`**:一个 `data_ref` 命名唯一一个底层对象,却可能被多个物理视图同时挂着名字(`import` 同一源文件进两个视图解析成同一 key、派生视图/别名共享基视图字节)。视图内的引用计数看不见别的视图,单看它会误删他视图仍在引用的对象。故记账落在 `<base>/meta/refs.db`(PG 后端落共享表 `grovefs_refs.holders`):每个视图 ATTACH 该账本,holder 计数与节点增删进**同一个 IMMEDIATE 事务**,跨文件原子提交。物理删除的准确规则是「**该视图内此 key 的最后一个名字被摘除、且全局账本再无其它视图持有**」——只有此时才 `backend.remove`。视图 db 被 out-of-band 删除(`rm meta/x.db`、新 `--home`)只会让账本**高估**、对象多留不误删;`grovefs refs --db-path D rebuild` 按现存视图树重算 holder 归位(SQLite 后端专用)。历史库若没有账本行,退回「单视图」旧行为,既有部署不受影响。

### 5.4 备份与迁移

- **元数据无硬件绑定**:对象位置 = `blocks/<FNV1a(key)%256 两hex>/<key>`,由 `data_ref` 纯函数推出;`node` 行不存绝对路径/设备号/主机名,`ino` 也是 GroveFS 自有编号。换机器/换系统可直接用,只要元数据与字节一起走。
- **要迁的清单**:`blocks/`(字节)、`meta/<view>.db`(每视图树)、`meta/refs.db`(跨视图账本,可用 `grovefs refs rebuild` 重算)、`meta/views/*.layout`(派生/别名/fork 定义)、`meta/api_keys.db`(用过 key 才需要)。
- **整目录冷拷**:停掉所有挂载与 `meta-server` 后 `rsync -a -H` 整个存储根;**勿裸拷运行中的 SQLite**(WAL),不能停机就 `sqlite3 <db> ".backup"` / `VACUUM INTO`。
- **单视图导出/导入**:`grovefs view --db-path D export --view V --out F` → 目标机拷 `blocks/` 后 `grovefs view --db-path D2 import --view V --in F`;保留 id/inode/硬链接/权限/属主/符号链接,原子替换(失败不伤旧树),`--as-fork B` 可恢复 fork 语义。详见 §3.4.2。
- **拓扑边界**:多盘 `--root` 的数量与顺序要一致(或合并成单 root);跨 server 条带 `s<N>:` 要保留 N 个 data-server 及顺序;整目录拷会把 HA `epoch` 带过去,旧主必须先停;PG 后端用 `pg_dump`/逻辑复制。
- **多基视图**:任意多个基视图各自一棵 `meta/<name>.db`,可只迁其中一棵,key 也可按视图分别绑定。
- **HA** 模式下另可由 meta-server 用备份 API 复制(见 5.5)。

### 5.5 HA 主备(meta-server)

```bash
# 主:读写,并把快照周期推送给备
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110
# 备:只读服务,主心跳丢失后自动提升
$Y meta-server --db-path /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主(可选 RPO=0):加 --sync-repl,写返回前同步等备落盘 ACK
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110 --sync-repl
# 真 fencing(可选,opt-in):加 --witness <逗号分隔的 data-server 仲裁者列表,推荐奇数 N>,多数派授权
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --peer 10.0.0.6:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
$Y meta-server --db-path /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
```

- 两个节点必须用**各自独立的 `--db-path`**(各存一份视图副本,节点间靠快照复制)。
- 客户端 `--meta` 支持故障转移列表:`--meta 10.0.0.5:50110,10.0.0.6:50111`——主挂了自动切到备继续读写。
- **`--sync-repl`(可选,opt-in,默认关闭)**:启用后每个写 op 在返回客户端前,同步推快照并等备 fsync + 原子 rename 的 ACK,把 **RPO 从 ~500ms 降到 0**。代价:写延迟多一个控制通道 RTT + 一次 `VACUUM INTO` fsync。备不可达/超时时**诚实降级为 async 并打 warn**(单 op 的 RPO 回落到 REPL_INTERVAL,不谎报 0)。由 `.build-bin/test_ha.sh` 段 7(RPO=0)/段 8(分区降级)证明。
- **`--witness`(可选,opt-in,默认关闭)**:指定一个或多个主备都能访问的 `data-server`(`host:port`,逗号分隔列表,推荐奇数 N;N=1 退化为单仲裁、行为与旧版完全一致)作**独立仲裁者集合**。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予(meta 侧对全列表并发 fan-out、按多数派判决 `witness_quorum`;两个多数派必交于≥一个仲裁者,同值碰撞天然只有一主)。每个 data-server 维护一个单调 `fence` token(`OP_FENCE` 用 `fetch_max` CAS 折入并回传**折入前的值 `prev`**):claim 成功当且仅当 `prev < 我的 epoch`(我把 token 顶高了);`prev >= 我的 epoch` 说明有等于或更高的 epoch 已存在(对端胜出,或同值碰撞)→ 被 fence。用 `prev` 而非折后的 max,正是为了**区分同值碰撞**——仲裁者恢复时两半同时以 candidate 抢占,先到的赢、后到的看到 `prev == candidate` 被挡,绝不双主。在任 primary 每 tick 向多数派续一份**有界租约(lease,默认 3s)**(多数报 `prev > 我`则自降级);每个仲裁者**拒发重叠租约**——同一时刻至多一份活租约,故**活租约本身即独占权威的证明**,写请求由 primary 的**本地租约有效性**直接放行(不再每写一个仲裁 RTT,仅每续租一次,摊薄到整窗),被 fence 的过期主在租约到点后的下一次复证即拒;待提升 standby 也**非重叠**——须待前任租约到期才可抢占(`prev >= candidate` 同值碰撞/对端更高,或前任租约尚活则拒绝/延后)。元数据面复证拦不住内容字节(`write()` 先落 backend、再 debounced touch meta),故**数据面同遭门控**:客户端把实时 epoch 贴到每个写意图数据帧(create/write/truncate/remove/fsync),作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`,过期主无法绕过元数据面静默写入内容(被封帧向权威重取 epoch 并自动重试一次);且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化(claim 取独占锁会排在途写排空,而 witness 与存储同节点),故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合(注:数据面门控仅对 --witness 列表内的 data-server 生效,即 quorum 会顶高它们的 token;故应把 witness 与客户端实际写入的内容盘同节点部署,列表外的内容盘 token 恒为 0、数据面 gate 不生效,仅剩元数据面 fence 兜底)。**FAIL-SAFE**:一旦配置了 witness 却**组不到租约多数派且连续超过宽限(=PROMOTE_TIMEOUT 3s)**,节点无法证明独占权威,便**放弃写权限**——primary 自降级为 standby、standby 拒绝提升(**写冻结**,宁可牺牲可用性也不冒双写)。各 data-server 独立把 token 持久化到 `<root>/.fence_token`,**重启不会把 epoch 清零**、放过期 primary 复活。**相比单仲裁的可用性红利**:宕**少数**仲裁者仍能组多数续租、不冻结写;少数盘丢也不复活过期主(多数仍保单调 token);唯**失去多数**才 fail-safe 冻结。由此把双写窗口收敛到**租约安全余量内**(而非 peer-only 的不封顶):不重叠租约使对端只能在我方租约到点后取 token,而我方提前余量停止信任缓存租约;瞬时仲裁错误不误伤单写,持续不可达仍由宽限(=PROMOTE_TIMEOUT 3s)兼底。`test_ha.sh` 段 9(过期 primary 重启被 witness 挡成 standby + token 持久化)、段 10(杀单 witness→primary 自降级 & standby 拒绝盲升→恢复 witness→恰好一主)、段 11(3 仲裁者宕 1 仍续租不冻、宕 2 破多数才 fail-safe)证明。
- **诚实标注**:
  - 默认(async)**RPO ≈ 500ms**(主每 ~500ms 推一次快照),极端故障下最后 <500ms 内的写可能丢失;加 `--sync-repl` 可把这批写的 RPO 降到 0。
  - fencing 需要**仲裁者**:严格两节点、**不配 `--witness`** 时无法防脑裂(两半互断会各自认为自己是主 → 不封顶双写),这是 CAP/共识的硬边界,须部署第三方仲裁(witness 或第三节点)才能收敛。**配了 `--witness` 即 fail-safe**:仲裁者**多数派**不可达时冻结写而非盲写/盲升;primary 持一份不重叠的有界**租约**(仲裁者拒发重叠租约,多数活租约即独占证明),写由本地租约校验放行、每续租一次向全列表 fan-out,并对内容数据帧按 token 门控,把双写窗口收敛到**租约安全余量内**且保证收敛到单主。**多数派仲裁的可用性红利**:`--witness` 取逗号分隔的 N 个独立仲裁者(推荐奇数),权威需多数派 `N/2+1`——**宕少数仲裁者仍不冻结写**(仅失去多数才停写)、少数盘丢也不复活过期主,比单仲裁消除了 witness 这个单点可用性依赖(N=1 则退化为单仲裁、witness 本成为可用性依赖:它挂→主备都停写)。

---

## 6. 安全:准入(token / API key)与视图访问控制

中心 server 若裸监听 LAN,任何同网段主机都能按 key 读写对象 / 打开他人视图。加共享口令:

```bash
# 两个 server 用同一 token
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster --token s3cr3t
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 --token s3cr3t

# 客户端挂载带同一 token(同时覆盖数据面 OP_OPEN 与元数据面 M_OPEN)
$Y mount --view alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 --token s3cr3t -m /mnt/alice
```

- token 不匹配 → 连接时直接 permission denied,不重试。
- 空 token(默认)= LAN-trusted 无鉴权,向后兼容,仅限可信内网。

**但 `--token` 是一份共享密钥**:拿到它的主机都能挂,审计上也分不出「是谁挂的」,要踢掉一个调用方只能轮换 token 并断全部人。需要**逐调用方可撤销凭据**时,把同一份 `grovefs apikey` 注册表当挂载面的门用(铸造/scope/审计/撤销的完整用法见 §3.5.1):

```bash
# 服务端:把 key 门与 token 叠加(两道都得过),或只上 key 门
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 --require-api-key
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster --require-api-key --keys-from /data/cluster
# 客户端:呈递一个 `--for mount`(或 `--for any`)的 key
$Y mount --view alice --data 10.0.0.5:50100 --meta 10.0.0.5:50110 \
     --api-key '<id>:<secret>' -m /mnt/alice          # 或 export GROVEFS_API_KEY 不进 argv
```

与 token 的两处关键差异:① **可逐调用方撤销且封停活连接**——服务端每秒复证一次(有流量才查),被撤销的 key 在该挂载撤销后的首个服务端往返上被拒(E_AUTH/EACCES 携拒因原文),挂载打印原因后**自卸载**(不会留下 stale endpoint),而其他 key 的挂载照旧服务;② **可审计**——每次连(含被拒)落 `apikey calls`(method=meta/data,tool=M_OPEN/OP_OPEN,detail 记 view 与 uid)。两道门都是**入口门**;再叠加 key 的 `--view`/`--ro` 两个访问控制维度后,「谁能碰哪棵树、读写还是只读」已由凭据自身携带并在服务端逐 op 强制。

### 6.1 共享 view 与逐调用方差异化权限（视图绑定 / 只读 key）

**默认行为下 `--view` 不隔离 uid**：任何 uid 只要 `mount --meta <addr> --view alice` 都落到 alice 同一棵权威树上,看到彼此写入。这就是「多人共享一个 view」开箱可用的形态——元数据集中、内容集中、实时失效推送保证收敛。

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
│  ③ view 名 → 引擎缓存  首次 M_OPEN 懒开，之后同 view 命中同一实例
└──┬──────────────────────────────────
   ▼ view=alice                        ▼ view=bob
  alice 权威树（A、B 共享，写串行化）    bob 权威树（仅 C，与 alice 互不可见）
   ├─ SQLite（默认）：       一 view = 一个独立 db 文件  <db-path>/meta/alice.db · bob.db
   └─ PostgreSQL（--store）：一 view = 一个独立 schema   grovefs_v_616c696365 · grovefs_v_626f62
```

> 数据库里独立存储的单位是「视图」而非「每个客户端」：SQLite 一 view 一个 `.db` 文件、PG 一 view 一个 schema（均非单张表）；数据字节走 data-server → 存进其 `--root` 下的 `blocks/<key>`，树里只存 `data_ref` 引用。

客户端上报的 uid 默认 `geteuid()`，可 `mount --as-uid N` 显式覆盖——它只作为条目的属主记进锁仲裁与审计，属 **advisory**（TCP 客户端可伪造），**从不作为访问控制门**。确定性靠 `--token`/`--require-api-key`（准入），「谁能碰哪棵树、读写还是只读」靠 key 的 `--view`/`--ro` 绑定在服务端逐 op 强制。两个必须知道的行为边界：① **加锁类 op（`flock`/`F_SETLK`，含锁续约）归类为写意图**（它们要写中心 `locks` 表），只读 key（`--ro`）对它们的请求一律被拒（`E_NOTPERM`，客户端 FUSE 侧落 `EPERM`，区别于 HA 备机的 `E_ROFS` 切换信号）——只读角色若需共享锁做读协调，得用一把读写 key；② **撤销是 op 驱动**的（服务端每秒在流量上复证一次），被撤销/降级的 key 在该挂载撤销后的首个服务端往返即被拒，完全由内核缓存回答的读不需往返、故不被打断（下一次往返即封）。

### 6.2 PostgreSQL 元数据后端（可选）

`meta-server` 默认将权威 view 树存在 embedded SQLite（一 view 一 `<db-path>/meta/<view>.db`）；可将它切为 PostgreSQL：

```bash
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 \
     --store postgres://user:pw@10.0.0.9:5432/grovefs
```

客户端挂载命令完全一致(`--meta host:port --view alice`),线协议不变。服务端内部将每个 view 映射到一个 `grovefs_v_<hex(view)>` schema（hex 保证 identifier 安全、注入无关）,根节点固定 id=1,`ROOT_INO` 与 SQLite 行为一致。建表为幂等 `CREATE ... IF NOT EXISTS`,无需预迁移脚本。rename/unlink/lock 等需多语句原子性的 op 走显式事务。

**注意**：
- **与自研 HA 不兼容**：`--store postgres://…` + 任一 `--role standby` / `--peer` / `--witness` 会在启动时 fail-fast（`VACUUM INTO` 快照机制仅适用于 SQLite）。PG 高可用请交给 **PostgreSQL 自身的流复制 / logical replication / PITR**——本来就比文件快照更强。
- 为什么不提供 Redis：K/V+TTL 与视图树不同构（事务/行锁/多列索引都是自然需要,硬塞只会同实例重建一个更弱的 PG）。

---

## 7. 只读挂载

```bash
$Y --home $BASE mount --view alice --mountpoint /mnt/alice_ro --ro
```
所有写操作(create/write/mkdir/unlink/rename/chmod/truncate 等)返回 `EROFS`(内核 MS_RDONLY + 文件系统侧守卫双保险)。适合分发/归档视图。

---

## 8. 性能旋钮

- `--threads N`(默认 4):FUSE 并发派发工作线程,每线程独立 `/dev/fuse` fd(`clone_fd`,Linux 4.5+)。`0` = 单线程(遗留行为,可作 A/B 对照)。
- 客户端大 IO 自动分块并行:读 ≥1MiB 按 1MiB 切片并发,写 ≥4MiB 分片并发(零协议改动)。
- **元数据读并行**(SQLite 写不并行;PG 写并行见 §6.2):每个视图的 `MetaEngine` 除单条写连接外开 `READ_POOL_SIZE = 4` 条附加连接,`lookup`/`getattr`/`readdir` 与 nlink 解析走池,因此同视图并发读不再排在写锁后面;SQLite **写仍串行在同一权威连接上**(单写者语义不变),PG 写则另走 SERIALIZABLE 写池(§6.2)。池连接按 WAL 读事务读已提交数据,自己的写立刻可见(`cargo test` 内 `reads_through_the_pool_observe_committed_writes` 钉住这一点)。HA 备机/只读打开的引擎不预开池,首次读按需懒建并归还,同样有界。PG 引擎(`PgEngine`)现也持一条对等的 `PG_READ_POOL_SIZE = 4` 只读连接池(按 PG 事务读已提交数据,取不到只读连接时回退写连接以如实暴露故障);写侧 PG 另由 `PG_WRITE_POOL_SIZE = 8` 的 SERIALIZABLE 写池并行提交(§6.2)。
- **目录列举分页 + READDIRPLUS**(不带开关即生效):`MetaProvider::readdir_page(parent, offset, limit)` 在 SQLite 与 PG 引擎里都下推成 SQL `LIMIT/OFFSET`,FUSE 回调按 32→1024 几何块流式填回复缓冲,因此一个内核 READDIR/READDIRPLUS 请求付 `O(缓冲)` 而不是 `O(整棵目录)`(改造前每个分页都重扫整棵目录并为每条重算 nlink);属性随目录项一次带回(内核 ≥4.6 的 `FUSE_DO_READDIRPLUS`,与上面能力位分属不同的 all-or-nothing 协商组,内核不支持则自然回退普通 READDIR)。同一份 5001 条目录、同一挂载点、每趟新挂载(内核 dentry 空)实测 `ls -l`:**2.56s**;`--no-readdirplus` 只留分页:**5.22s**;改造前基线(HEAD,无分页无 PLUS):**55.34s** —— 三份输出排序后逐字节相同。远端同样已接分页——新 `M_READDIRP` 帧带 `offset/limit` 下推到服务端,同一 5001 条目录经 `meta-server`+`data-server` 实测由改造前 ~54s 降到 **2.29s**(只关 PLUS 保留分页 4.22s,两份输出逐字节相同)。注意作这个改动时先把 READDIRPLUS 接在了不能分页的 provider 上,反而把全量重取次数乘了 ~4 倍,因此现在两者都由 `supports_paged_readdir()` 门控(见下条)。
- **作用范围**(与上条同口径):只读连接池与 SQL 分页现在两个存储引擎上都——SQLite `MetaEngine` 与 PG `PgEngine`(各 4 连接只读池)。本地 `--home`(SQLite 或 PG)吃满分页与 PLUS;`meta-server` 侧并发 RPC 处理器吃各自后端读池,且 `M_READDIRP` 帧把分页下推到服务端,远端客户端每缓冲只取一页并协商 PLUS(同数据集远端实测 ~54s → 2.29s)。仍走 trait 默认实现(每缓冲一次全量列表、不协商 PLUS)的只有 `--layout`/派生视图的 `LayoutMeta`(隐藏/映射形状无法表示为单条 SQL 页)与对接不含该能力位的老 `meta-server`(`M_OPEN` ack 无第二字节则回退)。元数据**写**并行已覆盖 PG 后端(单集群内 SERIALIZABLE 写池,见 §6.2);跨节点多权威/分片的写扩展仍未做。判据由 `cargo test` 内 `store_level_paging_is_advertised_only_by_providers_that_page_in_sql` 钉住。
- `--no-readdirplus`:关闭 READDIRPLUS 协商,回退普通 READDIR+逐条 `LOOKUP`,供 A/B 基准与兼容性排障(不认该能力的老内核自然就是此行为;provider 不能自身分页的挂载——派生/透镜视图与老服务端——本来也不协商,此开关对它们为空操作)。
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

覆盖的阶段(`group` 分类):`op`(read/write 的 op_total——内核看到的端到端单 op 延迟,减去下面子阶段即 FUSE 回包/派发开销、key_resolve、backend_total、write.touch_mtime)、`data-io`(local/remote 的 read/write/len 往返、remote.pool_wait、remote.chunked、shard 的 read_total/write_total/server_op/split/concat)、`meta`(lookup/getattr/readdir/readdir_page/create/unlink/rename/setattr/statfs、db.sqlite_read/write_txn/finalize_count、db.pg_query/write_txn/pool_wait/ssi_retry、remeta.rpc)。非计时计数器:`sqlite read-pool lazy-opens`、`pg ssi-retries`、`remote reconnects`、`broadcast send timeouts`、`ha fencing demotions`、`ha quorum refusals`、`lock lease renewals`、`lock expired reclaims`、`gc objects freed`、`bytes read`、`bytes written`——用来给上面的耗时曲线**提供解释信号**(例如 pool_wait 尾部抬高→池太小;ssi_retry 计数上升→并发写冲突;remote reconnects 增长→后端在重启/漂移)。报告同时提供文本表与 CSV(`--stats-file` 内两段都写),便于离线绘图或 A/B 对拍。

> 注意口径:遥测数字是**单次挂载进程内**各阶段的实测分布,用于排障;它不等于、也不应用于论文里的吞吐/规模结论(本系统对吞吐/可扩展性刻意不作声明)。`errno 计数` 暂未纳入固定注册表(需扩 `Counter` 集与报告 schema),作为后续项。

### 8.2 读写/元数据加速(内核自适应 + 自动降级)

系统的加速改动都遵循**新内核走快路、老内核自动降级**的能力协商原则:FUSE 能力位由 `INIT` 阶段内核授予,不支持的位只是不生效,绝不影响其余路径;SQLite/PG 侧优化与内核版本无关。本机(5.4)实测 `--stats` 下会打印 `grovefs negotiated caps: PARALLEL_DIROPS=true AUTO_INVAL_DATA=true WRITEBACK=true MAX_PAGES=true`,即本机全部命中。

**元数据写尾延迟(P0)**:每个可写视图引擎起一条**后台 `wal_checkpoint(PASSIVE)` 线程**(每 1s 一次,每 ~30s 一次 `TRUNCATE` 收缩 `-wal` 文件),并把写连接的 `wal_autocheckpoint` 置 0——把原本内联在提交里的同步 checkpoint 停顿(实测曾贡献 `create` 单次 **125ms** 尾刺)移出写热路径。线程经 `Arc<StopSignal>` 随引擎 `Drop` 关停(不 join、不阻塞卸载),只读/备机引擎不启动。配合热读路径 `prepare_cached` 复用语句(避免每次重编译 SQL)。同一 3000 文件建目录负载 `--stats` 前后对比:

| 指标(µs) | 优化前 | 优化后 |
| --- | --- | --- |
| `meta.create` total | 1.64 s | **0.38 s**(↓4.3×) |
| `meta.create` p99 / max | 1151 / **125439** | **230 / 33535**(尾刺消除) |
| `meta.lookup` p50 | 39 | **17** |
| `meta.getattr` p50 | 66 | **36** |
| `db.sqlite_read` p50 | 44 | **21** |

**读缓存 TTL(P1)**:`--tune-cache <秒>` 把内核 attr/dentry 缓存 TTL 从默认 2s 放大,削减对同一目录/文件的重复 `LOOKUP`/`GETATTR` 往返——适合读多或不可变树(如天文“数据不落地”的一次性流读)。安全边界:跨端新鲜度**仍由默认 `notify` 实时失效推送保证**,且新内核(≥3.15)协商到 `AUTO_INVAL_DATA` 后,size/mtime 变化会顺带丢弃对端陈旧页数据;`--strict` 恒把 TTL 钉为 0(完全旁路)。`run_all.sh` 的 `freshness`/`disp_fresh` 套件默认 TTL 未变下持续证明 <500ms 对端可见。

**大块 IO(P2,内核自适应)**:`max_write`/`max_readahead` 申请 4MiB,由 fuser 与内核按 `FUSE_MAX_MAX_PAGES` 各自截断——老内核(如 5.4)自动降到页上限,6.x+ 给到更大块=更少往返;`PARALLEL_DIROPS`(并发目录 op)与 `WRITEBACK_CACHE`(批量合并写)早已协商。大块读写已逼近底层文件系统地板,实测 FUSE 每 op 仅加 ~50–70µs。

> 诚实边界:上表是本机单进程 `--stats` 实测分布(存在 run-to-run 抖动,但 4.3× 的 create total 与 125ms→33ms 的尾刺收敛远超噪声,且机制可归因);这些是排障/调优观测,**不构成吞吐或可扩展性声明**。远程/PG 后端瓶颈是网络 RTT,须另用 `bench_remote.sh` 量测。

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

```bash
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
bash .build-bin/run_all.sh                # 全板 28 套件:build+unit+posix+edge+e2e+remote+errors+resilience
                                          #       +concurrent+metasrv+ha+striping+locks+freshness+disp_fresh
                                          #       +layout+fork+migrate+import+govcheck+govtime+grovesci+mcp+apikey
                                          #       +mountkey+pg+pg_parallel+bench
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准(纯功能回归)
```

端到端演示脚本(视图隔离 + 实时失效推送)见会话中的 `.demo_real` 流程 / `test_metasrv.sh`。

---

---

## 11. 手把手部署教程(生产环境,从零到验收)

> 本章按顺序走一遍真实部署:预检 → 构建安装 → 规划 → token → systemd 常驻 → 客户端挂载 → 防火墙 → 验收;
> §11.8 是 HA 主备拓扑,§11.9 是 PostgreSQL 后端拓扑。示例 IP/路径请按实际替换。

### 11.0 部署前预检(每台节点都跑一遍)

内核 ≥ 4.5(多线程 `clone_fd` 依赖)、`/dev/fuse` 存在可读写、`fusermount`/`fusermount3`
在 PATH 里(`umount` 与信号优雅卸载都通过它;运行时只需 libfuse 的用户态工具,**不需要** libfuse 开发包)。
构建机额外需要 Rust 工具链(edition 2021)。

```bash
uname -r                                   # 期望 >= 4.5
ls -l /dev/fuse                            # 存在且当前用户可读写
which fusermount fusermount3 || true       # 至少有一个
```

数据盘要求:`data-server --root`(本地模式为 `--home`)所在文件系统要有真实容量——
文件的字节全部以对象形式落在其 `blocks/` 下,**不要**把数据根建在 tmpfs 上。

### 11.1 构建并安装二进制

```bash
cargo build --release                                    # 产物:target/release/grovefs
sudo install -m 755 target/release/grovefs /usr/local/bin/grovefs
grovefs --version
```

> 无系统 `pkg-config` 的环境按 §1 挂仓库自带 shim(`export PKG_CONFIG=.../.build-bin/pkg-config`)。
> 每台节点(中心 + 客户机)装同一个二进制即可,子命令决定角色。

### 11.2 规划:节点、目录、端口

本教程拓扑(最常见形态:单中心节点 + 多客户机):

| 节点 | 角色 | 目录/端口 |
| --- | --- | --- |
| 中心 `10.0.0.5` | `data-server` | 数据根 `/data/cluster/blocks`,监听 `0.0.0.0:50100` |
| 中心 `10.0.0.5` | `meta-server` | `--db-path /data/cluster`(权威视图在 `/data/cluster/meta/<view>.db`),监听 `0.0.0.0:50110` |
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
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --root /data/cluster --token ${GROVE_TOKEN}
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
ExecStart=/usr/local/bin/grovefs meta-server --db-path /data/cluster --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
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

> `meta-server` 的元数据根用子命令参数 `--db-path`(优先于全局 `--home`,后者仅为兼容保留);`data-server` 的数据根是 `--root`,两者互不相干,都不需要全局 `--home`。

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
ExecStart=/usr/local/bin/grovefs mount --view %i --foreground --data 10.0.0.5:50100 --meta 10.0.0.5:50110 --token ${GROVE_TOKEN} -m /mnt/%i
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

- systemd 默认以 root 起挂载进程,上报给 meta-server 的 uid 是 **0**;如需以固定角色挂载,可在 ExecStart 加 `--as-uid N` 指定该挂载的属主槽位(记进锁仲裁与审计)。
- 需要**其他用户也能访问**挂载点时加 `--o allow_other`,并确认 `/etc/fuse.conf` 打开了 `user_allow_other`。

想改成**逐调用方可撤销凭据**(踢掉一台机不再需要轮换共享 token 并断全部人):两个 server 的 ExecStart 各加 `--require-api-key`(data 侧另需 `--keys-from` 指到元数据根,若它不等于首个 `--root`),客户端把 `--token ${GROVE_TOKEN}` 换成 `--api-key ${GROVE_API_KEY}` 并把该 key 用 `grovefs apikey create --label <主机名> --for mount` 铸造后写进同一份 root-owned 0600 的 `grovefs.env`(密钥因而进不了 `ps` 可见的 unit 文本)。撤销后的真实行为(实测):该挂载在撤销后的首个服务端往返上打印服务端拒因(`grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view`)、**干净自卸载并以退出码 0 结束**——一次有意的收手而不是崩溃,所以 `Restart=on-failure` **不会**重拉,unit 落在 `inactive (dead)`(`systemctl status` 看起来像运维手动卸过,要辨“被踢”请看 journal 里那行拒因,或挂 `OnFailure=`/`ExecStopPost=` 告警);拿被撤销的 key 重挂则 fail-fast。若希望被踢后自动重试(例如运维会重新铸一把),改用 `Restart=always`;想保持一次就停则无需额外配置。其他主机的挂载全程不受波及——这正是逐调用方凭据相对共享 token 的全部价值。

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

# 3) 新视图开箱即用:直接挂一个从未 init 过的 view,中心会在
#    /data/cluster/meta/ 下按需建权威库(远程模式无需预先 init;--manifest
#    预建层级只在本地模式用,见 §3)
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

全部通过即部署完成。想跑整套回归(28 套件)见 §10 与 §12.7。

### 11.8 HA 主备拓扑(在 §11.4 基础上改)

要点:两台 meta-server 用**各自独立的 `--db-path`**(快照复制,不是共享盘!`blocks/` 才是共享的,客户机 `--data` 始终指向同一组 data-server):

```bash
# nodeA(10.0.0.5):primary
$Y meta-server --db-path /data/nodeA --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50111 --token $TOK
# nodeB(10.0.0.6):standby(同样写成 systemd unit,只改 --db-path/--listen/--role/--peer)
$Y meta-server --db-path /data/nodeB --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --token $TOK
# 客户端把故障转移列表写进 --meta:
$Y mount --view alice --meta 10.0.0.5:50110,10.0.0.6:50111 --data 10.0.0.5:50100 --token $TOK -m /mnt/alice
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
# 2) meta-server 启动参数加 --store(与 --role standby/--peer/--witness 互斥,同时给会启动即 fail-fast):
$Y meta-server --db-path /data/cluster --listen 0.0.0.0:50110 \
     --store postgres://grove:pw@10.0.0.9:5432/grovefs --token $TOK
```

客户端命令一字不变(见 §4.1:客户机不装 PG、不持连接串、不感知后端)。每个 view 首次被打开时服务端幂等建 `grovefs_v_<hex(view)>` schema,无需预迁移脚本;凭据控制面(`api_keys.db`)永远是本地 SQLite、不随 `--store` 进 PG(见 §3.5.1)。备份/HA 改走 PG 自己的工具链(`pg_dump`/`pg_basebackup`/流复制/PITR),见 §12.3。为什么上/不上 PG 的诚实边界见 §4.1。

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
du -sh /data/cluster/blocks; df -h /data/cluster

# 昨夜到现在有没有降级/冻结/提升事件
journalctl -u grove-meta-server --since yesterday | grep -E 'meta HA|sync-repl|fenced|promotion'
```

日志关键字 → 含义 → 处置(字串以源码 eprintln 为准):

| 日志关键字 | 含义 | 处置 |
| --- | --- | --- |
| `meta sync-repl: ... degraded to async for this op` | `--sync-repl` 备不可达/ACK 超时,该笔写 RPO 回落 ~500ms(诚实降级,不谎报 0) | 查备机存活/网络;持续出现则要么修备要么接受 async RPO |
| `meta HA: promoted to primary at epoch N` | standby 提升成功 | 确认旧主已停/将降级,事后排查主为何失联 |
| `meta HA: demoted to standby (peer epoch N)` / `(a quorum ... holds ... > our ...)` | 发现对端/仲裁持有更高 epoch,自降级 | 正常收敛行为;若双侧频发查网络抖动 |
| `meta HA: cannot form lease quorum ... fail-safe self-demote` | witness 多数派不可达超宽限(3s),**写冻结**(fail-safe,宁停写不双写) | 预期保护而非故障:恢复仲裁节点可达即自动解冻 |
| `meta HA: promotion refused/deferred` | 提升被否(对端持更高 token 或前任租约尚活) | 正常:绝不双主;若两节点都拒升,查 witness 多数派 |
| `invalidation listener stopped: token rejected` | 客户端失效推送通道被 token 拒(中心 token 换了) | 用新 token 重启对应 grove-mount@ 实例 |

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
| 数据对象 | data-server `--root` 下的 `blocks/`(本地模式为 `<home>/blocks/`) | 普通目录,`rsync -a`/文件系统快照直接备;若用过 `grovefs import`(§3.3),部分对象是与源目录共享的硬链接——用 `rsync -a -H` 或 `cp -a` 保留链接避免内容翻倍,且源目录本身也应进同一备份 |
| 权威视图 | `<db-path>/meta/<view>.db`(含 WAL) | 见下三种流程;**勿在有写活动时裸拷** |
| 跨视图账本 | `<db-path>/meta/refs.db` | 随 `meta/` 一起拷;丢失/替换后可用 `grovefs refs --db-path D rebuild` 按现存视图重算(只会高估不误删) |
| API key 注册表 | `<db-path>/meta/api_keys.db`(含 WAL) | 同上(控制面状态,永远是本地 SQLite,不随 PG 走) |
| 视图定义(派生/别名) | `<db-path>/meta/views/*.layout` | 普通文本文件,需一并拷(HA 备机也需手工同步,§3.2/§3.4) |
| token | `/etc/grovefs/grovefs.env` | 单独加密保存(丢失 = 所有客户端挂不上) |
| API key 注册表与审计 | `<db-path>/meta/api_keys.db`(含 `-wal`;用过 `apikey create` 才存在) | 随 `meta/` 一起**冷拷**(先停写)；开了 `--require-api-key` 时这是准入的唯一依据——丢了 = 所有 key 鉴不上、历史审计也断档。**切勿单独删除 `-wal`**(那会连带删掉尚未 checkpoint 的 key 行与审计行)；分盘部署时 `data-server --keys-from` 必须指到放着这份库的那个元数据根（它不复制、只读那一份），指错即 fail-closed |

**单机/本地模式**:先 `umount` 停写(或停 meta-server),再 `cp -a`/`rsync` 整个 `--home`。恢复:拷回原位 → 重挂 → `ls`/`cat` 抽查。二进制本身不在备份范围——重装 `grovefs` 即可,数据全在 `--home`。

**SQLite + 自研 HA**:standby 每 ~500ms 收到的是 `VACUUM INTO` 完整快照(在线备份语义),但它是**复制体不是归档备份**(仍建议对主/备任一侧做定期异地快照)。节点修复流程:

- 备机坏了→修好后直接 `systemctl start` 对应 standby meta-server:它从主继续收快照追平,不需要手工灌数据。
- 主机坏了→备自动提升(见 §12.1 日志);旧主修好后 `start`:发现更高 epoch **自动降级为 standby**,不会复活成双主(配了 `--witness` 时,即使旧主带更高 epoch 重启也会被仲裁挡在 standby,`test_ha.sh` 段 9 证明)。
- 两半都丢且要重建:从最近一份归档快照恢复 `<db-path>/meta/`,epoch 从头开始的前提是**所有 data-server 也同时换新**(否则残留 `.fence_token` 会挡住写——这是 fail-safe 的正确行为,不是损坏)。

**PostgreSQL 后端**:视图树备份完全交给 PG 工具链(`pg_dump`/`pg_basebackup` + WAL 归档/PITR);GroveFS 侧只备 `blocks/` + env 文件（另：`api_keys.db` 永远是本地 SQLite——即使走 `--store` PG 也一样——开了 `--require-api-key` 的部署须一并冷拷它，见 §3.5.1/上表）。`--store` 模式下自研快照备份 API 有意返回错误(设计如此)。

**迁到新机器/换系统**:元数据不绑定硬件(只存对象 key,路径是 key 的纯函数),整目录冷拷即可用;要按视图选择性迁移用 `view export/import`(详见 §3.4.2)。三个拓扑边界:多盘 `--root` 的数量/顺序要一致(或合并成单 root);条带 `s<N>:` 要保留 N 个 data-server 及顺序;整目录拷会把 HA `epoch` 带过去,旧主必须先停。PG 后端用 `pg_dump`/逻辑复制。

### 12.4 升级与滚动变更

客户端二进制升级(单台无中断):`systemctl stop grove-mount@alice` → 替换 `/usr/local/bin/grovefs` → `start`。(stop 会干净卸载;直接 kill 二进制不行——死挂载要 `fusermount -u` 手工清。)

中心 server 升级(无 HA 拓扑,接受秒级 IO 暂停):替换二进制 → `systemctl restart grove-data-server grove-meta-server`。客户机挂载进程**不需要**跟着重启——远程链路对 server 重启透明(resilience 套件证明:连接池整体丢弃后有限退避重连);只有客户端侧协议/功能需要新版时才重挂。

HA 拓扑滚动升级(写不中断,切换瞬间除外):

```bash
# 1) 先升级并拉起 standby(追平快照)
# 2) stop primary → standby 在 ≤3s(PROMOTE_TIMEOUT)内提升,客户端自动切地址
# 3) 升级旧主并以 standby 角色拉起 → 见高 epoch 自动降级,回到初始形态
```

> 协议兼容注意:线协议是自定义帧,同一次大版本升级请中心与客户机保持同版(混版未经验证,不声称兼容)。

### 12.5 故障处理速查表

| 症状 | 第一反应 | 解释 |
| --- | --- | --- |
| `ls` 报 `Transport endpoint is not connected` | `fusermount -u /mnt/x`(或 `grovefs umount -m /mnt/x`)后重启对应 mount 服务 | 挂载 daemon 死了/被 kill,留下死挂载点 |
| 挂载即报 `Permission denied` | 看错误里的**拒因原文**(现已随握手逐字节回传):`meta-server rejected the access token` / `no api key presented` / `unknown api key id` / `invalid api key secret` / `malformed api key` / `scoped to 'mcp', not 'mount'` / `this api key has been revoked` | `OP_OPEN`/`M_OPEN` 握手不过不重试,fail-fast;前者比对 token,后者查 `grovefs apikey list` 的 scope/revoked 列与 `apikey calls` 的归属行 |
| 挂载用着用着自己卸了(日志 `access refused by the server … — unmounting this view`) | `grovefs apikey calls` 看那把 key 是否被 revoke/过一分钟前刚被撤销 | 预期行为:服务端每秒复证活连接,撤销后首个往返即封停该挂载(§6),避免留一个还能读的服务端已不认的视图 |
| 写全部 `EPERM`、读正常 | `grovefs apikey list` 看该 key 是否 `--ro`(只读) | 只读 key 逐 op 服务端强制拒 `EPERM`、即时生效(不缓存);**这是权限不是故障**,故意区别于 `EROFS`。另一种来源:旧/改造客户端挂着派生(虚拟)视图名写(服务端派生只读门禁同样回 `EPERM`,§3.2) |
| 挂基名被拒 `"governed by an alias"`(本地与远端回显同一段文案,§3.4) | `grovefs view list` 查该真实名是否被纯别名治理 | 预期行为:别名是其唯一挂载入口;要恢复基名直挂,`grovefs view rm --name <别名>`(仅影响新挂载) |
| 远程挂载写全部 `EROFS` | `journalctl -u grove-meta-server` 看 `fenced/写冻结/promotion refused` 或角色;`findmnt` 看挂载行是否带 `ro` | 三种正常情形:① 该节点是 standby(只读等提升);② witness 多数派不可达触发 fail-safe 写冻结——修仲裁可达性,宁停写不双写;③ 挂的是**派生(虚拟)视图名**(只读契约,§3.2)——改挂基名写 |
| `flock` 看起来被占住、持有者机器已整机崩溃 | 等 ≤15s(`LOCK_LEASE_SECS`)lease 过期自动扫 | 无 `release` 信号时 lease 是唯一兜底;正常进程崩溃是即时回收 |
| data-server 重启后客户机 IO 卡一下又好了 | 无需动作 | 连接池整体丢弃后有限退避重连(resilience 证明) |
| 非 root 用户看不到挂载点/`allow_other` 不生效 | 查 `/etc/fuse.conf` 的 `user_allow_other`;查 fusermount SUID 位 | FUSE 惯例限制,与 GroveFS 无关 |
| `--store postgres://…` 加了 `--peer` 启动即退 | 去掉 HA 参数 | PG 与自研快照 HA 互斥是设计决定(§6.2),高可用用 PG 流复制 |
| 大文件没看到条带分块 | 确认文件 ≥16MiB 且 `--data` 给了逗号分隔多地址 | 小文件自然只落 chunk0,是设计不是 bug |
| 挂载点磁盘写满 | 先扩 `blocks/` 所在盘 | 容量治理没有 magic:对象只增删不压缩,`du -sh` 监控(§12.1) |

### 12.6 容量与回收的实话

- `unlink`(最后一个链接消失)时后端据 key 尽力删除对象/全部分块,幂等;但 **CLI 没有提供孤儿对象扫描/回收命令**(未实现),长期运行的 `blocks/` 容量以 `du` 实测为准,不要假设能整目录清空重采。
- SQLite 视图库的回收在引擎内部处理(锁表事务回收、hardlink refcount GC);PG 模式下交给 VACUUM。

### 12.7 变更后的回归验证

任何升级/配置变更后,在有构建环境的机器上跑一遍全板(28 套件),或只跑关心的面:

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
- **不要**给 `meta-server` 传 `--store postgres://…`(传了才切 PG)——不传 = 本案例要的 SQLite;
- 权威树的物理形态就是普通文件 `/data/meta/share.db`(+同名 `-wal`/`-shm` 伴生文件,WAL 模式正常现象);
- 有写活动时**不要**用 sqlite3 CLI 去打开/复制这个文件(会撞上 WAL  checkpoint);要查看内容先停写,备份流程见 §12.3。

### 13.1 第一步:三台机器都装同一个二进制

在**构建机**(或存储机本身)编译,然后拷到每台机器:

```bash
# 构建机(需 Rust 工具链;无系统 pkg-config 时先 export PKG_CONFIG=…/.build-bin/pkg-config)
cargo build --release
# 分发到三台机器并安装
for H in 192.168.1.10 192.168.1.11 192.168.1.12; do
  scp target/release/grovefs $H:/tmp/ && ssh $H 'sudo install -m 755 /tmp/grovefs /usr/local/bin/grovefs'
done
# 每台验证
grovefs --version
```

**意义**:GroveFS 是单二进制多角色——同一个 `grovefs` 程序,子命令决定它当存储端(`data-server`/`meta-server`)还是客户端(`mount`)。三台必须同版(混版未经验证,不声称兼容)。二进制静态链入了 SQLite,客户机**除了 `fusermount` 没有任何额外依赖**。

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
grovefs data-server --listen 0.0.0.0:50100 --root /data --token "$TOK"
# 窗口 2:元数据服务
grovefs meta-server --db-path /data --listen 0.0.0.0:50110 --token "$TOK"
```

逐参数解释——`data-server`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--listen` | `0.0.0.0:50100` | 监听地址。**缺省是 `127.0.0.1:50052`(只接本机)**——要接两台客户端必须显式写 `0.0.0.0`;端口号本身自定,客户端 `--data` 指对即可(50100/50110 是本文档约定值) |
| `--root` | `/data` | 数据基目录:服务会在其下自动建 `blocks/` 子目录与 256 个桶,文件字节以 UUID 为 key 存入 `blocks/<2hex>/<key>`。**可传逗号分隔多路径(`/data,/mnt/d2,...`)= 机内多盘自动放置**:整对象按 key 哈希选定一条盘(客户端与 `data_ref` 全不知情,statfs 自动聚合容量)。但根列表对同一存储须**固定**——放置是哈希 mod 根数,增删盘会重映射所有 key、使旧对象按新布局不可达;扩容宜一次性配齐多根,或在 OS 层用 LVM/mdadm 合盘后只挂单路径 `--root`。server 只认这个布局,基目录指错 = 数据"丢"(其实是没去找) |
| `--token` | grovefs.env 里那串 | 数据面握手(`OP_OPEN`)口令。客户端不带对同一串 → 连接即 `PermissionDenied`,不重试。不给 = LAN-trusted 无鉴权,仅限全可信内网 |
| (可加 `--require-api-key`) | — | 把数据面准入改成**逐调用方可撤销的 API key**(客户端 `mount --api-key`):只收 `grovefs apikey create --for mount`(或 `any`)铸造的活 key,每次连(含被拒)落审计,且每秒复证活连接——撤销会断掉已建立的连接并使其自卸载。与 `--token` 同时给出则两道都得过;不给 = 行为与从前一致(只是依旧落审计)。key 库路径由 `--keys-from <元数据根>` 指定,**默认取首个 `--root`**(分盘部署时必须指对,指到没铸造过 key 的目录 = 一个也不放) |

逐参数解释——`meta-server`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--db-path` | `/data` | **元数据根**(子命令参数):view 库落在 `<db-path>/meta/`(即 `/data/meta/share.db`)。优先于全局 `--home`——后者仅为向后兼容保留,同时给出时以 `--db-path` 为准 |
| `--listen` | `0.0.0.0:50110` | 同上,缺省 `127.0.0.1:50053` 只接本机;元数据面单独占一个端口,与数据面 50100 互不串扰 |
| `--token` | 同一串 | 元数据面握手(`M_OPEN`)口令,与数据面相互独立校验、建议用同一份便于管理 |
| (可加 `--require-api-key`) | — | 元数据面改按 `M_OPEN` 里的 API key 放行(`mount --api-key ID:SECRET`,或客户端 env `GROVEFS_API_KEY`):读的是 `<db-path>/meta/api_keys.db`——与 `grovefs mcp --require-api-key` / `grovefs apikey --db-path` **同一份注册表**,靠 scope 分入口;撤销能在首个往返内封停已建立的挂载(拒因随帧回传,挂载自卸)。详见 §6 / §3.5.1 |
| (不传 `--store`) | — | **这就是“数据库用 sqlite3”**——不传即 embedded SQLite,一 view 一 `.db`;传 `postgres://…` 才会切 PG(且与 HA 互斥,见 §11.9) |
| (不传 `--role/--peer/--witness`) | — | 单权威节点形态:本案例不做主备/仲裁,`192.168.1.10` 宕机 = 元数据面暂停(磁盘数据不丢)。要 HA 按 §11.8 扩 |

验证(第四台机器或本机另开窗口):

```bash
ss -lntp | grep -E '50100|50110'      # 两行都在,进程名 grovefs
```

两个前台窗口应分别打出一行 banner——`data-server listening on 0.0.0.0:50100` 和 `meta-server listening on 0.0.0.0:50110 (base /data, role Primary)`;长什么样的完整终端见 §13.11 实录①。

### 13.4 第四步:存储机 systemd 常驻 + 防火墙

确认手工能起后,Ctrl-C 停掉两个前台进程,交给 systemd(unit 全文见 §11.4,路径换成本案例的):

```ini
# /etc/systemd/system/grove-data-server.service
[Unit]
Description=GroveFS data-server
After=local-fs.target
[Service]
EnvironmentFile=/etc/grovefs/grovefs.env
ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --root /data --token ${GROVE_TOKEN}
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
ExecStart=/usr/local/bin/grovefs meta-server --db-path /data --listen 0.0.0.0:50110 --token ${GROVE_TOKEN}
Restart=on-failure
RestartSec=2
[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now grove-data-server grove-meta-server   # enable=开机自起,now=立刻起
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
     --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --token "$TOK" -m /mnt/da
# 看到 "grovefs: mounted view 'share' at /mnt/da (background pid …)" 即成功;另开窗口:
echo hello-$(date +%s) | sudo tee /mnt/da/probe.txt && sudo ls -l /mnt/da
# 验证完 sudo grovefs umount -m /mnt/da 卸载(后台进程随之退出),下一步换 systemd
```

逐参数解释——`mount`:

| 参数 | 本案例取值 | 意义 |
| --- | --- | --- |
| `--home`(客户机**不需要传**) | — | 远程挂载不在本地落盘任何数据,此参数与数据面/元数据面都无关——只有本地模式(`init`/无 `--meta` 的 `mount`)才用它定位权威库;访问元数据根的命令(`meta-server`/`apikey`)各自用 `--db-path` 命名,`--home` 仅作回退 |
| `--view` | `share` | 选树标签:挂哪棵权威视图。与 Linux 用户名无关(§2);A/B 用同一个名字才会共享同一棵树。`share` 首次挂载时中心自动建 `/data/meta/share.db`,**无需预先 init** |
| `--data` | `192.168.1.10:50100` | 数据面地址:文件字节走自定义 TCP 帧到这个 `data-server`,不碰本机盘(非 NFS)。逗号分隔多地址才启用跨机条带,单地址无条带 |
| `--meta` | `192.168.1.10:50110` | 元数据面地址:目录树由中心 `meta-server` 提供——这是“多机共享一棵树”的根;同时它也订阅失效推送(谁写了什么,这台机下次访问即时重取)。逗号分隔多个 = 故障转移列表(§11.8) |
| `--token` | grovefs.env 那串 | 同时递给数据面 `OP_OPEN` 与元数据面 `M_OPEN` 两处握手;不匹配 = 挂载命令直接报权限错,fail-fast |
| (可加 `--api-key`) | — | 服务端开了 `--require-api-key` 时递上的 per-caller 凭据 `<id>:<secret>`(`grovefs apikey create --for mount` 铸造);也可导出 `GROVEFS_API_KEY` 让密钥不进 argv。缺失/无效/已撤销/只给 mcp scope = 挂载 fail-fast 并打印具体拒因;被撤销的 key 封停**正在用**的挂载(§6) |
| `-m` / `--mountpoint` | `/mnt/da` | 挂载点。**注意它是选项,必须带 `-m`/`--mountpoint`,不是写在行尾的位置参数**(实测:裸写报错 `unexpected argument`)。目录不存在会被自动创建 |
| (不加 `--ro`) | — | 读写模式(默认) |
| (不加 `--strict`) | — | 默认靠 meta-server 的实时失效推送保新鲜(§4),不需要 `--strict` 的零缓存代价;只有要极端旁路内核缓存时才加 |
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
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --token ${GROVE_TOKEN} -m /mnt/da
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
     --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --token "$TOK" --ro -m /mnt/db
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
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --token ${GROVE_TOKEN} --ro -m /mnt/db
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

思路:给两台的挂载各配一把**视图绑定的 key**——A 用读写 key、B 用只读 key（`--ro`），中心服务端逐 op 强制，只读不再依赖客户端自觉带 `--ro`。存储机上铸造（`--db-path /data` 与 meta-server 指同一个元数据根，`--for mount` 使其只开门挂载面）：

```bash
sudo grovefs apikey --db-path /data create --label A-rw --for mount --view share          # A:读写 key
sudo grovefs apikey --db-path /data create --label B-ro --for mount --view share --ro     # B:只读 key
sudo grovefs apikey --db-path /data list                                                  # 确认两行（view=share，rw/ro）
```

然后 meta-server / data-server 都加 `--require-api-key`，两台客户端的 ExecStart 用 `--api-key` 呈递各自的 key 并重启服务（改后两行全文，直接替换原 ExecStart）：

```ini
# 服务端(存储机)两服务都开门:meta-server ... --require-api-key;data-server ... --require-api-key --keys-from /data

# 客户端 A 的 /etc/systemd/system/grove-mount.service 改成:
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --api-key ${GROVE_A_KEY} -m /mnt/da

# 客户端 B 的改成(--ro key 已由服务端强制,客户端 --ro 可留作双保险):
ExecStart=/usr/local/bin/grovefs mount --view share --foreground --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --api-key ${GROVE_B_KEY} --ro -m /mnt/db
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
| 存储机 | `grovefs data-server --listen 0.0.0.0:50100 --root /data --token $TOK` | 字节存储面:`--listen` 接全网卡、`--root` 基目录(自动建其下 `blocks/`)、`--token` 数据面口令 |
| 存储机 | `grovefs meta-server --db-path /data --listen 0.0.0.0:50110 --token $TOK` | 元数据面(默认 SQLite):`--db-path` 指元数据根(视图库在其 `meta/` 下)、不传 `--store`=SQLite、不传 `--role/--peer`=单节点 |
| 客户端A | `grovefs mount --view share --data 192.168.1.10:50100 --meta 192.168.1.10:50110 --token $TOK --as-uid 1001 -m /mnt/da` | 读写挂载:`--view` 选树、`-m` 必填不可当位置参数、不写 `--strict` 靠默认推送保新鲜 |
| 客户端B | 同 A 改 `--ro --as-uid 1002 -m /mnt/db` | 只读挂载:`--ro` 拦本机正常写路径;配合中心对该客户端的 `--ro` key = 服务端硬只读 |
| 存储机 | `grovefs apikey --db-path /data create --for mount --view share [--ro]` | 视图绑定 / 只读 key:key 只能开 `share` 视图(开别的 `M_OPEN` 处 `E_AUTH` 拒);`--ro` 令每个写意图 op 服务端拒(`E_NOTPERM`→`EPERM`)、`revoke` 即时生效 |

### 13.11 全程终端实录(真实实测输出,逐字未改)

拍摄条件:grovefs 0.1.0 @ Ubuntu 20.04,**单机三合一等价拓扑**——命令里的 `127.0.0.1:5010x` 对应三机案例的 `192.168.1.10:5010x`,`/tmp/grove_case/data` 对应 `/data`,`/tmp/grove_case/mnt_da|mnt_db` 对应 `/mnt/da|/mnt/db`。每段都是真实终端回显(含中文报错);token 是一次性随机值已作废。重跑脚本:仓库 `.build-bin/run_case_demo.sh`。

**① 存储机:取 token、前台起两服务、确认监听**

```console
$ grep -o '=.*' /etc/grovefs/grovefs.env | cut -d= -f2
690987478d9cb1a4fee062db6a3d04c4
$ ./grovefs data-server --listen 127.0.0.1:50100 --root /tmp/grove_case/data --token "$TOK"
data-server listening on 127.0.0.1:50100        # 三机上用 0.0.0.0:50100,这行会显示 0.0.0.0
$ ./grovefs meta-server --db-path /tmp/grove_case/data --listen 127.0.0.1:50110 --token "$TOK"
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
```

**② 客户端 A:读写挂载 + 写文件**

```console
$ ./grovefs mount --view share --data 127.0.0.1:50100 --meta 127.0.0.1:50110 --token "$TOK" -m /tmp/grove_case/mnt_da
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
$ ./grovefs mount --view share --data 127.0.0.1:50100 --meta 127.0.0.1:50110 --token "$TOK" --ro -m /tmp/grove_case/mnt_db
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

把只读从"客户端自觉带 `--ro`"升级为"服务端逐 op 强制":给 A、B 各铸一把绑定 `share` 视图的 key(`--db-path` 与 meta-server 同名,指同一个元数据根;B 的那把再加 `--ro`)。命令与 §13.7 同一套,只是把元数据根指向本案例的 `/tmp/grove_case/data`:

```bash
# A:读写 key(只能开 share)
./grovefs apikey --db-path /tmp/grove_case/data create --label A-rw --for mount --view share
# B:只读 key(只能开 share,且写一律被服务端拒)
./grovefs apikey --db-path /tmp/grove_case/data create --label B-ro --for mount --view share --ro
# 确认两行(view=share,一份 rw 一份 ro;secret 只在 create 时打印一次,list 里只有哈希)
./grovefs apikey --db-path /tmp/grove_case/data list
# 撤销即时生效:被撤销 key 的存活挂载在下一次服务端往返即被拒并自卸载
./grovefs apikey --db-path /tmp/grove_case/data revoke --id <A-rw 的 id>
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
| `data-server --root DIR` | 数据基目录:启动时自动建 `<root>/blocks/` 与 256 个桶,字节按 key 哈希存入 `blocks/<2hex>/<key>`;可逗号分隔多路径做多盘自动放置(整对象按 key 哈希选盘;根列表须固定,增删盘会重映射所有 key) | 任意路径/任意挂载点,不存在会自动建 |
| `meta-server --db-path DIR` | 权威视图树 `<db-path>/meta/<view>.db`(控制面另有 `api_keys.db`/`refs.db`) | 任意路径,与对象盘可不同盘 |

大数据量放独立大盘的例子(192.168.1.10 上 /data 只是小系统盘,对象盘另挂 /mnt/bigdisk):

```bash
# /etc/fstab 或手工挂好大盘后,把 unit 的 --root 改掉并声明挂载依赖:
# ExecStart=/usr/local/bin/grovefs data-server --listen 0.0.0.0:50100 --root /mnt/bigdisk --token ${GROVE_TOKEN}
# [Unit] 加两行,保证大盘挂载完成才起服务(否则服务会在空盘上开局):
#   Requires=mnt-bigdisk.mount
#   After=mnt-bigdisk.mount
sudo systemctl daemon-reload && sudo systemctl restart grove-data-server
```

**搬迁已有对象目录**(安全,因为 key 只是 UUID、不含路径信息):

```bash
# ① 客户端停写(建议 systemctl stop grove-mount)② 停两服务 ③ 整体 rsync ④ 改 --root ⑤ 拉起
rsync -a /data/blocks/ /mnt/bigdisk/blocks/
```

> **例外——条带文件的搬迁**:条带布局自描述在 `data_ref`(`s<N>:<stripe>:<uuid>`)里,但 chunk i 落在哪台是由客户端 `--data` 列表的**顺序**(`servers[i%N]`)决定的——多地址列表迁移时,列表顺序与台数一变,已写条带文件就找不回分块。单地址 `--root` 搬迁无此问题。

**多块盘/容量扩展**：两条正路① 一条 data-server 挂多盘：`--root /data,/mnt/d2,...` 逗号分隔多存储根,整对象按 key 哈希自动放置到其中一条盘,`df`(statfs)自动聚合全盘容量——但根列表对同一存储须**固定**(放置是哈希 mod 根数,增删盘会重映射所有 key、使旧对象按新布局不可达),故宜一次性配齐多根;② 起多台 data-server + 客户端 `--data a,b,c` 条带(≥16MiB 才分块,小文件只落一台;链路未饱和才有带宽收益,见 §5.2 诚实标注)。也可在 OS 层用 LVM/mdadm 把多盘合成一个文件系统再挂给单路径 `--root`。没有 per-directory 配额与存储分层(未实现)。

### 14.2 存量大量数据怎么进来:两条正路,首选零拷贝 `grovefs import`

**首选——原地登记不搬家**(同文件系统,§3.3):字节零复制、秒级完成(只建链接+目录项,不读内容)、时间戳/权限保真:

```bash
# 在存储机上(能同时触到 meta 根与 blocks 根的机器)一条命令登记整棵树
grovefs import --db-path /data/cluster --view legacy --source /data/legacy --prefix /legacy
# 跨设备的源?import 自动退化为流式拷贝并如实计数(仍可用,只是不再零字节)
```

**备选——经 FUSE 写路径灌入**(需要重组/清洗数据、PG 后端、或想让大文件享受 ≥16MiB 跨 server 条带时):

```bash
# 在任一台已挂载的客户端(或存储机自己也挂一份)分批灌入
rsync -a /srv/legacy/ /mnt/da/imported/           # 目录层级、权限随写自动成型
# 单文件 ≥4MiB 自动分片并发写;配了多地址 --data 且 ≥16MiB 自动条带,导入无需特殊调优
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
- per-directory 强制权限——目录级权限未实现(`--layout` 可做目录级显示裁剪,但如上述不构成强制隔离)。

---

## 附:命令速查

| 目的 | 命令 |
|------|------|
| 建视图 | `grovefs --home H init --view U [--manifest M]` |
| 本地挂载 | `grovefs --home H mount --view U -m MP [--ro] [--strict] [--threads N] [--as-uid N] [--layout F] [--foreground|--log-file F]` |
| 远程挂载 | `grovefs mount --view U -m MP --data A[,A2] --meta M[,M_bak] [--token T] [--api-key ID:SECRET] [--as-uid N] [--layout F] [--foreground|--log-file F]` |
| 定义派生视图 | `grovefs view --db-path D define --name V --base B --rules F`(存于 `D/meta/views/V.layout`,`mount --view V` 即得形状；虚拟视图挂载只读，写走基名 `B`) |
| 定义纯别名 | `grovefs view --db-path D alias --name A --real R`(公开名 1:1 映射真实视图:`A` 全树可写,`R` 不可直挂,§3.4) |
| 创建 fork 视图 | `grovefs view --db-path D fork --name L --base R`(复制目录项成独立 `meta/L.db`,字节共享;结构可写、内容 `EROFS`,§3.4.1) |
| 导出/导入单视图元数据 | `grovefs view --db-path D export --view V --out F [--force]`(宿主无关 JSONL,不含字节) / `grovefs view --db-path D import --view V --in F [--replace] [--as-fork B]`(原子替换,字节另拷 `blocks/`,§3.4.2) |
| 泄露后轮换公开名 | `grovefs view --db-path D alias --name A2 --real R --retire A`(先写后删零窗口:`A` 失效、`R` 全程不可直挂,§3.4) |
| 列出/删除视图定义 | `grovefs view --db-path D list`(定义槽 + 裸基视图) / `grovefs view --db-path D rm --name V`(派生/别名只删定义不动数据;fork 连自己的 `meta/V.db` 一并删除并重建账本) |
| 导入存量目录 | `grovefs import --view V --source DIR [--prefix P] [--data-root R] [--dry-run]`(同盘硬链接零拷贝,重跑增量;在存储机执行,§3.3) |
| 卸载 | `grovefs umount -m MP` |
| 数据服务 | `grovefs data-server --listen IP:PORT --root DIR [--token T] [--require-api-key [--keys-from D]]` |
| 元数据服务（默认 SQLite） | `grovefs meta-server --db-path D --listen IP:PORT [--token T] [--require-api-key] [--role primary\|standby --peer ADDR [--sync-repl] [--witness ADDR]]` |
| 元数据服务（PostgreSQL 后端） | `grovefs meta-server --db-path D --listen IP:PORT --store postgres://user:pw@host:5432/db` |
| 铸造接入 key | `grovefs apikey --db-path D create --label L [--for any\|mcp\|mount]`(明文只打印一次;`mount` = 只开挂载/数据面,`mcp` = 只开治理接口) |
| 列出/撤销/审计 key | `grovefs apikey --db-path D list [--view V]`(带 `--view` 时只列能开 V 的 key,纯别名也匹配绑定到真实名的 key) / `revoke --id ID`(精确 id 或唯一前缀;撤销会封停该 key 的**活挂载**) / `calls --limit N`(含被拒的挂载连接) |
