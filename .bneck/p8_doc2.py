#!/usr/bin/env python3
# P8-d: docs.html wording for the migration/rollback channel (mirrors p8_doc1.py).
#
# Same fact, same evidence: main.rs:2223/2296 deliberately skip the SQLite-only
# guard when the store is fdb://, and fdb_store.rs exports a subspace into the
# portable JSONL and imports one back (raising both allocator water marks the
# way pg_store.rs:464 does with setval). The three suites that run it are cited
# with their dual-tier numbers.
#
# What must NOT move: the refusals that are genuinely still refusals — the
# zero-copy `import`, `view rm --base`, `fork`, `refs rebuild`, `gc`.
import io

P = '/data/grovefs/grovefs/docs.html'
lines = io.open(P, encoding='utf-8').read().split('\n')


def find(marker, n=1):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == n, 'anchor %r hit %d times' % (marker[:60], len(idx))
    return idx[0]


def setline(marker, new):
    lines[find(marker)] = new


def patch(marker, old, new):
    i = find(marker)
    assert lines[i].count(old) == 1, 'sub-anchor %r on line %d: %d hits' % (old[:50], i, lines[i].count(old))
    lines[i] = lines[i].replace(old, new)


def insert_after(marker, block, offset=0):
    i = find(marker) + offset
    lines[i + 1:i + 1] = block.split('\n')


# ---- §5.9 三后端定位表: 一行新的「单视图搬运」+ 尚未提供的操作里去掉它 ----
ROW = '''        <tr><td>单视图元数据搬运（<code>view export</code> / <code>view import</code>）</td><td>可用：树就是一个 <code>&lt;V&gt;.db</code> 文件，导出即读它</td><td>不走这对命令：权威树在 schema 里，用 <code>pg_dump</code> / 逻辑复制（§16.2）</td><td><b>可用</b>：一个 subspace 导出成与 SQLite 档逐行同形的 JSONL，一份 dump 导入即<b>开出</b>该名字的 subspace（<code>view create</code> 之外的建树入口）；SQLite ⇄ FDB 双向是同一条命令对，往返 dump 逐行 <code>diff</code> 相同（<code>fdb</code> 套件 §12、<code>migrate</code>/<code>import</code> 套件的 fdb 档）</td></tr>'''
insert_after('<tr><td>引用计数账本</td>', ROW)

patch('<tr><td>尚未提供的操作</td>',
      '<td><code>view rm --base</code>/<code>view export</code>/<code>import</code>/<code>fork</code>/<code>refs rebuild</code> 全部按“拒绝而非降级”给出点名文案；删除 subspace 的命令本后端还没有，清理要走 FDB 侧运维工具</td>',
      '<td><code>view rm --base</code>/<code>import</code>（零拷贝纳管）/<code>fork</code>/<code>refs rebuild</code>/<code>gc</code> 全部按“拒绝而非降级”给出点名文案；删除 subspace 的命令本后端还没有，清理要走 FDB 侧运维工具。<b>不在这一列的</b>：<code>view export</code>/<code>view import</code> 在此档可用（上一行）</td>')

# ---- §11.2 换库那条 bullet ------------------------------------------------
patch('<li><b>中途换库是单向搬移</b>',
      '（目前只面向本地 SQLite 树）',
      '（面向本地 SQLite 树与 FDB subspace 两种权威；PG 的树在 schema 里，走 <code>pg_dump</code>）')

# ---- §9.3 后端差异清单里的 SQLite-only 断言 ------------------------------
setline('<li><b><code>view export/import</code> 面向本地 SQLite 树</b>',
        '        <li><b><code>view export/import</code> 面向本地 SQLite 树与 FDB subspace</b>：同一份 JSONL 两端通用，跨后端搬运与回滚都走这对命令（<a href="#s15-8-2">§14.8.2</a>）；PG 侧迁移用 <code>pg_dump</code> / 逻辑复制。</li>')

