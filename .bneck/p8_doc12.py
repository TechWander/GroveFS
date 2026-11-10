#!/usr/bin/env python3
# The board's one RED this round was the harness, not the lock surface; it hit the
# suite that P4 parameterised for FDB, so the project log owes a line.
import io

K = '/data/grovefs/grovefs/docs/fdb-keyspace.md'
T = io.open(K, encoding='utf-8').read()
OLD = "事后查库确认那个临时 schema 已不存在（`grovefs_v_6477%` 计数 0）。"
NEW = OLD + """

最后一轮全板认证跑出过一次 `RESULT: RED`，失败的是 `locks` 套件第 3 节（`pass=15 fail=2`：
`holder never acquired` + `overlap expected refused`），而同一套件空载连跑三次都是 17/0——
判据本身没红，红的是台架：脚本给持锁进程 2s 报"已拿到锁"，而持锁进程只活固定的 3s，
机器一忙，2s 预算先到期，两条断言就打在已经释放的锁上（无冲突请求当然放行）。
修法是拆掉这层时间耦合而不是把常数调大：持锁进程改为**一直持到脚本撤掉标记文件**
（另有 120s 上限兜住挂载卡死，而"活持锁者越过一个 3s 租约窗口仍存活"本就是第 4b 节断言过的
受支持状态），就绪等待放宽到 ~10s 且**把实测耗时打进日志**，失败时带上持锁进程自己的输出。
断言条数与判据不变（SQLite 17/0、`LOCKS_BACKEND=fdb` 16/0 各复跑通过），改后的全板重新 GREEN。"""
assert T.count(OLD) == 1, 'anchor hit %d' % T.count(OLD)
io.open(K, 'w', encoding='utf-8').write(T.replace(OLD, NEW))
print('keyspace v1.7: locks harness defect recorded')
