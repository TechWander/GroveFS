# GroveFS 使用说明

> 用户视图 FUSE 文件系统:物理数据集中存放在硬盘,不同用户挂载各自看到独立的文件列表,
> 列表支持自定义文件夹层级,彻底脱离 NFS。本文件说明**如何使用**与**如何处理数据**。

---

## 0. 核心概念(先理解这三点)

| 概念 | 说明 |
|------|------|
| **视图(view)** | 每个用户一棵独立的目录树(含自定义层级),存放在一个 SQLite 库里:`<home>/meta/<user>.db`。不同用户互不可见。 |
| **数据对象(object)** | 文件的真实字节。一个文件 = `blocks/` 下的一个对象文件,以 UUID 命名(或条带化的多个分块)。视图里的目录项指向对象 key。 |
| **基目录(base / `--home`)** | 同时容纳 `blocks/`(数据)和 `meta/`(视图)的物理根。默认 `$GROVEFS_HOME` 或 `~/.grovefs`。 |

数据与视图**天然分离**:视图只是"目录树 + 指针",字节永远在 `blocks/`。

### 关于 `--user` 与路径容量

- **`--user` 与 Linux 用户名无关**——只是视图命名空间的**任意标签**。二进制不调 `getpwnam`、不校验 uid、也不要求 `useradd`。`alice`、`team-a`、`u42`、`研究组` 都能当作用户名——只要它是目标文件系统的合法文件名。它的作用只有两个:① 确定 `<home>/meta/<user>.db` 库文件名;② 在 FUSE mount 上打 `grovefs:<user>` 标签供 `mount`/`findmnt` 展示。**鉴权来自「能访问 `--home` / `blocks/`」+ 可选 `--token`**,不来自 Linux 用户。
- **一个用户一棵树、树内路径完全自主**——挂载后,在该 mountpoint 下可以 `mkdir`/`touch` **任意多个**文件与目录、**任意深度**、任意层级,与在自己 home 里操作无差异。不同用户隔离的粒度是「整棵视图树」,不是单个路径。manifest 只是预建骨架的便捷入口,不是路径数量的上限;挂载后随时可 `mkdir -p a/b/c/d/...`。

---

## 1. 环境准备

```bash
# 依赖:Linux + fuse3(fusermount)、pkg-config shim(已内置,无需 libfuse 开发包)
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config   # 构建时需要
export LC_ALL=C LANG=C
cargo build --release                                       # 产物:target/release/grove
```

可执行文件子命令:`init` / `mount` / `umount` / `data-server` / `meta-server`。

---

## 2. 快速上手:单机本地模式(最简单)

数据和视图都在本机同一个 `--home`,无需任何 server。适合先体验功能。

```bash
Y=./target/release/grove
BASE=/data/myfs                       # 物理基目录

# 1) 初始化两个用户的视图
$Y --home $BASE init --user alice
$Y --home $BASE init --user bob

# 2) 挂载(前台运行,Ctrl-C 卸载;或加 & 后台)
$Y --home $BASE mount --user alice --mountpoint /mnt/alice
$Y --home $BASE mount --user bob   --mountpoint /mnt/bob     # 另一终端

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
$Y --home $BASE init --user alice --manifest ./seed.txt
# 输出:applied seed manifest ./seed.txt
```

挂载后即可看到预建的层级。已存在的 mkdir/touch 是幂等的(重复执行不报错)。

---

## 4. 生产拓扑:中心存储 + 多客户机 LAN 挂载

真实场景:数据放中心集群,多台客户机经 LAN 挂载,同一用户跨机器看到**同一份权威视图**。
这需要拆开两个服务:

```bash
# ===== 中心节点 =====
# 数据服务:把物理对象存到本节点的硬盘目录
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster/blocks

# 元数据服务:托管每用户权威视图(多客户机共享同一棵,不再各存各的副本)
$Y --home /data/cluster meta-server --listen 0.0.0.0:50110
#   注意 meta-server 的 --home/meta/ 下按需为每个新用户建权威库

# ===== 每台客户机(各自有本地 --home,但不存权威数据/视图) =====
$Y --home /local/home mount --user alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 /mnt/alice
```

- `--data <host:port>`:文件 IO 走网络到 data-server,不碰客户机本地盘。
- `--meta <host:port>`:视图树由中心 meta-server 提供,**多台客户机挂同一用户 = 共享同一棵权威树**。
- 客户机本地**不留视图副本**(`--home` 只放临时缓存)。

### 跨客户端一致性:两种模式

