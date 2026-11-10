# FDB 键空间与编码设计（P1 交付物）

> 本文件是 GroveFS FoundationDB 元数据后端的**编码契约**。P3 的 `src/fdb_store.rs` 以本文件为唯一蓝本；
> 评审后修改须记版本号。所有"现有形态"条目均逐条取证自 `src/pg_store.rs` 与 `src/meta/engine.rs`，
> 标注处即为源码行号。本文件不是用户手册：用户可见行为只写在 `USAGE.md` 与 `docs.html` 里。
>
> **版本：v1.3**。v1.0 = 随 `v1.0` 标签发版的 P1 原始设计；v1.1 = P2/P3 实现期间的修订；
> v1.2 = P4 锁面；v1.3 = P5 治理面（新增 §3.10 的 I1–I4 对照表）。
> 逐条修订（含取证依据与影响面）见 §14。编号 **C1–C7** 即 §14 的条目号。

## 0. 编码意图与载体（RecordLayer 取舍）

**意图 = RecordLayer 式**：分层键空间（每视图一个子空间）、记录整体序列化进值、二级索引以独立键区间表达、
用 key-range 扫描取代 SQL 索引扫描。

**载体 = Rust `foundationdb` crate 的 tuple layer + subspace layer**。理由：FoundationDB 官方的
RecordLayer 是 Python/Java 侧的遗留绑定层，无 Rust 实现；把它引进来意味着跨语言服务链，而 GroveFS 的
元数据面是**进程内同步 trait 面**（`MetaProvider`，`src/meta.rs:205`，被 fuser 工作线程直接驱动，
见 `src/remote/proto.rs` 的"一连接一问一答"设计契约）。跨语言跳板与该 trait 面不兼容，也会把
`--meta-store` 的进程内分派变成 RPC 依赖。因此本后端**不声明"使用 RecordLayer"**，只声明"用 tuple/subspace
复刻 RecordLayer 式编码语义"。文档与对外表述一律采用后者，禁止写成"用了 RecordLayer"。

## 1. subspace 布局（决策 D1，不可逆）

```
根前缀（cluster 内唯一）:  grovefs
每视图子空间:              \x01 + tuple(view_name)         ← subspace(view)
视图内键族:                view_subspace + tuple(kind_tag, ...kind_key)
跨视图共享账本:            \x01 + tuple("refs") + tuple(data_ref)   ← 不在任何视图子空间内
```

**实测字节形态（v1.1 补，取代上面示意式的写法；由 `src/fdb_store.rs` 的
`tests::key_encoding_is_byte_frozen` 逐字节钉住）**：tuple 层的字符串以 `0x02`（UTF-8 STRING）
或 `0x01`（BYTES）打头，变长元素后面**总是跟终止符 `0x00`**（串内的 NUL 被转义为 `0xff 0x00`，
因此 `0xff` 只作转义引导、从不充当终止符）。于是：

```
根前缀        \x01 g r o v e f s \x00
视图 "v1"     + \x02 v 1 \x00
键族 'n'      + \x02 n \x00
名字键        + \x15 \x07 \x02 a \x00      (7, "a")：\x15=正整数 7，\x07=长度 1 的串
完整形态      \x01grovefs\x00 \x02v1\x00 \x02n\x00 \x15\x07\x02a\x00
账本键        \x01grovefs\x00 \x02refs\x00 \x02abc\x00
```

- 视图名 → 子空间键的映射函数必须做**长度与字符校验**，对位 PG 的 `schema_for`
  （`src/pg_store.rs:71-80`：`grovefs_v_` + 小写 hex，视图名空或 >25 字节直接拒绝 open）。
  FDB 侧不做 hex 编码（键无标识符长度限制、无引号注入面），但**必须**沿用
  `crate::layout::valid_view_name` 的入参校验，且不得让两个不同视图名映射到同一前缀
  （tuple 层按类型定界，天然无 `a|b` 与 `ab` 的拼接歧义——这是用 tuple 而非手拼字节串的硬性理由）。
- 共享账本独立于视图子空间：对位 PG 的 `grovefs_refs.holders`
  （`src/pg_store.rs:103-106` 注释："Lives in its own schema, not the per-view schema,
  so every view books/unbooks the same counts"）。
- **禁止**再挂一个 SQLite `refs.db` 作账本：那会在 FDB 权威之外引入第二个持久化权威，
  HA 切换与备份恢复下必然裂（决策 D5 的否决项）。

## 2. 记录编码（决策 D2，不可逆）

一条节点记录 = 一个键 + 一个值：

| 部分 | 内容 | 依据 |
|---|---|---|
| 键 | `view_sub + ('n', parent_id, name)` | 同目录名字唯一（下节 §3.1） |
| 值 | tuple：`(id, is_dir, data_ref, link_target, mode, uid, gid, mtime, atime, ctime, ino)` | `COLS`（`src/pg_store.rs:100-101`）去掉已进键的 `parent_id`/`name` |

- 整型一律走 tuple 层的**符号保持有序编码**：tuple 保证同类型编码保序，`i64` 负数排在正数之前。
  **禁止**手写定长大端整数（`l_start`/`l_end` 与 `id` 现网非负，但 `ino` 差值、`expires_at` 计算处迟早踩负数）。
- 字段顺序即格式：改动 = 全量搬迁。新增字段只允许追加在值 tuple 尾部，并在读侧容忍短 tuple（前向兼容）。
- 空值表达：`data_ref`/`link_target`/`parent_id` 可空，用 tuple 的 `None`（`Unit`/缺省）而非哨兵值，
  对位 SQL 的 `NULL`。`parse_node`（`src/pg_store.rs:108+`）把 PG 的 `ino IS NULL` 读成 0，
  且 `insert` 的注释（`src/pg_store.rs:837-843`）明确警告："ino IS NULL 会让该名字上的每次 FUSE lookup 失效"
  ——FDB 侧同样必须保证写入即带 `ino`（见 §4 的原子发号）。
- 根节点：`id = 1`、`parent_id = NULL`，open 时播种（对位 `PgEngine::open` 的 "seed the root row (id=1)"）。
  根的名字键用 `('n', 0, "")` 形态回避（root 不参与 lookup/readdir），实现时以常量固定，禁止散落猜测。

## 3. 查询形态映射表（逐条对照现有索引与热路径）

### 3.1 同目录名字唯一：`UNIQUE(parent_id, name)` → 键存在性

| | 现有 | FDB |
|---|---|---|
| 载体 | `UNIQUE(parent_id, name)`（`src/pg_store.rs:412`、`src/meta/engine.rs:1553`） | 键 `(v,'n',parent_id,name)` 本身 |
| 冲突判定 | PG 报 `UNIQUE_VIOLATION` → 映射 `EEXIST`（`src/pg_store.rs:1556`）；并发双插的落败方拿 `SQLSTATE 40001` 重试后第二次命中已提交行 → `EEXIST` | 两笔事务写同一个键 → OCC 读写冲突，落败方 `not_committed` 重试，重试后读到对手行 → 返回同一个 `MetaError::Exists` |
| 等价要求 | 无论走"约束违例"还是"重试后再看"，POSIX 调用方看到的都是 `EEXIST`，**不得**把冲突码外泄 | 同左（见 §6 映射表） |

### 3.2 按 ino 取属性：`idx_node_ino` → 指针键

- 现状：`SELECT {COLS} FROM node WHERE ino=$1 LIMIT 1`（`src/pg_store.rs:707-719`）；
  索引注释（`src/meta/engine.rs:1563-1566`）称其为 **"the single hottest metadata read"**，
  无该索引则每次 getattr/setattr 全表扫。
- FDB：`(v,'i',ino)` → 值 `(parent_id, name)`；getattr = 同一事务内**两次 point read**（先指针、再节点键）。
  **v1.1 更正（C3）**：原写"由 FDB 并行发起，只有一次 RTT 量级的往返"不成立——第二次的**键**就是第一次的
  **值**，存在真数据依赖，任何批量原语都无法把两者重叠。实际形态是：两读共处一个事务（因此只付一次
  冲突检测与一次提交），但串行两个 RTT。`Tx::get_multi` 仍保留，其适用面限于**互不依赖**的读。
- **显式否决**"把节点值冗余复制到 ino 键上"：`set_times`/`touch_mtime`（`src/pg_store.rs:1129-1160`）
  是只改时间的独立写，冗余副本会产生更新异常（POSIX mtime 与 stat 结果分裂）。
- 后果：`link_node`（硬链接）下多个名字共享一个 `ino`，`(v,'i',ino)` 必须**只存一个权威名字**
  （取 `name` 序最小者，与 PG 的 `LIMIT 1` 无序性相比这里是确定的——确定性更好，但要注意
  PG 现有行为未定义、迁移对拍时可能出现"getattr 返回的 name 不同"，属可归因差异，记进 §12）。

### 3.3 目录枚举：`idx_node_parent` + `readdir_page` → 前缀 range 扫描

- 现状：`readdir` = `WHERE parent_id=$1 AND name>'' ORDER BY name`
  （`src/pg_store.rs:724`；`name>''` 是**计划钉住**用的空操作，注释明说名字永不为空，作用是把扫描绑在
  `UNIQUE(parent_id,name)` 索引上按 name 序走）；`readdir_page(parent, offset, limit)`
  （`src/pg_store.rs:741-801`）用 `LIMIT/OFFSET` + 步长 32 的**检查点游标**
  （`READDIR_CURSOR_STRIDE=32`、`READDIR_CURSOR_CAP=65_536`，`src/pg_store.rs:165-170`）把
  每内核 READDIR 缓冲的代价压到 `O((limit+stride) log n)`；任何名字变更调 `drop_cursors()`
  （`src/pg_store.rs:803-810`）。
- FDB：`(v,'n',parent)` 前缀 range 扫描，**键序即 name 序**，无需 ORDER BY、无需计划钉住；
  分页 = `RangeOption{begin: first_greater_or_equal(key), limit}`。
- `offset` 语义必须原样保留（内核 `seekdir` 的 cookie 就是它）：FDB range 无法服务端跳过 offset，
  因此**照搬同款步长检查点**——进程内 `HashMap<(parent, stride_base), next_key>`，命中则从该键
  `start_after` 续扫、只跳剩余步数；未命中退回"从头扫过 offset 个"。
- `drop_cursors()` 的对位：FDB 侧游标只是**加速提示**，过期提示最多导致退化为全扫跳过，
  不影响正确性；但为与 PG 行为一致，任何名字变更仍清空该 map（成本极低，收益是行为可对照）。
- `supports_paged_readdir()` 返回 `true`（PG 侧靠 inherent impl 报告 store-level 分页能力，
  `src/pg_store.rs:1567-1572` 有说明；FDB 同理需覆盖该默认方法）。

### 3.4 目录 nlink：`idx_node_parent_kind` → "仅目录"存在性子空间

- 现状：`SELECT COUNT(*) FROM node WHERE parent_id=$1 AND is_dir=1`（`src/pg_store.rs:595-608`
  的 `finalize`，与 `finalize_batch` 的分组版 `src/pg_store.rs:639-673`）；
  复合索引注释（`src/meta/engine.rs:1556-1561`）：无 `is_dir` 入键时 10 万子项要 9.6ms，
  入键后 0.02ms（index-only scan）。
- FDB：新增 `(v,'k',parent_id, name)` **存在性键**（空值），只对 `is_dir=1` 的子项写。
  nlink = `2 + 该前缀的 range 计数`。批量 finalize（READDIRPLUS 一缓冲）对每个父目录各做一次前缀 probe，
  **v1.1 更正（C4）**：原写"用一次前缀扫完成"不对——FDB 无 `ANY($1)` 式的多前缀单扫描，M 个父目录就是
  M 次前缀 probe（可同事务并发），与 PG 的 `= ANY($1) GROUP BY`（`src/pg_store.rs:662-673`）同构而非等价，
  属 P6 差分对拍的可归因代价。
- 普通文件的 nlink = 同 `ino` 的名字数：新增 `(v,'h',ino, parent_id, name)` 存在性键（"该 ino 在本视图的名字集"），
  **v1.1 修订（C2）**：原设计为 `(v,'h',ino, name)`，实现加入 `parent_id`。存在性键的性质不变（空值、nlink 由
  前缀计数），多出的分量使 §3.2 的指针修复（unlink 后若该 ino 还有名字，需选一个权威名字写回 `'i'` 键）
  从"再扫一次"变成"一次解码"；`'h'` 的键序仍是 `(parent, name)` 序，与 §3.2 的"取 name 序最小者"一致。
  对位 `SELECT ino, COUNT(*) ... WHERE ino = ANY($1) GROUP BY ino`（`src/pg_store.rs:652-660`）。
  代价 = 每次 create/link/unlink/rename 多写一个短键；**否决**"改用 range 扫 data_ref 反查键再过滤 ino"
  （那会把 nlink 计算变成 O(同 key 名字数) 的扫描，且与 §3.5 的用途耦合）。

### 3.5 内容键反查：`idx_node_dataref` → `(v,'r',data_ref, parent, name)`

- 现状：`view_name_count` = `SELECT COUNT(*) FROM node WHERE data_ref=$1`
  （`src/pg_store.rs:1444-1457`），在 `insert`（判断 0→1 首次持有，`src/pg_store.rs:847-850`）、
  `link_node`（`src/pg_store.rs:949`）、`unlink`/`rename` 覆盖删除（`src/pg_store.rs:1062`、`1117`）
  四处被调用，语义都是"本视图还有没有别的名字挂同一个 key"。
- FDB：`(v,'r',data_ref, parent_id, name)` 存在性键；`view_name_count` = 该 `(v,'r',data_ref)` 前缀计数。
  **必须是存在性键而非计数器**：计数器会在 rename+unlink 交错时产生"减到 0 又有人加"的窗口，
  而前缀计数在读事务快照下与同事务的写集自洽（正是 PG 注释"the final `refs == 0` read is inside the
  same snapshot as the DELETE"所要求的，`src/pg_store.rs:1082-1084`）。

### 3.6 共享字节账本（跨视图）

- 现状：`ledger_add_tx` = `INSERT INTO grovefs_refs.holders(data_ref,holders) VALUES($1,1)
  ON CONFLICT DO UPDATE SET holders = holders + 1`（`src/pg_store.rs:1463-1472`，注释：
  0→1 时与节点 INSERT **同事务**）；`ledger_remove_tx` = `UPDATE ... holders-1 RETURNING holders`，
  `<=0` 则 DELETE 并返回 `Some(dr)` 交调用方 GC（`src/pg_store.rs:1478-1502`，注释明确
  "无账本行 = 没有其他持有者 → 按前账本时代行为释放"）。
- FDB：`(REFS_SUB, data_ref)` → `i64` 计数，读写都在同一事务内用 `transaction.atomic_op` 的
  `Increment` 完成加；减用 `Increment(-1)` 后**再读一次**取终值（或用读-改-写：FDB 同事务内读后写
  会被冲突检测保护，读-改-写更直白且与 PG 的 `RETURNING` 语义逐字对应）。**采用读-改-写**，
  理由是"缺行 → 视为无其他持有者 → 返回可 GC"这条边界行为必须与现有实现一致。
- 热点提示：账本键是**跨视图全局键**，同一 key 的并发首持/末释会冲突重试。现状 PG 同样如此
  （`ON CONFLICT` 行锁），不新增劣化；若压测显示热点，优化方向是"按 data_ref 前缀分桶"，属 §11 后置项。
- SQLite 侧的等价机制是 attach 共享 `refs.db` 并让每个 IMMEDIATE 事务**先取账本写锁**以形成统一全局锁序
  （`src/meta/engine.rs:1583+` 注释）。FDB 无锁序概念（OCC 自动检测），**但顺序仍要一致**：
  统一"先视图内键、后账本键"或反之，为使差分对拍可解释、避免未来引入真实锁时出歧义。

### 3.7 POSIX 文件锁与租约

- 现状：`locks` 主键 + 四索引（`src/pg_store.rs:422-436`）：`locks_ino(ino,l_start,l_end)`、
  `locks_owner(owner)`、`locks_sid(sid)`、`locks_expire(expires_at)`。
  `lock_op` 在一个 SSI 事务里跑四步（`src/pg_store.rs:1239-1318`）：
  ① `DELETE WHERE expires_at < now`（租约清扫）② 冲突 SELECT
  `WHERE ino=$1 AND owner<>$2 AND l_start<=$3 AND l_end>=$4 AND ($5=1 OR ltype=1) LIMIT 1`
  ③ `DELETE` 自己区间内的旧锁 ④ `INSERT` 新锁（`expires_at = now + lease_secs()`）。
  另有 `get_lock`（只读，READ COMMITTED 走读池，`src/pg_store.rs:1321-1360`）、
  `release_locks_of_owner`、`renew_locks`（按 sid 续期）、`release_locks_of_session`、`sweep_expired_locks`。
