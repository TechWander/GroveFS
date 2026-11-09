#!/bin/bash
# Two decisive loss probes:
#   L1 local store, one WHOLE object unlinked  -> does du warn?
#   L2 striped object, its FIRST chunk unlinked -> what does du report?
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
ENV=/etc/grovefs/cluster.env
P=$HOME/gftprobe
SRC=/tmp/gft_probe_src
as_root_env(){ sudo sh -c "set -a; . $ENV; set +a; $1"; }
q(){ sudo python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[1]);print(' ; '.join(','.join(str(x) for x in r) for r in c.execute(sys.argv[2])))" "$1" "$2" 2>&1; }

echo "===== L1 本地存储根: 整个对象被 unlink ====="
rm -rf "$P" "$SRC"; mkdir -p "$SRC/docs"
head -c 300000 /dev/urandom > "$SRC/docs/one.bin"
head -c 700000 /dev/urandom > "$SRC/docs/two.bin"
grovefs --home "$P" init --view probe 2>&1 | sed 's/^/  init: /'
grovefs import --meta-dir "$P" --view probe --source "$SRC" 2>&1 | sed 's/^/  import: /'
echo "  blocks 对象数: $(find $P/blocks -type f | wc -l)"
echo "  du --meta-dir 基线: $(grovefs du --meta-dir "$P" --view probe -b --total 2>&1 | tr '\n' '|')"
gone=$(q "$P/meta/probe.db" "select data_ref from node where name='two.bin'" | awk '{print $1}')
path=$(find $P/blocks -name "$gone" -type f | head -1)
echo "  视图树 data_ref: two.bin=$gone"
echo "  将 unlink: $path  ($(stat -c %s "$path") 字节)"
rm "$path"
echo "  unlink 后 du --meta-dir: $(grovefs du --meta-dir "$P" --view probe -b --total 2>&1 | tr '\n' '|')"
echo "  unlink 后 du -b --path /docs: $(grovefs du --meta-dir "$P" --view probe -b --path /docs 2>&1 | tr '\n' '|')"
echo "  挂载该本地视图后读缺失文件:"
sudo mkdir -p /tmp/gft_probe && sudo chown hanjun:hanjun /tmp/gft_probe
sudo systemd-run --unit=gft-probe --collect --quiet -p User=hanjun /usr/local/bin/grovefs mount --view probe --home "$P" -m /tmp/gft_probe --foreground
for i in $(seq 1 40); do mountpoint -q /tmp/gft_probe && break; sleep 0.25; done
echo "    挂载: $(mountpoint -q /tmp/gft_probe && echo 已挂载 || echo 未挂载)"
cat /tmp/gft_probe/docs/one.bin >/dev/null 2>/tmp/r1; echo "    读存在的 one.bin: rc=$? 报错=[$(cat /tmp/r1)]"
cat /tmp/gft_probe/docs/two.bin >/dev/null 2>/tmp/r2; echo "    读缺失的 two.bin: rc=$? 报错=[$(cat /tmp/r2)]"
echo "    stat two.bin: $(stat -c 'size=%s' /tmp/gft_probe/docs/two.bin 2>&1)"
sudo systemctl stop gft-probe; sudo rm -rf /tmp/gft_probe
grovefs view --meta-dir "$P" rm --name probe --base --data-dir "$P" >/dev/null 2>&1
rm -rf "$P" "$SRC"

echo "===== L2 条带对象: 第一个分块 chunk#0 被 unlink(gfs1) + 重启该 data-server ====="
DB=$HOME/meta/meta/alice.db
ref=$(q "$DB" "select data_ref from node where name='big.bin' and data_ref like 's%'" | awk '{print $1}')
uuid=${ref##*:}
tgt=$(ssh -n gfs1 "sudo find /data/gfs*/grove/blocks -name \"${uuid}#0\" -type f")
echo "  data_ref=$ref  目标: gfs1:$tgt ($(ssh -n gfs1 "sudo stat -c %s $tgt") 字节)"
echo "  基线 du --total: $(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | tr '\n' '|')"
ssh -n gfs1 "sudo cp $tgt /dev/shm/c0.obj && sudo rm $tgt && echo '    已 unlink'"
ssh -n gfs1 "sudo systemctl restart grovefs-data-server && echo '    gfs1 data-server 已重启(fd 缓存清空)'"
sleep 2
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
echo "  unlink 后 du --total:      $(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | tr '\n' '|')"
echo "  unlink 后 du -b --path /datasets: $(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 -b --path /datasets' 2>&1 | tr '\n' '|')"
echo "  挂载 stat size: $(stat -c %s $HOME/mnt/datasets/big.bin 2>&1)"
md5sum $HOME/mnt/datasets/big.bin >/dev/null 2>/tmp/m2; echo "  挂载整文件读: rc=$? 报错=[$(cat /tmp/m2)]"
echo "  du 单地址(gfs1 已无 chunk#0)必须拒绝: $(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100 --total' 2>&1 | tr '\n' '|')"
ssh -n gfs1 "sudo mv /dev/shm/c0.obj $tgt && sudo systemctl restart grovefs-data-server && sudo stat -c '    已复原 %n %s 字节' $tgt"
sleep 2
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
echo "  复原后 du --total: $(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | tr '\n' '|')"
echo "  复原后挂载 md5: $(md5sum $HOME/mnt/datasets/big.bin 2>&1)"
REMOTE
