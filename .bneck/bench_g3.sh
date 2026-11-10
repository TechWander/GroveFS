#!/usr/bin/env bash
# G3 performance gate: metadata write throughput of the FDB backend against the
# PG backend, SAME host, SAME session, SAME op sequence.
#
# The op sequence is the one the plan's R3 line cites its "~4.6k ops/s PG 平台值"
# from: `.build-bin/bench_pg_conc.sh`'s time-boxed distinct-parent namespace writes
# (mkdir + O_EXCL create of zero-byte files through real FUSE mounts, so the data
# plane stays out and only the authority path — meta-server + backend — is measured).
# The aggregate metric is the script's own: sum(ops) / (max end - min start).
#
# Arms are interleaved per cell and the whole matrix is run twice, so a drift in
# host load shows up as spread instead of silently favouring one backend.
#
# usage: bash .bneck/bench_g3.sh
#        N_LIST='1 4' T=8 RUN_SECS=6 REPS=2 bash .bneck/bench_g3.sh
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib:${LD_LIBRARY_PATH:-}
Y=./target/fdb/release/grovefs
CLUSTER3=${CLUSTER3:-/data/grovefs/.fdb3/cluster3}
PGURI="postgres://grove:grovepw@127.0.0.1:5433/grovefs"
PG_META=127.0.0.1:15462
FDB_META=127.0.0.1:15463
DATA=127.0.0.1:15461
N_LIST=${N_LIST:-"1 4"}
T=${T:-8}
RUN_SECS=${RUN_SECS:-6}
REPS=${REPS:-2}
P=.g3bench
# View names must be unique per session: the load driver writes fixed relative
# paths, so reusing a view that an earlier run already populated turns the cell
# into a storm of EEXIST and reports ~0 ops/s instead of steady-state throughput.
RUN_TAG=${RUN_TAG:-$(date +%s)}
# G3_STATUS_SNAP=<path> captures the cluster's own view of the load (latency
# probe, committed tx/s, durability lag) from the middle of the run.
G3_STATUS=${G3_STATUS_SNAP:-}

[ -x "$Y" ] || { echo "need the fdb build first: cargo build --release --features fdb --target-dir target/fdb"; exit 3; }
[ -f "$CLUSTER3" ] || { echo "no RF=3 cluster file at $CLUSTER3; run .bneck/fdb3_up.sh"; exit 3; }

rm -rf $P; mkdir -p $P/data $P/sql_home $P/pg_home $P/fdb_home $P/logs
pids=()
cleanup(){
  for m in $P/mnt*; do fusermount -u "$m" 2>/dev/null; done
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null; pkill -f "grovefs.*$P" 2>/dev/null
}
trap cleanup EXIT
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

echo "===== 0) servers ====="
$Y data-server --listen $DATA --data-dir $P/data > $P/logs/data.log 2>&1 & pids+=($!)
wait_port $DATA || { echo "data-server failed"; exit 1; }
$Y meta-server --home $P/pg_home --meta-store "$PGURI" --listen $PG_META > $P/logs/meta_pg.log 2>&1 & pids+=($!)
$Y meta-server --home $P/fdb_home --meta-store "fdb://$CLUSTER3" --listen $FDB_META > $P/logs/meta_fdb.log 2>&1 & pids+=($!)
wait_port $PG_META && wait_port $FDB_META || { echo "meta-servers failed"; tail -3 $P/logs/meta_*.log; exit 1; }
echo "backends as bannered by the servers:"
grep -m1 -iE "foundationdb|sqlite|postgres" $P/logs/meta_pg.log | sed 's/^/  pg-arm: /'
grep -m1 -iE "foundationdb|sqlite|postgres" $P/logs/meta_fdb.log | sed 's/^/ fdb-arm: /'
echo "up: data=$DATA pg=$PG_META fdb=$FDB_META  (N_LIST='$N_LIST' T=$T RUN_SECS=$RUN_SECS REPS=$REPS)"

# run_one <tag> <view> <meta> <N>
run_one(){
  local be=$1 view=$2 meta=$3 N=$4
  local bar=$P/logs/bar.${be}.${view}
  rm -f "$bar"
  local mpids=()
  for i in $(seq 1 $N); do
    local m=$P/mnt_${be}${i}
    mkdir -p "$m"
    $Y mount --view "$view" --meta-server "$meta" --data-servers $DATA --threads 16 --mountpoint "$m" --foreground > $P/logs/mount_${be}${i}.log 2>&1 & mpids+=($!)
    local alive=0
    for _ in $(seq 1 200); do findmnt -T "$m" 2>/dev/null | grep -q grovefs && { alive=1; break; }; sleep 0.05; done
    [ $alive = 1 ] || { echo "FATAL: mount $m not live (see logs/mount_${be}${i}.log)"; exit 1; }
  done
  local pids2=()
  for i in $(seq 1 $N); do
    python3 .build-bin/bench_pg_load.py "$P/mnt_${be}${i}" $T 0 0 $RUN_SECS "$bar" > $P/logs/run_${be}_${view}_m${i}.log 2>&1 & pids2+=($!)
  done
  sleep 0.4; touch "$bar"
  if [ -n "$G3_STATUS" ] && [ "$be" = fb ]; then
    sleep 2
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      /data/grovefs/.fdbbin/fdbcli.x86_64 -C "$CLUSTER3" --exec "status json" 2>/dev/null >> "$G3_STATUS"
      sleep 1
    done
  fi
  for p in "${pids2[@]}"; do wait $p; done
  awk -v be=$be -v view=$view -v N=$N -v T=$T '
    /RESULT/{ for(kv=1;kv<=NF;kv++){split($kv,a,"="); if(a[1]=="ops")o+=a[2]; if(a[1]=="start"){if(min==""||a[2]<min)min=a[2]} if(a[1]=="end"){if(a[2]>max)max=a[2]} } }
    END{ printf "G3ROW %s %s N=%d T=%d ops=%d wall=%.2fs agg=%.0f ops/s\n", be, view, N, T, o, max-min, o/(max-min) }' $P/logs/run_${be}_${view}_m*.log
  for p in "${mpids[@]}"; do kill $p 2>/dev/null; wait $p 2>/dev/null; done
  for i in $(seq 1 $N); do fusermount -u "$P/mnt_${be}${i}" 2>/dev/null; rm -rf "$P/mnt_${be}${i}"; done
}

for r in $(seq 1 $REPS); do
  echo "===== rep $r/$REPS : time-boxed distinct-parent writes (steady state) ====="
  for N in $N_LIST; do
    $Y view --meta-store "$PGURI" create --view "pg_${RUN_TAG}_r${r}n${N}" > /dev/null 2>&1
    run_one pg "pg_${RUN_TAG}_r${r}n${N}" $PG_META $N
    $Y view --meta-store "fdb://$CLUSTER3" create --view "fb_${RUN_TAG}_r${r}n${N}" > /dev/null 2>&1
    run_one fb "fb_${RUN_TAG}_r${r}n${N}" $FDB_META $N
  done
done
echo "===== done ====="
