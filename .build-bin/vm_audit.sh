#!/bin/bash
# 步骤 1/2 的收尾复核: 物理机 168/170 无残留 + 三台虚机 systemd 唯一入口.
# 输出即测试报告要照抄的回显.
set -uo pipefail
export LC_ALL=C
export LC_ALL=C
echo "##### [168 本机] 程序侧残留复核 #####"
echo "-- which grovefs: $(command -v grovefs || echo 无)"
echo "-- /usr/local/bin/grovefs: $(ls -l /usr/local/bin/grovefs 2>&1)"
echo "-- 单元文件: $(ls /etc/systemd/system/ 2>/dev/null | grep -i grovefs || echo 无)"
echo "-- 进程: $(pgrep -x grovefs | wc -l) 个 ; 详情: $(pgrep -ax grovefs | tr "\n" "|")"
echo "-- 挂载: $(mount | grep -i grovefs | wc -l) 条 ; 详情: $(mount | grep -i grovefs | awk "{print \$1, \$3}" | tr "\n" "|")"
echo "-- 单元文件 systemd 状态: $(systemctl list-unit-files --no-legend "grovefs*" | tr "\n" "|")[空=无单元]"
echo "-- 监听: $(ss -ltnp 2>/dev/null | grep -cE ':5005[0-9]|:501[0-9][0-9]' ) 个 grovefs 端口"
echo "-- 遗留脚本入口(cron/fstab/profile.d/补全): $(grep -rIl grovefs /etc/cron* /etc/fstab /etc/profile.d /etc/bash_completion.d 2>/dev/null | wc -l) 处"
echo "-- 此前商定保留的数据(不动): /tmp/du-test=$(du -sh /tmp/du-test 2>/dev/null | cut -f1)"
echo
echo "##### [170 远端] 程序侧残留复核 #####"
ssh -p 2023 -o BatchMode=yes me@10.3.10.170 'bash -s' <<'REMOTE170' 2>&1 | grep -v setlocale
echo "-- which grovefs: $(command -v grovefs || echo 无)"
echo "-- /usr/local/bin/grovefs: $(ls -l /usr/local/bin/grovefs 2>&1)"
echo "-- 单元文件: $(ls /etc/systemd/system/ /lib/systemd/system/ 2>/dev/null | grep -i grovefs || echo 无)"
echo "-- 进程: $(pgrep -x grovefs | wc -l) 个"
echo "-- 挂载: $(mount | grep -i grovefs | wc -l) 条"
echo "-- 监听: $(sudo ss -ltn 2>/dev/null | grep -cE ":5000[0-9]|:5005[0-9]") 个"
echo "-- /etc/grovefs: $(ls /etc/grovefs 2>&1)"
echo "-- 商定保留: /data/gfs=$(du -sh /data/gfs 2>/dev/null | cut -f1) ; 容器 $(sudo docker ps --format "{{.Names}} {{.Status}}" 2>/dev/null | grep grovefs-pg || echo 无)"
REMOTE170
echo
for h in gfs1 gfs2; do
  echo "##### [$h] data-server 单元复核 #####"
  ssh -o BatchMode=yes $h '
  U=/etc/systemd/system/grovefs-data-server.service
  echo "-- 单元清单: $(systemctl list-unit-files "grovefs-*" --no-legend | tr "\n" " ")"
  echo "-- is-enabled / is-active: $(systemctl is-enabled grovefs-data-server) / $(systemctl is-active grovefs-data-server)"
  echo "-- ExecStart: $(grep "^ExecStart" $U)"
  echo "-- Restart/LimitNOFILE: $(grep -E "^(Restart|RestartSec|LimitNOFILE)" $U | tr "\n" " ")"
  echo "-- 单元文本里是否出现 token: $(grep -icE "token|secret|key" $U) 处"
  echo "-- EnvironmentFile: $(grep "^EnvironmentFile" $U)"
  echo "-- env 文件权限: $(stat -c "%a %U:%G %n" /etc/grovefs/cluster.env)"
  echo "-- 密钥是否出现在 ps/argv: $(tok=$(sudo cut -d= -f2 /etc/grovefs/cluster.env | head -1); n=0; while read -r l; do case "$l" in *"$tok"*) n=$((n+1));; esac; done < <(ps -eo args=); echo $n)"
  echo "-- grovefs 进程(全路径 argv): "; pgrep -x grovefs | while read p; do tr "\0" " " < /proc/$p/cmdline; echo " [cgroup: $(cut -d: -f3 /proc/$p/cgroup)]"; done
  echo "-- 监听: $(ss -ltn | grep 50100 | awk "{print \$4}")"
  echo "-- 数据根: $(ls -d /data/gfs*/grove 2>/dev/null | tr "\n" " ")"
  echo "-- 二进制 md5: $(md5sum /usr/local/bin/grovefs | cut -d" " -f1)"
  echo "-- 非 systemd 入口残留: $(ls $HOME/grove_cluster.sh $HOME/grove_start.sh $HOME/start-grovefs.sh 2>/dev/null | tr "\n" " ")[空=已清]"
  echo "-- nohup 日志残留: $(ls /tmp/grove*.log /tmp/mnt.log 2>/dev/null | tr "\n" " ")[空=已清]"' 2>&1 | grep -v setlocale
  echo
