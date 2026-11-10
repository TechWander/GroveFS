#!/usr/bin/env bash
# G3 performance gate: bring up a replication-3 FoundationDB cluster and tear it
# down again.  Spec "SATA（无 NVMe）降级档说明" asks for 3-5 nodes; this host has
# no SSD media at all (sda/sdb both sit behind an MR9361-8i with queue/rotational=1,
# and the gfs1/gfs2 qcow2 images live on the same array), and the three VMs are
# currently serving another team's live grovefs cluster, so the honest topology
# available here is 3 storage/log servers as separate processes with separate data
# directories.  That satisfies the replication factor; it does NOT satisfy the disk
# tier, which is why the result is read as a bound, never as a SATA-SSD claim.
#
# usage: bash .bneck/fdb3_up.sh                       # RF=3, ports 4510-4512
#        FDB3_RF=1 FDB3_PORTS=4513 FDB3_ROOT=/data/grovefs/.fdb1 \
#            bash .bneck/fdb3_up.sh                     # RF=1 control on the same disk
#        bash .bneck/fdb3_down.sh    # stop and remove it
set -u
BIN=/data/grovefs/.fdbbin
ROOT=${FDB3_ROOT:-/data/grovefs/.fdb3}
CLUSTER=$ROOT/cluster3
RF=${FDB3_RF:-3}
# `ssd` (redwood) is the engine every real deployment uses and the one the plan's
# tiers are defined for.  `memory` exists here only as an attribution control: it
# removes the disk from the commit path so a low number can be read as "media" and
# a high one as "implementation".  A memory-engine run is never a performance
# claim (durability is not on disk), so it stays in the project log, not in USAGE.
ENGINE=${FDB3_ENGINE:-ssd}
read -r -a PORTS <<< "${FDB3_PORTS:-4510 4511 4512}"
MEM=${FDB3_MEM:-2000000000}

rm -rf $ROOT; mkdir -p $ROOT
# Every coordinator is spelled out in full (`ip:port`); a connection string that
# short-cuts the address list is rejected as "2104 Connection string invalid".
ADDRS=""
for p in "${PORTS[@]}"; do ADDRS="${ADDRS:+$ADDRS,}127.0.0.1:$p"; done
printf 'c3perf:s3cretG3pass@%s\n' "$ADDRS" > $CLUSTER
cat $CLUSTER

for i in "${!PORTS[@]}"; do
  p=${PORTS[$i]}; n=$((i+1))
  mkdir -p $ROOT/n$n/data $ROOT/n$n/logs
  # Both locality keys are required, not decorative: with only `machineid` set the
  # zone falls back to the IP, so all three processes sit in ONE fault zone and a
  # `triple` cluster never recovers — the cluster controller logs
  # RecruitTLogMatchingSetNotAvailable ("Not enough physical servers available",
  # 1008) and status stays "Unable to locate the data distributor worker".
  setsid nohup $BIN/fdbserver.x86_64 --cluster-file $CLUSTER -d $ROOT/n$n/data -L $ROOT/n$n/logs \
      -l 127.0.0.1:$p -p 127.0.0.1:$p -m $MEM \
      --locality-zoneid n$n --locality-machineid m$n > $ROOT/n$n/stdout.log 2>&1 &
  echo "  n$n pid=$! port=$p"
done

# A brand-new cluster answers "unavailable" until it has been configured once, so
# the readiness probe cannot come before `configure` (fdb_env.sh does the same).
for _ in $(seq 1 60); do
  grep -q "FDBD joined cluster" $ROOT/n1/stdout.log 2>/dev/null && break
  sleep 0.5
done
echo "--- configure new $([ "$RF" = 3 ] && echo triple || echo single) $ENGINE ---"
$BIN/fdbcli.x86_64 -C $CLUSTER --exec "configure new $([ "$RF" = 3 ] && echo triple || echo single) $ENGINE" 2>&1
for _ in $(seq 1 120); do
  $BIN/fdbcli.x86_64 -C $CLUSTER --exec "status minimal" 2>/dev/null \
      | grep -q "The database is available" && break
  sleep 1
done
echo "--- status minimal ---"
$BIN/fdbcli.x86_64 -C $CLUSTER --exec "status minimal" 2>&1
echo "--- replication / servers as seen by the cluster ---"
for _ in $(seq 1 60); do
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
print("redundancy_mode          :", cl.get("configuration",{}).get("redundancy_mode"))
print("storage_engine           :", cl.get("configuration",{}).get("storage_engine"))
print("database_available       :", cl.get("database_available"))
print("full_replication         :", cl.get("full_replication"))
print("degraded_processes       :", cl.get("degraded_processes"))
print("storage_servers          :", sum(1 for p in proc if "storage" in roles(p)))
print("log_servers              :", sum(1 for p in proc if "log" in roles(p)))
print("coordinators             :", sum(1 for p in proc if "coordinator" in roles(p)))
print("machine_ids              :", sorted({p.get("machine_id") for p in proc}))
print("fault_domains            :", sorted({str(p.get("fault_domain")) for p in proc}))
print("zone_failures_wo_loss    :", cl.get("fault_tolerance",{}).get("max_zone_failures_without_losing_data"))
PY
echo "CLUSTER_FILE=$CLUSTER"
