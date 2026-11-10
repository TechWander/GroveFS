#!/usr/bin/env python3
"""G3 measured FAIL, so R3's disposition has to reach the board: `fdb` leaves CANON.

Why this is the faithful reading and not a convenience:
  * the gate is P4 末 and its criterion is "不劣于 PG 平台值 ~4.6k ops/s"; on the same
    host, in the same session, over the same op sequence, FDB peaked at 893 ops/s
    (ssd engine) and 1642 ops/s (memory engine, i.e. with the disk removed from the
    commit path), while the PG arm reproduced 4.2-4.9k ops/s. The gate cannot be
    certified here, and the shortfall is not only the media;
  * R3's 改走路线 is explicit: 停在 feature 关闭的实验分支、**不入 CANON**、不入文档承诺,
    对外口径回到 "SQLite/PG + 分片";
  * P6 asked for `fdb` to be registered in CANON, but that phase sits *after* G3, so the
    gate decides what P6 registers. The suite itself stays runnable **by name** — the
    experiment's evidence is exactly what a re-run on SSD hardware will need, and a
    default-off feature that cannot be exercised is not an experiment any more.

Anchors are verified before anything is written.
"""
import io

F = '/data/grovefs/grovefs/.build-bin/run_all.sh'
T = io.open(F, encoding='utf-8').read()

Edits = []

# 1) the suite list: fdb is no longer a canonical stage
Edits.append((
    'CANON="build unit posix edge e2e remote errors resilience concurrent metasrv ha striping locks freshness disp_fresh layout fork viewrm gc bwlimit migrate import govcheck govtime grovesci mcp apikey mountkey config pg pg_parallel evict control fdb bench"',
    '# `fdb` is NOT in CANON: the G3 performance gate (docs/fdb-keyspace.md §15) measured\n'
    '# fail on 2026-11-09 — 893 ops/s against a 4.6k ops/s bar, same host and same op\n'
    '# sequence as the PG arm, which reached 4.2-4.9k in the same session (and 1642 ops/s\n'
    '# even with the disk taken out of the commit path, so the gap is not only media).\n'
    '# Per the plan\'s R3 disposition the backend stays a default-off experiment: reachable\n'
    '# by name, never part of the gating board, and promised nowhere in the user docs.\n'
    'CANON="build unit posix edge e2e remote errors resilience concurrent metasrv ha striping locks freshness disp_fresh layout fork viewrm gc bwlimit migrate import govcheck govtime grovesci mcp apikey mountkey config pg pg_parallel evict control bench"\n'
    '# EXPERIMENTAL suites gate nothing but can still be selected explicitly.\n'
    'EXPERIMENTAL="fdb"',
))

# 2) name validation must keep accepting the experimental one
Edits.append((
    '''for s in $SEL; do
  case " $CANON " in *" $s "*) ;; *) echo "unknown suite '$s'; valid: $CANON"; exit 2 ;; esac
done''',
    '''for s in $SEL; do
  case " $CANON $EXPERIMENTAL " in *" $s "*) ;; *) echo "unknown suite '$s'; valid: $CANON $EXPERIMENTAL"; exit 2 ;; esac
done''',
))

# 3) the board table has to show it when it was asked for by name
Edits.append((
    '''for s in $CANON; do
  should "$s" || continue
  printf "  %-7s %s\\n" "$s" "${RESULT[$s]:-SKIP}"
done''',
    '''for s in $CANON $EXPERIMENTAL; do
  should "$s" || continue
  printf "  %-7s %s\\n" "$s" "${RESULT[$s]:-SKIP}"
done''',
))

# 4) the header: the fdb entry moves under an explicit experimental heading
Edits.append((
    '''#   fdb         FoundationDB metadata backend e2e (`--meta-store fdb://<cluster
#               file>`): subspace provisioning, the tree body, the lock surface,
#               derived/alias governance and an explicit fork verdict, the two-way
#               `view export`/`view import` migration chain (ids/inodes kept), and
#               what `--engine-idle-secs` means on this backend. Registers SKIP
#               when no cluster is reachable; FDB_TEST=1 turns that absence into
#               a FAIL (CI)''',
    '''#
# Experimental — selected by name only, never part of the gating board:
#   fdb         FoundationDB metadata backend e2e (`--meta-store fdb://<cluster
#               file>`): subspace provisioning, the tree body, the lock surface,
#               derived/alias governance and an explicit fork verdict, the two-way
#               `view export`/`view import` migration chain (ids/inodes kept), and
#               what `--engine-idle-secs` means on this backend. Registers SKIP
#               when no cluster is reachable; FDB_TEST=1 turns that absence into
#               a FAIL (CI). OUT OF CANON because the G3 throughput gate failed on
#               this host (893 ops/s vs the 4.6k ops/s PG platform value measured in
#               the same session); the functional verdict below stands, the capacity
#               verdict does not. Run it with:
#                 bash .build-bin/run_all.sh fdb''',
))

texts = {}
for old, _ in Edits:
    assert T.count(old) == 1, 'anchor hit %d: %r' % (T.count(old), old[:60])
for old, new in Edits:
    T = T.replace(old, new)
io.open(F, 'w', encoding='utf-8').write(T)
print('run_all.sh: fdb out of CANON, kept selectable as EXPERIMENTAL')