- FDB：
  - 锁主键 `(v,'l',ino, l_start, l_end, owner)`，值 `tuple(ltype, pid, sid, expires_at)`。
  - **区间重叠无法用单一定界 range 表达**（`l_start<=$e AND l_end>=$s` 是二维条件）：
    与 PG 一样走 `(v,'l',ino)` 前缀扫 + 事务内过滤。可接受的理由同 PG——单文件锁条目数有界。
    若压测发现"锁极多的热文件"退化，加"每 ino 锁桶"二级编码（P4 的退出/回滚项，属编码级追加，见 §10）。
  - `owner<>$2` 的排除在过滤阶段做；`(want_write OR ltype=1)` 的"读请求只与写锁冲突"语义
    **必须逐字保留**（这是 `fcntl` 读锁相容性的裁决点）。
  - 清扫：加 `(v,'lx', expires_at, ino, l_start, l_end, owner)` 存在性键，`lock_op` 的第①步与
    `sweep_expired_locks` 走该前缀扫（对位 `idx_locks_expire`）；写/删锁时同步维护。
    清扫上界把 `expires_at` 编成 `(now,)` 的**严格小于**界，使 `expires_at == now` 仍算活锁
    ——逐字对位 PG 的 `DELETE ... WHERE expires_at < now`（C5-a）。
  - owner/sid 清理：加 `(v,'lo', owner, ino, l_start, l_end)`、`(v,'ls', sid, ino, l_start, l_end, owner)`
    存在性键，对位 `idx_locks_owner`/`idx_locks_sid`——**这是 close/exit 隐式释放不泄漏的记账基础**
    （既有 pitfall：owner 记账漏导致 `fcntl` 锁隐式释放泄漏。FDB 侧必须同样按 owner 与 sid 双维度可反查）。
  - **二级键的字段集不是设计时能拍板的（C5，v1.1 §3.7 初稿已被实现推翻）**：判据是
    "**二级键必须能无歧义地重建主键**"。初稿写的 `'lo'(owner,ino,l_start)` / `'ls'(sid,ino,l_start)`
    缺 `l_end`（更缺 `owner`），两种真实形态下会塌成同一个键：① 同一会话的两个 owner
    在**同一 `l_start`** 上持相容读锁（`fcntl` 读锁允许，POSIX 语义要求两行都在）；② 同一 owner 的
    两个相邻区间共享起点（`[0,50)` 与 `[0,100)`）。塌键 = 删一个锁时抹掉另一个的记账，
    而这类丢失在只读路径上不可见，只会在隐式释放时表现为"泄漏了别人的锁"——
    正是本项目最不能接受的那类错误。补齐主键全字段后四键互解，由
    `tests::lock_keys_reconstruct_their_primary_exactly` 锁死（含每条二级键落在自己的扫描前缀/范围内）。
  - `expires_at` **同时存在于值和 `'lx'` 键**：主键不含 expires，故续期 = 值更新 + 一次 `'lx'` 换位
    （删旧 `'lx'`、插新 `'lx'`），**不移动锁身份**（`ino`/区间/owner 不变，冲突判定与 POSIX 一致）。
  - FDB 没有"自动维护的索引"：**主键与 `'lx'`/`'lo'`/`'ls'` 三条二级键必须同生同死**，
    加锁与放锁各自写一个四键原子集合（`put_lock_tx` / `delete_lock_tx`）。
    任何一处漏维护都等价于 SQLite/PG 上"少删了一行索引"，但 FDB 侧没有 `ANALYZE`
    或重建索引可以救——只能靠 §13 的键互解测试与 P6 差分对拍兜住。
  - `get_lock` 是纯读：FDB 用只读事务（对位 PG 的读池 READ COMMITTED，不产生写冲突），
    且**永不调用 commit**——读集不进冲突集是 `getlk` 不拖慢 `setlk` 的原因。

### 3.8 ctrl：epoch / fence

- 现状：`ctrl(k TEXT PRIMARY KEY, v BIGINT)`，`get_epoch` = `SELECT v FROM ctrl WHERE k='epoch'`
  （`src/pg_store.rs:1198`），`set_epoch` = `INSERT ... ON CONFLICT(k) DO UPDATE SET v=excluded.v`
  （`src/pg_store.rs:1215`）。
- FDB：`(v,'c','epoch')` → `i64`。`set_epoch` 为单调更新（读后比较再写，冲突即重试），
  与现有 `fence_handle`/`fetch_fence_epoch`（trait 默认方法）协作： fencing 语义依赖
  "陈旧节点的重放被 epoch 拒绝"，键形态变化不影响该判定，但**必须**保证读写同键、
  否则备节点/网络分区下的自降级会失效。

### 3.9 rename：多步单事务（最难的形态）

- 现状（`src/pg_store.rs:973-1073`，一整个 SSI 事务）：
  SELECT src(`id,ino`) → `is_descendant_tx` 环检查（沿 `parent_id` 上溯，上限 4096 跳，
  `src/pg_store.rs:1414-1436`）→ SELECT tgt(`id,is_dir,data_ref,ino`) → 同 inode no-op 判定
  （`rename(a,a)` 与两个硬链接名互改，漏判会**误删目标链接、把 nlink 改小**）→
  `noreplace` 则 `EEXIST` → 类型不符 `TypeMismatch` → 目录目标非空 `NotEmpty` →
  DELETE 目标行 → 目标 data_ref 的账本/GC → UPDATE src 的 `parent_id,name,ctime`。
- FDB：同一事务内
  ①读 `('n',parent,name)` 取 src 记录；②环检查 = 沿 `parent_id` 逐级读 `('n',gp,child_name)`：
  **注意** FDB 的 ino 键只给 `(parent,name)` 不给上溯用的"父的名字键"，环检查需要
  从 `new_parent`(id) 出发找到它的**父键位置**——现 PG 用 `SELECT parent_id FROM node WHERE id=$1`，
  即按 `id` 回表。故必须再加 `(v,'d',id)` → `(parent_id, name)` 的**id 反查键**（与 §3.2 的 ino 键不同物：
  `id` 是内部主键、`ino` 是 POSIX inode）。
  > 这一条是映射表里最容易漏的：`node.id` 在 SQLite 是 rowid、在 PG 是 BIGSERIAL，
  > 代码里有 6 处 `WHERE id=$1`（环检查、`SELECT is_dir FROM node WHERE id`、`DELETE ... WHERE id`）。
  > FDB 无 rowid 概念，**必须显式建 `'d'` 反查键**，否则环检查与"按 id 删目标"都无法定址。
  ③④⑤…与上列顺序逐一对应；⑥UPDATE = 删旧名字键 + 插新名字键 + 同步 `'d'`/`'i'`/`'r'`/`'k'`/`'h'`
  全部二级键（一次事务内完成，外部不可见中间态 → I2 可见原子性成立）。
- 并发：两笔 rename 的源/目标键集相交时 OCC 让一方重试，重试后看到已提交形状，返回正确 POSIX 结局
  （`ENOENT`/`ENOTEMPTY`/`EEXIST`/成功）——与 `src/pg_store.rs:985-993` 注释的论证同构，
  只是把"SSI 谓词锁中止"换成"读写集冲突中止"。

### 3.10 治理不变量 I1–I4 ↔ FDB 落点（对位 TOS §5）

权威口径是 `paper/grovefs-tos.tex` 的 `\subsection{The Four Invariants}\label{sec:inv}`：
**I1** 条目唯一性、**I2** 身份保持的共享/回收/加锁、**I3** 失效完整性、**I4** 写穿透到源
（`\begin{theorem}[Invariants I1--I4 hold]` 组装它们）。§3.9 的 rename 行要求本文件给出这张对照表，
要求的形式是**逐条写明"FDB 靠什么让它成立"与"本仓哪条断言证伪过它的反面"**——
"换了地基应当仍然成立"不是判据，只是期望。

| 不变量（TOS §5 陈述要点） | FDB 侧的承载（键形态 / 事务层） | 反证断言（`.build-bin/` 套件，fdb 档实测） |
|---|---|---|
| **I1** 任何 lens 既不为真 inode 增删 `D` 的行，显示别名也永不物化为存储条目；真 `i` 的 `nlink` 在 Λ 下不变 | 存储侧只有 `(v,'n',parent,name)` 一族的**真名键**。`LayoutMeta` 在翻译层合成显示条目：合成目录取 `i64::MAX` 向下的保留虚拟 ino（`src/layout.rs:29`、`is_virtual` 567），**从不下沉到 `FdbStore`**，故不占 `'n'`/`'i'`/`'d'`/`'h'` 任何一键；`nlink` 存在节点记录里，只有 `link_node`/`unlink` 会改（§3.5） | `test_govcheck.sh` B1「display alias does NOT inflate nlink (I1: no stored entry added)」；同套件 A1/A2 是**被实测证伪的反面**（overlay copy-up 另起一个对象、symlink 为名另起一个 inode） |
| **I2** 引用计数、GC、建议锁一律键在**真 inode** 上，跨任何暴露它的 lens 达成一致（含"共享命运"） | 三条同时到位：①账本在**全局** subspace `(root,'refs')`（`src/fdb_store.rs:267`/`291`/`305`，对位 PG 的 `grovefs_refs` schema，决策 D5），两个视图 surface 同一 `data_ref` 时扣同一份计数；②锁主键 `(v,'l',ino,…)` 首字段是真 ino（§3.7），lens 先把显示路径翻译回源路径才落到 `FdbStore`，故经别名加的锁与直挂 base 加的锁同键；③一 POSIX op 一事务（§5）+ 严格可串行化 ⇒ 中间态不可见，"一起消失"来自提交原子性而非清理扫描 | `test_govcheck.sh` B2「source and its lens alias both disappear together (I2: one fate, unlike a dangling symlink)」、B3「map-relocated path is the SAME inode (no copy)」；反面是 A2「after removing the source the symlink DANGLES」 |
| **I3** 推失效事件引用**源** inode；对 `i` 的写使每个 lens 暴露 `i` 的挂载都失效 | TOS 明写这条属于 notify 层而非纯元数据层（`paper/grovefs-tos.tex:834` 一带），所以换地基**不新增机制**；但它对元数据有一处硬要求：事件要能拿到真 inode 的**当前** `(parent,name)`。服务端用 `engine.getattr(ino)` 解析（`src/remote/meta/server_ext.rs:1357`），FDB 侧即 `'i'` 指针键 + 名字键在同一只读事务内两次读（§3.2、修订 C3） | `test_govtime.sh`「derived viewer saw each base write via the push channel」、「live mount converged across 3 widen+narrow cycles (no remount)」、「retired alias does not reach the real tree (no leak window)」；`test_layout.sh` §9「derived view sees base write immediately (push)」及其 hot-reload 版 |
| **I4** 显示路径上的每次改动都作用在 `resolve_Λ(p)` 上；合成节点拒直接创建（`EPERM`），派生视图只读 | 写意图在翻译层就换成源坐标才交给 `FdbStore`（`src/layout.rs` 的 `create_node` 1128 起），所以 FDB **只见真 `(parent_id,name)`**，结构上没有"影子行"可写；合成目录内 create 由 `LayoutMeta` 拒绝，派生只读由两层独立成立（kernel RO 挂载 + 服务端按名拒写意图，TOS `lem:derived`），fork 额外挂 `content_ro`（`src/remote/meta/server_ext.rs:1656`） | `test_govcheck.sh` B1「source reflects the lens write IN PLACE (I4 write-through; no stale lower copy as in overlay)」；`test_layout.sh`「derived view refuses writes (EROFS)」「derived view refuses deletes」「read-only contract survives hot reload」；反面是 A1 的 overlay「stale lower copy」 |

换地基时**只有这四点是静默破口的候选**，已逐一核对：

1. **虚拟 ino 区间 vs 发号器**：SQLite 的 rowid、PG 的序列都由存储引擎 own，grovefs 不控制值域；
   FDB 下 `ino = id`（C1）由我们自己的 `(v,'m','id')` 号段升序发放，因此"`i64::MAX` 向下保留"
   与真实发号可证不相交。若日后把发号改成高位段或随机化，I1 的"合成条目不占键"论证即刻失效。
2. **`'i'` 键不得冗余值副本**：§3.2 当初否决"把 `(parent,name)` 冗余进 `'i'` 的值"，在 I3 上拿到
   第二个理由——rename 只改一处就会推出**从未提交过**的 `(parent,name)` 坐标，破的是失效完整性。
3. **`'refs'` 是 per-view 隔离的唯一例外**，且这个例外正是 I2 要求的（对位 PG 注释"every view
   books/unbooks the same counts"）。任何"顺手把账本挪进视图 subspace"的改动会静默造出两份计数。
4. **`fork` 与 TOS `lem:derived`（派生投影不 fork）在 FDB 档从性质升为实现约束**：一个视图就是一个
   subspace，没有 snapshot/clone 路径可复制整棵树，所以 `fork` 要么真能用、要么点名拒绝，二者只取其一。
   实测取后者，且拒绝文案与 PG 文案**同等明确**——五要素（`not implemented` / 点名后端 / 点名操作 /
   给出理由 / 指向真正可用的替代）由单测 `src/mcp.rs:3212` `fork_and_graft_refuse_explicitly_on_an_fdb_store`
   与 e2e `.build-bin/test_fdb.sh` §10d 双重钉住，并断言拒绝后**不留**本地 `meta/<fork>.db`、不留 fork 定义。

与 §12 的交叉核对：§12 的四条可归因差异（`getattr` 的 name 选择、时间精度、`ino IS NULL`、id 空洞）
**都不落在 I1–I4 的判据上**——前两条是展示与精度，后两条是 FDB 严格强于原实现。因此 P6 差分对拍若出现
这四条之外的差异，一律按 R4/G4 处理（停合并），不得当"可归因差异"吸收进本节。

## 4. id / ino 发号（决策 D4，不可逆）

- 现状：SQLite `id INTEGER PRIMARY KEY`（rowid 自增）；PG `id BIGSERIAL PRIMARY KEY`
  （`src/pg_store.rs:399`），`insert` 用 `INSERT ... RETURNING id` 再 `UPDATE node SET ino=id WHERE id=id`
  使 `ino == id`（`src/pg_store.rs:851-877`，注释强调必须原子，否则 `ino IS NULL` 破坏 lookup）。
  `link_node` 则复用 `src_ino` 作为新行的 `ino`（`src/pg_store.rs:952-964`）——**id 不同、ino 相同**，
  这正是硬链接的表示法。所以 `id` 与 `ino` 是**两个独立计数器**，不能合并。
- FDB：
  - `(v,'m','id')` 与 `(v,'m','ino')` 两个 `i64` 分配器键。**v1.1 修订（C1）**：`ino` **不是第二个发号器**，
    `create_node` 直接取 `ino = id`（对位 PG 的 `UPDATE node SET ino=id WHERE id=id`，`src/pg_store.rs:851-877`）：
    两个独立计数器会让 P6 的三后端对拍把每一棵树都报成"不同"。因此 `(v,'m','ino')` 的含义改为
    **迁移高水位**（仅 P8 导入携带外部 `ino` 时抬高，取 `max(现有)`），上面"两个独立计数器、不能合并"
    的结论在 FDB 下收窄为"一个计数器 + 一个高水位"；`link_node` 仍复用 `src_ino`（id 不同、ino 相同）不变。
  - **批量号段**：事务内 `Increment(+1024)` 后读回基值，进程内顺序发放；号段耗尽再取下一段。
    单键自增会成为整个视图的写热点（每次 create 都要抢同一键），分片号段把冲突面降到 1/1024。
  - 号段在**未落盘前不外泄**：进程崩溃丢号不重号（允许空洞，与 PG 序列的 `setval` 语义一致）。
  - 迁移续接：外部导入时若携带原 `id`/`ino`，分配器键须 `max(现有)` 续到
    （对位 PG 的 `pg_store.rs:464` 的 `setval`）——**P8 已交付**：`Tx::raise_to_max`
    （`src/fdb_store.rs:916`）在导入事务内把 `(v,'m','id')` 与 `(v,'m','ino')` 抬到导入行的
    最大值（`src/fdb_store.rs:2980-2981`），判据不是"看起来续上了"，而是**导入后新建节点的 id
    严格大于导入侧最大 id、且全树 id 无重复**（`test_fdb.sh` §12c）。
- 空间口径不变：`i64`（9.2e18），与两现有后端一致。

## 5. 事务粒度（决策 D3，不可逆）

- 一个 POSIX 操作 = 一个 FDB 事务；rename 全程单事务（§3.9）；lock_op 四步单事务（§3.7）。
- 写集上界核算（对照现实现）：`create_node` = 1 主键 + 最多 4 二级键 + 账本 1 键 + 分配器 1 键 ≈ **7 键**；
  `rename` 覆盖非空目录以外的常规情形 ≈ 主键 2 + 二级键 ≤10 + 账本 1 ≈ **13 键**；
  每键平均 < 200 B（值 tuple ≈ 节点 13 列），远小于 FDB 事务软限。
  记录进本表的用途：**任何实现若让单 op 写集超过 ~100 键，即视为偏离设计**，须回到本文件评审。
- 冲突重试预算：沿用现有常数语义——`SERIALIZATION_MAX_ATTEMPTS = 12`、
  退避 `200µs` 翻倍至 `25ms`（`src/pg_store.rs:181-190` 的注释理由：8 写者抢热点 rename 也能 ~1 秒收敛）。
  FDB 侧初值照抄，压测后再调（§11）。

## 6. OCC 冲突 → POSIX errno 映射（决策 D6，不可逆）

| 现有 PG 分类（`src/pg_store.rs:473-479`、`1556`） | 语义 | FDB 对位 |
|---|---|---|
| `SQLSTATE 40001` serialization_failure | SSI 中止，**可重试** | `transaction_not_committed`（OCC 冲突），重试整笔 |
| `SQLSTATE 40P01` deadlock_detected | 锁等待环，**可重试** | FDB 无阻塞锁，理论无此码；若出现 `transaction_timed_out` 归入同一重试分支 |
| `UNIQUE_VIOLATION` | 真冲突，**不重试**，映射 `MetaError::Exists` | 重试后读到对手行 → `Exists`（不新增分类：FDB 无约束违例，唯一性由冲突承担） |
| 其余 SQL 错误 | 硬失败 `MetaError::Db` | FDB 客户端/集群错误，同分支 |

**硬要求**：冲突码绝不外泄给 POSIX 调用方（与 PG 的 `40001/40P01 绝不冒泡`同构，docs.html §5.7 亦有该表述）。
重试预算耗尽才报错，且报的是"操作未完成"类 io::Error，不是数据库方言错误串。

## 7. 边界：只换元数据权威（约束 3）

不改动的部分（逐项，来源已核）：

| 项 | 位置 | 状态 |
|---|---|---|
| `blocks/` 对象与 256 桶分片、条带化、单节点多盘放置 | `src/backend/mod.rs`（`StorageBackend` 10 方法：`new_key/create/read/write/len/fsync/truncate/remove/import_at/statvfs`）、`LocalBackend`/`ShardedBackend`/`PlacementBackend`/`RemoteBackend` | **一字不动** |
| 字节内容的实际存储与带宽路径 | 同上 | 不进 FDB（FDB 值软限 100KB 反而收窄数据路径） |
| 账本**机制**（跨视图共享一份 + 引用计数 + 归零即 GC） | §3.6 | 机制保持，仅物理落点换到 FDB 共享 subspace |
| 线协议与客户端 | `src/remote/proto.rs`（MAGIC/MAX_FRAME/操作码表） | 禁改（约束 1） |
| 治理装饰器 | `src/layout.rs` 的 `LayoutMeta`（源路径↔显示路径翻译，不物化目录项） | 复用，FDB 只提供更底层的 `MetaProvider` |

## 8. 与两现有后端的结构对照（一表看全）

