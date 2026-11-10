#!/usr/bin/env python3
# P8-d: USAGE.md wording for the migration/rollback channel.
#
# The fact being corrected is not stylistic. `view export` / `view import` are
# implemented for the fdb:// store (main.rs:2223/2296 skip the SQLite-only guard
# for it, fdb_store.rs:2521 export_jsonl + the import that raises the allocator),
# and three suites prove it: fdb §12 (158/0, SQLite -> subspace -> back to SQLite),
# migrate under MIGRATE_BACKEND=fdb (71/0), import under IMPORT_BACKEND=fdb (47/0).
# The docs still described that pair as refused on this backend, which is a
# negative claim about a command that works — the worst kind of stale, because an
# operator reading it would reach for rsync on a whole tree instead.
#
# Doc rules honoured here: state current behaviour only (no "now supports"), and
# keep the refusal rows that ARE still true (`import`, `view rm --base`, `fork`,
# `gc`) untouched in substance.
import io

P = '/data/grovefs/grovefs/USAGE.md'
lines = io.open(P, encoding='utf-8').read().split('\n')


def find(marker, n=1):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == n, 'anchor %r hit %d times' % (marker[:60], len(idx))
    return idx[0]


def setline(marker, new):
    lines[find(marker)] = new


def patch(marker, old, new):
    i = find(marker)
    assert lines[i].count(old) == 1, 'sub-anchor %r in line %d: %d' % (old[:50], i, lines[i].count(old))
    lines[i] = lines[i].replace(old, new)


def insert_after(marker, block, offset=0):
    i = find(marker) + offset
    lines[i + 1:i + 1] = block.split('\n')


# ---- 1) §3 命令落盘表: export/import 不再只是 SQLite 的一行 ------------------
setline('| `grovefs view export/import` | 导出:一个 JSONL 文件',
        '| `grovefs view --meta-dir D export/import --view V` | 导出:一个 JSONL 文件;导入:新建 `D/meta/V.db`(带 `--meta-store fdb://…` 时导入开出一个 subspace,§6.3) | 不搬字节(字节要另拷 `blocks/` 或交给 data-server 的数据根) |')

# ---- 2) §3.4.2: the cross-backend move, in the existing pair of commands ----
BLOCK = '''
**同一对命令也跨后端**（SQLite ⇄ FDB，不需要第三个工具）：一棵 SQLite 树导出的 JSONL 正是一个 subspace 导入的输入，反向同理——`view import --meta-store fdb://…` 在集群里**开出这个名字的 subspace**（`view create` 之外唯一的建树入口），`view export --meta-store fdb://…` 把一个 subspace 写成同一格式的 JSONL。字节仍各走各的对象存储（`rsync -a` 数据根，或让 data-server 直接服务同一批对象）：

```bash
S=/data/sqlite-store; C=/data/cluster; CL=fdb:///path/to/fdb.cluster
grovefs view --meta-dir $S export --view team --out /tmp/team.jsonl
rsync -a -H $S/blocks/ $C/blocks/                        # 内容字节另走,元数据已在 JSONL 里
grovefs view --meta-store $CL --meta-dir $C import --view team --in /tmp/team.jsonl
# 回滚方向:同一个集群 dump 再搬回一棵 SQLite 树
grovefs view --meta-store $CL --meta-dir $C export --view team --out /tmp/team2.jsonl
grovefs view --meta-dir $S import --view team_rolledback --in /tmp/team2.jsonl
```

往返不重写任何行:导入后从 subspace 再导出的 dump 与源 dump **逐行相同**(`diff` 判据),所以 id/ino/硬链接/mode/属主/三个时间/symlink 文本都是按原值旅行的,而不是被重新分配;导入时两个分配器水位抬到导出最大值(PG 档 `setval` 的同位动作),迁移后新建的节点 id 严格大于导入侧的最大 id,不会撞号。共享字节账本(FDB 是集群里的 `refs` subspace)在同一事务里随导入/`--replace` 更新,失败则整体不生效——一棵树要么原样、要么完整是新树。迁移窗口内**源存储不被写入**,原树仍在,回滚就是同一对命令反向跑一遍。PG 档没有这条通道:它的权威树在 schema 里,走 `pg_dump`/逻辑复制(§15.9)。

e2e:`fdb` 套件第 12 节(SQLite→subspace→SQLite 全链,含逐行 `diff`、水位抬升、守卫与源不动)、`migrate` 套件(`MIGRATE_BACKEND=fdb` 档)、`import` 套件(`IMPORT_BACKEND=fdb` 档,把这条拒绝之外的替代路线端到端跑通)。'''
insert_after('/data/dst mount --view team', BLOCK, offset=1)

# ---- 3) §3.4.2 拓扑约束里的 PG 那条: 口径按后端分列 -------------------------
setline('- PG 后端不能用文件拷贝,用 `pg_dump`/逻辑复制;',
        '- PG 后端不能用文件拷贝,用 `pg_dump`/逻辑复制——`view export/import` 面向本地 SQLite 树与 FDB subspace 两种权威,PG 的树在 schema 里不走这对命令(§15.9)。')

