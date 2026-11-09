#!/bin/bash
# GroveFS 三机测试集群的唯一部署入口: systemd 托管, 没有其他启停方式.
#
#   bash vm_redeploy.sh phase1      拆旧(单元/env/数据根/二进制/并行入口脚本)
#   bash vm_redeploy.sh phase2      上传新二进制并核对 md5
#   bash vm_redeploy.sh phase3      写 EnvironmentFile + 单元, enable --now, 建视图
#   bash vm_redeploy.sh verify      复核: 开机自起/无 argv 密钥/端口/挂载/读写
#
# 拓扑(固定, 改动等于换存储):
#   gfs1 192.168.122.124  data-server :50100  根 /data/gfs1/grove,/data/gfs2/grove
#   gfs2 192.168.122.193  data-server :50100  根 /data/gfs1/grove,/data/gfs2/grove
#   gfs-client 192.168.122.137  meta-server :50053 (~/meta) + 挂载 ~/mnt (view alice)
#
# 密钥约定: GROVEFS_TOKEN 随机生成, 只写进各机 /etc/grovefs/cluster.env(0600,
# root:root), 单元文本与命令行都不出现; 客户端命令靠 source 该文件鉴权.
# `--data-dir` 列表和 stripe 数一经建立必须固定, 增删盘/增删地址会重映射所有 key.
set -uo pipefail

BIN=/data/grovefs/grovefs/target/release/grovefs
EXPECT_MD5=$(md5sum "$BIN" | awk '{print $1}')
SSH="ssh -o BatchMode=yes -o ConnectTimeout=8"
DS_PORT=50100
MS_PORT=50053
DS_ROOTS=/data/gfs1/grove,/data/gfs2/grove
DS_ROOT_DIRS="/data/gfs1/grove /data/gfs2/grove"
VIEW=alice
CLIENT=hanjun
MS_HOME=/home/$CLIENT/meta
MNT=/home/$CLIENT/mnt
ENV_FILE=/etc/grovefs/cluster.env
U_DATA=/etc/systemd/system/grovefs-data-server.service
U_META=/etc/systemd/system/grovefs-meta-server.service
U_MOUNT=/etc/systemd/system/grovefs-mount-$VIEW.service
LAUNCHERS='$HOME/grove_cluster.sh $HOME/grove_start.sh $HOME/start-grovefs.sh'

# 停掉不属于 systemd 的残留进程(按 cgroup 判定, 不碰托管中的服务).
reap_strays() { # $1 = host
  $SSH $1 'for p in $(pgrep -x grovefs); do
      if ! grep -q "grovefs-.*\.service" /proc/$p/cgroup 2>/dev/null; then
        printf "    stray pid=%s %s\n" "$p" "$(tr -d "\0" < /proc/$p/cmdline | cut -c1-60)"; kill "$p"
      fi
    done; sleep 1
    for p in $(pgrep -x grovefs); do
      grep -q "grovefs-.*\.service" /proc/$p/cgroup 2>/dev/null || { echo "   强杀 stray pid=$p"; kill -9 "$p"; }
    done' 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
}

phase1() {
  echo "######## phase1 拆旧: 单元 / env / 数据根 / 二进制 / 并行入口 ########"
  for h in gfs1 gfs2 gfs-client; do
    echo "======== [$h] ========"
    $SSH $h "
      echo '-- 停并删单元 --'
      for u in grovefs-data-server grovefs-meta-server grovefs-mount-$VIEW; do
        sudo systemctl disable --now \$u.service 2>/dev/null || true
      done
      sudo rm -f $U_DATA $U_META $U_MOUNT
      sudo systemctl daemon-reload; sudo systemctl reset-failed 2>/dev/null || true
      echo '-- 卸挂载 --'
      findmnt -rn -o TARGET | grep -qx $MNT && { fusermount -u $MNT 2>/dev/null || sudo umount -l $MNT; } || echo '   无挂载'
    " 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
    echo '-- 非 systemd 的残留进程 --'; reap_strays $h
    $SSH $h "
      echo '-- 删 env 文件与并行入口脚本 --'
      sudo rm -rf /etc/grovefs
      rm -f $LAUNCHERS
      echo '-- 删旧数据根与视图树 --'
      sudo rm -rf $DS_ROOT_DIRS
      rm -rf $MS_HOME $MNT
      echo '-- 删旧二进制 --'
      sudo rm -f /usr/local/bin/grovefs
      command -v grovefs || echo '   PATH 已清空'
      echo \"   剩余: roots[\$(ls -d $DS_ROOT_DIRS 2>/dev/null | tr '\n' ' ')] meta[\$(ls -d $MS_HOME 2>/dev/null)] units[\$(ls /etc/systemd/system/grovefs* 2>/dev/null | wc -l)] env[\$(ls -d /etc/grovefs 2>/dev/null)]\"
      echo \"   并行入口脚本: \$(ls $LAUNCHERS 2>/dev/null | tr '\n' ' ')\"
    " 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
  done
}