| 维度 | SQLite | PG | FDB |
|---|---|---|---|
| 视图隔离单位 | `<view>.db` 文件 | `grovefs_v_<hex>` schema | subspace 前缀 |
| 每视图常驻 | 1 写连接 + ≤4 读连接 + ~15 fd + 页缓存 | 8 写 + 4 读 = 12 条物理连接 | **仅一个进程内 subspace 句柄**（无连接、无 fd） |
| 隔离级别 | IMMEDIATE 事务（单写者） | `default_transaction_isolation='serializable'`（SSI） | 原生严格可串行化（OCC + MVCC） |
| 索引 | B-tree 索引 4 + UNIQUE | 同形态镜像 4 + UNIQUE | 全部改为**显式二级键**（§3.2–§3.7） |
| 分页 readdir | 游标 + LIMIT/OFFSET | 游标 + LIMIT/OFFSET | range + start_after + 同款步长检查点 |
| fencing/epoch | `ctrl` 表 | `ctrl` 表 | `(v,'c','epoch')` 键 |
| 控制面（view 定义/别名/API key） | `<meta-dir>/meta/views/*.layout` + `meta/api_keys.db` 本地文件 | 共享 `grovefs_control` schema（`view_defs`/`api_keys`） | **仍是本地文件**：`fdb://` 不是控制面后端，只是数据面换了 subspace（§14 v1.3 C6） |
| HA | grovefs 自研快照主备（`VACUUM INTO`） | PG 流复制/`pg_basebackup`/PITR | 集群自带三副本，grovefs 侧**不叠加** |

## 9. 键长核算表（FDB 软限：键 ~10KB，值 ~100KB，事务写集软限 10MB）

| 键 | 组成 | 上界 |
|---|---|---|
| 节点主键 | subspace(视图名 ≤25B) + `'n'` + i64 + name(≤255B) | **< 300B** |
| ino/id 二级键 | + i64 | < 300B |
| data_ref 反查键 | + data_ref（内容键串） | < 400B |
| 锁主键 | `'l'` + 4×i64 | < 120B |
| 锁二级键（`'lx'`/`'lo'`/`'ls'`，C5） | `'lx'` + 5×i64（含 owner）、`'ls'` + 5×i64 | < 160B（每把锁共 4 个键） |
| 值（节点记录） | 11 列 tuple | **< 600B**（路径不入库，最长 name 255B 已在键侧） |

结论：本项目记录粒度（约 230 B/节点量级）与 FDB 限制余量四个数量级以上，**不存在需要分块值的形态**。

## 10. 二级索引"只增不改"清单（决策 D10）

允许新增键前缀（安全，不动既有数据）；**禁止**删除或调换既有键的字段顺序（= 全量搬迁）。
已冻结前缀：`'n'` 主键族、`'i'`(ino→名)、`'d'`(id→位置)、`'r'`(data_ref→名字)、`'k'`(仅目录)、
`'h'`(ino→名字集)、`'l'`/`'lo'`/`'ls'`/`'lx'`(锁)、`'c'`(ctrl)、`'m'`(分配器)、`'refs'` 共享账本。
锁侧四条键的**字段集**在 v1.2 定稿（C5）：二级键携带主键全字段，以便无歧义反解；
此前缀集合在 P4 之前无数据写入（锁面 P4 才落地），故该修订不需要搬迁，v1.2 之后即为不可逆。

## 11. 可后置参数（决策 D11/D12）

号段步长（初值 1024）、退避常数、重试次数、账本是否分桶、锁是否分桶、集群拓扑与副本因子、
`ssd`/`memory` 存储引擎选择、NVMe/SATA 档位——全部为压测后再定项，不影响键空间契约。

其中**"协调器不可达的等待上限"这一项未设，后果已实测并写进用户文档**：`libfdb_c` 对
`coordinator_not_available` 按自身策略持续重试，本后端不强加超时，于是数据面 op 在集群不可达时
**静默阻塞而非报错**（150s 无任何输出实测；`meta-server` 启动不探测集群，横幅照打）。它属运维可见
行为而非键空间契约，故 USAGE §15.16 / docs.html §5.9 按"不可达 = 无声等待"记账，判据指向
`fdbcli status minimal`；将来若引入该上限，改的是这一项的表述，不是任何编码或事务语义。

## 12. 已知的可归因差异（诚实负空间，P6 差分对拍须按此核对）

1. `getattr` 在硬链接多名下返回的 `name` 可能不同（PG `LIMIT 1` 未定义，FDB 取 name 序最小）——
   对拍须**按 ino 集合比对，不按 name 比对**。
2. 时间精度：`mtime/atime/ctime` 秒级（`now()` 取 `as_secs`，`src/pg_store.rs:58-64`），三后端一致，无差异。
3. `ino IS NULL` 这类"半写入行"在 FDB 下结构上不可能出现（写集原子提交），属**改进**而非差异。
4. id 空洞：FDB 号段丢弃的粒度是 1024（崩溃时），PG 序列丢弃粒度是单次 cache 步长——
   `id` 不对外可见，仅 `ino` 可见；迁移场景按 §4 的 `max()` 续接消除。
5. **共享账本的计数只增不减的那一面**：`(root,'refs')` 是集群级、跨视图的唯一计数（§3.6、D5），
   视图被**绕过 unlink 的路径**销毁（`clear_view`/`DROP SCHEMA CASCADE`/直接删文件）时，它留给账本的
   计数不会回收，于是该 `data_ref` 永远「被持有」。这是**保守**方向的多计（GC 宁可不删），
   `src/gc.rs` 把它归为 `held` 类并给出成文补救（`grovefs refs rebuild`）；SQLite 臂按视图一文件、
   账本 `refs.db` 随目录一起消失，结构上不可能出现这一条，PG 与 FDB 两臂则会。
   对差分对拍的后果是实打实的：一条多出的计数就能让 `(gc Kx)` 变成 `ok`（§14 v2.1 全链路复现），
   所以 harness 必须让每轮的 `data_ref` 唯一并在拆视图前按名 unlink。

## 13. 验收清单（P1 出口判据）

- [x] §3.1–§3.9 九类映射齐备，每条带现有实现行号取证
- [x] §0 RecordLayer 取舍段存在且表述为"复刻语义、不用该绑定"
- [x] §9 键长核算表（视图名/路径/name 编码后均 < 键上限）
- [x] §5 单 op 写集上界与超出即回评审的硬线
- [x] §6 冲突分类与"绝不外泄"硬要求
- [x] §7 数据面边界逐项列名
- [x] §10 不可逆项冻结清单
- [x] §3.10 与 TOS §5 的 I1–I4 对照表（rename 行要求；**v1.3 已补**，附四点静默破口核对）
- [x] P0 环境门通过后方可进入 P2（本文件不判 G1）—— **v1.1 已达成**：G1 三条判据均取证（见 §14 末）

## 14. 版本注记

### v1.0（随 `v1.0` 标签发版）

P1 原始设计：§0–13 全文，九类查询形态到 tuple 键的逐条映射、号段分配器、事务粒度与写集上界、
OCC → errno 映射、三后端对照与可归因差异清单。未动一行代码。

### v1.1（P2/P3 实现期间）

四处偏离均源于"实现时以真实客户端库的行为为准"，而不是设计时的推演。每条在正文就地修正，
并在此给出取证依据与影响面。编号与正文的（C1）…（C4）对应。

| # | 修订 | 取证依据 | 影响面 |
|---|---|---|---|
| **C1** | `ino = id`，`'m','ino'` 降为迁移高水位 | `src/pg_store.rs:851-877`（PG 本身就是 `ino=id`）；`src/fdb_store.rs::create_node` | 改变 §4 的计数器语义；使 P8 迁移必须单独处理外部 ino |
| **C2** | `'h` 键加 `parent_id`：`(ino, parent, name)` | `src/fdb_store.rs::Keys::holder`；`tests::key_encoding_is_byte_frozen` | 仅影响簇内尚无数据时（现仍在 P3 实验分支）；属性不变 |
| **C3** | getattr 两次读串行、同事务，不是一 RTT | `foundationdb-tuple`/`Transaction::get` 的键依赖；`src/fdb_store.rs::getattr` | §3.2 的延迟承诺降级；热路径比原设计多一个 RTT |
| **C4** | 批量 finalize = M 次前缀 probe（同事务） | FDB 无多前缀单扫描原语；实现处是逐条解析 nlink 的 `src/fdb_store.rs::FdbStore::info`（由 `readdir`/`readdir_page` 在同一只读事务内逐项调用），对位 PG 的 `pg_store::finalize`/`finalize_batch` | READDIRPLUS 代价随批内父目录数线性增长；P6 对拍需按此对齐 |

另有一处**字节层面的事实更正**（不编 C 号，因为它不是设计决策而是原示意式含糊）：§1 的
`\x01 + tuple(...)` 展开为实测字节表后，变长元素的终止符是 `0x00` 而非 `0xff`（后者只作 NUL 转义引导）。
根前缀实为 `\x01grovefs\x00`，该字节已写入 `tests::key_encoding_is_byte_frozen` 的 5 个期望值，
并在 `src/fdb_store.rs` 的 `ROOT_PREFIX` 注释里同步更正（v1.1 前的注释写的是 `0xff`，属注释错、非格式错）。

**G1（P0 环境门）取证**：① `fdbcli --exec "status minimal"` 对 `configure new single ssd` 的本地集群
返回 `The database is available.`；② `cargo build --release --features fdb --example fdb_spike` 链接
`libfdb_c` 成功（用户态制品：`/data/grovefs/.fdbbin`，`FDB_CLIENT_LIB_PATH` 指向它，无需装包）；
③ spike 跑 `--reads 100000 --pairs 10000`：winners=1/exhausted=0、2000 项严格有序、
**aborts=2705 retries=2705 gave_up=0**、10 万 point read 全命中、`P0_SPIKE_VERDICT=PASS`。
复制因子 = 1，所以这些数字只证明机制（OCC 重试环真实发生且收敛），**不得用作吞吐结论**。

### v1.2（P4 锁面实现期间）

| # | 修订 | 取证依据 | 影响面 |
|---|---|---|---|
| **C5** | 锁的三条二级键加宽为主键全字段超集：`'lx'(expires,ino,start,end,owner)`、`'lo'(owner,ino,start,end)`、`'ls'(sid,ino,start,end,owner)`；并把"二级键必须能无歧义重建主键"写成硬判据 | 初稿形态在同 start 的相容读锁与共享 start 的相邻区间上撞键（§3.7）；`src/fdb_store.rs::Keys::{lock_expire,lock_owner,lock_sid}`；`tests::lock_keys_reconstruct_their_primary_exactly`、`tests::lock_value_and_overlap_follow_pg` | 每把锁 4 个键而非 1 个（§9 已计入）；因 P4 前锁面无数据，不需搬迁；v1.2 后不可逆 |
| **C5-a** | `'lx'` 清扫上界取严格小于 `now`，`expires_at == now` 算活锁 | PG/SQLite 均为 `expires_at < now`（`src/pg_store.rs:1259`、`src/meta/engine.rs:2373`）；`Keys::lock_expired_upto` | 三后端在整秒边界上行为一致，P6 对拍无需特例 |

同时修正一处**文档自身的假陈述**（不编 C 号）：v1.1 的 §3.7 把四步写完了，但 `src/fdb_store.rs`
的模块注释在 P4 之前仍声称锁面"报 not yet implemented"——实际行为是 `get_lock` 返回 `None`、
其余静默 no-op（对 POSIX 调用方表示"锁全部可用"，是危险方向上的默认值）。该注释已按实现改写，
并把"no-op 不等于 unsupported"列为存量分派臂转真实实现时的固定检查项。

### v1.3（P5 派生视图、治理与控制面期间）

新增 §3.10（与 TOS §5 的 I1–I4 对照表），补 P1 rename 行欠下的那张表。两条实现期偏离：

| # | 修订 | 取证依据 | 影响面 |
|---|---|---|---|
| **C6** | `fdb://` 部署的**控制面仍是本地文件**：`FileControlStore::new_fdb(home, uri)` 是第三个形态（文件控制面 + subspace 数据面），`backend()` 必须回 `foundationdb` 而不是 `sqlite`，`describe()` 同时报出两面 | P2 的"控制面暂留 File/PG 两实现"裁决落地：`src/mcp.rs:325`/`359`；单测 `db_path_override_keeps_the_fdb_backend`（`src/mcp.rs:3268`）钉住"改根不降级后端" | §8 新增"控制面"一行。后果：`view define`/`alias`/`apikey` 在 fdb 档读写的是 `<meta-dir>/meta/views/*.layout` 与 `meta/api_keys.db`，**不需要集群可达**；`test_apikey.sh` fdb 档与 SQLite 档逐条一致即为该裁决的证据 |
| **C7** | refuse-not-downgrade 从"分派臂"**推广到 CLI 物理树操作**：`import` 在 P5 之前没有任何后端判定，会在 `fdb://` 部署旁写出 `meta/<view>.db` + `blocks/` 对象而服务端永不读取（影子树） | 由 `test_govcheck.sh` fdb 档实测冒出的 FAIL 定位（`import should refuse under fdb://: … imported 1 object(s)`）；修正在 `src/main.rs:2722`（PG 文案）/`2728`（FDB 文案），新增 `import --meta-store URI`，与 `GROVEFS_META_STORE` 同效 | 新一个用户可见参数（不新一个行为开关：它只用于判定该拒谁）；§12 负空间不变——这属"静默半可用"缺陷的封堵，不是可归因语义差异 |

同阶段核对 P2 两项旧裁决仍成立：① `--meta-store fdb://…` 与 `--role standby`/`--peer`/`--witness`
同时给时 fail-fast，且 FDB 的文案独立（`src/remote/meta/server_ext.rs:586`/`597`，单测覆盖
`FDB + --peer` 与 `FDB + --role standby` 两种给法）；② 派生/别名/只读契约与治理写路径全部走
§3.10 的 I1–I4 论证，未因换地基新增任何例外机制。

**P5-b/P5-c 取证（四套治理套件换后端参数复跑，双档数字）**：套件顶部的 `*_BACKEND=fdb` 把
"挂载够到哪棵树、定义写进哪个控制面、服务监听哪个端口"变成参数，**断言本身不为后端改写**；
部署由 `.build-bin/fdb_scaffold.sh` 供给（`fdb_gate`/`fdb_binary`/`fdb_up`/`mnt_gate`/`kp`/`wk`）。

| 套件 | SQLite 档 | FDB 档 | FDB 档新增的那一条边界 |
|---|---|---|---|
| `test_layout.sh` | 78/0 | 82/0 | 派生名在 `view list` 里可见但**不列举 subspace**（C6 的另一面：定义是文件、树是 subspace） |
| `test_govtime.sh` | 8/0 | 10/0 | 改定义热生效走的是文件 mtime，与集群无关 |
| `test_apikey.sh` | 33/0 | 34/0 | key 与审计落 `meta/api_keys.db`，逐条与 SQLite 档一致 = C6 的直接证据 |
| `test_govcheck.sh` | 30/0 | 29/0 | I1–I4 的 B4 行在此档读作"本地存储 yes / `fdb://` 拒绝并给替代"，是**提供位置的边界**，不是后端违反不变量 |

§3.10 的 I1–I4 对照表即按这四套的 fdb 档逐条复核，未出现需要"放宽不变量"才能通过的项。

### v1.4（P6 台架与 P7 文档口径）

P6 把 `fdb` 套件登记进 `run_all.sh`（**板上只登记这一套**：其余 `*_BACKEND=fdb` 档按 P4/P5 的先例
在板外跑，因为它们要独占同一台机器上的集群与端口，串进板内只会互相撞车）。全板 GREEN（34 套，
含 `fdb` PASS）。P7 把三后端口径写进 `USAGE.md` §6.3/§11.10/§15.9 与 `docs.html` §5.9/§14.x/§16，
`bash skills/grovefs-docs/scripts/doccheck.sh` 取 `EXIT=0`（变更史叙述 0 项、参数差集 0 项、
docs.html 标签闭合 0 错）。

一处**口径事实更正**（不编 C 号，因为它改的是文档而非机制）：P7 首版把 `view export`/`view import`
列进了 FDB 档的"拒绝"清单，而 `src/main.rs:2223`/`2296` 的判定是 `if !store_is_fdb { require_sqlite(..) }`
——这对命令在 `fdb://` 下**本来就可用**。P8 的实现与三套双档取证把它坐实，两份文档同步改写（v1.5）。

### v1.5（P8 迁移与回滚双向）

Spec 的判据是"不新建命令"：搬运走既有的 `view export`/`view import`，回滚是同一对命令反向跑。
两处实现期偏离值得留名，因为都不是设计时能推演出来的：

| # | 修订 | 取证依据 | 影响面 |
|---|---|---|---|
| **C8** | **dump 的便携根名 `/` 不是 FDB 里根键的存储名**：导入必须按 `(ROOT_PARENT, "")` 写根的名字键，并把 `('i',1)` 指向它（`root_entry_name`/`dir_entry_of`），而不是照抄 JSONL 里的 `/` | `src/fdb_store.rs:2718`（`root_entry_name`）与导入循环里 `('i',ino)` 的三分支（目录取 `dir_entry_of`、文件取 `first_holder`）；`test_fdb.sh` §12a 直接断言挂载根 `stat -c %i == 1` | 写错不是"根显示成 `/`"这种外观问题，而是**整棵树挂载后 getattr 全 ENOENT 而所有键都在**——最难归因的一类缺陷。§3.2 的 `'i` 指针语义因此补一条：指针值必须来自键空间自己的存储名，不得来自导出格式 |
| **C9** | **只读路径绝不开出结构**：`view export` 在 fdb 档以 `FdbStore::open(uri, view, read_only = true)` 打开，名字没有 subspace 时按名拒绝（`not a provisioned subspace`）且不落空 dump | `src/main.rs:2252-2265` 的只读探针分支；`test_fdb.sh` §12d、`test_migrate.sh` §8（拒绝后 `meta/views/<name>.layout` 也没写） | 若走普通 `open(...)`，`view create` 作为唯一建树入口（D1/决策表）就被一次导出悄悄破掉；该判据此后列入"任何新增只读 op 的固定检查项" |

另有一处**与键空间无关但必须留档的生命周期事实**（P8 期间修掉的真缺陷）：短命 CLI 进程在
FDB 网络线程尚未起来时就退出，会在 `fdb_shutdown_network` 之前析构顺序不定 → SIGSEGV。
修法是 `src/fdb_store.rs:195/246` 的静态 `NETWORK_GUARD`（`OnceLock<Mutex<Option<NetworkAutoStop>>>`）
+ `src/main.rs:1299` 在 flush 之后显式 `shutdown_network()`；判据是连续 100 次 `--meta-store fdb://…`
一次性命令 0 crash。教训：**"一 view 一 subspace"让每个 CLI 调用都成为一次独立的 FDB 客户端生命周期**，
网络启动/关闭的次序在库形态（服务端常驻）下不成问题，在 CLI 形态下就成了必守约束。