| 模式 | 命令 | 行为 |
|------|------|------|
| **实时失效推送(默认,推荐)** | 仅 `--meta` | 一客户端写成功后,meta-server 经事件总线向同用户其它订阅者广播失效(`inval_entry`+`inval_inode`),对端下次访问**即时可见**,无需轮询。 |
| **`--strict` 强一致兜底** | 加 `--strict` | 关闭内核属性/dentry 缓存并 `FOPEN_DIRECT_IO` 直读,写立即可见。代价:零客户端缓存,元数据/数据往返更多。适合对新鲜度要求极端严苛的场景。 |

> **默认即新鲜**:非 `--strict` 的推送失效模型已保证「A 写成功 → B 下次访问即时可见」,无需轮询、无需 `--strict`。这由 `.build-bin/test_freshness.sh` 端到端证明(顺序追加 100 行、截断、反向读三类场景均在 <500ms 内对端可见)。`--strict` **不是**保新鲜的唯一通道,只是在 writeback 脏页 flush 与远端失效交错的极端竞争窗口下,选择完全旁路内核缓存的更保守兜底。

> **分布式 advisory 锁**:`flock` / `fcntl F_SETLK/F_GETLK` 经 meta-server 的 `locks` 表(SQLite)集中仲裁,**跨进程、跨机生效**;主备切换后锁状态随快照复制存活。锁的回收有两条路径:①**持锁进程崩溃**——内核关闭其 fd 触发 FUSE `release`(携带匹配的 `lock_owner`),锁行被**即时删除**,远小于 lease;②**mount daemon 整机崩溃**——无 `release` 也无续约,锁在**一个短 lease(默认 15s)内**过期被扫。存活持有者由 mount daemon 的**续约心跳**(每 lease/3 bump `expires_at`)持续续期,**永不超时**。非合作应用(cp/dd)仍不阻塞,与 NFS/SMB advisory 语义一致。由 `.build-bin/test_locks.sh`(段 4 崩溃即时回收 / 段 4b 续约跨过 lease)证明。

---

## 5. 如何处理数据(重点)

### 5.1 物理布局

- **单对象**:`blocks/<key>`,一个文件 = 一个对象文件,扁平存放(不用深层目录,避免 inode/目录项膨胀)。
- **key 命名**:视图写入时生成 UUID 作为对象 key,存进视图 `data_ref` 字段。
- **条带化(大文件跨多 data-server)**:`data_ref` 编码为 `s<N>:<stripe_bytes>:<uuid>`,
  第 i 块 key = `{uuid}#{i}`,落在 `servers[i % N]`。**旧对象无 `s` 前缀 = 单对象**,天然向后兼容。

### 5.2 多 data-server 条带(聚合容量/带宽)

```bash
# 挂载时给逗号分隔的多个 data-server 地址 → 自动启用确定性条带
$Y --home /local/home mount --user alice \
     --data 10.0.0.5:50100,10.0.0.6:50100 --meta 10.0.0.5:50110 /mnt/alice
```

- 文件 ≥ 16MiB 才分块;小文件自然只落 chunk0(单节点)。
- **价值**:把一个大文件摊到多节点的**聚合容量**上。注意:单条已饱和的链路(loopback/单网卡)上不会有吞吐增益,须用真正独立的链路才能测出收益。
- 单地址 `--data host:port` 与旧行为完全一致。

### 5.3 删除 / GC

- `unlink`(删除文件)后,视图解除引用,后端据 key 尽力删全部分块;条带 remove 会展开删所有 chunk,**幂等**(残留块可被 GC 清)。

### 5.4 备份与迁移

- **数据对象**:`blocks/` 就是普通文件目录,可直接 `rsync`/快照备份。
- **视图库**:走 SQLite(`<home>/meta/<user>.db`)。**勿在对有写活动时裸拷 DB 文件**;HA 模式下由 meta-server 用备份 API 复制(见 5.5),单机可先停写或 checkpoint 后再拷。

### 5.5 HA 主备(meta-server)

```bash
# 主:读写,并把快照周期推送给备
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110
# 备:只读服务,主心跳丢失后自动提升
$Y --home /data/nodeB meta-server --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110
# 主(可选 RPO=0):加 --sync-repl,写返回前同步等备落盘 ACK
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --role primary --peer 10.0.0.6:50110 --sync-repl
# 真 fencing(可选,opt-in):加 --witness <逗号分隔的 data-server 仲裁者列表,推荐奇数 N>,多数派授权
$Y --home /data/nodeA meta-server --listen 10.0.0.5:50110 --peer 10.0.0.6:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
$Y --home /data/nodeB meta-server --listen 10.0.0.6:50111 --role standby --peer 10.0.0.5:50110 --witness 10.0.0.7:50120,10.0.0.8:50120,10.0.0.9:50120
```

