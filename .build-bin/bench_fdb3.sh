#!/usr/bin/env bash
# FDB re-test at TRIPLE redundancy: the same namespace-write matrix as
# .build-bin/bench_pg_conc.sh, but the meta authority is a FoundationDB cluster
# that actually holds three replicas per key (4 zones, redundancy `triple`).
#
# Why a separate script: the earlier FDB numbers on this host came from a
# SINGLE-NODE dev cluster (`.build-bin/fdb_env.sh up`, redundancy `single`). A
# triple cluster pays a different write path — commit must reach a quorum of
# three logs across three failure domains — so comparing those two is a
# different question, not a regression. This script refuses to run against a
# non-triple cluster instead of printing a number nobody can compare.
#
# 口径 (aligned with the PG/SQLite rows in USAGE):
#   * workload = pure namespace ops (mkdir + O_EXCL zero-byte create), so the
#     data plane stays out and only the authority path is measured;
#   * N = mount processes on this host, T = threads per mount, barrier start,
#     RUN_SECS = time box; aggregate = total ops / (max end - min start);
#   * views are dropped and recreated before each arm because a leftover tree
#     makes every op EEXIST and the row would read ops=0 (see the same guard in
#     bench_pg_conc.sh);
#   * three runs per point, report the median — a triple cluster's commit path
#     is bimodal under a fresh `Moving data` window.
#
# usage: bash .build-bin/bench_fdb3.sh                  # N_LIST='1 4 8', T=8
#        N_LIST='1' RUN_SECS=6 bash .build-bin/bench_fdb3.sh
#        RUNS=3 bash .build-bin/bench_fdb3.sh           # median of 3 points
set -u
cd "$(dirname "$0")/.." || exit 1
export LC_ALL=C LANG=C
unset GROVEFS_META_STORE

FDB_HOME=${FDB_HOME:-/data/grovefs/.fdbbin}
CLUSTER=${GROVEFS_FDB_CLUSTER_FILE:-/data/grovefs/.fdb3vm/cluster3vm}
FDBURI="fdb://$CLUSTER"
Y=${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}
P=.fdb3bench
N_LIST=${N_LIST:-"1 4 8"}
T=${T:-8}
RUN_SECS=${RUN_SECS:-6}
RUNS=${RUNS:-1}
DATA=${DATA_PORT:-127.0.0.1:15461}
META=${META_PORT:-127.0.0.1:15462}

fdb_cli() { "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" "$@"; }

echo "===== 0) preflight: the cluster must be TRIPLE-replicated and healthy ====="
[ -x "$FDB_HOME/fdbcli.x86_64" ] || { echo "no fdbcli at $FDB_HOME"; exit 3; }
[ -f "$CLUSTER" ] || { echo "no cluster file at $CLUSTER (bring the 3-VM cluster up with .bneck/fdb3vm_up.sh)"; exit 3; }
ST=$(fdb_cli --exec "status" 2>&1) || { echo "fdbcli status failed"; echo "$ST" | head -5; exit 3; }
# `status` prints the configuration; only `status minimal` states availability.
MIN=$(fdb_cli --exec "status minimal" 2>&1)
echo "$MIN" | grep -q "The database is available" || { echo "cluster not available:"; echo "$MIN" | head -10; exit 3; }
RED=$(echo "$ST" | awk -F'- ' '/Redundancy mode/{print $2; exit}')
FT=$(echo "$ST" | awk -F'- ' '/Fault Tolerance/{print $2; exit}')
HEALTH=$(echo "$ST" | awk -F'- ' '/Replication health/{print $2; exit}')
NZONE=$(echo "$ST" | awk -F'- ' '/Zones/{print $2; exit}')
echo "  redundancy=$RED zones=$NZONE fault_tolerance=$FT health=$HEALTH"
[ "$RED" = "triple" ] || { echo "REFUSING: redundancy is '$RED', not 'triple' — a single/double cluster measures a different commit path (bring up .bneck/fdb3vm_up.sh)"; exit 3; }
[ "$HEALTH" = "Healthy" ] || { echo "REFUSING: replication health is '$HEALTH' — wait for $ST to settle"; exit 3; }
[ "${NZONE:-0}" -ge 3 ] || { echo "REFUSING: only $NZONE zone(s); triple needs >= 3 failure domains"; exit 3; }