**双档取证（P8 三套，SUMMARY 原文）**：

| 套件 | SQLite 档 | FDB 档 | 迁移链的判据 |
|---|---|---|---|
| `test_fdb.sh` | —（本套件只在 fdb 上跑） | 169/0 | §12：SQLite→subspace→SQLite 全链，正向 `diff` 逐行相同、挂载根 `%i == 1`、12c 水位续接、12d 守卫（`--replace`/ghost/坏格式/悬挂父节点，且失败的 `--replace` 不伤已装树）、12e 回滚同 dump、12f 源存储未被写入；12a 另把挂载侧 `stat -c %i` 钉到导出侧的 `ino` 原值、12c 在属性 TTL 过期后重读同一 inode；13 节实测本档的驱逐语义 |
| `test_migrate.sh` | 74/0 | 71/0 | 同一套断言换后端：`--as-fork` 在此档按 subspace 边界拒绝（点名 flag、点名缺失、给出 alias/derived 替代、并证明拒绝未开 subspace 也未写定义槽）；一部署一对象存储 ⇒ **零字节搬运**；四个基 subspace 的边界断言 |
| `test_import.sh` | 35/0 | 47/0 | 顶层 `import` 在 `fdb://` 拒绝（含 `--dry-run` 之前即拒、`GROVEFS_META_STORE` 同拒、两处根都未落 db、源检查先于后端检查），并把拒绝点名的三步替代路线端到端跑通：暂存 `import` → `view export` → 交字节 → `view import` → 挂载保真 → 派生视图组合 → re-move 幂等 |

搬运**失去**的东西同样写进判据而不是藏起来：`import` 的零拷贝是"本地存储 + 同盘硬链接"的性质，
跨后端搬进集群后视图读的是部署自己的对象，两侧编辑互不可见——`test_import.sh` fdb 档 §3 对
两个方向各做一条负向断言。`MetaStore::backup_to_bytes` 的第三臂（`src/fdb_store.rs:1462-1467`）
如实报 unsupported 并指向 `fdbbackup`，不复用 PG 文案，`test_fdb.sh` 的 `--role standby` 段实证。

**台架教训（两条，都会让整套件假失败或污染下一套）**：① 双引号字符串里的反引号**会做命令替换**——
`ok` 的一句文案里用双引号包了反引号围起的 `view create`，于是断言执行时真的拉起了 `view`
（vim 只读模式）并阻塞等终端，把 fdb 档挂到超时；文案里的反引号要么转义、要么整句放进单引号
（`test_govcheck.sh:312` 同批修正）。② `set -u` 下
EXIT trap 引用未绑定变量会**中断 trap 本身**，实测泄漏 3 个挂载 + 2 个监听端口给下一个套件；
trap 内一律 `${VAR:-}`（`test_import.sh` 的 `cleanup` 已按此改写并注明后果）。


### v1.6（P8 收尾：把两条"写在文档里但没被测过"的判据坐实）

P8-d 复核 Spec 的 P2/P8 判据时发现两处口径只有实现与文档、没有断言，本轮补成可重跑的判据
（`fdb` 套件 158 → 163 → 169）：

| # | 补的判据 | 之前为什么不算过 | 取证 |
|---|---|---|---|
| **C10** | 迁移保真必须看**挂载侧的 inode 等于导出侧的 `ino` 原值**，而不是只看"硬链接两项相等" | 12a 原来只断言 `a.md` 与 `b.md` 的 `%i` 相等、12b 只 `diff` 两份 dump。重发一对编号同样满足"两项相等"，而 Spec P8 保留 id/ino 的理由恰恰是"客户端已缓存的句柄对得上"。12c 又补一条：属性 TTL（远程挂载 ≤2s）过期后重读同一 inode——这正是 P8 点名的 `.grove-stats` 页缓存 pitfall 的形状：缓存只会让"重发的编号"看起来没变 | `test_fdb.sh` §12a（`AINO`/`NINO` 取自源 dump 的 `"ino"` 字段，取不到就直接 `bad`，免得断言变成空转）与 §12c 的 `sleep 1.5` 后重读；SQLite 侧两处挂载本来就是 `--no-cache` |
| **C11** | 本档的驱逐语义要**实测**：挂载中不驱逐、卸载后回收、重开透明 | Spec P2 把"驱逐退化为纯内存 LRU + 会话门保留"写成裁决，USAGE §6.3/§11.10 也这么写；在补第 13 节之前，这条只有代码结构（`evict_idle` 与后端无关）作依据，没有 FDB 档的观测 | `test_fdb.sh` §13：独立一对服务器 + `--engine-idle-secs 3` + 自己的 scratch subspace（不与 1–12 节共享端口，`15196/15197`，开段前先查端口空闲，避免"对着陌生部署打印绿色数字"）；三条断言分别是挂载 8s 后日志 0 条 `evicted`、卸载后出现 `evicted 1 idle engine(s)`、重挂读回同一对象 |

顺带把 P7 的一条判据做实：`--help` 里 `view export`/`view import` 的说明此前只描述 SQLite 的
`meta/<view>.db`，而这两条命令在 `fdb://` 下同样可用（§14 v1.4 的那处口径事实更正）。现在
help 文本直接写明 fdb 档的行为——导出读的是该名字的 subspace、没被 provisioned 的名字按名拒绝且
不留空 dump；导入**开出**该 subspace（`view create` 之外唯一的建树入口）、`--as-fork` 在此拒绝。
判据是 `--help` 与 USAGE §6.3/§15.9 不再各说一套，且 `doccheck.sh` 的参数对账仍取 `EXIT=0`。


### v1.7（P8 之后的逐项完成审计：改的是口径，不是行为）

审计把 Spec 的每条判据与当前工作树的实况对了一遍。代码零改动，纠了三处"文档写的和板子跑出来的不是一回事"：

| 项 | 之前 | 现在（取证） |
|---|---|---|
| 全板阶段数 | USAGE 三处分别写 31 / 33 / 32，docs.html 又写 31 与 32，两处互相矛盾 | `run_all.sh` 的 `CANON` 是 **35 段 = 34 门控（build + unit + 32 套件）+ `bench`**（`record bench INFO`，永不门控）。本轮全板 `RESULT: GREEN`、`BOARD_EXIT=0`，BOARD 块恰好 34 行 PASS |
| 套件清单 | 两份用户文档的清单都漏 `evict`、`fdb`；`run_all.sh` 头注释的 "Suites (canonical order)" 漏 `layout`/`viewrm`/`bwlimit`/`evict`/`control`/`fdb` 六个套件的说明 | 文档清单补齐；头注释补写六段说明（各带"无 PG / 无集群自动 SKIP"的口径）。改后 `SKIP_BENCH=1 bash .build-bin/run_all.sh fdb` → `RESULT: GREEN`、`fdb 169/0`，以此证明只动了注释、执行面未坏 |
| 各套件断言数 | docs.html 的专项套件表写 layout 78 / fork 70 / import 34 / pg 19 / control 14，且整张表里没有 `fdb` 一行 | 一律换成实测值：layout 81、fork 75、import 35、pg 18、control 16，并登记 `fdb 169`；`migrate 74（FDB 档 71）`、`import 35（FDB 档 47）`双档并列 |

顺带把 P4/P5/P8 的"双档复跑"以当前树重测一遍，数字与 §14 v1.4/v1.5 记录逐项相符，没有一项靠记忆：`LOCKS_BACKEND=fdb` 16/0、`LAYOUT_BACKEND=fdb` 82/0、`GOVCHECK_BACKEND=fdb` 29/0、`GOVTIME_BACKEND=fdb` 10/0、`APIKEY_BACKEND=fdb` 34/0、`MIGRATE_BACKEND=fdb` 71/0、`IMPORT_BACKEND=fdb` 47/0。

另两条本轮重新取证的判据：`cargo test --release --features fdb` 0 警告、EXIT=0（单元测试 151 通过，含三臂分派的三条与 `fdb_tree_ops_match_sqlite_semantics` 差分对拍）；P0 spike 用当前二进制重跑 `--reads 100000 --pairs 10000` → `P0_SPIKE_VERDICT=PASS`，本轮 `retries=2330`——与上面记的 2705 一样都只是单次运行值，不是常量，G1 判据要的是"重试次数 > 0 且 `gave_up=0`"。集群侧 `fdbcli --exec "status minimal"` 重答 `The database is available.`。

审计里唯一一处**补了代码**的地方：P6 把台架写作"`Sqlite` vs `Pg` vs `Fdb` 三档同一 op 序列跑、比对 `dump_tree()`"，本文件 §4 也按"三后端对拍"立过口径，而 `tests/fdb_tree.rs` 当时只有两档（SQLite↔FDB）。于是加了 `all_reachable_backends_agree_on_one_op_sequence`：复用同一条 `script()` 与同一个 `canonical()`，把主机上**可达的每一档**都挂进同一张比对表，PG 档按 `.build-bin/test_pg.sh` 的口径探 `PGHOST:PGPORT`（默认 `127.0.0.1:5432`），跑完自己 `DROP SCHEMA`（库侧从不删 schema——`view rm` 明确拒绝 PG 部署，所以谁造的临时 schema 谁收）。取证：`PGHOST=127.0.0.1 PGPORT=5433 cargo test --release --features fdb --test fdb_tree -- --nocapture` → `three-way differential: 3 arms [sqlite, postgres, foundationdb] agree on 49 ops and 7 rows`，两测 2 passed / 0 failed；`cargo test --release`（不带 `fdb`）与 `--features fdb` 两态均 0 警告，事后查库确认那个临时 schema 已不存在（`grovefs_v_6477%` 计数 0）。

最后一轮全板认证跑出过一次 `RESULT: RED`，失败的是 `locks` 套件第 3 节（`pass=15 fail=2`：
`holder never acquired` + `overlap expected refused`），而同一套件空载连跑三次都是 17/0——
判据本身没红，红的是台架：脚本给持锁进程 2s 报"已拿到锁"，而持锁进程只活固定的 3s，
机器一忙，2s 预算先到期，两条断言就打在已经释放的锁上（无冲突请求当然放行）。
修法是拆掉这层时间耦合而不是把常数调大：持锁进程改为**一直持到脚本撤掉标记文件**
（另有 120s 上限兜住挂载卡死，而"活持锁者越过一个 3s 租约窗口仍存活"本就是第 4b 节断言过的
受支持状态），就绪等待放宽到 ~10s 且**把实测耗时打进日志**，失败时带上持锁进程自己的输出。
断言条数与判据不变（SQLite 17/0、`LOCKS_BACKEND=fdb` 16/0 各复跑通过），改后的全板重新 GREEN。

### v1.8（G3 性能门：把「本环境不具判定资格」换成有数字的判定）

v1.7 之前 G3 记的是「未判定」。这一轮把它判了，因为 Spec 的门槛本身就是可执行的：
阈值两档（SATA SSD 三副本 ≥ **4.6k ops/s**，定义为「不劣于 PG 平台值」；NVMe 三副本 ≥ 9k），
而那个平台值的出处就是 `.build-bin/bench_pg_conc.sh`——所以 op 序列不需要另造，照抄它：
N 个挂载 × T=8 线程、barrier 同起、distinct-parent 的 `mkdir` + `O_EXCL` 零字节文件
（纯 namespace op，数据面完全不参与），聚合口径也用脚本自己的 `Σops / (max end − min start)`。
两臂同机、同一次会话、同一 op 序列交替跑，`REPS=2`，配方是 `.bneck/bench_g3.sh`。

**介质前提先说清**：本机 `sda`/`sdb` 都挂在 LSI MR9361-8i 后面，`queue/rotational=1`——
没有 SATA SSD，也没有 NVMe，Spec 的两档阈值在字面条件上不可复现。三台 VM
（gfs1/gfs2/gfs-client）在线，但跑着**别人的** grovefs 集群（root 的 grovefs 进程持有
`/data/gfs*` 下的打开文件、50100 在听），且 gfs1/gfs2 共享同两块 30G 数据盘——不动他人资源。
于是判定按**同场对照**做：同机、同 op 序列、同一个 `--features fdb` 二进制起的两个
meta-server（一臂 `postgres://`、一臂 `fdb://`），这恰好是「不劣于 PG 平台值」的原义。

**拓扑**：`FDB3_ENGINE=ssd FDB3_RF=3 bash .bneck/fdb3_up.sh`（FDB 7.1.67 用户态三进程，
`configure new triple ssd`，端口 4510–4512）。集群侧健康逐项目测：
`redundancy_mode=triple`、`storage_engine=ssd-2`、`database_available=True`、
`full_replication=True`、3 storage + 3 log + 3 coordinator、`machine_ids=[m1,m2,m3]`、
`fault_domains=[n1,n2,n3]`、`zone_failures_without_loss_of_data=1`。

**实测矩阵**（`RUN_TAG=ssd1794267489`、`T=8 RUN_SECS=8 REPS=2`，原始行落盘
`.bneck/g3_ssd_round.log`；agg 单位 ops/s）：

| 单元 | PG 臂 | FDB 臂（`ssd` 三副本） |
|---|---|---|
| rep1 N=1 | 1992 | 312 |
| rep1 N=4 | **4709** | 417 |
| rep1 N=8 | 4099 | 681 |
| rep1 N=16 | 426 | 998 |
| rep2 N=1 | 461 | 133 |
| rep2 N=4 | 4167 | 360 |
| rep2 N=8 | 3121 | 563 |
| rep2 N=16 | 1073 | **1601** |

