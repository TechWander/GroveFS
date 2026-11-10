#!/usr/bin/env python3
"""`.build-bin/run_all.sh` describes every suite it runs in its "Suites (canonical
order)" header, and six of them had been added to CANON without ever being
described: layout, viewrm, bwlimit, evict, control, fdb. A reader of the header
could not tell what the board runs, and could not tell that `evict`/`control`
self-skip without PostgreSQL while `fdb` self-skips without a cluster.

Comment lines only — the executable part is untouched. Verified afterwards by
re-running the board's `fdb` suite with this script as its own entry point.
"""
import re

F = '/data/grovefs/grovefs/.build-bin/run_all.sh'
L = open(F, encoding='utf-8').read().split('\n')

INS = [
    ('#               converges via enriched push (not TTL); exercises display_targets', [
        '#   layout      presentation overlay e2e (`mount --layout`): one stored view, a',
        '#               different lens per client — hide/only/map/link, write-through to',
        '#               the real entries, lens-side deletes, the synthesized-dir guard.',
        '#               `LAYOUT_BACKEND=fdb` re-runs every assertion against an',
        '#               FDB-backed meta-server instead of the local SQLite tree',
    ]),
    ('#               correctness (paper Sec. 5 I3 + Sec. 9 scalability boundary)', [
        '#   evict       idle-engine eviction e2e (`meta-server --engine-idle-secs`): N',
        '#               mounted views hold N engines, the sweeper retires an engine once',
        '#               its last session closes, and a remount serves the same tree.',
        '#               PG-backed (it measures the connection wall); self-skips without PG',
        '#   control     control-plane unification e2e: under `--meta-store postgres://…`',
        '#               view definitions AND api keys live in `grovefs_control`, nothing',
        '#               lands in the meta-server\'s local dir, and M_OPEN resolves both out',
        '#               of PostgreSQL. Self-skips without PG',
        '#   fdb         FoundationDB metadata backend e2e (`--meta-store fdb://<cluster',
        '#               file>`): subspace provisioning, the tree body, the lock surface,',
        '#               derived/alias governance and an explicit fork verdict, the two-way',
        '#               `view export`/`view import` migration chain (ids/inodes kept), and',
        '#               what `--engine-idle-secs` means on this backend. Registers SKIP',
        '#               when no cluster is reachable; FDB_TEST=1 turns that absence into',
        '#               a FAIL (CI)',
    ]),
    ('#               (SQLite stores only; a PG deployment is refused)', [
        '#   viewrm      whole base view removal e2e (`view rm --base`): the guard set',
        '#               (live local mount, governed name, base still named by an',
        '#               alias/derived/fork definition, missing tree, PG deployment), tree',
        '#               +WAL removal, ledger rebuild, and shared bytes surviving for the',
        '#               fork that still names them',
        '#   bwlimit     per-mount bandwidth shaping e2e: `--bw-read`/`--bw-write` pin a',
        '#               24 MiB transfer to its floor with no byte lost, the unlimited',
        '#               mount of the same view stays materially faster,',
        '#               GROVEFS_BW_READ/GROVEFS_BW_WRITE env fallback',
    ]),
]

for anchor, block in INS:
    idx = [i for i, l in enumerate(L) if l == anchor]
    assert len(idx) == 1, 'anchor hit %d: %r' % (len(idx), anchor[:50])
    for name in block:
        m = re.match(r'^#   ([a-z_]+)\s\s', name)   # a suite heading line, not a continuation
        if m:
            suite = m.group(1)
            assert not re.search(r'^#   %s\s\s' % suite, '\n'.join(L), re.M), \
                'already described: %s' % suite
    L[idx[0] + 1:idx[0] + 1] = block

open(F, 'w', encoding='utf-8').write('\n'.join(L))
print('run_all.sh header: layout / evict / control / fdb / viewrm / bwlimit described')
