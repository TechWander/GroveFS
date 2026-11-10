#!/usr/bin/env python3
"""G3 failed on measurement, so the user-facing口径 has to stop promising a third
supported backend (plan R3: 不入文档承诺, 对外口径回到 "SQLite/PG + 分片").

What changes and why each word is chosen:
  * the board counts drop by one everywhere they are stated (fdb left CANON):
    35 段 → 34 段, 34 门控 → 33 门控, 32 套件 → 31 套件;
  * §6.3 / §5.9 keep their behavioural content — it is how the experiment is run —
    but are re-labelled 实验性 with the measured numbers and the retry condition;
  * what is NOT removed: the refusal texts, the capability-boundary table, the
    suite's 169 assertions. Those are facts about the code, and the plan's
    disposition is about promises, not about deleting evidence.

Anchors are verified before anything is written.
"""
import io

U = '/data/grovefs/grovefs/USAGE.md'
H = '/data/grovefs/grovefs/docs.html'
M = '/data/grovefs/grovefs/src/main.rs'

E = []

# ---------------- USAGE.md ----------------
E.append((U,
"全板一共 **35 段**:`build` → `unit` → **32 个套件** → `bench`。**前 34 段门控 GREEN,`bench` 只打吞吐参考值、永不门控**(只要纯功能回归就 `SKIP_BENCH=1`)。`pg`/`pg_parallel`/`control` 在无可达 PostgreSQL 的主机上自动 SKIP,不拉低 GREEN。`fdb` 同理:主机上无可达 FoundationDB 集群时整套自动 SKIP,置 `FDB_TEST=1` 时缺集群即判 FAIL(不允许把没跑过的后端算作通过)。",
"全板一共 **34 段**:`build` → `unit` → **31 个套件** → `bench`。**前 33 段门控 GREEN,`bench` 只打吞吐参考值、永不门控**(只要纯功能回归就 `SKIP_BENCH=1`)。`pg`/`pg_parallel`/`control` 在无可达 PostgreSQL 的主机上自动 SKIP,不拉低 GREEN。`fdb` **不在这 34 段里**:它是实验档(§6.3),按名 `bash .build-bin/run_all.sh fdb` 才跑,跑成什么都不影响默认全板的颜色;显式跑时主机上无可达 FoundationDB 集群则整套 SKIP,置 `FDB_TEST=1` 时缺集群即判 FAIL(不允许把没跑过的后端算作通过)。"))

E.append((U,
"bash .build-bin/run_all.sh                # 全板 35 段(34 门控 + bench)",
"bash .build-bin/run_all.sh                # 全板 34 段(33 门控 + bench)"))

E.append((U,
"                                          #   config pg pg_parallel evict control fdb\n"
"                                          #   bench(只打吞吐,不门控)",
"                                          #   config pg pg_parallel evict control\n"
"                                          #   bench(只打吞吐,不门控)\n"
"bash .build-bin/run_all.sh fdb            # 实验档 FoundationDB,不在 CANON(§6.3)"))

E.append((U,
"全部通过即部署完成。想跑整套回归(34 个门控阶段)见 §10 与 §12.7。",
"全部通过即部署完成。想跑整套回归(33 个门控阶段)见 §10 与 §12.7。"))

E.append((U,
"任何升级/配置变更后,在有构建环境的机器上跑一遍全板(34 个门控阶段),或只跑关心的面:",
"任何升级/配置变更后,在有构建环境的机器上跑一遍全板(33 个门控阶段),或只跑关心的面:"))

E.append((U,
"""### 6.3 FoundationDB 元数据后端（可选）

`meta-server` 也能把权威 view 树放进 FoundationDB——一 view 一 subspace（键编码与事务粒度的逐项设计见 `docs/fdb-keyspace.md`）：""",
"""### 6.3 FoundationDB 元数据后端（实验性，不构成支持声明）

> **这一档的状态：功能面全部判过，容量面判为未通过，因此不进 CANON、不进承诺面。**
> 支持的元数据后端仍是嵌入式 SQLite 与 PostgreSQL（§6.2）；元数据面的写并行只在单个 PG 集群内，跨集群分区元数据权威不在本轮承诺面；
> 口径出处是决策门 G3 的实测（2026-11-09，同机同场同 op 序列）：FoundationDB 臂三轮峰值
> **765–1200 ops/s**（`ssd` 引擎、三副本），把盘从提交路径拿掉（`memory` 引擎）也只有 **1642 ops/s**，
> 而同一次运行里 PostgreSQL 臂达到 **4100–4900 ops/s**——门槛是"不劣于 PG 平台值 ~4.6k ops/s"，
> 实测差 3–6 倍，且这台机器没有 SSD 介质（两块阵列卡后 `queue/rotational=1`），
> 计划里的 SATA/NVMe 档位无法在此复现。归因数据、复跑配方与撤销条件都在
> `docs/fdb-keyspace.md` §14/§15；换到 SSD 硬件重跑该实验是这一档转正的唯一前提。
>
> 下面是这一档**当前真实的行为**（实验用途）：`meta-server` 也能把权威 view 树放进 FoundationDB——
> 一 view 一 subspace（键编码与事务粒度的逐项设计见 `docs/fdb-keyspace.md`）："""))

