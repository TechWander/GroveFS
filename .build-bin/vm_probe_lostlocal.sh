#!/bin/bash
# L1b: local store, one object unlinked -> what does a mounted read do?
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
P=$HOME/gftprobe
SRC=/tmp/gft_probe_src
q(){ sudo python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[1]);print(' ; '.join(','.join(str(x) for x in r) for r in c.execute(sys.argv[2])))" "$1" "$2" 2>&1; }
echo "===== L1b 本地视图: 对象 unlink 后的挂载读 ====="
sudo systemctl stop gft-probe 2>/dev/null; sudo rm -rf /tmp/gft_probe
rm -rf "$P" "$SRC"; mkdir -p "$SRC/docs"
head -c 300000 /dev/urandom > "$SRC/docs/one.bin"
head -c 700000 /dev/urandom > "$SRC/docs/two.bin"
grovefs --home "$P" init --view probe 2>&1 | sed 's/^/  init: /'
grovefs import --meta-dir "$P" --view probe --source "$SRC" 2>&1 | sed 's/^/  import: /'
echo "  视图树: $(q "$P/meta/probe.db" "select '/'||name from node where parent=1")"
echo "  du -b --total 基线: $(grovefs du --meta-dir "$P" --view probe -b --total 2>&1 | tr '\n' '|')"
sudo mkdir -p /tmp/gft_probe && sudo chown hanjun:hanjun /tmp/gft_probe
sudo systemd-run --unit=gft-probe --collect --quiet -p User=hanjun /usr/local/bin/grovefs mount --view probe --home "$P" -m /tmp/gft_probe --foreground
for i in $(seq 1 40); do mountpoint -q /tmp/gft_probe && break; sleep 0.25; done
echo "  挂载: $(mountpoint -q /tmp/gft_probe && echo 已挂载 || echo 未挂载)"
echo "  挂载内清单: $(ls /tmp/gft_probe /tmp/gft_probe/gft_probe_src/docs 2>&1 | tr '\n' ' ')"
before=$(md5sum /tmp/gft_probe/gft_probe_src/docs/two.bin 2>&1)
gone=$(q "$P/meta/probe.db" "select data_ref from node where name='two.bin'" | awk '{print $1}')
path=$(find $P/blocks -name "$gone" -type f | head -1)
echo "  对象 $gone 的落点: $path ($(stat -c %s "$path") 字节)"
echo "  unlink 前读: rc=$(md5sum /tmp/gft_probe/gft_probe_src/docs/two.bin >/dev/null 2>&1; echo $?) md5=$(echo "$before" | awk '{print $1}')"
rm "$path"
echo "  unlink 后(旧挂载进程, 服务端 fd 缓存仍在):"
echo "    stat size: $(stat -c 'size=%s' /tmp/gft_probe/gft_probe_src/docs/two.bin 2>&1)"
md5sum /tmp/gft_probe/gft_probe_src/docs/two.bin >/dev/null 2>/tmp/r2; echo "    整文件读: rc=$? 报错=[$(cat /tmp/r2)]"
echo "    du -b --total: $(grovefs du --meta-dir "$P" --view probe -b --total 2>&1 | tr '\n' '|')"
echo "    du -b --path /gft_probe_src/docs: $(grovefs du --meta-dir "$P" --view probe -b --path /gft_probe_src/docs 2>&1 | tr '\n' '|')"
sudo systemctl stop gft-probe
sudo systemd-run --unit=gft-probe --collect --quiet -p User=hanjun /usr/local/bin/grovefs mount --view probe --home "$P" -m /tmp/gft_probe --foreground
for i in $(seq 1 40); do mountpoint -q /tmp/gft_probe && break; sleep 0.25; done
echo "  重挂后(fd 缓存已清):"
md5sum /tmp/gft_probe/gft_probe_src/docs/two.bin >/dev/null 2>/tmp/r3; echo "    整文件读: rc=$? 报错=[$(cat /tmp/r3)]"
echo "    stat size: $(stat -c 'size=%s' /tmp/gft_probe/gft_probe_src/docs/two.bin 2>&1)"
sudo systemctl stop gft-probe; sudo rm -rf /tmp/gft_probe
grovefs view --meta-dir "$P" rm --name probe --base --data-dir "$P" 2>&1 | sed 's/^/  收尾: /'
rm -rf "$P" "$SRC"
REMOTE
