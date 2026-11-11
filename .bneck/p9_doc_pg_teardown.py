#!/usr/bin/env python3
"""Align docs.html with the PG teardown/ledger CLI that now exists.

The PG backend used to be the one engine a view could not be removed from:
`view rm --base` refused it, `refs rebuild` had no PG path, and the only way
out was `.bneck/pg_drop_views.sh`.  This change adds
`PgEngine::remove_view` (unbook + DROP SCHEMA in ONE transaction) and
`rebuild_ledger` (recount `grovefs_refs.holders` from the live schemas), and
wires both to the CLI.  Every sentence below that said "PG 拒 / 别跑 / 用脚下
脚本" is therefore not merely stale but backwards, so it is rewritten here
rather than annotated.

The engine matrix (`.bneck/engine_matrix.sh`, 14 capabilities x 3 engines) is
added to §15.5 because "does this engine have that capability" was previously
answered only by prose.

usage: python3 .bneck/p9_doc_pg_teardown.py
"""
import re
import sys

P = "/data/grovefs/grovefs/docs.html"
s = open(P, encoding="utf-8").read()
orig = s
edits = []


def sub(pattern, repl, count=1, flags=0):
    """Replace `pattern` exactly `count` times; abort if the count differs."""
    global s
    new, n = re.subn(pattern, repl, s, flags=flags)
    if n != count:
        sys.exit(f"PATTERN MATCHED {n}x (want {count}): {pattern[:90]!r}")
    s = new
    edits.append(pattern[:70])


# --- 1. quick-reference row: ledger repair is the same command on all engines
sub(r"<td><code>refs rebuild</code>（SQLite）；PG 后端走 "
    r"<code>\.bneck/pg_drop_views\.sh --rebuild-ledger</code></td>",
    "<td><code>refs rebuild</code>（三档同名：SQLite 直接跑；PG 加 "
    "<code>--meta-store postgres://…</code>（重算 <code>grovefs_refs.holders</code>）；"
    "FDB 加 <code>--meta-store fdb://…</code>）</td>")

# --- 2. the PG-vs-FDB-vs-SQLite locating table: base-view teardown row
sub(r"<tr><td>基视图删除</td>.*?</tr>",
    "<tr><td>基视图删除</td><td><code>view rm --base</code> 可行（删 "
    "<code>meta/&lt;V&gt;.db</code>，按现存树重建 <code>refs.db</code>，回收无人持有的字节）</td>"
    "<td><code>view rm --base</code> 可行——<b>一个事务</b>里先销账（逐 key 减 "
    "<code>grovefs_refs.holders</code>，归零即删行）再 "
    "<code>DROP SCHEMA grovefs_v_&lt;hex&gt; CASCADE</code>，提交后按删除前读到的 "
    "<code>data_ref</code> 回收 <code>blocks/</code> 字节。PG 的 DDL 是事务性的，"
    "没有“树删了账还在”的中间态；裸 <code>DROP SCHEMA</code> 则相反：账本永远多计，"
    "且该视图的 ref 清单随 schema 一起消失（§13.3）</td>"
    "<td><code>view rm --base</code> 可行——清空该 subspace、按幸存树重算共享账本、"
    "释放无人持有的对象（§5.9 末）</td></tr>")

# --- 3. capability table: what PG still does not do
sub(r"<td><code>view rm --base</code> 拒，删除由 <code>\.bneck/pg_drop_views\.sh</code> 承担"
    r"（先销账再 <code>DROP SCHEMA … CASCADE</code>，加 <code>--rebuild-ledger</code> "
    r"顺带按存活树重算整本账；§13\.3）</td>",
    "<td><code>view fork</code>、<code>view export</code>/<code>view import</code>、"
    "零拷贝 <code>import</code>、<code>gc --apply</code>、<code>view scan</code>/"
    "<code>view lease</code>、原生 HA 均按“拒绝而非降级”给出点名文案。"
    "<b>可用的</b>：<code>view rm --base</code>（事务性销账 + <code>DROP SCHEMA</code> + "
    "字节回收）、<code>refs rebuild</code>（按存活树重算 <code>grovefs_refs.holders</code>）、"
    "<code>view define/alias/rm --name</code>、<code>apikey</code>、<code>mcp</code>（§13.3）</td>")