# ---- §14.0.1 建树入口表: 导入不再只产「本地」基树 ------------------------
patch('<tr><td><code>grovefs view --meta-dir D import --view V --in F</code></td>',
      '把 <code>view export</code> 的 JSONL 装成<b>新的本地基树</b>（保留 id/inode/硬链接）',
      '把 <code>view export</code> 的 JSONL 装成<b>新的基树</b>（保留 id/inode/硬链接）——SQLite 档是 <code>D/meta/V.db</code>，<code>--meta-store fdb://…</code> 档是集群里的 subspace')
patch('<tr><td>另一台机器 <code>view export</code> 出的 JSONL 想装回来</td>',
      '<td>目标端本地元数据根</td>',
      '<td>目标端存储根（本地元数据根，或 <code>--meta-store fdb://…</code> 的集群）</td>')

# ---- §14.7 顶层 import 的拒绝: 点名它指向的三步替代路线 ------------------
patch('<tr><td><code>... --meta-store URI</code>(或 <code>GROVEFS_META_STORE</code>)</td>',
      '报文本直接点名这条替代路径</td>',
      '报文本直接点名这条替代路径；已有目录要整批进集群走三步——本地 <code>import</code> 暂存 → <code>view export</code> → <code>view import --meta-store fdb://…</code>（<a href="#s15-8-2">§14.8.2</a>）</td>')
patch('语义要点(<code>import</code> 套件 34 断言)', '34 断言', '35 断言；<code>IMPORT_BACKEND=fdb</code> 档 47 断言')

# ---- §14.8.2: 跨后端搬运的命令序列 + 往返保真的判据 ---------------------
MOVE = '''    <p class="lead"><b>跨后端搬运：同一对命令，SQLite ⇄ FDB</b>（不新建工具）：一棵 SQLite 树导出的 JSONL 正是一个 subspace 导入的输入，反向同理——<code>view import --meta-store fdb://…</code> 在集群里<b>开出这个名字的 subspace</b>，<code>view export --meta-store fdb://…</code> 把一个 subspace 写成同一格式的 JSONL。</p>
    <pre><code><span class="cmt"># 源端：一棵 SQLite 树（挂载中也可导，读到的是快照）</span>
$ grovefs view --meta-dir /data/sqlite-store export --view team --out /tmp/team.jsonl
<span class="cmt"># 内容字节另走：交给目标部署的对象存储</span>
$ rsync -a -H /data/sqlite-store/blocks/ /data/cluster/blocks/
<span class="cmt"># 目标端：导入即在集群里开出 subspace，不必先 view create</span>
$ grovefs view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster import --view team --in /tmp/team.jsonl
<span class="cmt"># 回滚方向：同一个集群 dump 反向搬回一棵 SQLite 树</span>
$ grovefs view --meta-store fdb:///path/to/fdb.cluster --meta-dir /data/cluster export --view team --out /tmp/team2.jsonl
$ grovefs view --meta-dir /data/sqlite-store import --view team_rolledback --in /tmp/team2.jsonl</code></pre>
    <p class="lead"><b>往返不重写任何行</b>：导入后再从 subspace 导出的 dump 与源 dump <b>逐行相同</b>（<code>diff</code> 判据）——id/inode/硬链接/mode/属主/三个时间/symlink 文本都是按原值旅行，而不是被重新分配；导入时两个分配器水位抬到导出最大值（对位 PG 的 <code>setval</code>），迁移后新建节点的 id 严格大于导入侧的最大 id，不撞号；跨视图字节账本（FDB 是全局 <code>refs</code> subspace）在同一事务里随导入与 <code>--replace</code> 更新，被拒的 dump 整体不生效。迁移窗口内<b>源存储不被写入</b>，原树仍在，回滚就是这对命令反向跑一遍。<b>零拷贝性质不跨越搬运</b>：<code>import</code> 的「源文件与视图共享同一个 inode」只属于本地存储，搬进集群后视图读的是部署自己的对象，两侧编辑互不可见（<code>import</code> 套件的 fdb 档按这点做负向断言，而不是回避它）。PG 档没有这条通道：权威树在 schema 里，用 <code>pg_dump</code>/逻辑复制。</p>'''
