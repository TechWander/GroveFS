#!/bin/bash
# 集群功能实测 第四部分: 条带放置 / 跨端一致性与失效收敛 / HA 主备 / 带宽限速 / --stats 遥测.
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
M=$HOME/mnt
GV="grovefs view --meta-dir $HOME/meta"
ENV=/etc/grovefs/cluster.env
as_root_env(){ sudo sh -c "set -a; . $ENV; set +a; $1"; }
tmnt(){
  sudo mkdir -p "$2" && sudo chown hanjun:hanjun "$2"
  sudo systemd-run --unit="gft-$3" --collect --quiet \
    -p EnvironmentFile=$ENV -p User=hanjun \
    /usr/local/bin/grovefs mount ${4:-} --view "$1" -m "$2" --foreground
  for i in $(seq 1 60); do mountpoint -q "$2" && break; sleep 0.25; done
  echo "    [$1 -> $2: $(mountpoint -q "$2" && echo 已挂载 || echo 未挂载)]"
}
tkill(){ sudo systemctl stop "gft-$1" 2>/dev/null; sudo rm -rf "/tmp/gft_$1"; }
q(){ python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[1]);print(' ; '.join(','.join(str(x) for x in r) for r in c.execute(sys.argv[2])))" "$1" "$2" 2>&1; }

echo "===== E1 条带放置: 对象真的落在两台机器/两块盘上 ====="
cd "$M"; rm -f stripe200.bin
dd if=/dev/urandom of=stripe200.bin bs=1M count=200 2>/dev/null
sync stripe200.bin 2>/dev/null; sync; sleep 1
echo "  写入 200MiB 文件: $(stat -c '%s 字节' stripe200.bin)"
dref=$(q "$HOME/meta/meta/alice.db" "select data_ref from node where name='stripe200.bin'")
echo "  元数据里的 data_ref: $dref  (格式 s<服务器数>:<条带字节>:<uuid>)"
uuid=$(echo "$dref" | awk -F: '{print $3}')
echo "  条带几何: $((200*1024*1024)) / $(echo "$dref" | awk -F: '{print $2}') = $(( (200*1024*1024 + $(echo "$dref" | awk -F: '{print $2}') - 1) / $(echo "$dref" | awk -F: '{print $2}') )) 个分块, 第 i 块放第 i%N 台"
for h in gfs1 gfs2; do for r in /data/gfs1/grove /data/gfs2/grove; do
  printf "  %s : %s -> 本文件分块 %s\n" "$h" "$r" "$(ssh -n $h "sudo find $r/blocks -name '${uuid}#'\\* -printf '%f %s\\n' 2>/dev/null | sort | tr '\n' ';'")"
