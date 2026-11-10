#!/usr/bin/env bash
# Bring up a REAL three-node FoundationDB cluster: one fdbserver per guest VM,
# on three separate hosts on the libvirt default network, each with its own data
# disk.  This is the functional counterpart of .bneck/fdb3_up.sh, which could
# only run three processes on ONE host and therefore had to fake the fault
# domains with locality keys.
#
# Topology (after the gfs grovefs cluster was torn down by
# .bneck/vm_gfs_teardown.sh, user authorisation 2026-11-10):
#   n1 = gfs1       192.168.122.124  data on /dev/vdb (30G) mounted at /data/gfs1
#   n2 = gfs2       192.168.122.193  data on /dev/vdb (30G) mounted at /data/gfs1
#   n3 = gfs-client 192.168.122.137  data on /dev/vdb (30G) mounted at /data/fdb (new)
#   n4 = gfs-fdb4   192.168.122.248  provisioned by .bneck/vm_fdbnode_provision.sh
# The fourth zone is not padding.  Measured in .bneck/fdb3vm_fault.sh: with
# exactly three machines a `triple` cluster CANNOT stay available after losing
# one — the log set is three logs, FDB refuses two logs on one machine, so the
# survivors park in `recruiting_transaction_servers` and every write blocks until
# the dead machine returns.  Four zones is what makes "lose one host, keep
# serving" true.
#
# Each guest is a separate kernel + separate block backend, so machineid/zoneid
# map onto genuine failure units: killing a VM is a host failure, not a process
# failure.  Media is still the host RAID-10 (sda/sdb, ROTA=1), which is why this
# cluster buys *functionality*, not the G3 performance tier.
#
# usage: bash .bneck/fdb3vm_up.sh      # start + configure new triple ssd
#        bash .bneck/fdb3vm_down.sh    # stop the servers and wipe their data dirs
set -u
BIN=/data/grovefs/.fdbbin
ROOT=/data/grovefs/.fdb3vm
CLUSTER=$ROOT/cluster3vm
DESC=${FDBVM_DESC:-gfs3vm}
PASS=${FDBVM_PASS:-s3cretG3pass}
PORT=${FDBVM_PORT:-4500}
RF=${FDBVM_RF:-3}
ENGINE=${FDBVM_ENGINE:-ssd}
MEM=${FDBVM_MEM:-2000000000}

# <ssh alias> <VM ip> <data dir inside the guest> <node name>
NODES=(
  "gfs1       192.168.122.124  /data/gfs1/fdb  n1"
  "gfs2       192.168.122.193  /data/gfs1/fdb  n2"
  "gfs-client 192.168.122.137  /data/fdb/fdb   n3"
  "gfs-fdb4   192.168.122.248  /data/fdb/fdb   n4"
)

mkdir -p $ROOT
ADDRS=""
for spec in "${NODES[@]}"; do
  set -- $spec; ADDRS="${ADDRS:+$ADDRS,}$2:$PORT"
done
printf '%s:%s@%s\n' "$DESC" "$PASS" "$ADDRS" > $CLUSTER
echo "--- cluster file $CLUSTER ---"; cat $CLUSTER

echo "--- ship fdbserver + cluster file to each guest ---"
for spec in "${NODES[@]}"; do
  set -- $spec; h=$1; ip=$2; dd=$3; n=$4
  scp -q $BIN/fdbserver.x86_64 $h:~/fdbserver.x86_64
  scp -q $CLUSTER $h:~/fdb.cluster
  ssh $h "sudo mkdir -p $dd/data $dd/logs /etc/foundationdb && sudo cp ~/fdb.cluster /etc/foundationdb/fdb.cluster && sudo chmod 644 /etc/foundationdb/fdb.cluster && sudo rm -rf $dd/data/* $dd/logs/* && sudo cp ~/fdbserver.x86_64 /usr/local/sbin/fdbserver && sudo chmod 755 /usr/local/sbin/fdbserver" 2>/dev/null | grep -v setlocale
  echo "  $n ($h $ip) data=$dd ok"
done

