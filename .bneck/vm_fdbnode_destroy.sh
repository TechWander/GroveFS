#!/usr/bin/env bash
# Remove an FDB node VM created by .bneck/vm_fdbnode_provision.sh again
# (its own three files in /data/ifuse/vm and the record in /data/grovefs/vmbak).
# The three original gfs guests are never in scope here.
#
# Order matters: the fdbserver on that box has to leave the cluster BEFORE the
# machine disappears, otherwise the survivors spend a recovery cycle on a dead
# zone and the coordinator list still names an address nobody answers.  If the
# node is already gone, `coordinators auto` afterwards still repairs the list.
#
# usage: bash .bneck/vm_fdbnode_destroy.sh [name]      # default gfs-fdb4
set -u
NAME=${1:-gfs-fdb4}
BIN=/data/grovefs/.fdbbin
CLUSTER=/data/grovefs/.fdb3vm/cluster3vm
VMDIR=/data/ifuse/vm
URI="qemu:///system"

IP=$(awk -F= '/^IP=/{print $2}' /data/grovefs/vmbak/$NAME.ip 2>/dev/null)
if [ -n "${IP:-}" ]; then
  echo "--- take the fdbserver on $NAME ($IP) out of the cluster ---"
  ssh -n -o ConnectTimeout=8 hanjun@$IP 'sudo pkill -9 -f /usr/local/sbin/fdbserver; true' 2>/dev/null | grep -v setlocale
  sleep 3
  $BIN/fdbcli.x86_64 -C $CLUSTER --exec "coordinators auto" 2>&1 | sed 's/^/    /'
fi

echo "--- destroy/undefine the domain, delete its own images ---"
sudo virsh -c "$URI" destroy $NAME 2>/dev/null | sed 's/^/    /'
sudo virsh -c "$URI" undefine $NAME 2>/dev/null | sed 's/^/    /'
sudo rm -f $VMDIR/$NAME.qcow2 $VMDIR/$NAME-data1.qcow2 $VMDIR/$NAME-seed.iso
sudo rm -rf $VMDIR/$NAME-seed
sudo rm -f /data/grovefs/vmbak/$NAME.ip
echo -n "    domains left with that name: "
sudo virsh -c "$URI" list --all --name | grep -c "^$NAME$" || true
echo "FDBNODE_DESTROYED name=$NAME"