# --- 4. §13.3 ledger paragraph
sub(r"PG 后端的等值配方是 <code>\.bneck/pg_drop_views\.sh --yes --rebuild-ledger</code>，"
    r"逐条对位 <code>src/meta/engine\.rs</code> 里的 <code>rebuild_ledger</code>，"
    r"它不保留任何无树留名的行，而「无账本行的键即该视图私有」决定了重算只会把过计归位。",
    "PG 后端是<b>同一条命令</b>（<code>refs --meta-store postgres://… rebuild</code>）："
    "枚举 <code>grovefs_v_*</code> schema、逐树取 <code>SELECT DISTINCT data_ref</code>、"
    "按“还有几棵树命名它”重写 <code>grovefs_refs.holders</code>，删掉没有任何树认领的行。"
    "一棵树读失败就整体中止，所以方向永远是“多留字节”而不是少算。")

# --- 5. §14.8.3 guard ④ + the PG path paragraph that follows it
sub(r"④ <code>--meta-store postgres://…</code> 部署直接拒（基树是 PG schema，"
    r"字节由 PG/data-server 自持，删除由 <code>\.bneck/pg_drop_views\.sh</code> 承担"
    r"——它<b>先销账再</b> <code>DROP SCHEMA … CASCADE</code>，§13\.3）。</p>",
    "④ <b>PG 档走同一条命令</b>（<code>view --meta-store postgres://… rm --base</code>）："
    "基树是一个 schema，<code>remove_view</code> 在<b>一个事务</b>里逐 key 减 "
    "<code>grovefs_refs.holders</code>（归零即删行）再 <code>DROP SCHEMA … CASCADE</code>，"
    "提交后按 <code>unbooked_refs</code> 回收字节。绕过 CLI 手工 <code>DROP SCHEMA</code> "
    "留下的账本行能靠 <code>refs rebuild</code> 归位，但 ref 清单随 schema 消失、字节从此没有"
    "记录可回收（§13.3）。</p>\n"
    '    <p class="lead"><b>PG 档为什么可以“一条命令到底”</b>：PostgreSQL 的 DDL 是事务性的，'
    "所以“销账 + 删树”能一次提交，崩溃也不会落到“树删了、账还在”；SQLite 档做不到，才有了"
    "“删 db 再 <code>refs rebuild</code>”的两步。e2e 见 <code>.bneck/pg_view_teardown.sh</code>"
    "（<b>44 条断言</b>，真 FUSE 挂载 + 真 data-server + 真 PG：写入后 schema/账本/字节三方对账 → "
    "事务性删除 → 二次删除被拒 → out-of-band drop 后 <code>refs rebuild</code> 的两种方向 → "
    "残留恰好等于不可回收的那一部分），以及 <code>test_pg.sh</code>/<code>test_pg_parallel.sh</code> "
    "末尾的自清场（此前每跑一次漏一个 schema）。</p>")

# --- 6/7. two “磁盘没长 / 越界删 schema” rows
sub(r"<code>refs --meta-dir D rebuild</code>；PG 下用 "
    r"<code>\.bneck/pg_drop_views\.sh --yes --rebuild-ledger</code>",
    "<code>refs --meta-dir D rebuild</code>；PG 档加 <code>--meta-store postgres://…</code> "
    "跑同一条（按存活树重算 <code>grovefs_refs.holders</code>）")

sub(r"PG 下越界\s*删 schema 攒下的孤儿行，用 <code>\.bneck/pg_drop_views\.sh --yes "
    r"--rebuild-ledger</code> 按存活树重算。",
    "PG 下越界删过 schema 时，用 <code>grovefs refs --meta-store postgres://… rebuild</code> "
    "按存活树重算（多计的行删掉；被误删的 <code>holders</code> 行也能从树恢复）。")

# --- 8. §16.8 known-limits bullet
sub(r"受\s*治名/活挂载/PG 部署均拒绝，详 §14\.8\.3",
    "受治名/活挂载均拒绝，PG 档走同一条命令（§14.8.3）")