同拓扑的其余轮次（终端取证；`.g3bench/` 每次运行开头 `rm -rf`，只保住了最后两轮的原始日志）：
ssd RF=3 更早三轮的 N=16 顶点 1200 / 765 / 893，同场 PG 3669–4862；
RF=1 同盘一轮 503–1289；`FDB3_ENGINE=memory` RF=3（把盘整个移出提交路径）367/618/1359/**1642**，
同场 PG 951–4912。PG 臂在 N=4/N=8 稳定复现 Spec 的平台值（4.1k–4.9k），
N=16 反而塌（128 个压线程 × 16 挂载把连接池打死）——门槛取 PG 的最佳档，对 FDB 只会更严。

**判定：G3 未通过。** FDB 臂 24 个单元里没有一条接近 4.6k：本轮最好 1601 ops/s（门槛的 35%），
此前顶到 1200；而同一场 PG 臂跑到 4709。差 3–6 倍，且两轮之间 N=16 单元的散布（998 vs 1601）
比两臂之间的差距小一个量级——结论不是抖动造成的。

**归因**（`.bneck/g3_ssd_status.jsonl`，跑动中每 1s 采一份 `status json`，共 80 份）：

- **不是 OCC**：`workload.transactions.conflicted.hz` 峰值 8.4/s，对 committed 1931/s 是 0.4%；
- **不是配额**：`qos.transactions_per_second_limit` 7.0 万–104 万/s，committed 峰值 1931/s，差两个数量级；
- **不是键空间粒度病态**：committed tx/s ≈ ops/s 的 1.0–1.2 倍，即一个 namespace op 一个提交事务，
  没有隐藏的多事务展开（§5 的 D3 粒度决策在此得到正面证据）；
- **直接压力在提交耐久**：`qos.limiting_durability_lag_storage_server.seconds` 稳态 5.0–8.8s、
  峰段 18.7s；`latency_probe.commit_seconds` 3–200ms、偶发 400–600ms，而同场
  `read_seconds` 0.1–0.5ms——读不慢，慢在把 mutate 变 durable。盘就是这个引擎的 fsync 落点；
- **但介质不是全部**：`memory` 引擎（盘已移出提交路径）同拓扑只到 1642 ops/s，与 ssd 轮的 1601
  同一档 → 剩下约 3× 的差距不在盘上，在单实例提交路径/实现侧，本轮**未能归因**，
  它是 SSD 重跑之外的独立待办（重跑若只换盘而不改进，预期收益到 1.6k 就封顶）；
- 一条未解释的观测：`started` 与 `committed` 的稳态比 ≈ 3:1（5962 vs 1931），conflicted 补不上
  差额；同时 FDB 臂每个单元的墙钟是 11.5–13.5s（时间盒 8s），说明尾部有长时间未返回的 op。

**已执行的处置（Spec R3 的字面三条，逐条落）**：

1. **不入 CANON**：`run_all.sh` 的 `CANON` 去掉 `fdb`，新增 `EXPERIMENTAL="fdb"`，按名可选。
   改的时候抓出台架一个真缺陷：分派循环原来只走 `$CANON`，所以 `run_all.sh fdb` **其实从没跑过
   套件**，只在 BOARD 里打印 `${RESULT[fdb]:-SKIP}` 的缺省值——「按名可跑」当时是假陈述。
   循环改为 `$CANON $EXPERIMENTAL` 后取证：`bash .build-bin/run_all.sh fdb` →
   `### fdb … OK :: SUMMARY: pass=169 fail=0 SKIP=0`、BOARD `fdb PASS`；
   默认板 `SKIP_BENCH=1 bash .build-bin/run_all.sh` → 33 段全 PASS、`RESULT: GREEN`，
   且日志里一次都不出现 `fdb`（R5 的「无 FDB 环境必须 GREEN」不受影响）。
2. **不入文档承诺**：`USAGE.md` §6.3 标题改「实验性，不构成支持声明」并加实测状态块；
   §11.10 由「FDB 后端部署」降为「FDB 实验档复跑配方（不是部署承诺）」；§12.1 巡检去掉
   `fdbcli` 这条默认巡检项；段数口径 35→**34 段**、34→**33 门控**、32→**31 套件**（三处）。
   `docs.html` 同步：badge(440)、featuremap(459)、§5.9 标题 + `div.warn` 实测块(836/843)、
   §16 板面与套件表(2964/2968/2971/2976)。`src/main.rs` 的 `--meta-store` help 加
   「EXPERIMENTAL, kept out of the supported surface because the throughput gate measured fail」。
   `cargo build --release` EXIT=0、`doccheck.sh` EXIT=0。拒绝原文、能力边界表（§15.9）与
   169 条断言**一条不删**——R3 撤的是承诺，不是证据。
   同一节里还翻出一条自己写的口径矛盾：§6.3 开头已经给出 G3 实测吞吐数字，末尾旧条目却还写着
   「只在副本因子 1 上跑过、不给出任何吞吐/延迟数字」。按 R2 的本意改掉：数字的来源是 RF=3 同场对照，
   回答的是「相对 PG 臂差多少」；**未测**的是副本失效/恢复与跨故障域拓扑（本机无自有 VM 集群），
   这一条仍按 R2 不入文档。改后 `doccheck.sh` EXIT=0 复证。
3. **对外口径回到「SQLite/PG + 分片」**：受支持的元数据后端仍是嵌入式 SQLite 与 PostgreSQL；
   元数据面写并行只在单个 PG 集群内，跨集群分区元数据权威不在本轮承诺面。

**一处没按我自己写过的处置做，理由入档**：§15 的 G3 行原先记的第三种动作是「`fdb://` 分派处
返回『实验未就绪』错误串」。不执行。R3 的原文是「停在 feature 关闭的实验分支」，而 feature
本来默认就关（不带 `--features fdb` 的构建收到 `fdb://` 已按名拒绝、绝不降级成 SQLite，P2 实证）；
再在分派处返回一个假错误，等于把 P3–P8 的全部证据变成不可复跑，也把「实验分支仍在」这个
R3 要求的状态本身抹掉。判定后的正确状态是：能编译、能跑、按名可测，不门控、不承诺。

**转正条件**（下一轮判定配方）：真 SATA SSD 或 NVMe 介质 + RF=3，
`FDB3_ENGINE=ssd FDB3_RF=3 bash .bneck/fdb3_up.sh`，然后
`N_LIST="1 4 8 16" T=8 RUN_SECS=8 REPS=2 bash .bneck/bench_g3.sh`；
判据：FDB 臂 ≥ 同场 PG 平台值 4.6k（SATA 档）/ ≥ 9k（NVMe 档）。
过线才把 `fdb` 放回 `run_all.sh` 的 CANON，并把 §6.3/§11.10 与 docs.html 的口径改回第三受支持后端。
在此之前，任何「FDB 支持」的表述都算口径漂移（对应 R7 的同一条纪律）。

**复跑必用的台架知识**（每条都是本轮踩出来的）：

- 连接串必须逐条写全 `ip:port`（`127.0.0.1:4510,…`）；简写会得到 `2104 Connection string invalid`；
- `configure new triple` 的招募要求跨 fault domain：只给 `--locality-machineid` 会得到
  `RecruitTLogMatchingSetNotAvailable … Not enough physical servers available (1008)`，集群永不恢复；
  `--locality-zoneid` 与 `--locality-machineid` 都要给（`fdb3_up.sh` 已写死）；
- 新建集群 `configure new …` 之前永不答 `available`，等待次序必须是 joined → configure → available；
- `ssd` 引擎不能在 tmpfs 上跑：`processId` 文件创建即 `Disk i/o operation failed`（KAIO/O_DIRECT），
  所以「去盘对照」只能靠 `FDB3_ENGINE=memory`；
- 已存在的集群改存储引擎要 `perpetual_storage_wiggle=1 storage_migration_type=gradual|aggressive`，
  直接 `configure ssd` 会被拒；本轮实验数据只在 `fb_*` subspace 里，重建集群更便宜；
- 视图名每轮唯一（`RUN_TAG`）：复用上轮已填充的视图会让 `O_EXCL` 变成 EEXIST 风暴，
  报出 ~0 ops/s 的假数据（本轮第一版就中过，`bench_g3.sh` 头注释已写明）；
- **查共享账本不用写代码**：`FDB_CLUSTER_FILE=<cluster> fdbcli --exec "getrange \x01grovefs\x00\x02refs\x00 \x01grovefs\x00\x03 5000"`
  直接列 `(root,'refs')` 全部计数（键是 tuple 编码，`\x01`=BYTES、`\x02`=UTF-8 串、值 `\x15\xNN` = int64 `NN`；
  `\x15\x1d` 就是 29），读写都要 `writemode on;` 打头，`writewatch` 在 7.1.67 的 fdbcli 里不存在。
  差分对拍 red 时先查这张表，再查实现——§14 v2.1 那条 `op #28` 就是这么定位的；
- **验证「是否需要 setuid 助手」不能用 `strace`，也不能靠破 `PATH`**：ptrace 会让 setuid 程序丢权，
  `fusermount` 因此报出一个与真因无关的 `EPERM`；而 `fuser` 的 `detect_fusermount_bin()` 除了 PATH
  还试 `/bin/fusermount` 等绝对候选，所以 `PATH=/nonexistent` 也拦不住它（v2.2 第一轮就被这样“证明”了
  一个假结论）。可用的开关是 `FUSERMOUNT_PATH=/nonexistent`（它自己的逃生门），配合一个必须有过的
  对照组：对照组挂不上，探活组的失败就没有意义；
- **没有 root 也能测「有 `CAP_SYS_ADMIN` 会怎样」**：`unshare -r -m --propagation private bash -c …` 给出
  ns 内 uid 0 + 全 capability，足够走到需要 `CAP_SYS_ADMIN` 的那个 `mount(2)` 分支，而不必碰宿主权限。
  读数时要把它标成“namespace 里的 root”：挂载只对该 ns 可见，不能当成宿主真 root 的等价复现；
- feature 构建的二进制：**运行期不再需要 `LD_LIBRARY_PATH`**（v1.9 起 `build.rs` 把
  `$ORIGIN/lib`、`$ORIGIN/../lib` 与构建期的 `FDB_CLIENT_LIB_PATH` 写进 RUNPATH）；
  `FDB_CLIENT_LIB_PATH` 本身仍是**构建期**要给的链接搜索路径。无论哪样都**不要**
  `eval "$(… vars)"` 全量导出——它连带导出 `GROVEFS_META_STORE`，会把其它套件的默认后端改掉；
- **`run_all.sh` 的按名选择在 `fdb` 出 CANON 后本来是坏的**：分派循环写的是 `for s in $CANON`，
  实验档只进了 `EXPERIMENTAL` 与名校验 `case`，于是 `run_all.sh fdb` 匹配到名字、进不了循环，
  一句套件命令都没执行，却照样打印 `SKIP`（那是 `${RESULT[$s]:-SKIP}` 的缺省值，不是套件的结论）。
  这条 P6 判据「按名单独可跑」被我写进 §6.3 与 v1.8 的处置列时只有代码可证、没有执行可证，
  改成 `for s in $CANON $EXPERIMENTAL`（未选中仍不 `record()`，所以不门控）后实测
  `### fdb … SUMMARY: pass=169 fail=0 SKIP=0` 才算成立。可复跑性只能由一次真实执行背书；
- **全板跑动期间不得并发做基础设施变更**：本轮在 `run_all` 运行中并发做了三件事——
  `psql … DROP CASCADE` 清理 24 个 bench schema、`cp`+`gzip` 归档 116 份 per-mount 日志、
  拆 RF=3 实验集群——结果 `ha` 报 27/6，红的六条全部集中在 §2–5 的 replication/failover（时间敏感段）。
  单独复跑 `test_ha.sh` 两次 33/0，不并发重跑全板 `EXIT=0 / RESULT: GREEN / 33 PASS`。
  这不是回归，但它差点被写成一条「实测未通过」，代价是一次全板复跑（`ha` 之外没有第二条红）。

**完成审计的复测数字**（都在判定后的当前树上重跑，不是引用早期阶段的历史值）：
`bash .bneck/g3_tristate.sh` 给出 plain / `--no-default-features --features remote-only` /
`--features fdb` 三态构建各 0 警告（约束 1 的「三态皆零警告」）；`cargo test --release` 136 passed，
`--features fdb` 151 passed，其中含三臂差分（P3/P6 的「0 diff」判据）：
`3 arms [sqlite, postgres, foundationdb] agree on 49 ops and 7 rows`；
`LOCKS_BACKEND=fdb bash .build-bin/test_locks.sh` 16/0（P4 的参数化复跑判据）；
七套参数化套件也在当前树上用 feature 二进制重跑了一轮 fdb 档（`bash .bneck/audit_fdb_tiers.sh`，
全部 EXIT=0）：locks 16/0、layout 82/0、govcheck 29/0、govtime 10/0、apikey 34/0、
migrate 71/0、import 47/0——跑完立即重建默认（feature-off）二进制，`target/release/grovefs` 不留依赖 `libfdb_c` 的状态。
`doccheck.sh` EXIT=0 且 `fdb://` 在 USAGE.md/docs.html/src/main.rs 三处齐备（26/19/23 处）；
`git diff src/remote/proto.rs` 为空（约束 1 的禁止改动项）。G1 的三条判据也在审计当时逐条重跑：
`fdbcli --exec 'status minimal'` 回 `The database is available.`；
`cargo build --release --no-default-features --features remote-only` EXIT=0 且 0 警告；
`./target/release/examples/fdb_spike` 打 `conflicts: aborts=2418 retries=2418 gave_up=0 total=6400`
与 `P0_SPIKE_VERDICT=PASS`（重试确实发生、没有一次放弃）。R5 的两半也重跑：
以不存在的集群文件跑 `test_fdb.sh` → `SUMMARY: pass=0 fail=0 SKIP=1` 且 EXIT=0，
同一条件加 `FDB_TEST=1` → `SUMMARY: pass=0 fail=1 SKIP=0` 且 EXIT=1。实验集群与 24 个 bench schema 已拆除，
4510–4513 端口全 free、4500 测试集群按预期存活、0 处遗留挂载，gfs1/gfs2/gfs-client 只做只读探针。
全板在审计当时重跑了一遍不并发的完整一轮（`bash .bneck/audit_board.sh`，日志 `.bneck/audit_board.log`）：
默认板 `PGPORT=5433 bash .build-bin/run_all.sh` → 33 段 PASS + `bench` INFO、`RESULT: GREEN`、`EXIT=0`；
紧跟一条按名选档 `PGPORT=5433 bash .build-bin/run_all.sh fdb` → `### fdb … SUMMARY: pass=169 fail=0 SKIP=0`、
BOARD `fdb PASS`、`EXIT=0`。这两条一起才是 P6 的两半判据（按名可跑 + 全板不被拖 RED）。

板面口径的一处连带更正（不是新事实，是随 1. 的必然结果）：v1.7 记录的「CANON 是 35 段 =
34 门控 + bench」是 G3 判定**之前**的板面；v1.8 之后是 34 段 = 33 门控 + bench，`fdb` 在
EXPERIMENTAL 里，v1.7 的表格数字保留原样作为历史快照。

### v1.9（依赖自足化：这一臂到底要不要别的安装包）

触发是一条复核问题：「fdb 是否已经完备，我希望它不依赖其他安装包」。把「依赖」拆成运行期与
构建期两面逐条查实，并修掉运行期那一处真实缺口：

| 面 | 查实结果 | 证据 |
|---|---|---|
| 运行期—FDB 制品自身 | 四个官方制品（`libfdb_c` / `fdbserver` / `fdbcli` / `fdbmonitor`）的 `ldd` 只落在 glibc 核心（rt/dl/m/pthread/c）——官方构建已把 OpenSSL/Boost/curl 静态链进去，**不需要任何系统包** | 本轮 `ldd /data/grovefs/.fdbbin/*` |
| 运行期—grovefs 的 feature 二进制 | **原有一处不自足**：`foundationdb-sys` 只给构建期 `cargo:rustc-link-search`、不写 rpath，所以产物 `NEEDED libfdb_c.so` 而无 RUNPATH，空环境下起直接 `error while loading shared libraries: libfdb_c.so` | 修复前的 `readelf -d`；`ldconfig -p` 无 fdb_c 条目 |
| 修复 | 新增 `build.rs`：仅在 `CARGO_FEATURE_FDB` 下注入 `-Wl,-rpath,$ORIGIN/lib`、`$ORIGIN/../lib` 与构建期的 `FDB_CLIENT_LIB_PATH`；默认与 `remote-only` 构建走 early return，链接面一字不变 | feature 二进制 RUNPATH = `$ORIGIN/lib:$ORIGIN/../lib:/data/grovefs/.fdbbin/lib`；`target/release/grovefs` 既无 RUNPATH 也无 `libfdb_c` NEEDED（本轮实测） |
| 自足性验证 | 把 `fdb_spike` 与 `lib/libfdb_c.so` 放进一个空目录，`env -i`（无任何环境变量）下 `ldd` 解析到目录内的那份 `.so`，spike 打 `P0_SPIKE_VERDICT=PASS`，`grovefs view --meta-store fdb://… create` 正常开 subspace | 本轮 `/tmp/sc` 实跑 |
| 构建期—当时剩下的唯一外部依赖 | `foundationdb-sys 0.11.0` 的 build.rs 无条件 `extern crate bindgen;`（无 feature/env 逃生门），所以 `--features fdb` 需要 **libclang**；本机是 `/usr/lib/llvm-10/lib/libclang.so`。默认构建与 `remote-only` 构建的依赖图里 0 处 bindgen，**受支持的档位不受影响**——处置见 v2.0 | `cargo tree --features fdb` 的 `[build-dependencies] bindgen`；版本普查 0.8.0/0.9.0/0.9.1/0.10.0/0.11.0（`rsproxy.cn` 取包后逐个看 build.rs）全部如一，且 0.11.0 就是最新发布版 |

判据的连带更新：`test_fdb.sh` 新增 §0b 一条断言（空环境下 `ldd` 必须解析到 `libfdb_c.so`），
`fdb` 套件 **169 → 170 条**；两份用户文档的断言数与 §6.3/§11.10、docs.html §5.9 的口径同步改为
「运行期自解析、当时构建期仍需 libclang」（后半句到 v2.0 再改一次）。改后复跑（不并发任何清理，v1.8 的那条纪律）：
三态构建各 0 警告（plain / `--no-default-features --features remote-only` / `--features fdb`），
默认板 `PGPORT=5433 SKIP_BENCH=1 bash .build-bin/run_all.sh` → 33 段 PASS、`RESULT: GREEN`、EXIT=0，
按名 `PGPORT=5433 SKIP_BENCH=1 bash .build-bin/run_all.sh fdb` → `SUMMARY: pass=170 fail=0 SKIP=0`、
BOARD `fdb PASS`、EXIT=0。写这条断言时先犯了 `grep -q 'libfdb_c => /'` 这种漏看 `.so` 的错，
第一次跑板子报 169/1 FAIL——那次假失败暴露的是自己的探针写错，不是 rpath 没生效；
改匹配式后 170/0（同一个坑的镜像面：`ldd` 的行里是 `libfdb_c.so => /path`）。

对「完备」的回答口径：功能面 P0–P8 的判据全绿且可复跑，运行面已做到零安装包依赖；
但**这一臂仍不是受支持的第三个后端**（G3 未过、R3 处置生效），所以「完备」只到「实验档完备」为止。
构建期那一个 libclang 的处置在同一天里的 v2.0 里做完（vendor 预生成绑定），本小节的表格行保留
当时的查实原文，因为那是这个决策链的入口事实。

### v2.0（同一轮的第二半：把构建期最后那个安装包也去掉）

v1.9 把问题定位到一句上：绑定是构建期现生成的，所以要装 libclang。去掉它的三条路逐条查实：

| 路 | 查实 | 结论 |
|---|---|---|
| 换用带预生成绑定的 `foundationdb` 版本 | 从 `rsproxy.cn` 拉下 `foundationdb-sys` 0.8.0 / 0.9.0 / 0.9.1 / 0.10.0（连已有的 0.11.0）逐个读：五个版本的 build.rs **都无条件跑 bindgen**，而索引里 0.11.0 就是最新版 | **不可行**（无目标版本） |
| 只写文档、把 libclang 当实验档的构建前置 | 零风险，但用户的要求是「不依赖其他安装包」 | 不够 |
| vendor 一份预生成绑定的 `foundationdb-sys` | `src/lib.rs` 逐字节相同（sha256 `c0033f5b…`），`bindings/api710.rs` 就是 bindgen 从同一份 `include/710/fdb_c.h` 产出的那个文件（sha256 `53ae6dd5…`，只存不编），`build.rs` 保留上游的链接逻辑只把「生成」换成「安装」 | **采纳** |

落地与边界：

- 新增 `vendor/foundationdb-sys/`（4 个文件 + `VENDOR.md` 记录来源、`.crate` 的 sha256
  `0bac3aa4…`、MIT/Apache-2.0 与作者名单、刷新步骤、回退方式）；上游的 `include/`（680 KB
  十个 API 级别的 C 头）不拷——没有 bindgen 就没人读它。
- 顶层 `Cargo.toml` 加 `[patch.crates-io] foundationdb-sys = { path = "vendor/foundationdb-sys" }`。
  两条边界写进了注释：`[patch]` 只在工作区根是本清单的构建生效（grovefs-client 那两边不启 `fdb`，
  所以无需也不应生效）；绑定钉死 API 710，feature 选了别的版本时 vendor 的 build.rs 直接
  `compile_error!`，而不是链一批对不上的符号。
- 生效判据（不靠叙述，靠可重跑的命令）：`cargo tree --features fdb | grep -ciE 'bindgen|clang'` →
  **0**；构建日志里不再出现 `Compiling bindgen`；`cargo metadata` 在本包与 grovefs-client 两侧都
  EXIT=0 且无补丁告警。运行面不变：feature 二进制仍有 `$ORIGIN`-相对 RUNPATH。

一句话总结两面：**运行期不装包（v1.9），构建期也不装包（v2.0）**；剩下需要人提供的只有
一个 `libfdb_c.so` 所在的目录（`FDB_CLIENT_LIB_PATH`，由 `.build-bin/fdb_env.sh fetch` 拉官方制品）。

### v2.1（v2.0 的账单：差分对拍在 `op #28` 变红，归因到共享账本而不是实现）

改完 vendor 后跑 `cargo test --release --features fdb`，两个差分测试在**同一行**稳定失败：

```
assertion `left == right` failed: op #28 diverged (sqlite vs foundationdb)
  left:  "rename d1/f3 -> /f0 (overwrite) = ok(gc K0)"
 right:  "rename d1/f3 -> /f0 (overwrite) = ok"
```

这条正好落在 P3/P6/G4 的「0 diff」判据面上，所以必须先归因、再处置，不能沉默通过。四步查实：

| 步 | 做法 | 结果 |
|---|---|---|
| 排除 v2.0 | 临时摘掉 `[patch.crates-io]`、走上游 bindgen 路径重编（日志有 `Compiling bindgen v0.72.1`/`Compiling foundationdb-sys v0.11.0`）再跑 | **同一处 `op #28` 同样失败** ⇒ vendor、`build.rs`、RUNPATH 都不是诱因 |
| 定位机制 | `#28` 是脚本里第一次「覆盖改名并指望被顶掉的内容键释放」的观测点：目标 `/f0` 持 `K0`、源 `d1/f3` 持 `K2`，两个臂只差在 `(root,'refs')` 里 `K0` 那条计数能否归零（`drop_name_tx` 的 `left <= 0` 才 `clear` 并上报，`src/fdb_store.rs:1191-1217`） | SQLite 臂的账本在 tempdir 里、每轮全新；FDB/PG 臂的账本是**集群级**的，于是依赖全局状态 |
| 量泄漏 | `fdbcli --exec "getrange \x01grovefs\x00\x02refs\x00\x02K2\x00 …"` 读值：`\x15\x19`(25) → 跑一轮两个测试 → `\x15\x1b`(27) → 再跑 → `\x15\x1d`(29)；PG 侧 `grovefs_refs.holders` 里 `K2|4`、`KX|4`，整表 `count=807814, sum(holders)=807820` | harness 自己的拆视图在漏：脚本结束时 `/f0`(K2) 与 `/x`(KX) 还活着，`clear_view`/`DROP SCHEMA CASCADE` 删名不销账 ⇒ **每次测试执行 +1**（一轮 `cargo test` +2）。旧注释「上面的 unlink 已经把这些行释放掉了，所以没有残留」是错的——`ok(gc Kx)` 只证明脚本中段的释放，不证明收尾这两个名字 |
| 正向复现 | 手工注入一条恰好是该形状的残留：`fdbcli --exec "writemode on; set \x01grovefs\x00\x02refs\x00\x02K0\x00 \x15\x01"`（即 `ledger(K0)=1`）后跑测试 | **两个测试都以一字不差的 `op #28` 失败**；`clear` 掉该键后立刻转绿。且该残留自持：本轮 create `+1`→ 2、收尾 `−1`→ 1，行不清零 ⇒ 下一轮继续失败。这解释了为什么失败点恒在 #28、而「哪个测试先抓到」在漂 |

处置（**只改 harness，不动产品代码**，因为差分本身没有暴露 FDB 臂的语义缺陷）：

- `script()` 增加 `tag` 参数，五个内容键改为 `K0.<view>` 形状——每轮的视图名唯一，所以任何历史残留都撞不上；
  同一次运行的三个臂共用同一个 tag，transcript 仍逐行可比。
- 新增 `release_ledger()`：拆视图/删 schema **之前**按 `dump_tree()` 先把文件名 unlink 掉（文件一轮、目录逐层六轮），
  因为 unlink 是唯一会销账的路径；收尾断言 `dump_tree().len() <= 1`，否则直接失败并报出剩几行——泄漏再也关不掉。
- 改掉那条说过头的话（v2.1 表第四行的错判正是从它来的），并把机制写进函数注释与 §12 第 5 条。

复跑数字（一次真实执行背书，全部本轮实测）：连跑两轮 `cargo test --release --features fdb --target-dir target/fdb --test fdb_tree`
→ 两轮 `2 passed; 0 failed`；`K2` 稳定在 29（增长止住）；FDB 的 `(root,'refs')` 里带 tag 的计数 **0 条**、
PG 的 `grovefs_refs.holders` 里 `data_ref like 'K%.d%'` **0 行**（新配方不再留残留）；再把 `ledger(K0)=1` 那颗毒丸注回去
→ 仍 `2 passed`（对历史免疫），随后清除（读回 0 条）；全套 `cargo test --release --features fdb` → **223 passed / 0 failed**
（含 151 条单测与两条差分对拍）。集群侧那份历史残留（`K2`=29、PG 80 万行）不动：它是 §12 第 5 条那个保守多计的
既有状态，新 harness 已经不读那些固定键，删它反而抹掉这条证据。

### v2.2（诉求换了一种交付形态：把「不依赖其他安装包」做成一个可验收的投放包）

v2.0 让**构建**不再需要包管理器，v1.x 让**运行期**不再需要 `ldconfig`——但这两条都是关于源码树的断言，
要验证就得先有 Rust 工具链。用户这一轮的诉求是「fdb 是否已经完备，我希望它不依赖其他安装包」，
所以完备性在这里被拆成三面来回答，第三面此前没有实体：

| 面 | 判据 | 此前状态 | 本轮 |
|---|---|---|---|
| 构建期 | `cargo tree --features fdb \| grep -ciE 'bindgen\|clang'` → 0；不再需要 libclang | v2.0 已做（vendor 预生成绑定） | 复核仍为 0 |
| 运行期（有工具链的机器） | `env -i PATH=/usr/bin:/bin ldd ./grovefs` 把 `libfdb_c.so` 解析到包内 | v1.x 的 RUNPATH 已保证 | 复核通过 |
| 部署期（**只有二进制**的机器） | 解包 → 起服务 → 挂载 → 读写，全程不 `apt`、不 `cargo`、不 root | **缺**：`dist/` 里只有 v1.0（SQLite/PG 档，不含 fdb feature） | 本轮补 `dist/grovefs-fdb-experimental-linux-x86_64/` |

包内七件：`grovefs`（`--features fdb` 已编入、剥符号表，当时 6,625,104 B；v2.3 在 clean tree 重编后为
6,623,152 B）、`lib/libfdb_c.so`（21,027,376 B）、
`fdb/{fdbserver,fdbcli,fdbmonitor}`（官方 7.1.67，逐件 sha256 与 `.fdbbin` 里原件一致）、`README.md`、
`BUILDINFO.txt`、`SHA256SUMS.txt`。打包件 `grovefs-fdb-experimental-linux-x86_64.tar.gz`
60,052,495 B，`sha256 = 66cf9aabddcadc8524dff957aa258dc170ae5e80055e3aa83aab0fdb6eee72cf`
（指纹写在包**外**的 `.tar.gz.sha256` 里；包内那两份文件不可能固定自己的外层摘要）。上面是**当前在盘上的那一件**：
v2.2 写下时它是 60,052,472 B / `cd8737f6…`（那已是当日第三次打包），之后又重打两次：第四次只改包内两份文本、
第五次连二进制也换了，逐次缘由见 v2.3。

一次真实执行背书（`.bneck/pkg_final.sh` 与 `.bneck/pkg_verify.sh`，本轮实跑）：

- 把 tar 包解到 `/tmp/pkgfinal` 这种全新位置，`sha256sum -c SHA256SUMS.txt` 七件全「成功」；
  `env -i PATH=/usr/bin:/bin ldd ./grovefs` → `libfdb_c.so => /tmp/pkgfinal/…/lib/libfdb_c.so`，其余全是 glibc 核心；
  对构建机那个 `/data/grovefs/.fdbbin` 路径（RUNPATH 第三项）的命中数 **0**——即空环境里根本没有回落到源机器的余地。
- 全家进程 `env -i` 起：`data-server` + `meta-server --meta-store fdb://…`（横幅打出 `metadata backend foundationdb`，
  这条是防静默降级的 fail-fast 门）→ `view create`/`view list` → `mount` 起来 `fstype=fuse` →
  写/读/mkdir 穿越挂载点 → 用**包内** `fdb/fdbcli` 直读 subspace 看见 `'d'`/`'h'` 键 → 卸载关停 → 探针视图 subspace 清回 0 键。

顺带查到的两处**文档说法与实现不符**（投放包一侧已按实测改写；v1.0 一侧也已修正，见下）：

1. `mount` 没有 `--meta-store` 参数——`--meta-store` 是 `view` 的组级参数（必须写在子命令名之前），
   拓扑是 meta-server 指 `fdb://`、客户端只认 `--meta-server`（配方见 `.build-bin/test_fdb.sh:184-203`）。
2. 「挂载完全不需要发行版的包」这句话是**说过头了**——本轮差点写进包里，靠一组对照实验才拦住：
   `libfuse` 的**库**确实不链（`ldd` 里没有，也不需要开发包），但 `fuser` 0.18.0 在非 root 下的挂载
   实际走的是发行版的 **setuid 助手** `fusermount`/`fusermount3`（`fuse`/`fuse3` 包里的二进制，
   不可能随 tar 包投放——它需要 setuid root）。这条在 §11.10/§1 的既有文案里本来就说对了
   （「运行时只需 libfuse 的用户态工具，不需要 libfuse 开发包」），是投放包这一侧新写的句子偏了口径。
   反向的错也在 v1.0 的 BUILDINFO 里（「依赖本机 libfuse3」）：它**既不链 libfuse**（实物 `DT_NEEDED`
   只有 glibc 五件）**又漏了真正需要的 setuid 助手**，已在 `dist/v1.0` 分支上补一个提交修正（只改 BUILDINFO
   与 SHA256SUMS 里那一行，**二进制与 tar 包字节不变**，`sha256sum -c` 五项全过）；已 push（`a7f7fbc..23f185d`）。

测量过程本身留下两条台架知识（都已写进包 README）：

- **`PATH=/nonexistent` 不是「藏掉 fusermount」的有效手法**：`fuser` 的 `detect_fusermount_bin()`
  除了 PATH 里的名字，还会试绝对候选 `/bin/fusermount`、`/sbin/fusermount3`，所以破 PATH 完全拦不住它
  （本轮第一次探活就是这么“验证成功”了一个假结论）。干净开关是它自己的逃生门 **`FUSERMOUNT_PATH=/nonexistent`**。
- **`strace` 不能抓这一步**：ptrace 会让 setuid 程序丢权，`fusermount` 因此返 `EPERM`——这看起来完全像
  「非 root 挂载坏了」，实为观测手段本身所致。可信的读数只有三件：`mount(2)` 返 EPERM（说明确实回退了）、
  `execve("/usr/bin/fusermount", ["fusermount","-o","fsname=…","--",mp])` 返 0（说明确实在用助手），
  以及**不用 strace** 的 `FUSERMOUNT_PATH` 对照。

本轮实测的挂载面结论（`.bneck/pkg_fusermount_probe.sh`，uid 1005、无 capability）：

| 组 | 条件 | 结果 |
|---|---|---|
| 对照 | 默认（`/usr/bin/fusermount` 存在） | `fstype=fuse`，写/读正常，`fusermount -u` 后 `/proc/mounts` 行归 0 |
| 探活 | `FUSERMOUNT_PATH=/nonexistent` | mount 进程**直接报错退出**（`error: No such file or directory`），挂载点目录保持为空——没出现「字节落进裸目录却当成功」那种假阳性（fail-fast 生效） |
| root / `CAP_SYS_ADMIN` | 已补测（本机无 root，用 `unshare -r -m`：ns 内 uid 0、`CapEff=0000003fffffffff`） | 助手被 `FUSERMOUNT_PATH` 藏掉仍挂载成功：`fstype=fuse`、`/proc/mounts` 行 `user_id=0,group_id=0`、写/读/mkdir 正常、`umount` 走 `umount(2)` 直连、目录回 0 项——**有 cap 时确实不需要助手**。诚实限定：这是 **user namespace 里的 root**，挂载只对该 ns 可见；宿主真 root 未测，但两者走 `fuser` 同一条 `mount(2)` 分支 |

所以「不依赖其他安装包」的准确口径是：**元数据/数据面（包括整个 FDB 臂）零包依赖**——服务端、`view`、`data-server`、
`fdbcli` 全部只靠包内文件跑；`mount` 这一面在**非 root 且无 `CAP_SYS_ADMIN`** 时需要内核 FUSE + 一个发行版自带的
setuid 助手（有 cap 时直连 `mount(2)`，已实测），这与 SQLite/PG 档完全相同，不是 FDB 引入的新依赖。
三组实验的脚本都在仓库里：`.bneck/pkg_fusermount_probe.sh`（非 root 对照 + 藏助手）与
`.bneck/pkg_rootns_probe.sh`（userns 带 cap），每一格都有读数，没有留“按源码应该会”的空位。

口径不变的部分：**这不撤销 G3 的未通过判定，也不构成发布版**。v2.2 当时二进制连的是未提交的工作树（32 个路径）、
也无 tag；v2.3 把这批内容提交了并在 clean tree 上重编（见下），但**无 tag、非发布版、G3 未过**这三件事一件没变。
R3 仍生效——`fdb` 不进默认板、用户文档不写第三后端承诺、`--meta-store` 帮助仍标 EXPERIMENTAL。
投放包拆视图用的是 `clearrange`，按 §12 第 5 条**必然**在 `(root,'refs')` 留保守多计行：本轮实测该 subspace 共 503 行，
而 v2.1 修好的 harness 已不再增长（`K2` 三次读数均为 29、带 tag 的计数 0 条）。

### v2.3（把 v2.2 留下的空位逐个填掉，并抓到构建面的一处自相矛盾）

v2.2 交付了投放包，但同一轮里我给自己列了五件没收的事：root/`CAP_SYS_ADMIN` 那一格挂载没实测、
v1.0 的 BUILDINFO 有一句反向的错述、`dist/` 不在 `.gitignore` 里、跑板段数的口径过期、以及一条
一直没入台账的发现。这一节是这五件的账单，外加一件本来不在清单上、查证时才撞出来的问题。

**撞出来的问题：README 说构建需要 `pkg-config` + libfuse 开发包，USAGE 说不需要——两边不可能都对。**
读源码可以判定谁对，但「不需要」是一种否证主张，光读源码不够，所以两头都做：

- 源码侧两道门都在：`fuser` 0.18.0 的 `build.rs` 里，Linux 且 `not(feature = "libfuse")` 直接
  `cfg(fuser_mount_impl="pure-rust")`，`pkg_config::Config::new()` 只在 libfuse/macos 分支里；本仓库
  `fuser = { version = "0.18", optional = true }` 没开任何 `libfuse*`（其 `[features] default = []`）。
  `rusqlite = { features = ["bundled"] }` 则让 `libsqlite3-sys` 走 bundled 路径，同样不查 pkg-config。
  `pkg-config v0.3.34` 确实还在依赖图上（`cargo tree -i pkg-config`：fuser、libsqlite3-sys 两个 build-dep），
  但**只是挂着没被调用**——这是我一开始差点写成「依赖图里没有 pkg-config」的地方，实测前改口了。
- 实验侧（`.bneck/pkg_build_probe.sh`，本轮实跑）：`PKG_CONFIG=/nonexistent`、干净 target 目录，两种形态都建
  ——default release 106s rc=0，`--features fdb` release 112s rc=0（后者只需 `FDB_CLIENT_LIB_PATH` 来链接
  `libfdb_c`）；两份产物的 `DT_NEEDED` 分别是 glibc 五件（+`libgcc_s`）与 `libfdb_c` + glibc 五件，
  `grep -ci fuse` → **0**；`env -i … grovefs --version` 打出 `grovefs 0.1.0`。

所以准确口径是：**构建只要 Rust 工具链**，`pkg-config`/libfuse 开发包都不是前置。据此改的位置：
README（中英两处「前置」段 + 两处注 + 两处测试块里那句 `export PKG_CONFIG=…`）、USAGE（§1 代码块、§11.0、
§11.1、§10 与跑板段里的 `export PKG_CONFIG` 行）、docs.html（§5.1 段、§11.10 预检注释、§15.5 的 export 行）。
`.build-bin/pkg-config` 这个 shim 与 30 多个套件脚本里的 `export PKG_CONFIG=…` **保持不动**，只注明是历史遗留——
它们现在无害，删掉反倒要让 30 多个脚本重跑一遍才能证明没坏。

**补上挂载矩阵最后一格**（此前只能写「本机无 root，未实测」）：`unshare -r -m` 给出 ns 内 uid 0、
`CapEff=0000003fffffffff`，正好是 `fuser` 直连 `mount(2)` 分支需要的凭证。`.bneck/pkg_rootns_probe.sh`
全程带 `FUSERMOUNT_PATH=/nonexistent`（助手对所有进程都不可达）仍然挂载成功：`fstype=fuse`、
`/proc/mounts` 行 `user_id=0,group_id=0`、写/读/mkdir 正常、`umount` 走 `umount(2)` 直连、探针视图回 0 键。
诚实限定照写：**这是 user namespace 里的 root**，挂载只对该 ns 可见，宿主真 root 仍未测——但两者走同一条
`mount(2)` 分支，所以 v2.2 表格里那一格从「按源码应该会」升级成有读数。

**v1.0 那句反向错述的处理**：不猜，直接把 `dist/v1.0` 的实物取出来读——`objdump -p` 的 `DT_NEEDED` 只有
`libgcc_s/libpthread/libm/libdl/libc`（**不链 libfuse**），`strings` 里有 `FUSERMOUNT_PATH`/`fusermount3`/`/dev/fuse`，
即它跟助手的关系是运行期 `execve`，而 BUILDINFO 写成了「依赖本机 libfuse3」。修法上先确认了
v1.0 的 tar 包里**只有二进制一个文件**，所以改 BUILDINFO 不会让产物摘要失效：worktree 里只改 BUILDINFO
的这一段 + `SHA256SUMS.txt` 里 BUILDINFO 那一行（第一版手滑把整张表重排了，`git checkout` 回滚后改用逐行替换），
`sha256sum -c` 五项全过、二进制与 tar 字节不变。提交 `23f185d` 落在 `dist/v1.0`（孤儿分支），**未 push**。

**仓库卫生**：`/dist/` 加进 `.gitignore`（实测 `dist` 204M、`git ls-files dist/` = 0），否则 `git add -A` 会把
60MB 的投放包连同二进制一起带进 master 提交——那是个永远修不掉的仓库膨胀。

**投放包重打到第五次，其中一次换了二进制**：前四次只改包内两份文本（挂载面口径、构建面新结论），
`grovefs`/`lib/libfdb_c.so`/`fdb/` 三件字节未变。第五次不是文案问题：写「本包对应哪个提交」时发现
包内那份 `grovefs` 是当日 01:59 的产物，而 `Cargo.toml` 与 `vendor/foundationdb-sys/` 在 02:08 还有
改动——即**它不对应任何一个提交**，而这正是 BUILDINFO 当时只能写「32 个未提交路径」的原因。
本批内容提交后（见下）在 clean tree 上按**同一条命令**重编：`cargo build` 报 Finished 且判定为
up-to-date，`strip -s` 后得 6,623,152 B（与旧版差 1,952 B 即来自那批未进旧版的改动），RUNPATH 与
`--version` 均正常。最终包：`grovefs-fdb-experimental-linux-x86_64.tar.gz` 60,052,495 B /
`66cf9aabddcadc8524dff957aa258dc170ae5e80055e3aa83aab0fdb6eee72cf`；重打后 `.bneck/pkg_final.sh`（全新位置解包、
七件校验、`env -i ldd`、构建机路径命中 0）与 `.bneck/pkg_verify.sh`（`env -i` 端到端起服务→挂载→读写→
包内 fdbcli 直读→clearrange 回 0 键）**用新二进制复跑全绿**。同一份构建还跑过一次 fdb 档套件
（`bash .build-bin/run_all.sh fdb`：`pass=170 fail=0 SKIP=0`、`RESULT: GREEN`、日志
`.build-bin/logs/20261110-035628`），因为 `test_fdb.sh` 用的就是 `target/fdb/release/grovefs`——
也就是投放包里那一份的未剥符号前身。包内「源码坐标」现在写的是 clean 工作树
与实现提交，不再是一串漂移的路径计数。

**本批内容已提交并 push**（用户当轮授权）：`0aebe68` feat(fdb)（实现、vendor 绑定、套件与 `.bneck/` 里
被文档点名的 52 件配方脚本，共 210 KB）、`3e9c33e` docs（三面口径与段数）、`7601b22` chore(gitignore)；
`git push origin master` 得 `11e2056..7601b22`，`dist/v1.0` 的修正提交得 `a7f7fbc..23f185d`。
`.gitignore` 除了 `/dist/`，本轮还堵了两类残渣：`.bneck/` 的**捕获输出**（`central/`、`g3_evidence/`、`raw/`、
`*.jsonl`、`*.log`、`seghnd.so`）与本机集群目录 `.fdbtest/`、`.fdbtest-build.log`——配方入库、观测不入库。
提交不等于转正：仍无 tag，G3 仍未过，R3 口径一字未改。

**跑板段数口径**：README 四处还写着「33 段（32 门控 + bench）/30 个功能套件」。核对 `run_all.sh:121` 的 CANON
实为 **34 段（33 门控 + bench）= build + unit + 31 个功能套件**，`EXPERIMENTAL="fdb"`（:123）不变。段数是一项
主张，所以改正前先挂了一次真实全板（本轮 `RESULT: GREEN`、`EXIT=0`、日志 `.build-bin/logs/20261110-032014`，
34 段逐段 OK、`grep -c FAIL` 为 0），跑板期间按纪律不并发做清理。

口径不变的部分：G3 仍是**未通过**、R3 仍生效（`fdb` 不进 CANON、文档不写第三后端承诺）；v2.2 的挂载面结论没有被
推翻，只是补齐了缺的那一格；台账 §12 第 5 条的账也没变——投放包用 `clearrange` 拆视图，`(root,'refs')` 里的保守
多计行仍在——本轮它的读数从 v2.2 记的 503 行一直接着往上走：投放包验证每次收尾加几条（到 508 行），
再跑一次 fdb 套件后到 **525 行**（套件自建的十几个视图各留几条），而 `K2` 始终读作 `\x15\x1d`=**29**，
即 v2.1 修好的 harness 没有复发（涨的那部分是 `clearrange` 按定义留下的，不是泄漏）。
补救入口仍是 `grovefs refs rebuild`。仍受硬件阻塞、本轮不具判定资格的
两件也如实留名：G3 要 SSD/NVMe 介质重跑 `.bneck/bench_g3.sh`，多机房失效恢复要一具可自有的三机集群（本机三台 VM
跑着他人的集群、且 gfs1/gfs2 共享同两块数据盘——两条均不准，见 §14 v2.4 的更正）。

### v2.4（把「多机房失效恢复」那一格从「本环境不具资格」改成实测）

用户指令原文：**「把gfs的集群先撤掉，需要的话你可以多创建虚拟机。不用考虑介质问题，先功能跑通」**，随后在选项框里定了拓扑（复用 gfs 三台做 FDB 节点，grovefs 侧的 meta-server/data-server/挂载放宿主）与拆除深度（只停服务 + 清数据根）。这一节的每一条都是执行结果，不是计划。

**先备份再动手，且备份当场发现自己不自足**：`virsh managedsave gfs1 gfs2 gfs-client` + 7 个 qcow2 以 `cp --sparse=always` 收进 `/data/grovefs/vmbak/`（3.8G）配 `SHA256SUMS.txt`。第一批备份记了一条不成立的安全感：三台的 qcow2 全是 `/data/ifuse/vm/hanjun-vm-base.qcow2` 的**薄克隆 overlay**，只 cp overlay 的副本恢复不了任何事——随即补拷 base（621,284,352 B）并把它的 sha256 追加进同一张表。
回滚入口 `.bneck/vm_rollback.md`；拆除脚本 `.bneck/vm_gfs_teardown.sh` 先 `--dry-run` 再实跑：卸 3 个 fuse 视图 → `systemctl disable --now` 全部 grovefs 单元 → 删 `/data/gfs*/grove` 与 `/home/hanjun/meta`；OS 盘、二进制、unit 文件、cluster.env 一律不动，恢复是一条 systemctl。gfs-client 根盘当时只剩 436M，故热挂一条 30G 数据盘（`mkfs -L fdbdata` + fstab UUID）得到 `/data/fdb` 28G 可用。

**真跨机集群**：`.bneck/fdb3vm_up.sh` / `fdb3vm_down.sh`——四台机器各一个 `fdbserver`，`--locality-zoneid nN --locality-machineid m-nN`，cluster 文件把四个 `ip:port` 写全（省略地址列表会撞 2104），首建 `configure new triple ssd` 再 `coordinators auto`。实测态：`triple`/`ssd-2`、`database_available True`、`full_replication True`、degraded 0、storage 4 / logs 3 / coordinator 3、`fault_domains n1..n4`。

**这一轮最贵的一条发现是三节点给的**：恰好三台机器的 `triple` 集群挂掉一台后数据库**变为不可用**并卡在 `recovery_state: recruiting_transaction_servers`（messages 为 `unreachable_dataDistributor_worker`、`unreachable_ratekeeper_worker`、`unreadable_configuration`），直到把那台手工拉回来才恢复且探针键完好——日志集要 3 个 log，FDB 不允许多个 log 同机，两台幸存者组不出日志集。这条推翻了「三副本 = 挂一台还能用」的直觉，也让 v2.3 那句「要一具可自有的三机集群」成了一句**不够**的要求。
于是补第 4 故障域：新虚机 `gfs-fdb4`（192.168.122.248）按仓库既有的 `.build-bin/gfs_provision.sh` 路子（薄克隆 + NoCloud seed + virt-install）建，配方固化成 `.bneck/vm_fdbnode_provision.sh` / `vm_fdbnode_destroy.sh`。中途两次走错路各记一笔：`cp` overlay 加手改 `virsh dumpxml` 的机器拿不到 DHCP 租约（base 的 netplan 按建镜像时的 MAC 匹配，seed 不带 network-config 时 cloud-init 不重渲染，于是只有 link-local IPv6 与 mDNS、从不发 DHCP discover）；离线 `qemu-nbd` 改 netplan 也不通。正解是 seed 里写 `/etc/netplan/90-fdbnode.yaml`（`match: {name: "en*"}` + `dhcp4: true`）并在 runcmd 删 50-cloud-init 后 `netplan apply`。补第 4 台后 `fault_tolerance` 才读作 `max_zone_failures_without_losing_availability=1`、`max_zone_failures_without_losing_data=1`。

**功能面跨机重跑**：`GROVEFS_FDB_CLUSTER_FILE=/data/grovefs/.fdb3vm/cluster3vm bash .build-bin/test_fdb.sh` → **`pass=170 fail=0 SKIP=0`**（日志 `.build-bin/fdb_3vm_suite.log`）。这是同一套 170 条断言第一次不跑在单机集群上；`test_fdb.sh` 读的就是这个环境变量，所以复跑不需要改一行代码。

**失效恢复实测**（`.bneck/fdb3vm_fault.sh`，故障窗口内每秒采样一次可用性时间线，判据式而非形容词）：

| 阶段 | 手段 | 最长不可用 | 同场判据 |
|---|---|---|---|
| A 进程失效 | `pkill -9` 掉一台的 fdbserver（机器仍在） | **0s**（10 个采样） | degraded 期写入照常提交并读回；+3s 幸存者重排好存储副本、`full_replication` 恢复；进程归队后 4 zone 齐、degraded 0 |
| B 整机失效 | `virsh destroy` 一台 = 独立内核 + 它的数据盘 + 它的 IP 一起没了 | **0–1s**（两轮分别 0s 与 1s，采样分辨率 1s） | +4s **三台幸存者在死机不回来的情况下**重组出日志集与 3 副本（正是三节点做不到、逼出第 4 台的那条性质）；冷启动归队（ssh 回来 12s，无自启）后 `zones=[n1,n2,n3,n4]`、`noloc=0`、storage 回 4 |

五枚跨故障窗口的探针键（`baseline`/`A_degraded`/`A_recovered`/`B_degraded`/`B_recovered`）事后全部可读；账单 **pass=7 fail=0**、`FDB_FAULT_GREEN`（`.build-bin/fdb_3vm_fault.log`）。脚本自身在这轮修掉三处错：`$SECONDS` 与 `date +%s` 混算使时间戳无意义；`fdbcli get` 输出形如 `` `k' is `v' `` 未解析造成假阴性；以及一条**假阳性**——把「尚无 fault_domain 的进程」按 `str(None)` 计成一个故障域，于是三真域 + 一个未初始化进程就被当成满足 `zones>=4`。表里的数字出自修正后那次完整重跑。

