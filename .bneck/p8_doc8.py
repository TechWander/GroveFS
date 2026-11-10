#!/usr/bin/env python3
"""Audit finding: the board's gating-stage counts and the per-suite assertion
counts in the two user docs had drifted behind reality — and not by small
amounts. Measured against the board snapshot in .build-bin/logs (34 gating
stages = build + unit + 32 suites, bench informational):

    doc said            board says   which
    33 段 / 32 门控       35 / 34      run_all.sh CANON + the BOARD block
    31 个门控阶段 (x3)     34           "
    29 / 30 套件           32           "
    suite lists without evict/fdb   both are in CANON and run on the board
    layout 78              81          logs/*/layout.log  (78 before P5-b parameterised it)
    fork 70                75          logs/*/fork.log
    import 34 (summary)    35          logs/*/import.log ("SUMMARY import: 35 passed")
    pg 19                  18          logs/*/pg.log     (18 in every recent run)
    control 14             16          logs/*/control.log
    no fdb entry at all    169         .build-bin/test_fdb.sh SUMMARY (sections 1-13)

Every replacement below is a substring the file contains exactly the number of
times asserted, so a stale anchor fails loudly instead of silently skipping.
Nothing is written until all anchors for both files have validated.
"""
import io
import re

U = '/data/grovefs/grovefs/USAGE.md'
D = '/data/grovefs/grovefs/docs.html'

FDB_SKIP = (
    '`fdb` 同理:主机上无可达 FoundationDB 集群时整套自动 SKIP,'
    '置 `FDB_TEST=1` 时缺集群即判 FAIL(不允许把没跑过的后端算作通过)。'
)

FDB_DOC = (
    ';<code>fdb</code> 套件 FoundationDB 元数据后端 e2e 169 断言'
    '(<code>serve()</code> 在这一档直接拒绝 grovefs 原生 HA 并给出独立文案/'
    '一个视图一个 subspace 的建树与读写/rename·覆盖·EEXIST·ENOTEMPTY·硬链接·符号链接/'
    '两个挂载点共用同一棵权威树/<code>du</code> 复用同一份树 dump 工件/'
    '离线本地命令遇到 <code>fdb://</code> 存储按后端拒绝/'
    'POSIX 咨询锁四个方法跨两挂载/派生透镜与纯别名治理(含 <code>fork</code>·<code>graft</code> '
    '在这一档的显式裁决——按名拒绝而不留半可用的树)/卸载重挂内容逐字节相同/'
    '§12 <code>view export</code>→<code>view import</code> 的 SQLite→FDB→SQLite 双向迁移链'
    '(id·inode 保留原值,挂载侧 <code>stat -c %i</code> 必须等于导出侧的 <code>ino</code>,'
    '属性 TTL 过期后重读同一编号,源存储未被写入)/'
    '§13 本档的驱逐语义(<code>--engine-idle-secs</code> 只剩内存可回收:挂载中不驱逐、'
    '卸载后回收、重挂对客户端透明)——主机上无可达 FoundationDB 集群时自动 SKIP、不拉低 GREEN,'
    '置 <code>FDB_TEST=1</code> 时缺集群即判 FAIL)'
)

USE = []   # (path, old, new, expected_hits)

# ------------------------------------------------------------------- USAGE.md
USE += [
    (U, '全板一共 **33 段**', '全板一共 **35 段**', 1),
    (U, '**30 个套件**', '**32 个套件**', 1),
    (U, '**前 32 段门控 GREEN', '**前 34 段门控 GREEN', 1),
    (U, '在无可达 PostgreSQL 的主机上自动 SKIP,不拉低 GREEN。',
        '在无可达 PostgreSQL 的主机上自动 SKIP,不拉低 GREEN。' + FDB_SKIP, 1),
    (U, '# 全板 33 段(32 门控 + bench)', '# 全板 35 段(34 门控 + bench)', 1),
    (U, 'config pg pg_parallel control', 'config pg pg_parallel evict control fdb', 1),
    (U, '(31 个门控阶段)', '(34 个门控阶段)', 2),
]
# ------------------------------------------------------------------ docs.html
USE += [
    (D, '<b>31 个门控阶段</b>', '<b>34 个门控阶段</b>', 1),
    (D, 'build+unit+29 套件', 'build+unit+32 套件', 1),
    (D, '全板 33 段 = 32 门控 + bench', '全板 35 段 = 34 门控 + bench', 1),
    (D, 'config pg pg_parallel control', 'config pg pg_parallel evict control fdb', 1),
    (D, '全板 32 个门控阶段', '全板 34 个门控阶段', 1),
    (D, 'build + unit + 30 套件', 'build + unit + 32 套件', 1),
    (D, '下面这十三个专项套件', '下面这十四个专项套件', 1),
    (D, '显示透镜 e2e 78 断言', '显示透镜 e2e 81 断言', 1),
    (D, '独立元数据树 e2e 70 断言', '独立元数据树 e2e 75 断言', 1),
    (D, 'PostgreSQL 后端 e2e 19 断言', 'PostgreSQL 后端 e2e 18 断言', 1),
    (D, '控制面统一 e2e 14 断言', '控制面统一 e2e 16 断言', 1),
    (D, '同样无 PG 自动 SKIP)。</p>', '同样无 PG 自动 SKIP)' + FDB_DOC + '。</p>', 1),
]

# The <summary> line carries its own per-suite tally; rewrite it whole (it is one
# line, and the tally order is the docs' own — not the board's).
SUM_OLD_RE = re.compile(r'^(\s*)<summary>断言明细：.*</summary>\s*$', re.M)
SUM_NEW = ('  <summary>断言明细：透镜 81 / fork 75 / viewrm 42 / gc 41 / '
           'migrate 74（FDB 档 71）/ import 35（FDB 档 47）/ mcp 77 / apikey 33 / '
           'mountkey 76 / config 28 / pg 18 / control 16 / bwlimit 17 / fdb 169</summary>\n')

texts = {p: io.open(p, encoding='utf-8').read() for p in (U, D)}
fail = []
for path, old, new, hits in USE:
    got = texts[path].count(old)
    if got != hits:
        fail.append('%s: anchor %r hit %d, expected %d' % (path, old[:44], got, hits))
if fail:
    raise SystemExit('ABORT, nothing written:\n  ' + '\n  '.join(fail))

for path, old, new, hits in USE:
    texts[path] = texts[path].replace(old, new)

m = list(SUM_OLD_RE.finditer(texts[D]))
assert len(m) == 1, 'summary line hit %d' % len(m)
assert '透镜 78 / fork 70' in m[0].group(0), 'unexpected summary content: %r' % m[0].group(0)[:80]
texts[D] = texts[D][:m[0].start()] + SUM_NEW + texts[D][m[0].end() + 1:]

for path in (U, D):
    io.open(path, 'w', encoding='utf-8').write(texts[path])
print('USAGE.md + docs.html: stage counts 33/32->35/34, 31->34, suite lists + evict/fdb, '
      'assertion tallies 78->81 70->75 34->35 19->18 14->16, fdb 169 registered')
