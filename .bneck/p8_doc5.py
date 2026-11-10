#!/usr/bin/env python3
# Spec closes each stage at a DECISION GATE (G1..G4 + R5). Version notes v1.1–v1.5
# recorded single measurements, but nothing in the design doc stated, gate by gate,
# what was judged and on what evidence. G3 in particular must NOT be written as a
# pass: the local cluster is `configure new single ssd` with replication factor 1,
# and the plan's own risk row R2 says performance conclusions are not admissible
# from such an environment (nor may they reach the user docs). So the honest record
# is "not adjudicated here + the smallest experiment that would adjudicate it".
K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'
TAIL = '已按此改写并注明后果'

SEC = '''
## 15. 决策门处置（G1–G4 与 R5，逐门留证）

计划把生死判定放在阶段末的决策门上。每个门的处置只有两种合法写法：**判过并留下证据**，
或**本环境不具判定资格并写明关闭它的实验**——沉默通过不算。逐门如下：

| 门 | 位置 | 处置 | 取证 / 关闭条件 |
|---|---|---|---|
| **G1** 环境门 | P0 末 | **通过** | 三条判据原文见 §14 v1.1 末（`The database is available.`、`cargo build --release --features fdb --example fdb_spike` 链接 `libfdb_c` 成功、spike `P0_SPIKE_VERDICT=PASS`）。环境路径按 Spec 要求"选定一条即写死命令"：发行版 apt 源与 docker 守护进程在本机都不通（前者无源、后者不可达），故走官方 release 资产 + sha256 校验 + 用户态运行——`bash .build-bin/fdb_env.sh fetch && bash .build-bin/fdb_env.sh up`，`eval "$(bash .build-bin/fdb_env.sh vars)"` 导出 `FDB_CLIENT_LIB_PATH` 与 `GROVEFS_FDB_CLUSTER_FILE`；不装系统包、不用 root，头注释里写明了降级次序 |
| **G2** 设计门 | P1 末 | **未触发** | 三条最难的形态都有表达且已落地：分页 readdir = `'n'` 前缀 range + `start_after` + 步长检查点（§3.3，`FdbStore::supports_paged_readdir()` 返回 `true`，`src/fdb_store.rs:1721`）、目录 nlink = `('k',parent,name)` 仅目录子空间计数（§3.4）、锁区间重叠 = `('l',ino)` 前缀扫 + 事务内过滤（§3.7，C5 另加"二级键须能无歧义重建主键"的硬判据）。P3/P4 的实现与差分对拍是这些映射真正成立的最终证据，不需要退回分片路线 |
| **G3** 性能门 | P4 末 | **未判定**（本环境不具判定资格；不写作"通过"） | 阈值两档——SATA 三副本 ≥ 4.6k ops/s、NVMe 三副本 ≥ 9k ops/s——都要求多副本集群，而本机集群是 `configure new single ssd`、复制因子 1。计划的风险行 R2 明确：这一档可以跑全部**功能**判据，但性能与故障结论属未验证，不得写进 USAGE。于是本轮的处置记为三条：① 功能判据全绿（§14 v1.4/v1.5 的双档表与全板 GREEN）；② `USAGE.md`/`docs.html` 不含任何 FDB 吞吐或延迟实测值（该两档里出现的数字只有设计常量：重试 12 次、退避 200µs→25ms、号段 1024，以及"容量看 proxy/GRV 与 log 副本写吞吐"这类规划口径）；③ spike 的十万读 / 一万双键搬迁只用于证明 OCC 重试环真实发生并收敛（§14 v1.1 末已如此标注）。**关闭 G3 的最小实验**：在 3–5 节点 SATA 三副本（或 gfs1/gfs2/gfs-client VM 集群）上以 `--meta-store fdb://…` 起 meta-server，跑与 PG 平台值同一 op 序列的 `bench` 套件，判据 ≥ 4.6k ops/s。不过则按计划撤下：`run_all.sh` 的 CANON 去掉 `fdb`、USAGE 降为实验声明、`fdb://` 分派处返回"实验未就绪"错误串 |
| **G4** 语义等价门 | P5 / P6 | **未触发** | 三后端 `dump_tree()` 差分对拍 identical（P3 建立、P6 复跑）；四套治理套件在 fdb 档逐条通过（§14 v1.3 双档表）；§3 的九类映射与 §10 的冻结前缀集合未因任何一条断言放宽。暴露出的差异全部落进 §12 的可归因清单（硬链接多名下 `name` 取值、秒级时间、`ino IS NULL` 在 FDB 结构上不可能），没有一条需要"改不变量"才通过 |
| **R5** 台架拖累 | P6 内 | **已封堵** | 集群是外部状态，缺失即 SKIP 而非 FAIL：`GROVEFS_FDB_CLUSTER_FILE=<不存在的文件> bash .build-bin/test_fdb.sh` 打 `SUMMARY: pass=0 fail=0 SKIP=1` 且 rc=0；同一条件加 `FDB_TEST=1` 打 `SUMMARY: pass=0 fail=1 SKIP=0` 且 rc=1（两条均为本轮在同一台机器上实测）。`run_all.sh::run_fdb` 把 `SKIP=1` 登记为 `RESULT[fdb]=SKIP`，不进 FAIL 计数，所以没装 FDB 的机器上全板仍 `RESULT: GREEN` |

'''


def main():
    txt = open(K, encoding='utf-8').read()
    lines = txt.split('\n')
    assert '## 15. 决策门处置' not in txt, 'already applied'
    idx = [i for i, l in enumerate(lines) if TAIL in l]
    assert len(idx) == 1, 'anchor hit %d' % len(idx)
    assert idx[0] >= len(lines) - 4, 'anchor is not at the tail: line %d of %d' % (idx[0], len(lines))
    tail = [l for l in lines[idx[0] + 1:] if l.strip()]
    assert not tail, 'unexpected content after anchor: %r' % tail[:2]
    lines.extend(SEC.split('\n'))
    open(K, 'w', encoding='utf-8').write('\n'.join(lines))
    print('keyspace: §15 gate dispositions appended')


main()