if [ ! -x "$Y" ]; then
    echo "===== 0b) building a feature-enabled binary ($Y) ====="
    FDB_CLIENT_LIB_PATH="${FDB_CLIENT_LIB_PATH:-$FDB_HOME/lib}" \
    LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}" \
        cargo build --release --features fdb --target-dir target/fdb > .fdbtest-build.log 2>&1 \
        || { echo "FAIL: build with --features fdb (see .fdbtest-build.log)"; tail -20 .fdbtest-build.log; exit 1; }
fi
export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"

rm -rf $P; mkdir -p $P/data $P/home $P/logs
pids=()
cleanup(){
  for m in $P/mnt*; do fusermount -u "$m" 2>/dev/null; done
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  for N in $N_LIST; do $Y view --meta-store "$FDBURI" rm --base --name "f${N}t${T}" >/dev/null 2>&1; done
}
trap cleanup EXIT
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

echo "===== 1) servers (meta authority = $FDBURI) ====="
$Y data-server --listen $DATA --data-dir $P/data > $P/logs/data.log 2>&1 & pids+=($!)
wait_port $DATA || { echo "data-server failed"; exit 1; }
$Y meta-server --home $P/home --meta-store "$FDBURI" --listen $META > $P/logs/meta.log 2>&1 & pids+=($!)
wait_port $META || { echo "meta-server failed"; tail -5 $P/logs/meta.log; exit 1; }
grep -qi "foundationdb" $P/logs/meta.log && echo "  meta-server reports the foundationdb backend"

run_one(){
  local be=$1 view=$2 N=$3
  local bar=$P/logs/bar.${be}.${N}
  rm -f "$bar"
  local mpids=()
  for i in $(seq 1 $N); do
    local m=$P/mnt_${be}${i}
    mkdir -p "$m"
    $Y mount --view "$view" --meta-server $META --data-servers $DATA --threads 16 --mountpoint "$m" --foreground > $P/logs/mount_${be}${i}.log 2>&1 & mpids+=($!)
    local alive=0
    for _ in $(seq 1 200); do findmnt -T "$m" 2>/dev/null | grep -q grovefs && { alive=1; break; }; sleep 0.05; done
    [ $alive = 1 ] || { echo "FATAL: mount $m not live (see logs/mount_${be}${i}.log)"; exit 1; }
  done
  local pids2=()
  for i in $(seq 1 $N); do
    python3 .build-bin/bench_pg_load.py "$P/mnt_${be}${i}" $T 0 0 $RUN_SECS "$bar" > $P/logs/run_${be}_${N}_m${i}.log 2>&1 & pids2+=($!)
  done
  sleep 0.4; touch "$bar"
  for p in "${pids2[@]}"; do wait $p; done
  awk -v be=$be -v view=$view -v N=$N -v T=$T '
    /RESULT/{ for(kv=1;kv<=NF;kv++){split($kv,a,"="); if(a[1]=="ops")o+=a[2]; if(a[1]=="start"){if(min==""||a[2]<min)min=a[2]} if(a[1]=="end"){if(a[2]>max)max=a[2]} } }
    END{ printf "ROW %s %s N=%d T=%d ops=%d wall=%.2fs agg=%.0f ops/s\n", be, view, N, T, o, max-min, o/(max-min) }' $P/logs/run_${be}_${N}_m*.log
  for p in "${mpids[@]}"; do kill $p 2>/dev/null; wait $p 2>/dev/null; done
  for i in $(seq 1 $N); do fusermount -u "$P/mnt_${be}${i}" 2>/dev/null; rm -rf "$P/mnt_${be}${i}"; done
}

echo "===== 2) matrix: namespace writes against the triple cluster ====="
for N in $N_LIST; do
  for r in $(seq 1 $RUNS); do
    # A leftover subspace makes every op EEXIST (ops=0). Drop it, then create.
    $Y view --meta-store "$FDBURI" rm --base --name "f${N}t${T}" >/dev/null 2>&1
    $Y view --meta-store "$FDBURI" create --view "f${N}t${T}" >/dev/null || { echo "CREATE_FAILED f${N}t${T}"; exit 1; }
    run_one fdb "f${N}t${T}" $N
  done
done

echo "===== 3) cluster after (replication cost is visible here) ====="
fdb_cli --exec "status" 2>&1 | awk -F'- ' '/Replication health|Sum of key-value sizes|Disk space used|Moving data/{printf "  %s\n", $2}'

echo "===== done (compare rows with the PG/SQLite arms of bench_pg_conc.sh) ====="