**backup/restore 链**（`.bneck/fdb3vm_backup.sh`）：`fdbbackup` 与 `backup_agent` 都不在既有资产里，从官方 release 的 `foundationdb-clients_7.1.67-1_amd64.deb`（68,734,644 B，sha256 `487c44f0…0ba0`）取出。三个坑记在脚本头注释：这两个二进制**按 argv[0] 判定程序类型**，所以 `.x86_64` 后缀不可用（`Unable to determine program type`）；7.1 仍需 agent，且它是 `backup_agent`（只吃 `-C/--log/--logdir/-m`，没有 `--data-dir`）而不是 `fdbbackup agent`；restore 的目标是 `--dest-cluster-file`（也正是「往非空库里恢复」的合法开关），源是带日期的 BackupURL 而不是容器根，停止版本要从 `status` 的「completed at version」读、`discontinue -w` 自己不打。
链上四条判据各有独立证据：快照版本 5106287190 < 停止版本 5126221292，故 `bkz01..bkz10` 只能来自日志重放；恢复前先证 60 键真被 `clearrange` 抹掉；按 `-k` 只恢复受损区间到指定版本后 60 键逐字节相同；在停止版本**之后**写的 `post-backup-junk` 恢复后消失、回到 `logonly-01`——时间点恢复的两个方向都测了，只测前一半等于没测。账单 **pass=16 fail=0**、`FDB_BACKUP_GREEN`（`.build-bin/fdb_3vm_backup.log`；容器 88K/16 文件留在盘上不 `expire`，因为被同一个脚本过期的备份证明不了第二次）。判据也修过一次：`LastError: None` 里含 "Error"、而冒号后有空格，所以 `grep -i error` 与 `grep ":None"` 两个方向都会误判，前两跑各中一个；改成只认 `LastError` 的**值**，并用一份合成日志反证「真错误仍能被抓住」。

