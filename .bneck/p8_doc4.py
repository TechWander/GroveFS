#!/usr/bin/env python3
# P7's remaining gap, closed with measurements rather than inference: the Spec asks
# §15.16 for "FDB 冲突/不可达类报错", and the honest finding is that UNREACHABILITY IS
# NOT AN ERROR AT ALL on this backend. Measured on this host, with the real binary:
#
#   cluster file that does not exist  -> `error: fdb open /nope/none.cluster: No
#       cluster file found in current directory or default location`, exits nonzero
#   cluster file whose coordinators are dead -> `view create` ran 150s and printed
#       NOTHING (rc=124 under timeout); meta-server with the same file printed its
#       banner, LISTENED, and a mount through it stalled 20s at "using remote
#       metadata service at …" with no error line on either side
#   fdbcli -C <the same dead file> --exec "status minimal" -> "The database is
#       unavailable" within seconds — so the check that answers this is cluster-side
#
# A row that said "报 cluster 不可达" would be a fabricated error string; the rows
# below state what the operator actually sees.
import io

U = '/data/grovefs/grovefs/USAGE.md'
D = '/data/grovefs/grovefs/docs.html'
K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'


def load(p):
    return io.open(p, encoding='utf-8').read().split('\n')


def insert_after(lines, marker, block, n=1):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == n, 'anchor %r hit %d times' % (marker[:50], len(idx))
    lines[idx[0] + 1:idx[0] + 1] = block.split('\n')


def patch(lines, marker, old, new):
    idx = [i for i, l in enumerate(lines) if marker in l]
    assert len(idx) == 1, 'anchor %r hit %d' % (marker[:50], len(idx))
    assert lines[idx[0]].count(old) == 1, 'sub-anchor %r: %d' % (old[:40], lines[idx[0]].count(old))
    lines[idx[0]] = lines[idx[0]].replace(old, new)


# ---------------------------------------------------------------- USAGE §15.16
rows = '''| `error: fdb open <路径>: No cluster file found in current directory or default location` | `fdb://` 指向的集群文件不存在/不可读（后半句是 `libfdb_c` 自己的原文，grovefs 只加 `fdb open <路径>:` 前缀） | 路径写错是这一类：在**执行命令的那台机器**上给真实存在的集群文件，或设 `GROVEFS_FDB_CLUSTER_FILE`（§6.3） |
| 命令**挂住不返回**：客户端只打到 `using remote metadata service at …` 就没了，退出只能靠外部超时；`meta-server` 那边横幅照打、端口照听、日志不再新增行 | **集群不可达在这一档不是报错，是无声等待**——`libfdb_c` 对协调器不可达按自己的策略持续重试，本后端没有给这类等待设上限（`docs/fdb-keyspace.md` §11 把 timeout/重试上限列为可后置项）。服务端启动**不探测集群**，所以"起来了"不等于"通" | 判据在集群侧：服务端机上 `fdbcli -C <集群文件> --exec "status minimal"`，`The database is unavailable` 就是这一类（几秒内给出，不像 grovefs 那样等）。恢复可达后卡住的那笔 op 自己走通，挂载不需重挂 |
| `serialization retry exhausted after 12 attempts (view 'x')` | 一笔 op 连撞 12 次仍未提交（OCC 中止，退避 200µs→25ms 用尽）。**PG 与 FDB 同一条 driver 文案**（`src/pg_store.rs:550`、`src/fdb_store.rs:1523`），FDB 档没有锁等待这一说，出现这行就是重试预算真耗尽 | 查争用面而不是查集群：同一目录内高频 `rename`/同名 `create` 风暴；PG 侧该串由 `pg_parallel` 套件在日志里实测，FDB 侧走的是同一常量与同一分支 |'''
L = load(U)
insert_after(L, '| `fdb:// uri names no cluster file: give fdb://<path> or set GROVEFS_FDB_CLUSTER_FILE`', rows)
io.open(U, 'w', encoding='utf-8').write('\n'.join(L))
print('usage: rows added')

# --------------------------------------------------------------- docs.html §5.9
warn = '''    <div class="warn"><b>集群不可达在这一档不产生报错，而是无声等待</b>（实测）：给一个协调器已死的集群文件跑 <code>view create</code>，150s 内<b>没有任何输出</b>、只能被外部超时打断；同一个文件启动 <code>meta-server</code>，横幅照打、端口照听、日志不再新增，挂载卡在 <code>using remote metadata service at …</code>。原因是 <code>libfdb_c</code> 对协调器不可达按自身策略持续重试，而服务端启动<b>不探测集群</b>——"起来了"不等于"通"。可达性判据在集群侧：<code>fdbcli -C &lt;集群文件&gt; --exec "status minimal"</code> 几秒内回 <code>The database is unavailable</code>；恢复可达后卡住的那笔 op 自己走通，挂载不需重挂。与之相对的两种真实报错：<code>fdb open &lt;路径&gt;: No cluster file found …</code>（集群文件不存在/不可读，后半句是客户端库原文）、<code>serialization retry exhausted after 12 attempts (view 'x')</code>（OCC 重试预算耗尽，与 PG 档同一条 driver 文案，查争用面而不是查集群）。</div>'''
L = load(D)
insert_after(L, '<b>最容易踩的两个坑</b>', warn)
io.open(D, 'w', encoding='utf-8').write('\n'.join(L))
print('docs: warn added')

# ----------------------------------------------------- keyspace §11（可后置项留名）
L = load(K)
# The §11 sentence wraps across two lines, so this is an insert after the line that
# ends it — not an in-line patch.
insert_after(L, '存储引擎选择、NVMe/SATA 档位', '\n'
      '其中**"协调器不可达的等待上限"这一项未设，后果已实测并写进用户文档**：`libfdb_c` 对\n'
      '`coordinator_not_available` 按自身策略持续重试，本后端不强加超时，于是数据面 op 在集群不可达时\n'
      '**静默阻塞而非报错**（150s 无任何输出实测；`meta-server` 启动不探测集群，横幅照打）。它属运维可见\n'
      '行为而非键空间契约，故 USAGE §15.16 / docs.html §5.9 按"不可达 = 无声等待"记账，判据指向\n'
      '`fdbcli status minimal`；将来若引入该上限，改的是这一项的表述，不是任何编码或事务语义。')
io.open(K, 'w', encoding='utf-8').write('\n'.join(L))
print('keyspace: §11 note added')
