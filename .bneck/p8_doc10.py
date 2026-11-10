#!/usr/bin/env python3
# P8-audit closeout note. The audit changed no behaviour; it changed口径, so the
# keyspace doc's version log (the P1 rule is "评审后修改需记版本") must record
# what was wrong, what it says now, and the command that proves it.
import io

K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'
L = io.open(K, encoding='utf-8').read().split('\n')
assert '### v1.7' not in '\n'.join(L), 'already applied'
assert len([l for l in L if l.startswith('### v1.')]) == 7, 'unexpected version-note count'

V17 = '''### v1.7（P8 之后的逐项完成审计：改的是口径，不是行为）

审计把 Spec 的每条判据与当前工作树的实况对了一遍。代码零改动，纠了三处"文档写的和板子跑出来的不是一回事"：

| 项 | 之前 | 现在（取证） |
|---|---|---|
| 全板阶段数 | USAGE 三处分别写 31 / 33 / 32，docs.html 又写 31 与 32，两处互相矛盾 | `run_all.sh` 的 `CANON` 是 **35 段 = 34 门控（build + unit + 32 套件）+ `bench`**（`record bench INFO`，永不门控）。本轮全板 `RESULT: GREEN`、`BOARD_EXIT=0`，BOARD 块恰好 34 行 PASS |
| 套件清单 | 两份用户文档的清单都漏 `evict`、`fdb`；`run_all.sh` 头注释的 "Suites (canonical order)" 漏 `layout`/`viewrm`/`bwlimit`/`evict`/`control`/`fdb` 六个套件的说明 | 文档清单补齐；头注释补写六段说明（各带"无 PG / 无集群自动 SKIP"的口径）。改后 `SKIP_BENCH=1 bash .build-bin/run_all.sh fdb` → `RESULT: GREEN`、`fdb 169/0`，以此证明只动了注释、执行面未坏 |
| 各套件断言数 | docs.html 的专项套件表写 layout 78 / fork 70 / import 34 / pg 19 / control 14，且整张表里没有 `fdb` 一行 | 一律换成实测值：layout 81、fork 75、import 35、pg 18、control 16，并登记 `fdb 169`；`migrate 74（FDB 档 71）`、`import 35（FDB 档 47）`双档并列 |

顺带把 P4/P5/P8 的"双档复跑"以当前树重测一遍，数字与 §14 v1.4/v1.5 记录逐项相符，没有一项靠记忆：`LOCKS_BACKEND=fdb` 16/0、`LAYOUT_BACKEND=fdb` 82/0、`GOVCHECK_BACKEND=fdb` 29/0、`GOVTIME_BACKEND=fdb` 10/0、`APIKEY_BACKEND=fdb` 34/0、`MIGRATE_BACKEND=fdb` 71/0、`IMPORT_BACKEND=fdb` 47/0。

另两条本轮重新取证的判据：`cargo test --release --features fdb` 0 警告、EXIT=0（单元测试 151 通过，含三臂分派的三条与 `fdb_tree_ops_match_sqlite_semantics` 差分对拍）；P0 spike 用当前二进制重跑 `--reads 100000 --pairs 10000` → `P0_SPIKE_VERDICT=PASS`，本轮 `retries=2330`——与上面记的 2705 一样都只是单次运行值，不是常量，G1 判据要的是"重试次数 > 0 且 `gave_up=0`"。集群侧 `fdbcli --exec "status minimal"` 重答 `The database is available.`。
'''

head = [i for i, l in enumerate(L) if l.startswith('## 15. 决策门处置')]
assert len(head) == 1, '§15 heading hit %d' % len(head)
L[head[0]:head[0]] = V17.split('\n')
out = '\n'.join(L).rstrip('\n') + '\n'
assert '“' not in out and '”' not in out, 'curly quotes leaked into a straight-quote file'
io.open(K, 'w', encoding='utf-8').write(out)
print('keyspace: v1.7 audit note added before section 15')