done
echo "##### [gfs-client] meta-server + 挂载单元复核 #####"
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTEC' 2>&1 | grep -v setlocale
for u in grovefs-meta-server grovefs-mount-alice; do
  echo "-- $u: enabled=$(systemctl is-enabled $u) active=$(systemctl is-active $u)"
  echo "   ExecStart: $(grep ^ExecStart /etc/systemd/system/$u.service)"
  echo "   运维项: $(grep -E "^(Restart|RestartSec|LimitNOFILE|TimeoutStopSec|User|EnvironmentFile)" /etc/systemd/system/$u.service | tr "\n" " ")"
done
echo "-- 单元文件清单(唯一入口): "; ls -1 /etc/systemd/system/grovefs-*.service | sed "s/^/   /"
echo "-- 是否有 disabled 的并行单元: $(systemctl list-unit-files "grovefs-*" --no-legend | grep -v enabled | tr "\n" " ")[空=无]"
echo "-- env 文件: $(sudo stat -c "%a %U:%G %n" /etc/grovefs/cluster.env) ; 变量名(只取名字, 不取值): $(sudo cut -d= -f1 /etc/grovefs/cluster.env | tr "\n" " ")"
tok=$(sudo grep "^GROVEFS_TOKEN=" /etc/grovefs/cluster.env | cut -d= -f2-)
n=0; while read -r l; do case "$l" in *"$tok"*) n=$((n+1));; esac; done < <(ps -eo args=)
echo "-- GROVEFS_TOKEN 值在 ps/argv 中出现次数: $n (0 = 不进 argv)"
echo "-- GROVEFS_TOKEN 值在单元文本中出现次数: $(sudo grep -RF -e "$tok" /etc/systemd/system/ 2>/dev/null | wc -l)"
echo "-- 进程与归属: "; pgrep -x grovefs | while read p; do printf "   pid=%s argv=[" "$p"; tr "\0" " " < /proc/$p/cmdline | cut -c1-120; printf "] cgroup=%s\n" "$(cut -d: -f3 /proc/$p/cgroup)"; done
echo "-- 挂载: $(mount | grep grovefs | awk "{print \$1, \$3}")"
echo "-- 非 systemd 管的 grovefs 进程(应为 0): $(for p in $(pgrep -x grovefs); do grep -q "grovefs-.*\.service" /proc/$p/cgroup 2>/dev/null || echo "$p"; done | tr "\n" " ")"
echo "-- du 实测(条带路径): "; sudo sh -c "set -a; . /etc/grovefs/cluster.env; set +a; grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total" 2>&1 | sed "s/^/   /"
echo "-- 二进制 md5: $(md5sum /usr/local/bin/grovefs | cut -d" " -f1)"
echo "-- 非 systemd 入口残留: $(ls $HOME/grove_cluster.sh $HOME/grove_start.sh $HOME/start-grovefs.sh /tmp/grove*.log 2>/dev/null | tr "\n" " ")[空=已清]"
echo "-- 开机自起验证(单元 WantedBy): $(grep -A1 "^\[Install\]" /etc/systemd/system/grovefs-mount-alice.service | tail -1)"
echo "-- 上次开机时间: $(uptime -s 2>/dev/null) ; 单元启动时间: $(sudo systemctl show grovefs-meta-server -p ExecMainStartTimestamp --value)"
echo
echo "-- 崩溃自动拉起验证(SIGKILL meta-server -> systemd 重起):"
PID0=$( systemctl show grovefs-meta-server -p MainPID --value )
ST0=$(sudo systemctl show grovefs-meta-server -p ExecMainStartTimestamp -p NRestarts | tr "\n" " ")
echo "   杀前: pid=$PID0 $ST0"
sudo kill -9 "$PID0"
for i in $(seq 1 40); do ss -ltn | grep -q :50053 && break; sleep 0.25; done
PID1=$( systemctl show grovefs-meta-server -p MainPID --value )
ST1=$(sudo systemctl show grovefs-meta-server -p ExecMainStartTimestamp -p NRestarts | tr "\n" " ")
echo "   杀后(${i}x0.25s 后监听已恢复): pid=$PID1 $ST1"
echo "   存活挂载仍可用: $(ls $HOME/mnt >/dev/null 2>&1 && echo 可读 || echo 异常) ; 写入 $(printf "after-kill" > $HOME/mnt/killprobe.txt 2>&1 && cat $HOME/mnt/killprobe.txt || echo 失败)"
rm -f $HOME/mnt/killprobe.txt
echo "   journal: $(sudo journalctl -u grovefs-meta-server -b --no-pager | tail -3 | tr "\n" "|")"
REMOTEC
echo
echo "##### [构建产物] md5 对账 #####"
md5sum /data/grovefs/grovefs/target/release/grovefs
