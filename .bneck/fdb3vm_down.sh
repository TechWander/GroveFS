#!/usr/bin/env bash
# Stop the real three-node FoundationDB cluster created by .bneck/fdb3vm_up.sh.
#
# CLEAN parameter decides how far the teardown goes:
#   keep (default) : stop the processes only, data dirs stay -> .bneck/fdb3vm_up.sh
#                    would then re-create the cluster file and wipe them anyway,
#                    so use fdb3vm_restart-style re-attach only for a live probe.
#   wipe           : stop and delete data/log dirs inside the guests.
# The guest OS disks, the shipped /usr/local/sbin/fdbserver and the unit-less
# layout are left alone; the VMs themselves are never destroyed here (see
# .bneck/vm_rollback.md for the snapshot-based rollback of the whole experiment).
set -u
ROOT=/data/grovefs/.fdb3vm
CLUSTER=$ROOT/cluster3vm
CLEAN=${FDBVM_CLEAN:-keep}

NODES=(
  "gfs1       /data/gfs1/fdb  n1"
  "gfs2       /data/gfs1/fdb  n2"
  "gfs-client /data/fdb/fdb   n3"
)

for spec in "${NODES[@]}"; do
  set -- $spec; h=$1; dd=$2; n=$3
  echo "-- $n ($h)"
  ssh -n $h "sudo pkill -f '/usr/local/sbin/fdbserver' ; sleep 1; pgrep -af fdbserver | grep -v pgrep || echo '  stopped'" 2>/dev/null | grep -v setlocale
  if [ "$CLEAN" = wipe ]; then
    ssh -n $h "sudo rm -rf $dd/data $dd/logs $dd/stdout.log" 2>/dev/null | grep -v setlocale
    echo "  data dirs wiped"
  fi
done

if [ "$CLEAN" = wipe ]; then rm -rf $ROOT; echo "removed $ROOT"; fi
echo "FDBVM_CLEAN=$CLEAN done"