- 两个节点必须用**各自独立的 `--home`**(各存一份视图副本,节点间靠快照复制)。
- 客户端 `--meta` 支持故障转移列表:`--meta 10.0.0.5:50110,10.0.0.6:50111`——主挂了自动切到备继续读写。
- **`--sync-repl`(可选,opt-in,默认关闭)**:启用后每个写 op 在返回客户端前,同步推快照并等备 fsync + 原子 rename 的 ACK,把 **RPO 从 ~500ms 降到 0**。代价:写延迟多一个控制通道 RTT + 一次 `VACUUM INTO` fsync。备不可达/超时时**诚实降级为 async 并打 warn**(单 op 的 RPO 回落到 REPL_INTERVAL,不谎报 0)。由 `.build-bin/test_ha.sh` 段 7(RPO=0)/段 8(分区降级)证明。
- **`--witness`(可选,opt-in,默认关闭)**:指定一个或多个主备都能访问的 `data-server`(`host:port`,逗号分隔列表,推荐奇数 N;N=1 退化为单仲裁、行为与旧版完全一致)作**独立仲裁者集合**。权威需**严格多数派** `quorum_size(N)=N/2+1` 个仲裁者同时授予(meta 侧对全列表并发 fan-out、按多数派判决 `witness_quorum`;两个多数派必交于≥一个仲裁者,同值碰撞天然只有一主)。每个 data-server 维护一个单调 `fence` token(`OP_FENCE` 用 `fetch_max` CAS 折入并回传**折入前的值 `prev`**):claim 成功当且仅当 `prev < 我的 epoch`(我把 token 顶高了);`prev >= 我的 epoch` 说明有等于或更高的 epoch 已存在(对端胜出,或同值碰撞)→ 被 fence。用 `prev` 而非折后的 max,正是为了**区分同值碰撞**——仲裁者恢复时两半同时以 candidate 抢占,先到的赢、后到的看到 `prev == candidate` 被挡,绝不双主。在任 primary 每 tick 向多数派续一份**有界租约(lease,默认 3s)**(多数报 `prev > 我`则自降级);每个仲裁者**拒发重叠租约**——同一时刻至多一份活租约,故**活租约本身即独占权威的证明**,写请求由 primary 的**本地租约有效性**直接放行(不再每写一个仲裁 RTT,仅每续租一次,摊薄到整窗),被 fence 的过期主在租约到点后的下一次复证即拒;待提升 standby 也**非重叠**——须待前任租约到期才可抢占(`prev >= candidate` 同值碰撞/对端更高,或前任租约尚活则拒绝/延后)。元数据面复证拦不住内容字节(`write()` 先落 backend、再 debounced touch meta),故**数据面同遭门控**:客户端把实时 epoch 贴到每个写意图数据帧(create/write/truncate/remove/fsync),作为 witness 的 data-server 对 `epoch < token` 的帧直接回 `EROFS`,过期主无法绕过元数据面静默写入内容(被封帧向权威重取 epoch 并自动重试一次);且 data-server 把 epoch 校验与落盘对仲裁 claim 线性化(claim 取独占锁会排在途写排空,而 witness 与存储同节点),故过期写字节不可能与新主写交叠——内容撕裂写在仲裁节点上被真正闭合(注:数据面门控仅对 --witness 列表内的 data-server 生效,即 quorum 会顶高它们的 token;故应把 witness 与客户端实际写入的内容盘同节点部署,列表外的内容盘 token 恒为 0、数据面 gate 不生效,仅剩元数据面 fence 兜底)。**FAIL-SAFE**:一旦配置了 witness 却**组不到租约多数派且连续超过宽限(=PROMOTE_TIMEOUT 3s)**,节点无法证明独占权威,便**放弃写权限**——primary 自降级为 standby、standby 拒绝提升(**写冻结**,宁可牺牲可用性也不冒双写)。各 data-server 独立把 token 持久化到 `<root>/.fence_token`,**重启不会把 epoch 清零**、放过期 primary 复活。**相比单仲裁的可用性红利**:宕**少数**仲裁者仍能组多数续租、不冻结写;少数盘丢也不复活过期主(多数仍保单调 token);唯**失去多数**才 fail-safe 冻结。由此把双写窗口收敛到**租约安全余量内**(而非 peer-only 的不封顶):不重叠租约使对端只能在我方租约到点后取 token,而我方提前余量停止信任缓存租约;瞬时仲裁错误不误伤单写,持续不可达仍由宽限(=PROMOTE_TIMEOUT 3s)兼底。`test_ha.sh` 段 9(过期 primary 重启被 witness 挡成 standby + token 持久化)、段 10(杀单 witness→primary 自降级 & standby 拒绝盲升→恢复 witness→恰好一主)、段 11(3 仲裁者宕 1 仍续租不冻、宕 2 破多数才 fail-safe)证明。
- **诚实标注**:
  - 默认(async)**RPO ≈ 500ms**(主每 ~500ms 推一次快照),极端故障下最后 <500ms 内的写可能丢失;加 `--sync-repl` 可把这批写的 RPO 降到 0。
  - fencing 需要**仲裁者**:严格两节点、**不配 `--witness`** 时无法防脑裂(两半互断会各自认为自己是主 → 不封顶双写),这是 CAP/共识的硬边界,须部署第三方仲裁(witness 或第三节点)才能收敛。**配了 `--witness` 即 fail-safe**:仲裁者**多数派**不可达时冻结写而非盲写/盲升;primary 持一份不重叠的有界**租约**(仲裁者拒发重叠租约,多数活租约即独占证明),写由本地租约校验放行、每续租一次向全列表 fan-out,并对内容数据帧按 token 门控,把双写窗口收敛到**租约安全余量内**且保证收敛到单主。**多数派仲裁的可用性红利**:`--witness` 取逗号分隔的 N 个独立仲裁者(推荐奇数),权威需多数派 `N/2+1`——**宕少数仲裁者仍不冻结写**(仅失去多数才停写)、少数盘丢也不复活过期主,比单仲裁消除了 witness 这个单点可用性依赖(N=1 则退化为单仲裁、witness 本成为可用性依赖:它挂→主备都停写)。

