# FDB 键空间与编码设计（P1 交付物）

> 本文件是 GroveFS FoundationDB 元数据后端的**编码契约**。P3 的 `src/fdb_store.rs` 以本文件为唯一蓝本；
> 评审后修改须记版本号。所有"现有形态"条目均逐条取证自 `src/pg_store.rs` 与 `src/meta/engine.rs`，
> 标注处即为源码行号。本文件不是用户手册：用户可见行为只写在 `USAGE.md` 与 `docs.html` 里。

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
- FDB：`(v,'i',ino)` → 值 `(parent_id, name)`；getattr = 同一事务内**两次 point read**（先指针、再节点键），
  由 FDB 并行发起，只有一次 RTT 量级的往返。
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
  nlink = `2 + 该前缀的 range 计数`。批量 finalize（READDIRPLUS 一缓冲）用一次前缀扫完成，
  对位 `finalize_batch` 的 `ANY($1) GROUP BY` 分组计数（`src/pg_store.rs:662-673`）。
- 普通文件的 nlink = 同 `ino` 的名字数：新增 `(v,'h',ino, name)` 存在性键（"该 ino 在本视图的名字集"），
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
  - 清扫：加 `(v,'lx', expires_at, ino, l_start, owner)` 存在性键，`lock_op` 的第①步与
    `sweep_expired_locks` 走该前缀扫（对位 `idx_locks_expire`）；写/删锁时同步维护。
  - owner/sid 清理：加 `(v,'lo',owner, ino, l_start)`、`(v,'ls',sid, ino, l_start)` 存在性键，
    对位 `idx_locks_owner`/`idx_locks_sid`——**这是 close/exit 隐式释放不泄漏的记账基础**
    （既有 pitfall：owner 记账漏导致 `fcntl` 锁隐式释放泄漏。FDB 侧必须同样按 owner 与 sid 双维度可反查）。
  - `renew_locks(sid)` = 按 `'ls'` 前缀找到该会话全部锁，逐个改主键的 `expires_at`
    （**主键不含 expires**，故续期 = 值更新 + `'lx'` 键换位：删旧 `'lx'`、插新 `'lx'`）。
  - `get_lock` 是纯读：FDB 用只读事务（对位 PG 的读池 READ COMMITTED，不产生写冲突）。

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

## 4. id / ino 发号（决策 D4，不可逆）

- 现状：SQLite `id INTEGER PRIMARY KEY`（rowid 自增）；PG `id BIGSERIAL PRIMARY KEY`
  （`src/pg_store.rs:399`），`insert` 用 `INSERT ... RETURNING id` 再 `UPDATE node SET ino=id WHERE id=id`
  使 `ino == id`（`src/pg_store.rs:851-877`，注释强调必须原子，否则 `ino IS NULL` 破坏 lookup）。
  `link_node` 则复用 `src_ino` 作为新行的 `ino`（`src/pg_store.rs:952-964`）——**id 不同、ino 相同**，
  这正是硬链接的表示法。所以 `id` 与 `ino` 是**两个独立计数器**，不能合并。
- FDB：
  - `(v,'m','id')` 与 `(v,'m','ino')` 两个 `i64` 分配器键；
  - **批量号段**：事务内 `Increment(+1024)` 后读回基值，进程内顺序发放；号段耗尽再取下一段。
    单键自增会成为整个视图的写热点（每次 create 都要抢同一键），分片号段把冲突面降到 1/1024。
  - 号段在**未落盘前不外泄**：进程崩溃丢号不重号（允许空洞，与 PG 序列的 `setval` 语义一致）。
  - 迁移续接：外部导入时若携带原 `id`/`ino`，分配器键须 `max(现有)` 续到
    （对位 PG 的 `pg_get_serial_sequence` + `setval`，P8 交付项）。
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
| HA | grovefs 自研快照主备（`VACUUM INTO`） | PG 流复制/`pg_basebackup`/PITR | 集群自带三副本，grovefs 侧**不叠加** |

## 9. 键长核算表（FDB 软限：键 ~10KB，值 ~100KB，事务写集软限 10MB）

| 键 | 组成 | 上界 |
|---|---|---|
| 节点主键 | subspace(视图名 ≤25B) + `'n'` + i64 + name(≤255B) | **< 300B** |
| ino/id 二级键 | + i64 | < 300B |
| data_ref 反查键 | + data_ref（内容键串） | < 400B |
| 锁主键 | `'l'` + 4×i64 | < 120B |
| 值（节点记录） | 11 列 tuple | **< 600B**（路径不入库，最长 name 255B 已在键侧） |

结论：本项目记录粒度（约 230 B/节点量级）与 FDB 限制余量四个数量级以上，**不存在需要分块值的形态**。

## 10. 二级索引"只增不改"清单（决策 D10）

允许新增键前缀（安全，不动既有数据）；**禁止**删除或调换既有键的字段顺序（= 全量搬迁）。
已冻结前缀：`'n'` 主键族、`'i'`(ino→名)、`'d'`(id→位置)、`'r'`(data_ref→名字)、`'k'`(仅目录)、
`'h'`(ino→名字集)、`'l'`/`'lo'`/`'ls'`/`'lx'`(锁)、`'c'`(ctrl)、`'m'`(分配器)、`'refs'` 共享账本。

## 11. 可后置参数（决策 D11/D12）

号段步长（初值 1024）、退避常数、重试次数、账本是否分桶、锁是否分桶、集群拓扑与副本因子、
`ssd`/`memory` 存储引擎选择、NVMe/SATA 档位——全部为压测后再定项，不影响键空间契约。

## 12. 已知的可归因差异（诚实负空间，P6 差分对拍须按此核对）

1. `getattr` 在硬链接多名下返回的 `name` 可能不同（PG `LIMIT 1` 未定义，FDB 取 name 序最小）——
   对拍须**按 ino 集合比对，不按 name 比对**。
2. 时间精度：`mtime/atime/ctime` 秒级（`now()` 取 `as_secs`，`src/pg_store.rs:58-64`），三后端一致，无差异。
3. `ino IS NULL` 这类"半写入行"在 FDB 下结构上不可能出现（写集原子提交），属**改进**而非差异。
4. id 空洞：FDB 号段丢弃的粒度是 1024（崩溃时），PG 序列丢弃粒度是单次 cache 步长——
   `id` 不对外可见，仅 `ino` 可见；迁移场景按 §4 的 `max()` 续接消除。

## 13. 验收清单（P1 出口判据）

- [x] §3.1–§3.9 九类映射齐备，每条带现有实现行号取证
- [x] §0 RecordLayer 取舍段存在且表述为"复刻语义、不用该绑定"
- [x] §9 键长核算表（视图名/路径/name 编码后均 < 键上限）
- [x] §5 单 op 写集上界与超出即回评审的硬线
- [x] §6 冲突分类与"绝不外泄"硬要求
- [x] §7 数据面边界逐项列名
- [x] §10 不可逆项冻结清单
- [ ] P0 环境门通过后方可进入 P2（本文件不判 G1）
