#!/bin/bash
# phase4: 用 >16MiB 的文件验证跨服务器条带 (STRIPE_BYTES=16MiB), 并查清远端 du 的字节来源.
set -uo pipefail
TOKEN=gfstest
SSH="ssh -o BatchMode=yes -o ConnectTimeout=8"

echo "======== [gfs-client] 写 3 个 64MiB 文件 ========"
$SSH gfs-client 'for i in 1 2 3; do head -c 67108864 /dev/urandom > $HOME/mnt/big$i.bin; done; sync;
  ls -l $HOME/mnt/big*.bin | awk "{print \"  \", \$5, \$9}"' 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'

echo "======== data_ref 的真实形状(决定 du 能不能测) ========"
$SSH gfs-client 'python3 - <<PY
import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/meta/meta/alice.db"))
rows = db.execute("select path, data_ref from node where data_ref is not null order by path").fetchall()
for p, k in rows[:8]:
    print("   ", p, "->", k)
print("    带条带前缀 sN: 的对象数:", sum(1 for _, k in rows if k.startswith("s")))
print("    总对象行数:", len(rows))
PY' 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'

echo "======== 每台每根的对象数(两条 30G 盘 + 两台服务器) ========"
for h in gfs1 gfs2; do
  printf '%-11s ' $h
  $SSH $h 'for r in /data/gfs1/grove /data/gfs2/grove; do printf "%s: %s对象 %s   " "$(basename $r)" "$(find $r/blocks -type f | wc -l)" "$(du -sh $r | cut -f1)"; done; echo' 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
done

echo "======== 远端 du 的三种问法 ========"
for d in "gfs1:50100" "gfs2:50100" "gfs1:50100,gfs2:50100"; do
  printf -- '--- --data-servers %s ---\n' "$d"
  $SSH gfs-client "GROVEFS_TOKEN=$TOKEN grovefs du --view alice --meta-server 127.0.0.1:50053 --meta-token $TOKEN --data-servers $d --total; echo '  [逐目录]'; GROVEFS_TOKEN=$TOKEN grovefs du --view alice --meta-server 127.0.0.1:50053 --meta-token $TOKEN --data-servers $d 2>&1 | head -4" 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
done

echo "======== 挂载里 df 看到的容量(对照) ========"
$SSH gfs-client 'df -h $HOME/mnt | tail -1; du -sh $HOME/mnt 2>/dev/null | cut -f1' 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
