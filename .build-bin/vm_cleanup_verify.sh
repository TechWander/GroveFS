#!/bin/bash
# Post-test cleanup verification: prove the cluster is back to the pure systemd
# deployed state, no probe leftovers (transient units, dirs, mounts, definitions),
# and that objects moved/deleted during loss-simulation probes are restored.
set -uo pipefail
export LC_ALL=C
RD=$(mktemp -d)
trap 'rm -rf "$RD"' EXIT

cat >"$RD/client" <<'REMOTE'
GV="grovefs view --meta-dir $HOME/meta"
say(){ sudo sh -c 'set -a; . /etc/grovefs/cluster.env; set +a; '"$1" 2>&1; }
echo "-- 清场动作: 删除探针/测试遗留目录与临时文件"
for d in /tmp/gft_pva /tmp/gft_pva2 /tmp/gft_pvb /tmp/gft_pv /tmp/gft_pv3 /tmp/gft_src /tmp/gft_fk /tmp/gft_efk /tmp/gft_lab /tmp/gft_lab2 /tmp/gft_c2 /tmp/gft_dang /tmp/gft_pc /tmp/gft_probe /tmp/gft_two /tmp/gft_gate /tmp/gft_keystore /tmp/gft_stripe /tmp/gft_ha /tmp/gft_bw; do
  mountpoint -q "$d" 2>/dev/null && { fusermount -u "$d" 2>&1 | sed "s/^/    umount $d: /"; }
  [ -e "$d" ] && { rm -rf "$d" && echo "    removed $d"; }
done
rm -f /tmp/gft_stats.log /tmp/gft_*.out /tmp/gft_*.err /tmp/gft_imp_src /tmp/gft_imp.jsonl /tmp/gft_probe_src /tmp/c1.rules /tmp/e2.rules /tmp/b2.rules /tmp/alice.export.jsonl /tmp/x.jsonl /tmp/roerr /tmp/e1 /tmp/e1b /tmp/e2 /tmp/e3 /tmp/e4 /tmp/werr /tmp/hidden.obj
rm -rf /tmp/gft_imp_src /tmp/gft_probe_src
if [ -d "$HOME/gftimp" ]; then rm -rf "$HOME/gftimp" && echo "    removed $HOME/gftimp"; fi
if [ -d "$HOME/gftprobe" ]; then sudo lsof +D "$HOME/gftprobe" >/dev/null 2>&1 && echo "    WARN: gftprobe 仍被占用, 未删" || { rm -rf "$HOME/gftprobe" && echo "    removed $HOME/gftprobe"; }; fi
if [ -d "$HOME/gftstore" ]; then
  sudo lsof +D "$HOME/gftstore" >/dev/null 2>&1 && echo "    WARN: gftstore 仍被进程占用, 未删" || { rm -rf "$HOME/gftstore" && echo "    removed $HOME/gftstore"; }
fi
for n in b4.txt b4del.txt b4-renamed.txt only-here newdir; do
  [ -e "$HOME/mnt/$n" ] && { rm -rf "$HOME/mnt/$n" && echo "    基视图删除测试残留名: $n"; }
done
echo "-- 部署态单元: $(systemctl list-unit-files 'grovefs-*' --no-legend --no-pager | tr '\n' '|')"
echo "-- 探针 transient 单元残留(应为空): $(systemctl list-units --all --no-legend --no-pager 'gft-*' 2>/dev/null | tr '\n' '|')"
echo "-- grovefs 进程数(应为 2): $(pgrep -fc '/usr/local/bin/grovefs' 2>/dev/null || echo 0)"
echo "-- 非 systemd 管的 grovefs 进程(应为空):"
for p in $(pgrep -f '/usr/local/bin/grovefs'); do
  cg=$(cat /proc/$p/cgroup 2>/dev/null || sudo cat /proc/$p/cgroup 2>/dev/null)
  case "$cg" in *grovefs-meta-server*|*grovefs-mount-alice*) ;; *)
    echo "   pid=$p argv=[$(tr '\0' ' ' </proc/$p/cmdline)] cgroup=$cg";;
  esac
done
echo "-- 挂载清单: $(grep grovefs /proc/mounts | awk '{print $1" "$2}' | tr '\n' '|')"
echo "-- 探针目录残留 /tmp/gft_*: $(ls -d /tmp/gft_* 2>&1 | tr '\n' ' ')"
echo "-- 探针存储根/临时文件残留: $(ls -d $HOME/gftstore $HOME/gftprobe /tmp/b2.rules /tmp/bogus.rules /tmp/vm_b*.out 2>&1 | tr '\n' ' ')"
echo "-- 视图定义清单: $($GV list 2>&1 | tr '\n' '|')"
echo "-- 基视图内容(应为 P0 记录的水位基线): [$(ls $HOME/mnt 2>&1 | tr '\n' ' ')]"
echo "-- datasets: [$(ls $HOME/mnt/datasets 2>&1 | tr '\n' ' ')]"
echo "-- du 复原对账(基线 64.0M): $(say 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' | tr '\n' '|')"
REMOTE

cat >"$RD/data" <<'REMOTE'
echo "-- 对象数: $(sudo find /data/gfs1/grove/blocks /data/gfs2/grove/blocks -type f 2>/dev/null | wc -l)"
echo "-- 数据根用量: $(sudo du -sh /data/gfs1/grove /data/gfs2/grove 2>/dev/null | tr '\t' '=' | tr '\n' ' ')"
echo "-- /dev/shm 搬移残留(*.obj): $(sudo ls /dev/shm 2>/dev/null | grep -c '\.obj')"
echo "-- 单元: $(systemctl is-active grovefs-data-server.service) NRestarts=$(systemctl show -p NRestarts --value grovefs-data-server.service)"
echo "-- 二进制 md5: $(md5sum /usr/local/bin/grovefs | cut -d' ' -f1)"
REMOTE

echo "##### [gfs-client] 现场清场复核 #####"
ssh -o BatchMode=yes gfs-client 'bash -s' <"$RD/client" 2>&1 | grep -v setlocale
for h in gfs1 gfs2; do
  echo
  echo "##### [$h] 分块复原与残留复核 #####"
  ssh -o BatchMode=yes $h 'bash -s' <"$RD/data" 2>&1 | grep -v setlocale
done
