#!/bin/bash
# Idempotently (re)create the demo views and install persistent systemd mount
# units for the fork and the derived lens on the client node, so they auto-mount
# at boot and never need a token typed by hand (GROVEFS_TOKEN comes from
# /etc/grovefs/cluster.env via EnvironmentFile, exactly like the alice unit).
#
#   bash vm_add_demo_views.sh      create views + enable mounts + verify
#
# Safe to re-run. Run it after `vm_redeploy.sh all` (which wipes data roots and
# leaves only alice) to restore the full testable topology.
set -uo pipefail

SSH="ssh -o BatchMode=yes -o ConnectTimeout=8"
CLIENT=gfs-client
VIEW_FORK=team-fork
VIEW_LENS=nomore
BASE=alice
F_MNT=/home/hanjun/mnt_fork
L_MNT=/home/hanjun/mnt_lens
DB=/home/hanjun/meta
ENV_FILE=/etc/grovefs/cluster.env
G=/usr/local/bin/grovefs
U_FORK=/etc/systemd/system/grovefs-mount-$VIEW_FORK.service
U_LENS=/etc/systemd/system/grovefs-mount-$VIEW_LENS.service

echo "######## 1) (re)create demo views on the meta-server root ########"
$SSH $CLIENT "
  printf 'hide /datasets\nhide /README.txt\n' > /tmp/$VIEW_LENS.rules
  echo '   -- fork --';   $G view --meta-dir $DB fork   --name $VIEW_FORK --base $BASE 2>&1 | sed 's/^/   /'
  echo '   -- lens --';   $G view --meta-dir $DB define --name $VIEW_LENS --base $BASE --rules /tmp/$VIEW_LENS.rules 2>&1 | sed 's/^/   /'
  echo '   -- list --';   $G view --meta-dir $DB list 2>&1 | grep -vE '^view keys' | sed 's/^/   /'
" 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'

echo "######## 2) install persistent mount units (token only via EnvironmentFile) ########"
$SSH $CLIENT "sudo tee $U_FORK >/dev/null" <<UNIT
[Unit]
Description=GroveFS mount fork $VIEW_FORK at $F_MNT (structure rw, content EROFS)
Requires=grovefs-meta-server.service
After=grovefs-meta-server.service network-online.target
[Service]
User=hanjun
EnvironmentFile=$ENV_FILE
ExecStartPre=/bin/mkdir -p $F_MNT
ExecStartPre=-/usr/bin/fusermount -u $F_MNT
ExecStart=$G mount --view $VIEW_FORK -m $F_MNT --foreground
ExecStop=/usr/bin/fusermount -u $F_MNT
ExecStopPost=-/usr/bin/fusermount -uz $F_MNT
LimitNOFILE=65536
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
[Install]
WantedBy=multi-user.target
UNIT
$SSH $CLIENT "sudo tee $U_LENS >/dev/null" <<UNIT
[Unit]
Description=GroveFS mount derived lens $VIEW_LENS at $L_MNT (read-only view)
Requires=grovefs-meta-server.service
After=grovefs-meta-server.service network-online.target
[Service]
User=hanjun
EnvironmentFile=$ENV_FILE
ExecStartPre=/bin/mkdir -p $L_MNT
ExecStartPre=-/usr/bin/fusermount -u $L_MNT
ExecStart=$G mount --view $VIEW_LENS -m $L_MNT --foreground
ExecStop=/usr/bin/fusermount -u $L_MNT
ExecStopPost=-/usr/bin/fusermount -uz $L_MNT
LimitNOFILE=65536
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
[Install]
WantedBy=multi-user.target
UNIT
$SSH $CLIENT "
  sudo systemctl daemon-reload
  sudo systemctl enable --now grovefs-mount-$VIEW_FORK.service >/dev/null 2>&1
  sudo systemctl enable --now grovefs-mount-$VIEW_LENS.service >/dev/null 2>&1
  sleep 2
  printf '   fork unit=%s mount=%s\n' \"\$(systemctl is-active grovefs-mount-$VIEW_FORK.service)\" \"\$(findmnt -rn -o SOURCE $F_MNT)\"
  printf '   lens unit=%s mount=%s\n' \"\$(systemctl is-active grovefs-mount-$VIEW_LENS.service)\" \"\$(findmnt -rn -o SOURCE $L_MNT)\"
  echo '   -- argv carries no token --'
  ps -eo args | grep '[m]ount --view' | sed 's/^/   /'
" 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'

echo "######## 3) caps echo per mount (fork=content_ro -> WRITEBACK off; lens=base rw) ########"
$SSH $CLIENT "
  for u in grovefs-mount-$VIEW_FORK grovefs-mount-$VIEW_LENS; do
    printf '   %s: ' \$u
    sudo journalctl -u \$u.service --no-pager -o cat 2>/dev/null | grep 'caps granted' | tail -1
  done
" 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is'
echo "done."
