#!/usr/bin/env bash
# Tear down the grovefs test cluster on the three gfs VMs so the same three
# guests can host a real 3-node FoundationDB cluster instead (user authorisation
# 2026-11-10: "把 gfs 的集群先撤掉 ... 不用考虑介质问题, 先功能跑通").
#
# Depth chosen by the user: stop services + wipe data roots ONLY.  Nothing here
# touches the guest OS disks, the grovefs binaries, the unit files or
# /etc/grovefs/cluster.env, so the old deployment is one `systemctl enable
# --now` away from coming back.  Rollback of the whole experiment is
# documented in .bneck/vm_rollback.md (managedsave snapshot + qcow2 copies in
# /data/grovefs/vmbak/).
#
# usage: bash .bneck/vm_gfs_teardown.sh          # does the work
#        bash .bneck/vm_gfs_teardown.sh --dry-run  # only prints what would run
set -u
VMS="${VM_TEARDOWN_VMS:-gfs1 gfs2 gfs-client}"
DRY=""
[ "${1:-}" = "--dry-run" ] && DRY=1

run() { # <host> <command string>
  local h=$1; shift
  if [ -n "$DRY" ]; then echo "DRY [$h] $*"; else ssh "$h" "sudo bash -c '$*'" 2>&1 | grep -v setlocale; fi
}

for h in $VMS; do
  echo "########## $h"
  echo "-- pre-state --"
  ssh "$h" 'systemctl list-units --type=service --all | grep -i grove; mount | grep grovefs:; sudo du -sh /data/gfs1/grove /data/gfs2/grove /home/hanjun/meta 2>/dev/null' \
      2>/dev/null | grep -v setlocale
  [ -n "$DRY" ] && continue

  # 1. unmount the FUSE views first (mount units are ordered after meta-server,
  #    and a dead meta-server would leave a transport-error mount behind).
  for u in $(ssh "$h" 'systemctl list-units --type=service --all | grep -oE "grovefs-mount-[a-z0-9-]+\.service" | sort -u' 2>/dev/null | grep -v setlocale); do
    run "$h" "systemctl stop $u"
  done
  run "$h" 'umount -l /home/hanjun/mnt /home/hanjun/mnt_lens /home/hanjun/mnt_fork 2>/dev/null; true'

  # 2. stop and disable every grovefs unit on this guest.
  for u in $(ssh "$h" 'systemctl list-unit-files | grep -oE "grovefs-[a-z0-9-]+\.service" | sort -u' 2>/dev/null | grep -v setlocale); do
    run "$h" "systemctl disable --now $u"
  done

  # 3. wipe the object data roots and the meta tree (data disks only).
  run "$h" 'rm -rf /data/gfs1/grove /data/gfs2/grove /home/hanjun/meta'
  run "$h" 'mkdir -p /data/gfs1 /data/gfs2'

  echo "-- post-state --"
  ssh "$h" 'systemctl list-unit-files | grep -c grovefs; sudo du -sh /data/gfs1/grove /data/gfs2/grove /home/hanjun/meta 2>/dev/null; df -h / /data | tail -3; pgrep -af grovefs' \
      2>/dev/null | grep -v setlocale
done

echo "-- host-side reachability of the guests (for the FDB cluster file) --"
for h in $VMS; do
  printf '%s = %s\n' "$h" "$(ssh "$h" 'hostname -I | cut -d" " -f1' 2>/dev/null | tr -d '\r')"
done
