#!/bin/bash
# Decisive probe v2: a striped chunk is truly gone (unlinked + data-server
# restarted so its cached fd is dropped).  Does `du` and does a mounted read
# say so?  Everything printed verbatim; the chunk is restored at the end.
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
M=$HOME/mnt
ENV=/etc/grovefs/cluster.env
DB=$HOME/meta/meta/alice.db
as_root_env(){ sudo sh -c "set -a; . $ENV; set +a; $1"; }
q(){ sudo python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[1]);print(' ; '.join(','.join(str(x) for x in r) for r in c.execute(sys.argv[2])))" "$1" "$2" 2>&1; }
DU(){ as_root_env "grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 $1" 2>&1 | tr '\n' '|'; }

echo "===== D1 对象定位 ====="
ref=$(q "$DB" "select data_ref from node where name='big.bin' and data_ref like 's%'" | awk '{print $1}')
uuid=${ref##*:}
echo "  data_ref=$ref  uuid=$uuid"

echo "===== D2 分块分布 ====="
for h in gfs1 gfs2; do
  echo "  -- $h:"
  ssh -n $h "sudo find /data/gfs*/grove/blocks -name \"$uuid*\" -printf '    %h/%f %s\n'" | sort -t'#' -k2 -n
done

echo "===== D3 挑走前基线 ====="
echo "  du -b --path /datasets: $(DU '-b --path /datasets')"
echo "  du --total:              $(DU '--total')"
echo "  挂载 md5: $(md5sum $M/datasets/big.bin 2>&1)"

echo "===== D4 真正删除 chunk #1(gfs2): 复制到 tmpfs + unlink, 然后重启该 data-server 清掉 fd 缓存 ====="
tgt=$(ssh -n gfs2 "sudo find /data/gfs*/grove/blocks -name \"${uuid}#1\" -type f")
echo "  目标: gfs2:$tgt"
ssh -n gfs2 "sudo stat -c '    挑走前 %n %s 字节' $tgt
sudo cp $tgt /dev/shm/hidden.obj && sudo rm $tgt && echo '    已 unlink(副本留 /dev/shm)'
sudo ls $tgt 2>&1 | sed 's/^/    原路径: /'"
echo "  重启 gfs2 的 data-server 以清空服务端 fd 缓存:"
ssh -n gfs2 "sudo systemctl restart grovefs-data-server && echo '    restart rc=0'"
for i in $(seq 1 40); do ssh -n gfs2 'ss -ltn | grep -c :50100' | grep -qv '^0$' && break; sleep 0.25; done
echo "  监听: $(ssh -n gfs2 'ss -ltn | grep :50100 | tail -1') pid=$(ssh -n gfs2 'sudo systemctl show -p MainPID --value grovefs-data-server')"
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null && echo "  gfs-client 页缓存已清"

echo "===== D5 观察 ====="
echo "  du -b --path /datasets: $(DU '-b --path /datasets')"
echo "  du --total:              $(DU '--total')"
echo "  挂载整文件 md5: rc/md5 → $(md5sum $M/datasets/big.bin 2>/tmp/merr; echo "rc=$? 报错=[$(cat /tmp/merr)]")"
echo "  挂载 stat size: $(stat -c %s $M/datasets/big.bin 2>&1)"
dd if=$M/datasets/big.bin bs=1M skip=16 count=4 of=/dev/null 2>/tmp/derr
echo "  dd 读 chunk#1 区间 16MiB..20MiB: rc=$? 报错=[$(tr '\n' '|' < /tmp/derr)]"
echo "  未受影响区间 0..4MiB: rc=$(dd if=$M/datasets/big.bin bs=1M skip=0 count=4 of=/dev/null 2>/tmp/derr0; echo $?) 报错=[$(tr '\n' '|' < /tmp/derr0)]"

echo "===== D6 复原 ====="
ssh -n gfs2 "sudo mv /dev/shm/hidden.obj $tgt && sudo stat -c '    已复原 %n %s 字节' $tgt
sudo systemctl restart grovefs-data-server && echo '    data-server 已重启'"
for i in $(seq 1 40); do ssh -n gfs2 'ss -ltn | grep -c :50100' | grep -qv '^0$' && break; sleep 0.25; done
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
echo "  复原后 du --total: $(DU '--total')"
echo "  复原后挂载 md5: $(md5sum $M/datasets/big.bin 2>&1)"
echo "  挂载仍可用: $(ls $M | tr '\n' ' ')"
REMOTE