phase2() {
  echo "######## phase2 上传二进制(构建产物 = 被测对象) ########"
  for h in gfs1 gfs2 gfs-client; do
    scp -q -o BatchMode=yes "$BIN" $h:/tmp/grovefs.new || { echo "  [$h] scp 失败"; return 1; }
    printf '%-11s ' $h
    $SSH $h 'sudo install -m 0755 -o root -g root /tmp/grovefs.new /usr/local/bin/grovefs && rm -f /tmp/grovefs.new && md5sum /usr/local/bin/grovefs' \
      2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
  done
  echo "  构建产物 md5: $EXPECT_MD5"
}

phase3() {
  echo "######## phase3 systemd 落地(密钥只进 EnvironmentFile) ########"
  local tok
  tok=$($SSH gfs1 'openssl rand -hex 16' 2>/dev/null | grep -E '^[0-9a-f]{32}$')
  [ -n "$tok" ] || { echo "  生成 token 失败"; return 1; }
  echo "  token 已随机生成(不回显), 写入各机 $ENV_FILE"

  for h in gfs1 gfs2; do
    printf "GROVEFS_TOKEN=%s\n" "$tok" | $SSH $h "sudo install -d -m 0755 -o root -g root /etc/grovefs && sudo tee $ENV_FILE >/dev/null && sudo chmod 0600 $ENV_FILE && sudo chown root:root $ENV_FILE" 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
    $SSH $h "sudo tee $U_DATA >/dev/null" <<UNIT
[Unit]
Description=GroveFS data-server (striped over /data/gfs1,/data/gfs2)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=$ENV_FILE
ExecStartPre=/bin/mkdir -p /data/gfs1/grove /data/gfs2/grove
ExecStart=/usr/local/bin/grovefs data-server --data-dir $DS_ROOTS --listen 0.0.0.0:$DS_PORT
ExecReload=/bin/kill -HUP \$MAINPID
LimitNOFILE=65536
Restart=on-failure
RestartSec=3
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
UNIT
    printf '%-11s ' $h
    $SSH $h "sudo systemctl daemon-reload && sudo systemctl enable --now grovefs-data-server.service >/dev/null 2>&1;
      for i in \$(seq 1 20); do ss -ltn | grep -q :$DS_PORT && break; sleep 0.2; done
      printf 'data-server=%s 端口=%s env权限=%s\n' \"\$(systemctl is-active grovefs-data-server.service)\" \"\$(ss -ltn | grep -c :$DS_PORT)\" \"\$(stat -c '%a %U:%G' $ENV_FILE)\"" 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
  done

  printf 'GROVEFS_TOKEN=%s\nGROVEFS_META_DIR=%s\nGROVEFS_META_SERVER=127.0.0.1:%s\nGROVEFS_DATA_SERVERS=gfs1:%s,gfs2:%s\n' \
    "$tok" "$MS_HOME" "$MS_PORT" "$DS_PORT" "$DS_PORT" \
    | $SSH gfs-client "sudo install -d -m 0755 -o root -g root /etc/grovefs && sudo tee $ENV_FILE >/dev/null && sudo chmod 0600 $ENV_FILE && sudo chown root:root $ENV_FILE" 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'

  $SSH gfs-client "sudo tee $U_META >/dev/null" <<UNIT
[Unit]
Description=GroveFS meta-server (view trees under $MS_HOME)
After=network-online.target
Wants=network-online.target

[Service]
User=$CLIENT
EnvironmentFile=$ENV_FILE
ExecStartPre=/bin/mkdir -p $MS_HOME
ExecStart=/usr/local/bin/grovefs meta-server --listen 0.0.0.0:$MS_PORT
ExecReload=/bin/kill -HUP \$MAINPID
LimitNOFILE=65536
Restart=on-failure
RestartSec=3
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
UNIT
  $SSH gfs-client "sudo tee $U_MOUNT >/dev/null" <<UNIT
[Unit]
Description=GroveFS mount view $VIEW at $MNT (striped over gfs1,gfs2)
Requires=grovefs-meta-server.service
After=grovefs-meta-server.service network-online.target

[Service]
User=$CLIENT
EnvironmentFile=$ENV_FILE
ExecStartPre=/bin/mkdir -p $MNT
ExecStartPre=-/usr/bin/fusermount -u $MNT
ExecStart=/usr/local/bin/grovefs mount --view $VIEW -m $MNT --foreground
ExecStop=/usr/bin/fusermount -u $MNT
ExecStopPost=-/usr/bin/fusermount -uz $MNT
LimitNOFILE=65536
Restart=on-failure
RestartSec=3
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
UNIT

  $SSH gfs-client "
    sudo systemctl daemon-reload
    sudo systemctl enable --now grovefs-meta-server.service >/dev/null 2>&1
    sleep 1
    for i in \$(seq 1 20); do ss -ltn | grep -q :$MS_PORT && break; sleep 0.2; done
    printf 'meta-server=%s 端口=%s\n' \"\$(systemctl is-active grovefs-meta-server.service)\" \"\$(ss -ltn | grep -c :$MS_PORT)\"
    echo '-- 建视图树(必须先于挂载单元: M_OPEN 拒绝无树的视图名) --'
    grovefs --home $MS_HOME init --view $VIEW 2>&1 | sed 's/^/   /'
    sudo systemctl enable --now grovefs-mount-$VIEW.service >/dev/null 2>&1
    sleep 2
    printf 'mount=%s\n' \"\$(systemctl is-active grovefs-mount-$VIEW.service)\"
    findmnt -rn -o TARGET,SOURCE,OPTIONS $MNT | sed 's/^/   /'
  " 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
}

