#!/usr/bin/env python3
# The audit found that P6's harness was described as three-way (Spec P6 涉及, and
# docs/fdb-keyspace.md's own §4 line about "三后端对拍") while only two arms were
# actually run. The test now exists; these are the口径 lines that follow it.
import io

U = '/data/grovefs/grovefs/USAGE.md'
K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'

OLD_U = '语义等价另有差分对拍 `tests/fdb_tree.rs` 兜底(无集群时自行 SKIP,不拖红全板)。'
NEW_U = (
    '语义等价另有**三档差分对拍** `tests/fdb_tree.rs` 兜底:同一条 op 序列分别跑过 '
    '`sqlite`/`postgres`/`foundationdb`,先逐 op 比对回包、再比 `dump_tree()` 的结构同型'
    '(行号按从根遍历重新编号,故比的是树而不是发号轨迹);'
    '某一档不可达就只比剩下的档,两档都没有就自行 SKIP,不拖红全板。'
)

OLD_K = '集群侧 `fdbcli --exec "status minimal"` 重答 `The database is available.`。'
NEW_K = (
    '集群侧 `fdbcli --exec "status minimal"` 重答 `The database is available.`。\n\n'
    '审计里唯一一处**补了代码**的地方：P6 把台架写作"`Sqlite` vs `Pg` vs `Fdb` 三档同一 op 序列跑、'
    '比对 `dump_tree()`"，本文件 §4 也按"三后端对拍"立过口径，而 `tests/fdb_tree.rs` 当时只有两档'
    '（SQLite↔FDB）。于是加了 `all_reachable_backends_agree_on_one_op_sequence`：复用同一条 '
    '`script()` 与同一个 `canonical()`，把主机上**可达的每一档**都挂进同一张比对表，'
    'PG 档按 `.build-bin/test_pg.sh` 的口径探 `PGHOST:PGPORT`（默认 `127.0.0.1:5432`），'
    '跑完自己 `DROP SCHEMA`（库侧从不删 schema——`view rm` 明确拒绝 PG 部署，所以谁造的临时 schema 谁收）。'
    '取证：`PGHOST=127.0.0.1 PGPORT=5433 cargo test --release --features fdb --test fdb_tree -- --nocapture` '
    '→ `three-way differential: 3 arms [sqlite, postgres, foundationdb] agree on 49 ops and 7 rows`，'
    '两测 2 passed / 0 failed；`cargo test --release`（不带 `fdb`）与 `--features fdb` 两态均 0 警告，'
    '事后查库确认那个临时 schema 已不存在（`grovefs_v_6477%` 计数 0）。'
)

Edits = [(U, OLD_U, NEW_U), (K, OLD_K, NEW_K)]
texts = {p: io.open(p, encoding='utf-8').read() for p, _, _ in Edits}
for p, old, _ in Edits:
    assert texts[p].count(old) == 1, 'anchor in %s hit %d' % (p, texts[p].count(old))
for p, old, new in Edits:
    texts[p] = texts[p].replace(old, new)
for p in texts:
    io.open(p, 'w', encoding='utf-8').write(texts[p])
print('USAGE §16.3 + keyspace v1.7: three-way differential registered')