**就地改掉的三处表述**（被实测推翻的，不是文笔问题）：① §15 G3 行原文「三台 VM 跑着他人的 grovefs 集群且 gfs1/gfs2 共享同两块数据盘」——前半是假的（那是本项目自己的测试集群，本轮已撤），后半也是假的（`virsh domblklist` 与三台各自 `lsblk` 显示每台有自己两条 30G 盘，挂 `/data/gfs1`、`/data/gfs2`，共享的是宿主那条 RAID）；② v2.3 末尾同一句加了更正指向；③ `.bneck/fdb3_up.sh` 头注释里「the three VMs are currently serving another team's live grovefs cluster」同步更正，并注明真正剩下的障碍只有介质。

**仍然不动的两件事**：G3 判定资格没有变化——本轮明确「不用考虑介质问题」，宿主 sda/sdb 仍在 MR9361-8i 后面 `queue/rotational=1`、无 NVMe，性能面一个数字都没重测，§15 的 G3 仍是**未通过**；R3 口径一字未改，`fdb` 仍不进 CANON、文档仍不写第三后端承诺。投放包也不动：`fdbbackup`/`backup_agent` 只作为取证工具留在 gitignore 的 `.fdbbin/`，没有进 `dist/`（v2.2「包内无死重」保持）。本轮新增 7 个配方脚本（`fdb3vm_up/down/fault/backup.sh`、`vm_gfs_teardown.sh`、`vm_fdbnode_provision/destroy.sh`）与 `.bneck/vm_rollback.md` 已提交为 `adfee8f`，本节、USAGE §6.3 与 `.bneck/fdb3_up.sh` 头注释的更正是紧随其后的第二笔 docs 提交。

### v2.5（把 G3 门线从人肉判读改成可执行判定）

由一句提问触发（2026-11-10）：「按没有 SSD/NVMe 测的功能，是不是有 NVMe 后可以直接测试？」答案是"能"，
但当场查出两处不干净，这一节就是把它们钉死。

**① 对照臂的介质必须跟着搬**。`bench_g3.sh` 的 `PGURI` 原先是硬编码，改成
`${PGURI:-postgres://grove:grovepw@127.0.0.1:5433/grovefs}`，并在其上方写明理由：门线的尺子是
「不劣于 PG 平台值」，如果只把 FDB 的数据根搬到 NVMe（`FDB3_ROOT=`/`CLUSTER3=` 一条 env 就够）而 PG
仍留在原盘，那就变成"两个后端在两块盘上比"，过了也不算结论。实测确认默认那把尺子的落点：5433 是宿主
自己的 PostgreSQL 12（pid 3661526，datadir `/var/lib/postgresql/12/main`，在根 LVM 上、与 FDB 当时
用的目录同一条 MR9361-8i RAID，`rotational=1`），**所以 §15 记录的那轮同场对照是干净的**，不是巧合。

**② 判定必须有代码和退出码**。以前脚本只打印 ops/s，"过没过"是我读出来再写台账的。现在
`.bneck/g3_verdict.py` 承担判定，`bench_g3.sh` 末尾直接把它作为自己的退出码：
`G3_GATE`（默认 4600，NVMe 档传 9000）比 FDB 臂最好单元，`G3_CTRL`（默认 0.8）比**同场 PG 臂**。
控件下限不是形式主义：`~4.6k 平台值`是宿主的属性而不是本仓库的属性，PG 臂没复现它的轮次**既不能判过
也不能判不过**，只能报 `G3_INCONCLUSIVE`（退出码 2）。这条被一次 3 秒烟雾轮当场证伪了另一种做法——
那轮 fdb 287 / pg 1134，若只看 `fdb < gate` 就会把"轮次太短"误读成对门线的判定。

**验证账单**（四种分支都跑过，不是只跑通的那条）：合成行 GREEN(fdb 5200/pg 4700→exit 0)、
**RED(1601/4900→exit 1，用的就是 §15 记录的历史数字，判出的正是当年那个人肉结论，可当回归基线)**、
控件失效(pg 900→exit 2)、空行文件(exit 2)；端到端 `.bneck/fdb3_up.sh` 起本机 RF=3 → 标准矩阵 8 单元
→ `G3_RED` exit 1（PG 控件 4603/4600 复现平台值，FDB 臂最好 414）→ `.bneck/fdb3_down.sh` 收尾，
端口 4510-4512 释放、跨机集群 `The database is available.` 未受影响。

**一个必须写下的保留**：本轮 FDB 最好 414 ops/s，比 §15 记的 1601 更低，**不要拿它替换观测最好值**。
原因是负载不可比：跑这轮时宿主上有 17 台 running 虚机（含本项目四故障域集群，其 fdbserver 在客户机内、
宿主 `pgrep` 只看到本机 4 个）、load 5.9–8.6，全部压在同一个 RAID 上；FDB 臂单元里 wall 已漂到 9.8–13.0s
（时间盒只有 6s）。这轮的意义只在"判据会自己下正确的结论"，数值本身不是新基线。

**换到 NVMe 之后的动作**（两条命令，不需要改脚本）：
`FDB3_ROOT=/nvme/fdb3 bash .bneck/fdb3_up.sh`，然后
`CLUSTER3=/nvme/fdb3/cluster3 PGURI=postgres://…（同盘实例） G3_GATE=9000 bash .bneck/bench_g3.sh`。
`exit 0` 才有资格谈回 CANON；`exit 2` 先修控件所在的主机；`exit 1` 且 PG 臂正常，就是实现侧的账，
与介质无关（v1.8 的 `memory` 引擎对照已经把约 3× 的差距从盘上摘掉过一次）。

### v2.6（给基准留下的 PG 视图残留接上一个删除出口）

触发是一次收尾提问：跑完 G3 在两臂各建的视图谁去删。查完的结论是**没人能删**——
`grovefs view rm --base` 按设计拒绝 `--meta-store postgres://…`，帮助原文是「its tree is a PG
schema — drop the schema and let PG/data servers own the bytes」。也就是说 PG 档的基础视图**没有
CLI 删除路径**，而 `bench_g3.sh` 和 `pg` 套件都是"每单元一个视图"的写法，跑一轮就留一轮。

**新配方 `.bneck/pg_drop_views.sh`**，把那条"让操作者自己去删"的路补齐，且删法是全套的：

- 视图的树就是 schema `grovefs_v_<视图名的 hex>`。这条不是猜的：把库里已有的 schema 名逐个
  `bytes.fromhex` 反解回视图名（`pg_1794293409_r2n4` ↔ `grovefs_v_70675f…`）验证过再照正向写。
- **必须先删台账再 DROP SCHEMA**。跨视图台账 `grovefs_refs.holders` 是**全局表且没有 view 列**
  （当时 895,725 行，`data_ref` 全库唯一、`holders` 基本为 1），只删 schema 会把该视图订过的每个
  对象一行台账永久留下——这正是 SQLite 档 `view rm --base` 靠「从存活树重建台账」避免的状态。
  零字节文件也订对象：一个 3.7k op 的单元就留 3,700 行，不是边角情况。
- `grovefs_control.view_defs` 不用动：它只登记 derived/fork/alias，实测 0 行而当时有 29 个基础视图。
- 护栏：默认 dry-run，`--yes` 才删；不给名字时只命中 `--pattern`（默认 `^pgp?_?[0-9]`，即基准自己
  造名的三种形态）；非 `grovefs_v_*` 一律拒绝；视图已不存在时是无害 no-op（幂等，可重复跑）。

**`bench_g3.sh` 现在每单元自清理**（`KEEP_VIEWS=1` 可关）。FDB 臂不需要这一步：它的视图住在本机
`.fdb3` 这个**专门用来扔的**集群里，`.bneck/fdb3_up.sh` 起手就是 `rm -rf $ROOT`。

