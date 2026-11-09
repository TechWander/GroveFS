#!/usr/bin/env bash
# Concurrent metadata-write benchmark: SQLite single-writer vs PG
# SERIALIZABLE 8-connection write pool, driven through real FUSE mounts.
# N mount-processes (client proxies on this host) x T python threads per
# mount, barrier-started; workload is pure namespace ops (mkdir + O_EXCL
# create of zero-byte files), so the data plane stays out of the picture
# and we measure exactly the authority path: meta-server + backend.
#
# usage: bash .build-bin/bench_pg_conc.sh          # full matrix
#        N_LIST='1 4 8' T=8 bash .build-bin/bench_pg_conc.sh
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
Y=./target/release/grovefs
P=.pgbench
PGURI="postgres://grove:grovepw@127.0.0.1:5433/grovefs"
SQL_META=127.0.0.1:15451
PG_META=127.0.0.1:15452
DATA=127.0.0.1:15455
N_LIST=${N_LIST:-"1 2 4 8 16"}
T=${T:-8}
RUN_SECS=${RUN_SECS:-6}
CD=${CD:-30}   # contention: dirs / files per name-set
CF=${CF:-5}

rm -rf $P; mkdir -p $P/data $P/sql_home $P/pg_home $P/logs
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
$Y meta-server --home $P/sql_home --listen $SQL_META > $P/logs/meta_sql.log 2>&1 & pids+=($!)
$Y meta-server --home $P/pg_home --meta-store "$PGURI" --listen $PG_META > $P/logs/meta_pg.log 2>&1 & pids+=($!)
wait_port $SQL_META && wait_port $PG_META || { echo "meta-servers failed"; tail -3 $P/logs/meta_*.log; exit 1; }
echo "up: data=$DATA sql=$SQL_META pg=$PG_META  (N_LIST='$N_LIST' T=$T RUN_SECS=$RUN_SECS)"

# run_one <tag> <view> <meta> <N> [contend=0] [dirs=0 -> time-boxed] [files]
run_one(){
  local be=$1 view=$2 meta=$3 N=$4 contend=${5:-0} dd=${6:-0} ff=${7:-0}
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
    local extra=""; [ $contend = 1 ] && extra="--contend"
    python3 .build-bin/bench_pg_load.py "$P/mnt_${be}${i}" $T $dd $ff $RUN_SECS "$bar" $extra > $P/logs/run_${be}_${view}_m${i}.log 2>&1 & pids2+=($!)
  done
  sleep 0.4; touch "$bar"
  for p in "${pids2[@]}"; do wait $p; done
  # aggregate: total ops / (max end - min start)
  awk -v be=$be -v view=$view -v N=$N -v T=$T '
    /RESULT/{ for(kv=1;kv<=NF;kv++){split($kv,a,"="); if(a[1]=="ops")o+=a[2]; if(a[1]=="start"){if(min==""||a[2]<min)min=a[2]} if(a[1]=="end"){if(a[2]>max)max=a[2]} } }
    END{ printf "ROW %s %s N=%d T=%d ops=%d wall=%.2fs agg=%.0f ops/s\n", be, view, N, T, o, max-min, o/(max-min) }' $P/logs/run_${be}_${view}_m*.log
  if [ "$contend" = 1 ]; then
    nd=$(find $P/mnt_${be}1 -mindepth 2 -maxdepth 2 -type d 2>/dev/null | wc -l)
    nf=$(find $P/mnt_${be}1 -mindepth 3 -type f 2>/dev/null | wc -l)
    echo "  PRESENT dirs=$nd files=$nf total=$((nd+nf)) expect=$((dd*(1+ff)))"
  fi
  for p in "${mpids[@]}"; do kill $p 2>/dev/null; wait $p 2>/dev/null; done
  for i in $(seq 1 $N); do fusermount -u "$P/mnt_${be}${i}" 2>/dev/null; rm -rf "$P/mnt_${be}${i}"; done
}

echo "===== 1) matrix: time-boxed distinct-parent writes (steady state) ====="
for N in $N_LIST; do
  $Y view --meta-dir $P/sql_home create --view "s${N}t${T}" > /dev/null 2>&1
  run_one sql "s${N}t${T}" $SQL_META $N
  $Y view --meta-store "$PGURI" create --view "p${N}t${T}" > /dev/null 2>&1
  run_one pg "p${N}t${T}" $PG_META $N
done

echo "===== 2) contention: N mounts x T threads slamming identical names ====="
N=${N_COND:-4}
$Y view --meta-store "$PGURI" create --view "pc${N}" > /dev/null 2>&1
run_one pgc "pc${N}" $PG_META $N 1 $CD $CF
echo "  (expect ops == CD*(1+CF) == $((CD*(1+CF))) exactly-one-winner-per-name across all mounts)"

echo "===== done ====="