verify() {
  echo "######## 复核: 开机自起 / 密钥不外泄 / 进程归属 / 读写冒烟 ########"
  for h in gfs1 gfs2 gfs-client; do
    echo "======== [$h] ========"
    $SSH $h "
      echo '-- 单元 enabled/active --'
      systemctl list-unit-files --no-pager 'grovefs*' | grep grovefs | sed 's/^/   /'
      echo '-- 每个 grovefs 进程都归 systemd 管 --'
      for p in \$(pgrep -x grovefs); do printf '   pid=%s %s\n' \"\$p\" \"\$(tr -d '\0' < /proc/\$p/cgroup | grep -o 'grovefs-[^/]*')\"; done
      echo '-- argv 与单元文本里没有密钥 --'
      ps -eo args | grep '[g]rovefs ' | sed 's/^/   /'
      sudo grep -c -i 'token\|gfstest' /etc/systemd/system/grovefs*.service 2>/dev/null | sed 's/^/   单元内密钥命中: /'
      sudo stat -c '   env 文件: %n %a %U:%G' $ENV_FILE
    " 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
  done
  echo "======== 冒烟写入(鉴权来自 env 文件, 不进命令行) ========"
  $SSH gfs-client "
    mkdir -p $MNT/datasets
    head -c 33554432 /dev/urandom > $MNT/smoke.bin
    head -c 67108864 /dev/urandom > $MNT/datasets/big.bin
    sync; sleep 1
    md5sum $MNT/smoke.bin $MNT/datasets/big.bin > /tmp/smoke.md5
    sudo systemctl restart grovefs-mount-$VIEW.service; sleep 2
    md5sum -c /tmp/smoke.md5 2>&1 | sed 's/^/   /'
    echo '-- 远端 du(条带列表; env 文件 0600 root, 故由 root source 后交给命令) --'
    sudo sh -c 'set -a; . /etc/grovefs/cluster.env; set +a; grovefs du --view '"$VIEW"' --total' 2>&1 | sed 's/^/   /'
  " 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
  echo "======== 四块盘的落点 ========"
  for h in gfs1 gfs2; do
    printf '%-11s ' $h
    $SSH $h 'for r in /data/gfs1/grove /data/gfs2/grove; do printf "%s:%s对象/%s  " "$(basename $r)" "$(find $r/blocks -type f | wc -l)" "$(du -sh $r | cut -f1)"; done; echo' 2>&1 | grep -vE 'setlocale|Pseudo|stdin: is'
  done
}

case "${1:-all}" in
  phase1) phase1 ;;
  phase2) phase2 ;;
  phase3) phase3 ;;
  verify) verify ;;
  all) phase1 && phase2 && phase3 && verify ;;
  *) echo "usage: $0 [phase1|phase2|phase3|verify|all]"; exit 2 ;;
esac
