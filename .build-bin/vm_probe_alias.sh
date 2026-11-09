#!/bin/bash
# Alias-edge probe: chains / single-alias-per-view / dangling alias target.
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
GV="grovefs view --meta-dir $HOME/meta"
ENV=/etc/grovefs/cluster.env
echo "===== X0 现场定义 ====="
$GV list 2>&1 | sed 's/^/  /'

echo "===== X1 链式别名(目标仍是活的别名) ====="
$GV alias --name x-alias1 --real pv 2>&1 | sed 's/^/  第一个: /'
$GV alias --name x-alias2 --real x-alias1 2>&1 | sed 's/^/  链式: /'
echo "  同一真实视图的第二个别名:"
$GV alias --name x-alias3 --real pv 2>&1 | sed 's/^/    /'

echo "===== X2 悬空目标别名(既无物理树也无定义) 的挂载结果 ====="
$GV alias --name x-dangle --real no-such-tree 2>&1 | sed 's/^/  alias: /'
sudo mkdir -p /tmp/gft_dang && sudo chown hanjun:hanjun /tmp/gft_dang
sudo systemd-run --unit=gft-dang --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs mount --view x-dangle -m /tmp/gft_dang --foreground
for i in $(seq 1 30); do mountpoint -q /tmp/gft_dang && break; sleep 0.25; done
echo "    [挂载 x-dangle -> /tmp/gft_dang: $(mountpoint -q /tmp/gft_dang && echo 已挂载 || echo 未挂载)]"
echo "    日志: $(sudo journalctl -u gft-dang --no-pager -o cat 2>/dev/null | tail -2 | tr '\n' '|')"
sudo systemctl stop gft-dang 2>/dev/null; sudo rm -rf /tmp/gft_dang

echo "===== X3 已存在的 pub-c(目标 pub-a 已被 retire) 能否挂载 ====="
sudo mkdir -p /tmp/gft_pc && sudo chown hanjun:hanjun /tmp/gft_pc
sudo systemd-run --unit=gft-pc --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs mount --view pub-c -m /tmp/gft_pc --foreground
for i in $(seq 1 30); do mountpoint -q /tmp/gft_pc && break; sleep 0.25; done
echo "    [挂载 pub-c -> /tmp/gft_pc: $(mountpoint -q /tmp/gft_pc && echo 已挂载 || echo 未挂载)]"
echo "    内容: $(ls /tmp/gft_pc 2>&1 | tr '\n' ' ')"
echo "    日志: $(sudo journalctl -u gft-pc --no-pager -o cat 2>/dev/null | tail -2 | tr '\n' '|')"
sudo systemctl stop gft-pc 2>/dev/null; sudo rm -rf /tmp/gft_pc

echo "===== X4 收尾 ====="
for n in x-alias1 x-alias2 x-alias3 x-dangle pub-c; do $GV rm --name "$n" 2>&1 | sed "s/^/  撤 $n: /"; done
$GV list 2>&1 | sed 's/^/  现状: /'
REMOTE
