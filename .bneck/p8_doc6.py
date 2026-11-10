#!/usr/bin/env python3
# After the two test-side additions (§12's inode-vs-dump pinning, §13's eviction
# semantics) the docs must (a) cite the measurements that now back the claims they
# already made, and (b) carry the current suite numbers. Nothing here changes a
# behaviour statement — it points each one at the assertion that proves it.
import io

U = '/data/grovefs/grovefs/USAGE.md'
D = '/data/grovefs/grovefs/docs.html'
K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'


def load(p):
    return io.open(p, encoding='utf-8').read().split('\n')


def patch(lines, marker, old, new, n=1):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == n, 'anchor %r hit %d' % (marker[:48], len(idx))
    i = idx[0]
    assert lines[i].count(old) == 1, 'sub %r in line %d: %d' % (old[:40], i + 1, lines[i].count(old))
    lines[i] = lines[i].replace(old, new)


def append_unused(lines, marker, text, n=1):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == n, 'anchor %r hit %d' % (marker[:48], len(idx))
    lines[idx[0]] = lines[idx[0]] + text


# ------------------------------------------------------------------- USAGE 6.3
patch(
    (L := load(U)),
    '- **`--engine-idle-secs` 在此档只管内存**',
    '挂载中的视图仍永不被回收，客户端无感（§12.1）。',
    '挂载中的视图仍永不被回收，客户端无感（§12.1）。'
    '这三条都在 `fdb` 套件第 13 节实测：把阈值压到 3s，挂载期间静默 8s ⇒ 服务端日志里 0 条驱逐；'
    '卸载会话后 ⇒ `meta-server: evicted 1 idle engine(s) (>3s idle, no sessions)`；'
    '同一个视图重挂 ⇒ 读回同一个对象（重开对客户端透明）。',
)
io.open(U, 'w', encoding='utf-8').write('\n'.join(L))

# ---------------------------------------------------------------- docs.html 5.9
patch(
    (L := load(D)),
    '冲突处理是严格可串行化 + OCC 重试环（有界指数退避），不是锁等待',
    '不是锁等待</td></tr>',
    '不是锁等待（挂载中不驱逐、卸载后回收、重开透明三条由 <code>fdb</code> 套件第 13 节实测）</td></tr>',
)
io.open(D, 'w', encoding='utf-8').write('\n'.join(L))

# --------------------------------------------------- keyspace: v1.5 表 + v1.6 段
L = load(K)
patch(
    L,
    '| `test_fdb.sh` | —（本套件只在 fdb 上跑） |',
    '158/0',
    '169/0',
)
patch(
    L,
    '| `test_fdb.sh` | —（本套件只在 fdb 上跑） |',
    '12f 源存储未被写入 |',
    '12f 源存储未被写入；12a 另把挂载侧 `stat -c %i` 钉到导出侧的 `ino` 原值、12c 在属性 TTL 过期后重读同一 inode；13 节实测本档的驱逐语义 |',
)
V16 = '''
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
'''
lines_tail_ok = [l for l in L if l.startswith('### v1.')]
assert len(lines_tail_ok) == 5, 'unexpected version-note count: %d' % len(lines_tail_ok)
# v1.6 belongs INSIDE §14 (版本注记), not after §15 (决策门处置), which now ends the
# file — insert it immediately before that heading.
head = [i for i, l in enumerate(L) if l.startswith('## 15. 决策门处置')]
assert len(head) == 1, '§15 heading hit %d' % len(head)
L[head[0]:head[0]] = (V16.strip('\n') + '\n').split('\n')
io.open(K, 'w', encoding='utf-8').write('\n'.join(L).rstrip('\n') + '\n')
print('docs: eviction + §12/§13 citations + keyspace v1.6')