insert_after('<b>导入是原子的</b>', MOVE)

# ---- §14.8.2 拓扑约束段的口径与回归数字 ----------------------------------
patch('<b>迁移的拓扑约束</b>',
      'PG 后端用 <code>pg_dump</code>/逻辑复制，<code>view export/import</code> 目前面向本地 SQLite 树',
      'PG 后端用 <code>pg_dump</code>/逻辑复制；<code>view export/import</code> 面向本地 SQLite 树与 FDB subspace 两种权威（上一段）')
patch('<b>迁移的拓扑约束</b>',
      '回归：<code>migrate</code> 套件 74 条断言',
      '回归：<code>migrate</code> 套件 SQLite 档 74 条断言、<code>MIGRATE_BACKEND=fdb</code> 档 71 条')

# ---- §15 存量目录纳管行: 拒绝之外给出三步 --------------------------------
patch('<tr><td>存量目录纳管</td>',
      '字节走挂载点 rsync 灌入（§14.7）',
      '字节走挂载点 rsync 灌入（§14.7），或本地 <code>import</code> 暂存后经 <code>view export</code>/<code>view import --meta-store fdb://…</code> 三步入库（§14.8.2）')

# ---- §16 回归清单双档数字 ------------------------------------------------
patch('<code>migrate</code> 套件元数据迁移 e2e 74 断言', 'e2e 74 断言', 'e2e 74 断言(SQLite 档;FDB 档 <code>MIGRATE_BACKEND=fdb</code> 71 断言)')
patch('<code>import</code> 套件零拷贝导入 e2e 34 断言', 'e2e 34 断言', 'e2e 35 断言(FDB 档 <code>IMPORT_BACKEND=fdb</code> 47 断言)')

# ---- 附录「能力边界」li: 边界按后端分列 + 双档数字 ----------------------
setline('<li><b>元数据导出/导入 <code>grovefs view export / import</code></b>',
        '      <li><b>元数据导出/导入 <code>grovefs view export / import</code></b>：只搬 <b>元数据树</b>，内容字节需另拷 <code>blocks/</code>；导出是宿主无关 JSONL（无绝对路径/设备号/主机名/宿主 inode），可跨机器、也可跨后端（SQLite ⇄ FDB subspace）迁移。导入先建临时树再原子 <code>rename</code>（SQLite 档），FDB 档整体在一个事务里写入、被拒的 dump 不留半成品；完成后更新跨视图字节账本，保留 id/inode(硬链接)/mode/uid/gid/时间/symlink 文本，并把分配器水位抬到导出最大值。<b>边界</b>：虚拟名(别名/派生)无自己的树，拒绝导出（导其物理基）；<code>--as-fork</code> 只在本地 SQLite 档有效（FDB 档拒绝，理由同 <code>fork</code>）；PG 后端的树用 <code>pg_dump</code>/逻辑复制；多盘 <code>--data-dir</code> 与条带 <code>s&lt;N&gt;:</code> 属<b>数据分布</b>——目标机需保留相同 root 数量/顺序或相同 data-server 数量/顺序，否则会映射错/明确拒绝；整目录冷拷会把 HA <code>epoch</code> 带过去，旧主必须先停。由 <code>migrate</code> 套件 74 条断言（SQLite 档）与 71 条（<code>MIGRATE_BACKEND=fdb</code> 档）、<code>fdb</code> 套件第 12 节的双向 <code>diff</code> 判据证明。</li>')

out = '\n'.join(lines)
io.open(P, 'w', encoding='utf-8').write(out)
print('docs p8-doc ok: %d lines' % len(lines))