**本机实测账单**：清掉 29 个视图（今日 1 个烟雾 + 28 个历史基准/`pg` 套件残留），回收 **84,779**
行台账，库 **242 MB → 148 MB**，`remaining base-view schemas: 0`。端到端复跑证明自清理真的在跑：
`gc: dropped pg_1794294306_r1n1 (ledger rows removed: 4380)`，整轮跑完仍是 0 个 schema。收尾
`.bneck/fdb3_down.sh` 后端口 4510-4512 释放，跨机集群 `The database is available.` 未受影响。

**看到但没有动的一件事**（留给有明确结论时再下刀）：清完之后 `grovefs_refs.holders` 里仍有
**807,246 行，而本库已不存在任何视图 schema** —— 更早轮次用别的路子直接 `DROP SCHEMA`、没配台账
删除留下的孤儿行。要不要按「从存活树重建台账」的语义一次性清掉未引用行是另一个判断：其中 `KX`、`K2`
两行 `holders=5`、形态不是对象 UUID（像是控制键），盲目"删除所有未被引用行"会把它们一起清掉，所以
本层不下这一刀，只把数量和形态记在这里。

### v2.7（把 v2.6 记下却没下刀的那一事做了，并且顺手把还在漏的龙头关掉）

v2.6 留的那 807,246 行，本轮先按纪律把**语义**查实，再动手，最后回头发现真正的重点不在
那 80 万行，而在「谁还在往里漏」。

**语义出处**（三处，全部读源码而不是推）：

- `rebuild_ledger()`（`src/meta/engine.rs:1174`，写侧 `:1205-1238`）：一个 IMMEDIATE 事务里
  **`DELETE FROM holders` 全清**，再逐棵存活树 `SELECT DISTINCT data_ref FROM node WHERE
  data_ref IS NOT NULL` 并对每个 `data_ref` 计 `+1`。它**不保留任何无树留名的行**，也没有
  「控制键例外」这一分支——所以 `KX`/`K2` 按定义也该消失。
- 读侧：`grovefs_refs.holders` 在 `src/pg_store.rs` 只有 4 处引用（建表 + inc/dec/delete），
  **没有一处 SELECT**；判定只发生在 unlink 时的 decrement。而 `gc` 对 `postgres://` 与
  `fdb://` 启动即拒（`src/main.rs:2709`），所以没有任何离线路径读这些行。
- 风险方向：`USAGE.md` §5.3 与 `:2900` 都已写明，过计只会**多留字节**，从不误删；「无行即该
  视图私有」。⇒ 按存活树 DISTINCT 重算是安全语义，不存在「重算误放」方向。

而 `KX`/`K2` 的来历，**v2.1 已经记对了**（`:804`）：harness 收尾时 `/f0`(K2) 与 `/x`(KX)
还活着却走 `DROP SCHEMA CASCADE`，删名不销账，每跑一轮 +1。本轮在源码里找不到 `KX`/`K2`
字面量不是待查的疑点，而是它们本就是测试自造键。

**实现**：`pg_drop_views.sh` 加 `--rebuild-ledger`（等值 SQL，PG 侧没有 CLI 入口）与
`--no-drop`（只重算不删树）；干跑会报「N 棵存活树订了 M 个对象，现在台账有 K 行，其中 K-M
行无树留名」，先把结论摆在动手前。

**两个真 bug 是 fixture 抽出来的，不是读代码读出来的**（`.bneck/pg_rebuild_fixture.sh`，自带
一次性 `initdb` 小集群，因为本机 `grove` 角色无 `CREATEDB`）：

1. `subquery in FROM must have an alias`——预演那条聚合少了外层别名；
2. `syntax error at or near "-"`——快照表名用 `date +%Y%m%d-%H%M%S`，而未加引号的 PG 标识符
   不能含 `-`。这一条**本来会直接砸在真库上**，只因两条语句在同一事务里才无损。

fixture 五档全绿（账本故意播错）：`R1=1`（树内两个名字只计一次）、`R2=2`（两树共享）、
`R3=1`、`ORPHAN` 消失、NULL/`''` 跳过、无树时归 0、重跑幂等。**为什么需要 scratch 集群**：
真库当时 0 棵存活树，跑重算只能证明「会清空」，证不了那条 `INSERT … count(*) GROUP BY`。

**顺手关掉三处还在漏的龙头**（否则本周就得再清一次）：`test_engine_evict.sh`、
`test_control.sh`、`tests/mcp_probe_ro.rs` 备注里写的是「leave PG as we found it」，实际各自
在收尾裸跑 `DROP SCHEMA … CASCADE`，注释与行为不符，每跑一轮漏一批无树行。前两个改为
调用 `.bneck/pg_drop_views.sh` 并传入视图名（规则只有一份，与 `bench_g3.sh` 同一入口），
Rust 那个就地先按该 schema 的 `node.data_ref` 删台账行、再 drop。
`tests/fdb_tree.rs` 早在 v2.1 那轮已用 `release_ledger()` 修过，本轮核对后归为历史残留。

**还挖出一处假绿**：`test_pg.sh`/`test_pg_parallel.sh` 把端口钉在 `PGPORT:-5432`，而本机 PG 在
5433，于是两轮都是 `SKIP=1 pass=0`，板子却报 `PASS`——上一轮的「功能复验」其实什么都没跑。改成
像 `test_control.sh` 那样探 5432 再探 5433（显式 `PGPORT` 仍优先），跳过的套件不再能伪装成绿。

**账单**（均本机实测）：真库重算 `807,246 → 0` 行（重算前 `sum(holders)=807,254`，多出的 8
恰为 `:804` 已归因的 `KX`/`K2` 那两行各计到 5）；四支套件复跑 pg 18/0、pg_parallel 40/0、
evict 7/0、control 16/0（`RESULT: GREEN` 且 `pass` 非 0）；跑完台账 48 行、存活树恰好 48 个
DISTINCT 对象，**无树留名 0 行**——这是龙头关掉后唯一需要的判据；再走 pattern 清理后 0/0。
快照表 `holders_bak_*`（一个 807,246 行 59 MB、一个 0 行）在对账成立后手工 drop：它们记的就是
泄漏本身，回滚点留着它自己就是新的残留。库 **242 → 148 → 77 MB**。`tests/mcp_probe_ro.rs` 单独
以 `GROVEFS_TEST_PG_URI` 跑 5433：2/2 通过且留下 schemas=0、rows=0。

**文字同步**：`USAGE.md` 原 §15.8 第 3 步与第 1203 行那两句正在**教人跑会漏账的裸
 `DROP SCHEMA`**，已改成先销账再 drop 的配方并讲清两步为何必须成对；§5.3 与 §15.9 的「PG 侧没有
 `refs rebuild`」保留「不是 CLI」的口径，但指向新配方；§3.4.3 的守卫④与 §15.16 报错对症表里
「手工 `DROP SCHEMA`」那两格同理。`docs.html` 是同一份口径的镜像，它当时仍在教那条裸命令（§5.9
 两处表格、§14.8.3 守卫④、§13.3 账本段、§16.8 与顶部导航、报错对症表共八处），按它自己的章节号
 体系统一改到 §13.3；引用里不写源码行号（`engine.rs:1174` 改成 `engine.rs` 里的 `rebuild_ledger`）——
 行号是最先坏掉的那种精度。判据面：`doccheck.sh` 的参数对账一开始把配方自己的选项
（`--yes`/`--list`/`--rebuild-ledger`/`--no-drop`）当臆造的 grovefs 参数报红。没把它们塞进 FOREIGN
 白名单——`yes`、`list` 都是通用词，豁免一个名字等于放行今后任何同名臆造——而改成**按行剥离讲运维
 配方的行**：识别按 `.bneck/*.sh` 的 basename（散文里常只写短名 `pg_drop_views.sh --rebuild-ledger`，
 只匹配全路径会漏），已知代价是与配方脚本同行的 grovefs 臆造参数不再被这条规则抓到。改完全板
 `doccheck.sh` EXIT=0（真实参数 70 / 文档参数 90 / 差集 19 项全部归为外部工具、占位或负空间）。仍然没做：`refs rebuild` 本身没进 CLI
（R3 口径：不新增后端承诺），它只是一个运维配方。

### v2.8（规模用例把 readdir 分页逼到 30 页以上：抓到一处跳名/重名的游标错位）

这一轮做的是「规模用例」这一格：一个目录 2 万条，经挂载点逐条 `create`（每条一次事务，不是
`view import` 的单事务灌入），读回时按**集合**而不仅按**行数**比对。它第一次跑就红了，红得
很说明问题。

**现象**（6,000 条的单目录）：行数恰好 6,000，集合却是 851 个名字出现两次、851 个名字一次
没出现。`ls -1`（壳层排序）与 `ls -U`（getdents 原始顺序）给出同一份错误，`find` 也数出
6,000 —— 三种枚举走同一条路径，错在服务端的页拼接，不在客户端排序。原始流里能直接看见它：
`f00000…f00054` 连续 55 个之后直接跳到 `f00078`（丢 23 个），后续又在 `f00165` 处回头重发。

**根因**：`FdbStore::readdir_page` 把检查点存成 `(parent, 块号)` → “本页最后一个名字”，而块号
是 `(offset+limit)/STRIDE`。键的语义是**块**，值的语义是**行**（第 `offset+limit-1` 行）。续读时
它按 SQLite/PG 的行语义算跳过量 `offset % STRIDE`，于是扫描从第 `offset+limit` 行开始、再跳过
`offset % STRIDE` 行——续读点整体后移 `offset % 32` 行，跳掉的那段与后面重新扫回来的那段
恰好相互抵消，总数因此看起来正常。SQLite（`MetaEngine::readdir_page`）与 PG 的契约是：键 =
**行号**（STRIDE 的倍数），值 = 第 `键-1` 行 的名字；页内每跨过一个 32 的倍数就落一个检查点。

**修法**：把 FDB 一侧改成与 SQLite/PG 同一套契约（`base = offset - offset % STRIDE`；范围
从检查点名字之后开始；页内跨过的每个 32 倍数都落检查点，而不是只在页尾落一个）。这不只是
一行算术：**三个引擎的检查点是同一份契约**，因为续读偏移是挂载层按内核 4 KiB 回复边界算的，
与任何引擎的 STRIDE 都不同源。

**回归测试**：`tests/fdb_tree.rs::fdb_readdir_pages_stitch_into_the_sqlite_listing`——SQLite 作规格，
700 个名字，7 种 chunk（1/7/31/32/33/100/257，含不整除 STRIDE 的）× 20 个任意续读偏移（含
检查点窗口内与窗口外两侧），外加一次删除后的重读。它拿旧实现验过**会红**：chunk=1 的顺序
遍历返回 `f0000, f0002, f0005, f0009, f0014, …`，正是上述后移在逐页累积；换上新实现后全绿。
只断言“行数对”的写法在这里毫无作用，这一条因此专门比对集合。

**规模读数**（本机单节点 dev 集群，SATA 盘，`.bneck/fdb_bigdir_paging.sh`）：20,000 条 create
153.4s（7.7ms/条，写路径的本价）；`ls -1` 3.8s、`ls -U` 3.7s（约 5.2k 名/秒）、`find` 3.7s、
`du -s` 13.2s；页边界名字（0/31/32/33/63/64/65/1023/1024/1025/中点/末尾）逐个 `stat` 全可解
析；中间删 500 个后重读 19,500 条且集合精确等于幸存集，`view scan` 报 19,501 entries（根 +
19,500）。同一套判据在 6,000 条上复跑通过；修复前它是 3 FAIL，修复后是 22/0。

**顺手抓到一个 harness 自身的假红**：本轮第一次跑 `test_fdb.sh` 出现 ~25 条 FAIL，首条是第 10
节“SQLite 对照臂”的横幅变成了 `metadata backend foundationdb (cluster file )`。它不是实现回归，
是调用方 shell 里残留的 `GROVEFS_META_STORE=fdb://`（空集群文件）：对照臂本来靠“不给
`--meta-store` 也不给 env”来保证自己是 SQLite，一句环境变量就让它变成同一个集群对自身对拍，
后续 `view --meta-dir … export` 又被当成 FDB 导出而对一个没人建过的子空间报错，一路级联。
harness 现在在开头 `unset GROVEFS_META_STORE`（套件里每一处真要集群的命令都显式写了
`--meta-store "$FDBURI"`），加上这一行后同一套件 `pass=216 fail=0`。

**文字同步**：`USAGE.md` §15.9 的 FDB 表把 `view rm --base` 从“拒绝”改成真实行为（清子空间 →
重算 `refs` 账本 → 无人持有对象回收，回显三段分开陈述），并补上 `view scan`（集群侧清点，
区分 view/corpse/ledger）与 `view create` 撞残骸时的拒绝原文；表后新增三件实测（备份恢复的
空目标约束、集群整面消失时的阻塞语义、两万条目录的读数）；功能面断言数改为当前 216 条。
`view rm --base` 自己的长帮助此前提的是“local SQLite store / PG 拒绝”，没提 fdb 分支，已按
实现补齐。

## 15. 决策门处置（G1–G4 与 R5，逐门留证）

计划把生死判定放在阶段末的决策门上。每个门的处置只有三种合法写法：**判过并留下证据**、
**判为未通过并执行对应的风险处置**，或**本环境不具判定资格并写明关闭它的实验**——沉默通过不算。
G3 在 v1.8 前是第三种写法，现在是第二种。逐门如下：

| 门 | 位置 | 处置 | 取证 / 关闭条件 |
|---|---|---|---|
| **G1** 环境门 | P0 末 | **通过** | 三条判据原文见 §14 v1.1 末（`The database is available.`、`cargo build --release --features fdb --example fdb_spike` 链接 `libfdb_c` 成功、spike `P0_SPIKE_VERDICT=PASS`）。环境路径按 Spec 要求"选定一条即写死命令"：发行版 apt 源与 docker 守护进程在本机都不通（前者无源、后者不可达），故走官方 release 资产 + sha256 校验 + 用户态运行——`bash .build-bin/fdb_env.sh fetch && bash .build-bin/fdb_env.sh up`，`eval "$(bash .build-bin/fdb_env.sh vars)"` 导出 `FDB_CLIENT_LIB_PATH` 与 `GROVEFS_FDB_CLUSTER_FILE`；不装系统包、不用 root，头注释里写明了降级次序 |
| **G2** 设计门 | P1 末 | **未触发** | 三条最难的形态都有表达且已落地：分页 readdir = `'n'` 前缀 range + `start_after` + 步长检查点（§3.3，`FdbStore::supports_paged_readdir()` 返回 `true`，`src/fdb_store.rs:1721`）、目录 nlink = `('k',parent,name)` 仅目录子空间计数（§3.4）、锁区间重叠 = `('l',ino)` 前缀扫 + 事务内过滤（§3.7，C5 另加"二级键须能无歧义重建主键"的硬判据）。P3/P4 的实现与差分对拍是这些映射真正成立的最终证据，不需要退回分片路线 |
| **G3** 性能门 | P4 末 | **未通过（已实测，2026-11-09）** | 判据是"不劣于 PG 平台值 ~4.6k ops/s"（SATA SSD 三副本档；NVMe 档 ≥9k）。本机两块盘都在 MR9361-8i 后面 `queue/rotational=1`，没有 SSD 介质，gfs1/gfs2/gfs-client 三台 VM 当时跑着本项目的 grovefs 测试集群、且每台各有自己两条 30G 数据盘（原文「他人的集群」「共享同两块盘」两处均失实，更正见 §14 v2.4）——所以按 Spec 的原义改做**同场对照**：同机、同 op 序列（照抄平台值出处 `bench_pg_conc.sh`）、同一 feature 二进制起两臂，RF=3 三副本。结果：FDB 臂 24 个单元最好 1601 ops/s（ssd 引擎；更早三轮顶到 1200），同一场 PG 臂 4100–4900 复现平台值——差 3–6 倍，判为未通过。归因（集群侧 `status json` 80 份快照）：conflicted ≈0（非 OCC）、tx_limit 7 万+ 未饱和（非配额）、tx/op ≈1（非键空间粒度）、durability lag 5.0–18.7s 且 commit 探针 3–600ms 而 read 探针 0.1–0.5ms（提交耐久是直接压力）；但 `memory` 引擎去盘后仍只 1642，说明约 3× 差距不在盘上，实现侧余量未能归因。R3 的处置已执行：`run_all.sh` 的 CANON 去掉 `fdb`（改 `EXPERIMENTAL`，按名可跑，取证 169/0）、USAGE §6.3/§11.10 与 docs.html §5.9/§16 撤下第三后端承诺并写入实测数字、`--meta-store` help 标 EXPERIMENTAL、段数口径 35→34/34→33/32→31。**不执行**"分派处返回实验未就绪"一条——feature 本就默认关闭且 `fdb://` 已被按名拒绝（P2 实证），再返回假错误会让 P3–P8 的证据不可复跑，理由与全部数据见 §14 v1.8。转正条件：SSD/NVMe 介质上 RF=3 重跑 `.bneck/bench_g3.sh` 达 ≥4.6k（SATA）/ ≥9k（NVMe），过线才回 CANON 与承诺面；门线判定本身已可执行（`G3_GATE` 传档、`.bneck/g3_verdict.py` 出结论、退出码 0/1/2，并带同场 PG 控件下限），配方与一次自证复跑见 §14 v2.5
| **G4** 语义等价门 | P5 / P6 | **未触发** | 三后端 `dump_tree()` 差分对拍 identical（P3 建立、P6 复跑）；四套治理套件在 fdb 档逐条通过（§14 v1.3 双档表）；§3 的九类映射与 §10 的冻结前缀集合未因任何一条断言放宽。暴露出的差异全部落进 §12 的可归因清单（硬链接多名下 `name` 取值、秒级时间、`ino IS NULL` 在 FDB 结构上不可能），没有一条需要"改不变量"才通过 |
| **R5** 台架拖累 | P6 内 | **已封堵** | 集群是外部状态，缺失即 SKIP 而非 FAIL：`GROVEFS_FDB_CLUSTER_FILE=<不存在的文件> bash .build-bin/test_fdb.sh` 打 `SUMMARY: pass=0 fail=0 SKIP=1` 且 rc=0；同一条件加 `FDB_TEST=1` 打 `SUMMARY: pass=0 fail=1 SKIP=0` 且 rc=1（两条均为本轮在同一台机器上实测）。`run_all.sh::run_fdb` 把 `SKIP=1` 登记为 `RESULT[fdb]=SKIP`，不进 FAIL 计数，所以没装 FDB 的机器上全板仍 `RESULT: GREEN` |