# --- 9. speed-lookup table: ledger rebuild row
sub(r"PG 后端的等值做法是 <code>\.bneck/pg_drop_views\.sh --yes --rebuild-ledger</code>"
    r"（配方，非 CLI；§13\.3）",
    "PG 后端用 <code>grovefs refs --meta-store postgres://… rebuild</code>"
    "（CLI；静止时跑；§13.3）")

# --- 10. speed-lookup row for `view rm --base`: PG is no longer refused
sub(r"受治名/依赖\s*定义/本机活挂载/<code>--meta-store postgres://…</code> 均拒绝，见 §14\.8\.3",
    "受治名/依赖定义/本机活挂载均拒绝，PG 档走同一条命令（销账 + <code>DROP SCHEMA</code> + "
    "字节回收），见 §14.8.3")

# --- 11. §15.5: the completeness matrix (14 capabilities x 3 engines)
MATRIX = """    <h4>三引擎完备性矩阵：同一串治理序列，三个后端各跑一遍</h4>
    <p class="lead"><code>bash .bneck/engine_matrix.sh</code> 把<b>同一串治理命令</b>分别在 SQLite / PostgreSQL / FoundationDB 上执行，14 项能力并排成一张表：<b>OK</b>=这一档能用，<b>REFUSED</b>=这一档设计上不支持（引拒绝原文），<b>ERROR</b>=缺口。跑完再核对三个存储是否真的空了——<code>rmbase</code> 是表里的一行而不是收尾细节，因为“删不掉的引擎”和“跑一次漏一个 schema 的套件”曾经就是真的。最近一次：14 行全部 OK/REFUSED，<b>无 ERROR</b>，<code>TEARDOWN CLEAN</code>（sqlite <code>meta/*.db</code> 0、pg schema 0、pg 账本 0、fdb <code>no subspaces</code>）。</p>
    <table>
      <thead><tr><th>能力</th><th>SQLite</th><th>PostgreSQL</th><th>FoundationDB</th></tr></thead>
      <tbody>
        <tr><td><code>view create</code> / <code>define</code> / <code>alias</code> / <code>list</code> / <code>rm --base</code></td><td>OK</td><td>OK</td><td>OK</td></tr>
        <tr><td><code>refs rebuild</code>（跨视图账本重算）</td><td>OK</td><td>OK</td><td>OK</td></tr>
        <tr><td><code>view export</code> / <code>view import</code>（元数据搬运）</td><td>OK</td><td>REFUSED</td><td>OK</td></tr>
        <tr><td><code>view fork</code>（独立物理树）</td><td>OK</td><td>REFUSED</td><td>REFUSED</td></tr>
        <tr><td><code>grovefs import</code>（目录→视图，零拷贝）</td><td>OK</td><td>REFUSED</td><td>REFUSED</td></tr>
        <tr><td><code>gc</code>（离线孤儿回收）</td><td>OK</td><td>REFUSED</td><td>REFUSED</td></tr>
        <tr><td><code>view scan</code> / <code>view lease</code>（集群侧清点与租约）</td><td>REFUSED</td><td>REFUSED</td><td>OK</td></tr>
        <tr><td>原生 HA（<code>--role standby</code>）</td><td>OK</td><td>REFUSED</td><td>REFUSED</td></tr>
      </tbody>
    </table>
    <p class="lead">两列 REFUSED 都是<b>设计边界</b>而非缺口，每条都有“为什么”和替代做法（§13.3 逐行写全）；三档共同拥有的是<b>控制面 + 树 + 账本 + 基视图拆除</b>这四件事，也就是一个元数据引擎至少要有的那一部分。跨机专项另有 <code>.bneck/fdb3vm_lease.sh</code>（四机集群的租约/故障域/时钟偏移，59 pass）与 <code>.bneck/pg_view_teardown.sh</code>（PG 事务性删除与账本双向修复，44 pass）。</p>
"""
sub(r"(bash \.build-bin/run_all\.sh fdb[^\n]*\nSKIP_BENCH=1 bash \.build-bin/run_all\.sh[^\n]*</code></pre>\n)",
    lambda m: m.group(1) + MATRIX)

if s == orig:
    sys.exit("nothing changed")
open(P, "w", encoding="utf-8").write(s)
print(f"docs.html: {len(edits)} edits applied")
for e in edits:
    print("  ", e)