# ---- 4) §3.4.2 e2e 行: 双档数字 --------------------------------------------
setline('e2e 见 `migrate` 套件(74 条断言',
        'e2e 见 `migrate` 套件(SQLite 档 74 条断言;`MIGRATE_BACKEND=fdb` 把同一套断言对着集群复跑,71 条)。断言覆盖:挂载中导出/宿主无关性/虚拟名与覆盖守卫/跨绝对路径导入/硬链接与权限保真/原子替换/损坏输入拒绝/`--as-fork` 账本共享/多基视图独立(`--as-fork` 与「两个存储根」两事在 FDB 档按各自边界改判,见 §15.9)。')

# ---- 5) §3.3 import 套件的断言数 + FDB 档把替代路线跑通 --------------------
patch('语义要点(`import` 套件 34 条断言)', '语义要点(`import` 套件 34 条断言',
      '语义要点(`import` 套件 35 条断言;`IMPORT_BACKEND=fdb` 档 47 条')
patch('语义要点(`import` 套件 35 条断言', '`view fork`/`view rm --base` 同一条拒绝规则,`import`/`govcheck` 套件实证)。',
      '`view fork`/`view rm --base` 同一条拒绝规则,`import`/`govcheck` 套件实证)。FDB 档除了把这条拒绝本身按后端逐条复证(含 `--dry-run` 之前即拒、`GROVEFS_META_STORE` 同拒、拒绝不落任何影子 db),还把拒绝点名的替代路线端到端跑通:本地 `import` 只当暂存,真正入集群的是 `view export` → 交字节 → `view import --meta-store fdb://…`;搬完之后「源文件与视图共享 inode」这一零拷贝性质在两个方向上都不成立(视图读的是部署自己的对象),套件按这点做负向断言而不是回避。')

# ---- 6) §6.3: 迁移与回滚是同一对命令 ---------------------------------------
B63 = '''- **迁移与回滚走同一对命令**：`view export` / `view import` 在此档可用，且与 SQLite 档产出**同一种**宿主无关 JSONL——一个 subspace 能被导出，一份 dump 能在 `view create` 之外开出一个 subspace，导入后从集群再导出的 dump 与源 dump 逐行相同（id/ino/硬链接/权限/属主/时间原值旅行），两个分配器水位抬到导出最大值。回滚到 SQLite 就是同一份 dump 反向导入，迁移窗口内源存储不被写入、原树保留。字节不在这条通道上：它们归 data-server 的数据根（§3.4.2 有整条命令序列）。'''
insert_after('- **树是显式建的**', B63)

# ---- 7) §11.10 部署教程: 补一步「把已有 SQLite 树搬进来」 ------------------
STEP5 = '''
# 5) 把一棵已有的 SQLite 树搬进集群(不新建工具;目标名不必先 view create,导入即开 subspace):
$Y view --meta-dir /data/sqlite-store export --view team --out /tmp/team.jsonl
rsync -a -H /data/sqlite-store/blocks/ /data/cluster/blocks/
$Y view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster import --view team --in /tmp/team.jsonl'''
insert_after('cluster create --view team', STEP5)
patch('控制面(视图定义/别名/API key)仍在', 'FDB 下的命令边界见 §15.9。',
      'FDB 下的命令边界见 §15.9。第 4 步的 `view create` 与第 5 步的 `view import` 是此档仅有的两个建树入口(挂载不建树);第 5 步反向跑就是回滚。')

# ---- 8) §15.9 FDB 档三行: 两行是真的拒绝,一行是曾经写反了的断言 -----------
setline('| `view export` / `view import`（单视图元数据搬运）',
        '| `view export` / `view import`（单视图元数据搬运） | **可用**（跨后端与同后端的迁移通道，§3.4.2） | 导出把一个 subspace 写成与 SQLite 档逐行同形的宿主无关 JSONL（表头 + 每节点一行，无绝对路径/主机名/`blocks/` 路径）；导入在集群里**开出这个名字的 subspace**（`view create` 之外唯一的建树入口），保留 id/ino/硬链接/mode/uid/gid/时间/symlink 文本，同事务更新共享字节账本，并把两个分配器水位抬到导出最大值（对位 PG 的 `setval`）；目标已有树需 `--replace`，坏 dump 整体拒绝、不留半成品；`--as-fork` 在此档拒绝（理由同 `view fork` 行） | 字节另走对象存储：`rsync -a` 数据根，或让 data-server 服务同一批对象（§14.2） |')
setline('| `view rm --base`（连字节删整个基视图）',
        '| `view rm --base`（连字节删整个基视图） | 拒绝 | ``view rm --base operates on a local SQLite view db; under `--meta-store fdb://…` a base tree is a subspace of the FoundationDB cluster (create one with `view create`) and its bytes move through a mounted path — drop --meta-store for a local store`` | 目前没有能删除一个 subspace 的命令；`--meta-dir` 下的定义可先用 `view rm --name` 清 |')
patch('| `import`（零拷贝） | 拒绝 |', '挂载 + `rsync`/`cp` 走挂载点（§14.2） |',
      '挂载 + `rsync`/`cp` 走挂载点（§14.2）；已有目录批量入库走三步：本地 `import` 暂存 → `view export` → `view import --meta-store fdb://…`（§3.4.2） |')

out = '\n'.join(lines)
io.open(P, 'w', encoding='utf-8').write(out)
print('usage p8-doc ok: %d lines' % len(lines))