# ---------------- docs.html ----------------
E.append((H,
'<span class="badge">全板 GREEN <b>34 个门控阶段</b>(build+unit+32 套件；<b>bench</b> 只打吞吐、永不门控)</span>',
'<span class="badge">全板 GREEN <b>33 个门控阶段</b>(build+unit+31 套件；<b>bench</b> 只打吞吐、永不门控；<b>fdb</b> 为实验档不进 CANON)</span>'))

E.append((H,
'<tr><td>元数据换 FoundationDB 后端（一视图一 subspace，需 feature 构建）</td>',
'<tr><td>元数据换 FoundationDB 后端（<b>实验档</b>：一视图一 subspace，需 feature 构建；未通过性能门）</td>'))

E.append((H,
'''    <pre><code><span class="cmt"># FoundationDB 是同一条位置的另一种换芯：先要有 feature 构建的二进制（默认构建不链 libfdb_c）</span>''',
'''    <div class="warn"><b>FoundationDB 这一档是实验，不是第三个受支持后端</b>：功能面（树/锁/治理/双向迁移/驱逐）逐条判过，容量面按决策门 G3 <b>实测未通过</b>——同机同场同 op 序列下 FDB 臂三轮峰值 765–1200 ops/s（<code>ssd</code> 三副本）、把盘移出提交路径仍只 1642 ops/s，而同一轮 PostgreSQL 臂 4100–4900 ops/s，门槛是"不劣于 PG 平台值 ~4.6k"。因此它不进回归 CANON（<code>run_all.sh fdb</code> 才跑），也不进任何部署承诺：受支持的元数据后端仍是 SQLite（默认）与 PostgreSQL（<code>--meta-store postgres://…</code>），跨集群分区元数据权威不在承诺面。归因与复跑配方见 <code>docs/fdb-keyspace.md</code> §14/§15。</div>
    <pre><code><span class="cmt"># FoundationDB 是同一条位置的另一种换芯：先要有 feature 构建的二进制（默认构建不链 libfdb_c）</span>'''))

E.append((H,
'bash .build-bin/run_all.sh                <span class="cmt"># 全板 35 段 = 34 门控 + bench</span>',
'bash .build-bin/run_all.sh                <span class="cmt"># 全板 34 段 = 33 门控 + bench</span>'))

E.append((H,
'                                          <span class="cmt">#   config pg pg_parallel evict control fdb       bench(只打吞吐,不门控)</span>',
'                                          <span class="cmt">#   config pg pg_parallel evict control             bench(只打吞吐,不门控)</span>\n'
'bash .build-bin/run_all.sh fdb            <span class="cmt"># 实验档 FoundationDB:不在 CANON,按名才跑</span>'))

E.append((H,
'<p class="lead">全板 34 个门控阶段（build + unit + 32 套件；<code>bench</code> 仅参考不门控）；下面这十四个专项套件各自钉住一类契约（点开看断言明细）。</p>',
'<p class="lead">全板 33 个门控阶段（build + unit + 31 套件；<code>bench</code> 仅参考不门控）；下面这十四个专项套件各自钉住一类契约（点开看断言明细），其中最后一个 <code>fdb</code> 是实验档——不进 CANON，按名运行。</p>'))

E.append((H,
'bwlimit 17 / fdb 169</summary>',
'bwlimit 17 / fdb 169（实验档，不门控）</summary>'))

E.append((H,
'——主机上无可达 FoundationDB 集群时自动 SKIP、不拉低 GREEN,置 <code>FDB_TEST=1</code> 时缺集群即判 FAIL)。</p>',
'——主机上无可达 FoundationDB 集群时自动 SKIP,置 <code>FDB_TEST=1</code> 时缺集群即判 FAIL;<b>该套件不在 CANON</b>:G3 性能门实测未过(三轮峰值 765–1200 ops/s 三副本 <code>ssd</code>、1642 ops/s 去盘 <code>memory</code>,同场 PG 4100–4900 ops/s,门槛 4.6k),见 <a href="#deploy">§5.9</a> 与 <code>docs/fdb-keyspace.md</code> §15)。</p>'))

# ---------------- src/main.rs : the --meta-store help ----------------
E.append((M,
"""        /// `fdb://<path-to-cluster-file>` (accepted only by a binary built
        /// `--features fdb`) to place it in FoundationDB: one tuple-layer""",
"""        /// `fdb://<path-to-cluster-file>` (accepted only by a binary built
        /// `--features fdb`) to place it in FoundationDB — EXPERIMENTAL, kept out
        /// of the supported surface because the throughput gate measured fail
        /// (see USAGE §6.3): one tuple-layer"""))

texts = {}
for path, old, _ in E:
    texts.setdefault(path, io.open(path, encoding='utf-8').read())
    n = texts[path].count(old)
    assert n == 1, 'anchor in %s hit %d: %r' % (path, n, old[:70])
for path, old, new in E:
    texts[path] = texts[path].replace(old, new)
for path, txt in texts.items():
    io.open(path, 'w', encoding='utf-8').write(txt)
print('G3 disposition: %d edits across %d files (CANON counts, §6.3/§5.9 banners, help)' % (len(E), len(texts)))
