#!/usr/bin/env bash
# Failure-recovery evidence for the real multi-host FoundationDB cluster that
# .bneck/fdb3vm_up.sh brings up (four zones: gfs1, gfs2, gfs-client, gfs-fdb4).
#
# Two severities, because they are different claims:
#   phase A  process failure : SIGKILL the fdbserver on one guest.  The machine
#            stays up, only the role disappears.
#   phase B  host failure    : `virsh destroy` a whole guest = hard power loss of
#            a separate kernel, its data disk and its IP.  This is what
#            .bneck/fdb3_up.sh could never demonstrate, because its three
#            processes shared one kernel and one block backend.
#
# Measured, not assumed: an availability timeline is sampled once a second across
# the whole failure window, so "it stayed available" is a number (longest
# consecutive unavailable stretch) and not an adjective.
#
# The first run of this script, on THREE machines, produced the finding that
# forced the fourth node: after one host died the database went unavailable and
# stayed there in `recruiting_transaction_servers` until the box came back —
# `triple` needs three logs, FDB will not co-locate two of them, and two
# survivors cannot re-form the set.  Re-running with four zones is the control
# for exactly that.
#
# usage: bash .bneck/fdb3vm_fault.sh                 # both phases
#        PHASES=A bash .bneck/fdb3vm_fault.sh        # process failure only
#        VICTIM=gfs-fdb4 bash .bneck/fdb3vm_fault.sh # choose the host to lose
set -u
BIN=/data/grovefs/.fdbbin
CLUSTER=/data/grovefs/.fdb3vm/cluster3vm
S=/data/grovefs/.fdb3vm/fault
FCLI="$BIN/fdbcli.x86_64 -C $CLUSTER"
mkdir -p $S
rm -f $S/*.json $S/*.log $S/*.txt 2>/dev/null
PHASES=${PHASES:-AB}
VICTIM=${VICTIM:-gfs-client}
VICTIM_IP=${VICTIM_IP:-192.168.122.137}
VICTIM_DD=${VICTIM_DD:-/data/fdb/fdb}
VICTIM_ZONE=${VICTIM_ZONE:-n3}

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
now(){ date +%s; }

# ---- status reader: one json field set per call, tolerant of a mid-recovery
# ---- answer that has no `database_available` at all (that is itself a signal)
status_fields(){ # writes $1 (json path), echoes "k=v k=v ..."
  $FCLI --exec "status json" > $S/cur.json 2>/dev/null
  python3 - <<'PY'
import json,sys
try:
    j=json.load(open('/data/grovefs/.fdb3vm/fault/cur.json'))['cluster']
except Exception:
    print('parse=bad'); raise SystemExit
proc=list(j.get('processes',{}).values())
def rl(p): return [r.get('role') for r in p.get('roles',[])]
# a process that has just joined can report NO fault_domain yet.  Counting
# str(None) as a zone made `zones>=4` true on three real zones plus one
# uninitialized process — the count has to be of zones actually present.
doms=sorted({str(p['fault_domain']) for p in proc if p.get('fault_domain')})
d={'avail':j.get('database_available'),'full':j.get('full_replication'),
   'deg':j.get('degraded_processes'),
   'sto':sum(1 for p in proc if 'storage' in rl(p)),
   'logs':sum(1 for p in proc if 'log' in rl(p)),
   'zones':len(doms),
   'noloc':sum(1 for p in proc if not p.get('fault_domain')),
   'minrep':(j.get('data') or {}).get('state',{}).get('min_replicas_remaining'),
   'ft_a':(j.get('fault_tolerance') or {}).get('max_zone_failures_without_losing_availability'),
   'ft_d':(j.get('fault_tolerance') or {}).get('max_zone_failures_without_losing_data'),
   'msgs':','.join(sorted(m.get('name','') for m in j.get('messages',[])))}
print(' '.join('%s=%s'%(k,v) for k,v in d.items()))
PY
}

wait_for(){ # wait_for <python expr over d> <timeout s> <label>
  local cond=$1 to=$2 label=$3 t0=$(now) d
  while [ $(($(now)-t0)) -lt "$to" ]; do
    d=$(status_fields)
    if python3 -c "
d=dict(kv.split('=',1) for kv in '''$d'''.split())
def b(x): return x=='True'
avail=b(d.get('avail','None')); full=b(d.get('full','None'))
deg=float(d.get('deg','0') or 0); sto=int(d.get('sto','0') or 0)
zones=int(d.get('zones','0') or 0); minrep=d.get('minrep')
noloc=int(d.get('noloc','0') or 0)
ft_a=d.get('ft_a'); ft_d=d.get('ft_d')
raise SystemExit(0 if ($cond) else 1)" 2>/dev/null; then
      echo "    [+$(($(now)-t0))s] $label : SATISFIED  ($d)"; return 0
    fi
    sleep 2
  done
  echo "    [timeout ${to}s] $label : NOT SATISFIED  (last: $d)"; return 1
}

# ---- one-second availability timeline across a window
timeline(){ # timeline <seconds> <outfile>
  local secs=$1 out=$2 t0=$(now) a
  while [ $(($(now)-t0)) -lt "$secs" ]; do
    a=$($FCLI --exec "status minimal" 2>/dev/null | tr -d '\r' | tail -1)
    case "$a" in *"database is available"*) echo "$(($(now)-t0)) avail $a";;
                                    *) echo "$(($(now)-t0)) BLOCKED $a";; esac >> $out
    sleep 1
  done
}
tl_worst(){ # longest consecutive BLOCKED stretch, in samples
  awk '{if ($2=="BLOCKED"){c++; if(c>m)m=c} else c=0} END{print m+0}' "$1"; }

probe(){ # probe <name> <value> : commit a write and read it back
  local k=faultprobe_$1 v=$2
  $FCLI --exec "writemode on; set $k $v" > $S/set_$1.log 2>&1
  grep -q "Committed" $S/set_$1.log || { bad "write $1 never committed: $(head -2 $S/set_$1.log|tr '\n' ' ')"; return 1; }
  local got
  got=$($FCLI --exec "get $k" 2>/dev/null | tr -d '\r' | tail -1 | sed -E "s/^.* is \`(.*)'\$/\1/")
  if [ "$got" = "$v" ]; then ok "write+read during $1 ('$got')"; else bad "read back '$got' for $1, expected '$v'"; fi
}

echo "===== baseline: the cluster as configured ====="
d=$(status_fields); echo "    $d"
wait_for "avail and full and zones>=4" 60 "baseline: available, full_replication, at least 4 zones"
probe baseline "pre-failure"

if [[ "$PHASES" == *A* ]]; then
echo "===== phase A: process failure (SIGKILL the fdbserver on $VICTIM) ====="
timeline 45 $S/A.timeline.txt & TL=$!
ssh -n $VICTIM 'sudo pkill -9 -f /usr/local/sbin/fdbserver; sleep 1; sudo pgrep -c -f /usr/local/sbin/fdbserver || echo 0' 2>/dev/null | tr -d '\r' | sed 's/^/    fdbserver procs left: /'
wait $TL
A_WORST=$(tl_worst $S/A.timeline.txt)
echo "    timeline: $(grep -c . $S/A.timeline.txt) samples, longest unavailable stretch = ${A_WORST}s"
[ "${A_WORST:-99}" -le 30 ] && ok "A: an fdbserver death costs at most ${A_WORST}s of availability" \
                             || bad "A: ${A_WORST}s without a database after a single process death"
probe A_degraded "written-while-degraded"
wait_for "avail and sto>=3" 180 "A: storage replicas re-placed without the dead process"
wait_for "avail and full" 240 "A: full_replication restored on the surviving zones"
echo "-- A: bring the process back --"
ssh -n $VICTIM "sudo bash -c 'setsid nohup /usr/local/sbin/fdbserver --cluster-file /etc/foundationdb/fdb.cluster -d $VICTIM_DD/data -L $VICTIM_DD/logs -l $VICTIM_IP:4500 -p $VICTIM_IP:4500 -m 2000000000 --locality-zoneid $VICTIM_ZONE --locality-machineid m-$VICTIM_ZONE > $VICTIM_DD/stdout.log 2>&1 </dev/null &'" 2>/dev/null | grep -v setlocale
wait_for "avail and full and zones>=4 and deg==0" 300 "A: the rejoined process restores all 4 zones with zero degradation"
probe A_recovered "after-A"
fi

if [[ "$PHASES" == *B* ]]; then
echo "===== phase B: host failure (virsh destroy $VICTIM = hard power loss) ====="
sudo virsh destroy $VICTIM 2>&1 | sed 's/^/    /'
timeline 60 $S/B.timeline.txt & TL=$!
wait $TL
B_WORST=$(tl_worst $S/B.timeline.txt)
echo "    timeline: $(grep -c . $S/B.timeline.txt) samples, longest unavailable stretch = ${B_WORST}s"
[ "${B_WORST:-99}" -le 30 ] && ok "B: losing a whole machine costs at most ${B_WORST}s of availability" \
                             || bad "B: ${B_WORST}s without a database after one host died"
probe B_degraded "written-while-host-down"
wait_for "avail and full and sto>=3 and zones==3" 300 "B: three survivors re-form log set + 3 replicas on their own (the dead box is NOT needed)"
d=$(status_fields); echo "    degraded-but-healthy snapshot: $d"
echo "-- B: power the host back on (cold boot, nothing of ours autostarts) --"
sudo virsh start $VICTIM 2>&1 | sed 's/^/    /'
B_T0=$(now)
for _ in $(seq 1 60); do ssh -o ConnectTimeout=3 $VICTIM 'true' 2>/dev/null && break; sleep 3; done
echo "    ssh back after $(( $(now)-B_T0 ))s of guest boot"
ssh -n $VICTIM "sudo mkdir -p $VICTIM_DD/data $VICTIM_DD/logs; df -h $VICTIM_DD 2>/dev/null | tail -1" 2>/dev/null | grep -v setlocale | sed 's/^/    /'
ssh -n $VICTIM "sudo bash -c 'setsid nohup /usr/local/sbin/fdbserver --cluster-file /etc/foundationdb/fdb.cluster -d $VICTIM_DD/data -L $VICTIM_DD/logs -l $VICTIM_IP:4500 -p $VICTIM_IP:4500 -m 2000000000 --locality-zoneid $VICTIM_ZONE --locality-machineid m-$VICTIM_ZONE > $VICTIM_DD/stdout.log 2>&1 </dev/null &'" 2>/dev/null | grep -v setlocale
wait_for "avail and full and zones>=4 and deg==0" 300 "B: the powered-back host rejoins and the cluster is whole again"
probe B_recovered "after-B"
wait_for "avail and full and zones>=4 and sto>=4 and noloc==0" 240 "B: every zone has its locality again and the dead box hosts a replica again"
fi

echo "===== final state, and every probe key still readable ====="
$FCLI --exec "status json" > $S/final.json 2>/dev/null
for k in baseline A_degraded A_recovered B_degraded B_recovered; do
  v=$($FCLI --exec "get faultprobe_$k" 2>/dev/null | tr -d '\r' | tail -1 | sed -E "s/^.* is \`(.*)'\$/\1/")
  # every probe key must still be readable — `baseline` has no underscore, so an
  # earlier version filtered it out and the run silently skipped the one key that
  # predates every failure.
  if [ -n "$v" ]; then echo "    faultprobe_$k = '$v'"; else echo "    faultprobe_$k MISSING"; fail=$((fail+1)); fi
done
python3 - <<'PY'
import json
cl=json.load(open('/data/grovefs/.fdb3vm/fault/final.json'))['cluster']
proc=list(cl['processes'].values())
def rl(p): return [r.get('role') for r in p.get('roles',[])]
print('    final: available=%s full_replication=%s degraded=%s' % (cl.get('database_available'),cl.get('full_replication'),cl.get('degraded_processes')))
# every id gets str(): during a rejoin a process can briefly report no
# machine_id/address at all, and sorting None against str aborted the whole
# summary on the first run of this script.
print('    zones=%s machines=%s addresses=%s' % (sorted({str(p['fault_domain']) for p in proc if p.get('fault_domain')}),sorted({str(p.get('machine_id')) for p in proc}),sorted({str(p.get('address')) for p in proc})))
print('    processes still reporting no fault_domain: %s' % sum(1 for p in proc if not p.get('fault_domain')))
print('    storage=%s logs=%s coordinators=%s' % (sum(1 for p in proc if 'storage' in rl(p)),sum(1 for p in proc if 'log' in rl(p)),sum(1 for p in proc if 'coordinator' in rl(p))))
print('    fault_tolerance=%s' % cl.get('fault_tolerance'))
print('    data state=%s min_replicas_remaining=%s' % (cl.get('data',{}).get('state'),cl.get('data',{}).get('state',{}).get('min_replicas_remaining')))
PY
echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" = 0 ] && echo "FDB_FAULT_GREEN" || echo "FDB_FAULT_RED"