echo "--- start one fdbserver per guest ---"
for spec in "${NODES[@]}"; do
  set -- $spec; h=$1; ip=$2; dd=$3; n=$4
  # zoneid == the guest name: with three real hosts this is not a workaround any
  # more, it is the actual failure domain.
  # `</dev/null` on the inner command matters: without it ssh keeps the channel
  # open waiting on a process that never exits, and the loop stalls.
  ssh -n $h "sudo bash -c 'setsid nohup /usr/local/sbin/fdbserver --cluster-file /etc/foundationdb/fdb.cluster \
      -d $dd/data -L $dd/logs -l $ip:$PORT -p $ip:$PORT -m $MEM \
      --locality-zoneid $n --locality-machineid m-$n > $dd/stdout.log 2>&1 </dev/null &'" 2>/dev/null | grep -v setlocale
  echo "  $n on $ip:$PORT"
done

echo "--- wait for all three to join ---"
for spec in "${NODES[@]}"; do
  set -- $spec; h=$1; dd=$3; n=$4
  for _ in $(seq 1 60); do
    ssh $h "sudo grep -q 'FDBD joined cluster' $dd/stdout.log" 2>/dev/null && { echo "  $n joined"; break; }
    sleep 0.5
  done
done

echo "--- configure new $([ "$RF" = 3 ] && echo triple || echo single) $ENGINE ---"
$BIN/fdbcli.x86_64 -C $CLUSTER --exec "configure new $([ "$RF" = 3 ] && echo triple || echo single) $ENGINE" 2>&1
for _ in $(seq 1 120); do
  $BIN/fdbcli.x86_64 -C $CLUSTER --exec "status minimal" 2>/dev/null \
      | grep -q "The database is available" && break
  sleep 1
done
echo "--- status minimal (from the host, over the NAT network) ---"
$BIN/fdbcli.x86_64 -C $CLUSTER --exec "status minimal" 2>&1

echo "--- replication / fault tolerance as seen by the cluster ---"
# Let the cluster pick its own coordinator set now that every process is in:
# with four machines this keeps a quorum available through one host loss without
# the file naming an address that no longer answers.
$BIN/fdbcli.x86_64 -C $CLUSTER --exec "coordinators auto" 2>&1 | sed 's/^/  /'
for _ in $(seq 1 90); do
  $BIN/fdbcli.x86_64 -C $CLUSTER --exec "status json" 2>/dev/null > $ROOT/status.json
  grep -q '"full_replication":true' $ROOT/status.json && break
  sleep 1
done
python3 - "$ROOT/status.json" <<'PY'
import json,sys
j=json.load(open(sys.argv[1]))
cl=j.get("cluster",{})
proc=list(cl.get("processes",{}).values())
def roles(p): return [r.get("role") for r in p.get("roles",[])]
print("redundancy_mode               :", cl.get("configuration",{}).get("redundancy_mode"))
print("storage_engine                :", cl.get("configuration",{}).get("storage_engine"))
print("database_available            :", cl.get("database_available"))
print("full_replication              :", cl.get("full_replication"))
print("degraded_processes            :", cl.get("degraded_processes"))
print("storage_servers               :", sum(1 for p in proc if "storage" in roles(p)))
print("log_servers                   :", sum(1 for p in proc if "log" in roles(p)))
print("coordinators                  :", sum(1 for p in proc if "coordinator" in roles(p)))
print("distinct machine_ids          :", sorted({p.get("machine_id") for p in proc}))
print("distinct fault_domains        :", sorted({str(p.get("fault_domain")) for p in proc}))
print("distinct addresses            :", sorted({p.get("address") for p in proc}))
print("hostnames                     :", sorted({p.get("fqdn_location",{}).get("host") if isinstance(p.get("fqdn_location"),dict) else p.get("fqdn") for p in proc}))
ft=cl.get("fault_tolerance",{})
print("zone_failures_without_loss    :", ft.get("max_zone_failures_without_losing_data"))
print("zone_failures_without_avail   :", ft.get("max_zone_failures_without_losing_availability"))
PY
echo "CLUSTER_FILE=$CLUSTER"