---

## 6. 安全:Token 鉴权

中心 server 若裸监听 LAN,任何同网段主机都能按 key 读写对象 / 打开他人视图。加共享口令:

```bash
# 两个 server 用同一 token
$Y data-server --listen 0.0.0.0:50100 --root /data/cluster/blocks --token s3cr3t
$Y --home /data/cluster meta-server --listen 0.0.0.0:50110 --token s3cr3t

# 客户端挂载带同一 token(同时覆盖数据面 OP_OPEN 与元数据面 M_OPEN)
$Y --home /local/home mount --user alice \
     --data 10.0.0.5:50100 --meta 10.0.0.5:50110 --token s3cr3t /mnt/alice
```

- token 不匹配 → 连接时直接 permission denied,不重试。
- 空 token(默认)= LAN-trusted 无鉴权,向后兼容,仅限可信内网。

---

## 7. 只读挂载

```bash
$Y --home $BASE mount --user alice --mountpoint /mnt/alice_ro --ro
```
所有写操作(create/write/mkdir/unlink/rename/chmod/truncate 等)返回 `EROFS`(内核 MS_RDONLY + 文件系统侧守卫双保险)。适合分发/归档视图。

---

## 8. 性能旋钮

- `--threads N`(默认 4):FUSE 并发派发工作线程,每线程独立 `/dev/fuse` fd(`clone_fd`,Linux 4.5+)。`0` = 单线程(遗留行为,可作 A/B 对照)。
- 客户端大 IO 自动分块并行:读 ≥1MiB 按 1MiB 切片并发,写 ≥4MiB 分片并发(零协议改动)。
- `--o allow_other` 等额外 fuser 选项透传给挂载。

---

## 9. 常用运维

```bash
$Y umount --mountpoint /mnt/alice        # 卸载
fusermount -u /mnt/alice                 # 等价(挂载进程前台运行则 Ctrl-C 即卸载)
ps aux | grep grove                     # 查看在跑的 server / mount
```

> 挂载进程是前台阻塞式(`Session::notifier() + run()`);要常驻用 `nohup ... &` 或 systemd。
> **不要**用 `spawn + BackgroundSession::join()` 当阻塞等待——`join()` 会立即卸载。

---

## 10. 验证与回归

```bash
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config LC_ALL=C LANG=C
bash .build-bin/run_all.sh                # 全板:build+unit+posix+edge+e2e+remote+errors
                                          #       +resilience+concurrent+metasrv+ha+striping+locks+freshness+bench
SKIP_BENCH=1 bash .build-bin/run_all.sh   # 跳过吞吐基准(纯功能回归)
```

端到端演示脚本(视图隔离 + 实时失效推送)见会话中的 `.demo_real` 流程 / `test_metasrv.sh`。

---

## 附:命令速查

| 目的 | 命令 |
|------|------|
| 建视图 | `grove --home H init --user U [--manifest M]` |
| 本地挂载 | `grove --home H mount --user U -m MP [--ro] [--strict] [--threads N]` |
| 远程挂载 | `grove --home H mount --user U -m MP --data A[,A2] --meta M[,M_bak] [--token T]` |
| 卸载 | `grove umount -m MP` |
| 数据服务 | `grove data-server --listen IP:PORT --root DIR [--token T]` |
| 元数据服务 | `grove --home H meta-server --listen IP:PORT [--token T] [--role primary\|standby --peer ADDR]` |