done; done
echo "  分块总数/总字节(两台相加应等于文件大小): $(for h in gfs1 gfs2; do ssh -n $h "sudo find /data/gfs*/grove/blocks -name '${uuid}#'\\* -printf '%s\\n'" 2>/dev/null; done | awk '{n++; s+=$1} END{print n" 块, "s" 字节"}')"
echo "  读取完整性: 挂载点内 md5=$(md5sum stripe200.bin | cut -d' ' -f1) 与 dd 源同随机流不可比, 改写回读校验:"
dd if=/dev/urandom of=stripe48.bin bs=1M count=48 2>/dev/null; sync; sleep 1
src_md5=$(md5sum stripe48.bin | cut -d' ' -f1)
drop_caches_ok=$(sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null && echo 已清 || echo 未支持')
echo "    写回校验($drop_caches_ok 后重读): $(md5sum stripe48.bin | cut -d' ' -f1) vs 写入时 $src_md5 $([ "$src_md5" = "$(md5sum stripe48.bin | cut -d' ' -f1)" ] && echo '(一致 OK)' || echo '(不一致 FAIL)')"
uuid48=$(q "$HOME/meta/meta/alice.db" "select data_ref from node where name='stripe48.bin'" | awk -F: '{print $3}')
echo "    48MiB 文件的分块分布: gfs1=$(ssh -n gfs1 "sudo find /data/gfs*/grove/blocks -name '${uuid48}#'\\* | wc -l") 个, gfs2=$(ssh -n gfs2 "sudo find /data/gfs*/grove/blocks -name '${uuid48}#'\\* | wc -l") 个"
echo "  同机双根的放置由 key 哈希决定(data-server --data-dir a,b), 跨机由条带决定"

echo "===== E2 跨端一致性与失效收敛窗口 ====="
tmnt alice /tmp/gft_two two
echo "  挂载 A=~/mnt, 挂载 B=/tmp/gft_two (同一 view, 同一 meta-server)"
printf 'converge-%s\n' "$(date +%s.%N)" > $M/conv.txt
t0=$(date +%s.%N)
for i in $(seq 1 200); do [ -f /tmp/gft_two/conv.txt ] && break; sleep 0.05; done
t1=$(date +%s.%N)
echo "  A 侧写入 -> B 侧可见耗时: $(python3 -c "print(f'{$t1-$t0:.3f} s')")"
echo "  B 侧内容: $(cat /tmp/gft_two/conv.txt 2>&1)"
echo "  B 侧目录列举已含新名: $(ls /tmp/gft_two | grep -c conv.txt)"
printf 'from-B\n' > /tmp/gft_two/fromb.txt
sleep 0.3
echo "  B 写入 -> A 可见: $(cat $M/fromb.txt 2>&1)"
echo "  目录删除的收敛: $(rm /tmp/gft_two/fromb.txt; sleep 0.3; test -e $M/fromb.txt && echo 'A 侧仍可见(未收敛)' || echo 'A 侧已消失(OK)')"
echo "  透镜定义改动的收敛(≤2s TTL):"
printf 'hide /conv.txt\n' > /tmp/e2.rules
$GV define --name e2lens --base alice --rules /tmp/e2.rules >/dev/null 2>&1
tmnt e2lens /tmp/gft_lens lens
echo "    初版: conv.txt $(test -e /tmp/gft_lens/conv.txt && echo 可见 || echo 不可见)"
printf 'map /datasets /dataX\n' > /tmp/e2.rules
t0=$(date +%s.%N)
$GV define --name e2lens --base alice --rules /tmp/e2.rules >/dev/null 2>&1
for i in $(seq 1 100); do [ -d /tmp/gft_lens/dataX ] && break; sleep 0.05; done
t1=$(date +%s.%N)
echo "    重定义后同挂载未重挂即收敛: dataX 出现耗时 $(python3 -c "print(f'{$t1-$t0:.3f} s')") (活挂载自动取新形状)"
tkill lens; tkill two; $GV rm --name e2lens 2>/dev/null

echo "===== E3 HA 主备切换 ====="
HA=/tmp/gft_ha; sudo systemctl stop gft-haA gft-haB gft-ham gft-haA2 gft-hamnt 2>/dev/null
sudo fusermount -u $HA/mnt 2>/dev/null; sudo fusermount -u $HA/sb 2>/dev/null
sudo rm -rf $HA; sudo mkdir -p $HA/A $HA/B $HA/mnt $HA/sb && sudo chown -R hanjun $HA
sudo systemd-run --unit=gft-haA --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs meta-server --meta-dir $HA/A --listen 127.0.0.1:50171 --peer 127.0.0.1:50172
sudo systemd-run --unit=gft-haB --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs meta-server --meta-dir $HA/B --listen 127.0.0.1:50172 --role standby --peer 127.0.0.1:50171
for i in $(seq 1 30); do ss -ltn | grep -q :50171 && ss -ltn | grep -q :50172 && break; sleep 0.2; done
grovefs view --meta-dir $HA/A create --view hatest 2>&1 | sed 's/^/  主上建视图: /'
tmnt hatest $HA/mnt ham "--meta-server 127.0.0.1:50171,127.0.0.1:50172"
sudo systemd-run --unit=gft-hamnt --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs mount --view hatest -m $HA/sb --meta-server 127.0.0.1:50172 --foreground
for i in $(seq 1 30); do mountpoint -q $HA/sb && break; sleep 0.25; done
echo "  只连备库的挂载: $(mountpoint -q $HA/sb && echo 已挂载 || echo 未挂载)"
printf 'before-failover\n' > $HA/mnt/w1.txt; sync $HA/mnt/w1.txt; sync
sleep 2   # 等 ~500ms 快照节拍把 w1 送到备
echo "  备侧已复制: $(cat $HA/sb/w1.txt 2>&1)"
echo "  备侧写必须被拒(standby 只读): $(printf x 2>/tmp/sberr > $HA/sb/nope.txt; echo "rc=$? 报错=[$(cat /tmp/sberr)] 文件=$(test -e $HA/sb/nope.txt && echo 存在 || echo 不存在)")"
echo "  杀死主库(异步快照复制, RPO≈500ms):"
SINCE=$(date '+%Y-%m-%d %H:%M:%S')
sudo systemctl stop gft-haA
t0=$(date +%s.%N)
for i in $(seq 1 40); do sudo journalctl -u gft-haB -b --no-pager --since "$SINCE" | grep -q 'promoted to primary' && break; sleep 0.2; done
t1=$(date +%s.%N)
echo "  备自升主耗时(stop -> promoted): $(python3 -c "print(f'{$t1-$t0:.2f} s')") 共轮询 $i 次"
echo "  备库日志(仅本轮): $(sudo journalctl -u gft-haB -b --no-pager --since "$SINCE" | grep -iE 'promot|standby|primary' | tail -3 | tr '\n' '|')"
t2=$(date +%s.%N)
printf 'after-failover\n' 2>/tmp/haerr > $HA/mnt/w2.txt; rc=$?
t3=$(date +%s.%N)
echo "  故障后第一次写: rc=$rc 报错=[$(cat /tmp/haerr)] 耗时 $(python3 -c "print(f'{$t3-$t2:.2f} s')")"
sleep 1
printf 'retry-1\n' 2>/tmp/haerr2 > $HA/mnt/w3.txt; rc2=$?
echo "  1s 后重试写: rc=$rc2 报错=[$(cat /tmp/haerr2)] w3.txt=$(cat $HA/mnt/w3.txt 2>&1)"
printf 'retry-2\n' 2>/tmp/haerr3 > $HA/mnt/w4.txt; rc3=$?
echo "  第二次重试写: rc=$rc3 报错=[$(cat /tmp/haerr3)] w4.txt=$(cat $HA/mnt/w4.txt 2>&1)"
echo "  切换前已复制的数据仍在: $(cat $HA/mnt/w1.txt 2>&1)"
echo "  切换后写入可读: $(cat $HA/mnt/w2.txt 2>&1)"
echo "  重启旧主 -> 必须以备身份回归(不得双主):"
sudo systemd-run --unit=gft-haA2 --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs meta-server --meta-dir $HA/A --listen 127.0.0.1:50171 --peer 127.0.0.1:50172
sleep 4
echo "    旧主日志(仅本轮): $(sudo journalctl -u gft-haA2 -b --no-pager --since "$SINCE" | grep -iE 'demoted|standby|primary|fenc|epoch' | tail -4 | tr '\n' '|')"
echo "    现有内容: $(ls $HA/mnt | tr '\n' ' ')"
sudo systemctl stop gft-hamnt gft-ham gft-haA2 gft-haB gft-haA 2>/dev/null
sudo fusermount -u $HA/mnt 2>/dev/null; sudo fusermount -u $HA/sb 2>/dev/null
sudo rm -rf $HA /tmp/haerr /tmp/sberr
echo "  HA 栈已拆: 残留 HA 进程 $(pgrep -af 'grovefs meta-server --meta-dir /tmp/gft_ha' | wc -l) 个, 监听 $(ss -ltn | grep -cE ':5017[12]') 个"

echo "===== E4 带宽限速 ====="
cd "$M"; dd if=/dev/urandom of=bw24.bin bs=1M count=24 2>/dev/null; sync; sleep 1
free=$(as_root_env 'true')
tmnt alice /tmp/gft_bw0 bw0 "--bw-read 0"
echo "  --bw-read 0(表示不限) 单测结果见上方挂载行"; tkill bw0
tmnt alice /tmp/gft_bw bw "--bw-read 8M --bw-write 8M"
echo "  8MiB/s 下顺序读 24MiB:"
t0=$(date +%s.%N); dd if=/tmp/gft_bw/bw24.bin of=/dev/null bs=1M 2>/dev/null; t1=$(date +%s.%N)
python3 -c "d=$t1-$t0; print(f'    实测 {d:.2f} s -> {24/d:.1f} MiB/s (限速 8 MiB/s, 期望 2.5~4.5 s)')"
echo "  8MiB/s 下写 24MiB:"
t0=$(date +%s.%N); dd if=/dev/zero of=/tmp/gft_bw/wr.bin bs=1M count=24 conv=fsync 2>/dev/null; t1=$(date +%s.%N)
python3 -c "d=$t1-$t0; print(f'    实测 {d:.2f} s -> {24/d:.1f} MiB/s')"
tkill bw
echo "  非法速率值的拒绝原文:"
sudo systemd-run --unit=gft-bwbad --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs mount --view alice -m /tmp/gft_bwbad --bw-read 12Z --foreground 2>&1 | sed 's/^/    /'
sleep 1.5; echo "    单元结局: $(systemctl is-active gft-bwbad 2>&1) ; 挂载: $(mountpoint -q /tmp/gft_bwbad && echo 在 || echo 不在)"
echo "    日志: $(sudo journalctl -u gft-bwbad -b --no-pager | tail -2 | tr '\n' '|')"
sudo systemctl stop gft-bwbad 2>/dev/null; sudo rm -rf /tmp/gft_bwbad /tmp/gft_bw0
echo "  只读挂载接受 --bw-write 但仍拒写:"
tmnt alice /tmp/gft_bro bro "--ro --bw-write 8M"
echo "    挂载: $(mountpoint -q /tmp/gft_bro && echo 已挂载 || echo 未挂载) ; 写: $(printf x > /tmp/gft_bro/z 2>&1 || echo 被拒)"
tkill bro

echo "===== E5 --stats 遥测 ====="
tmnt alice /tmp/gft_st st "--stats --stats-interval 2 --stats-file /tmp/gft_stats.log"
printf 'stats-probe\n' > /tmp/gft_st/sp.txt; cat /tmp/gft_st/bw24.bin >/dev/null 2>&1
sleep 5
echo "  --stats-file 内容(前 12 行):"; sudo cat /tmp/gft_stats.log 2>&1 | head -12 | sed 's/^/    /'
echo "  journal 里的遥测行:"; sudo journalctl -u gft-st -b --no-pager 2>&1 | grep -iE 'stats|ops|bytes' | tail -5 | sed 's/^/    /'
tkill st
echo "  未加 --stats 时不产出遥测: $(sudo journalctl -u gft-two -b --no-pager 2>/dev/null | grep -ciE 'stats') 行"
echo "  GROVEFS_STATS 环境变量兼底存在: $(grep -ac GROVEFS_STATS /usr/local/bin/grovefs) 处命中"
sudo rm -rf /tmp/gft_stenv
echo "===== E6 收尾: 集群回到部署态 ====="
cd "$M"; rm -f stripe200.bin stripe48.bin bw24.bin wr.bin conv.txt sp.txt 2>/dev/null; sync
$GV rm --name hatest 2>/dev/null; grovefs view --meta-dir /tmp/gft_ha rm --name hatest 2>/dev/null
$GV list 2>&1 | sed 's/^/  /'
systemctl is-enabled grovefs-meta-server.service grovefs-mount-alice.service 2>/dev/null | sed 's/^/  本机(meta+挂载)单元 enable: /'
systemctl is-active grovefs-meta-server.service grovefs-mount-alice.service 2>/dev/null | sed 's/^/  本机单元 active: /'
ssh -n gfs1 'systemctl is-enabled grovefs-data-server; systemctl is-active grovefs-data-server' 2>&1 | sed 's/^/  gfs1: /'
ssh -n gfs2 'systemctl is-enabled grovefs-data-server; systemctl is-active grovefs-data-server' 2>&1 | sed 's/^/  gfs2: /'
echo "  残留 transient 单元: $(systemctl list-units 'gft-*' --no-legend | wc -l)"
echo "  残留挂载: $(mount | grep -c grovefs)"
for u in $(systemctl list-units --type=service --no-legend 'gft-*.service' | awk '{print $1}'); do sudo systemctl stop "$u"; done
sudo systemctl reset-failed 2>/dev/null
echo "  清扫 transient 后: 单元 $(systemctl list-units 'gft-*' --no-legend | wc -l) 个, 挂载 $(mount | grep -c grovefs) 个"
echo "  非 systemd 管的 grovefs 进程(应为 0): $(for p in $(pgrep -x grovefs); do grep -q 'grovefs-.*\.service' /proc/$p/cgroup 2>/dev/null || echo "$p"; done | tr '\n' ' ')"
REMOTE
