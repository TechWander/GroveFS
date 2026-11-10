#!/usr/bin/env bash
# One-off probe: how much CPU does ONE data-server process spend per GiB
# moved, and does aggregate throughput track client concurrency?
# Page-cache path on purpose (no conv=fsync): the local array is a slow
# 50 MB/s RAID, so disk must not mask the CPU cost we are measuring.
set -u
cd /data/grovefs/grovefs
export LC_ALL=C LANG=C
Y=./target/release/grovefs
B=$PWD/.bneck
D=127.0.0.1:16910
M=127.0.0.1:16911

rm -rf "$B/data" "$B/central" "$B/home" "$B/m1" "$B/m2" "$B/m3" "$B/m4"
mkdir -p "$B/data" "$B/central" "$B/m1" "$B/m2" "$B/m3" "$B/m4"

wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

"$Y" data-server --listen $D --data-dir "$B/data" > "$B/data.log" 2>&1 & DP=$!
"$Y" meta-server --meta-dir "$B/central" --listen $M > "$B/meta.log" 2>&1 & MP=$!
wait_port $D || { echo "data-server down"; cat "$B/data.log"; exit 1; }
wait_port $M || { echo "meta-server down"; cat "$B/meta.log"; exit 1; }
"$Y" view --meta-dir "$B/central" create --view v1 > "$B/create.log" 2>&1 || { cat "$B/create.log"; exit 1; }

PIDS=""
for i in 1 2 3 4; do
  mkdir -p "$B/mnt$i"
  "$Y" --home "$B/m$i" mount --view v1 --mountpoint "$B/mnt$i" \
      --meta-server $M --data-servers $D --threads 8 --foreground > "$B/m$i.log" 2>&1 &
  PIDS="$PIDS $!"
done
sleep 2
cleanup(){ for i in 1 2 3 4; do fusermount -u "$B/mnt$i" 2>/dev/null; done; kill $PIDS $DP $MP 2>/dev/null; }
trap cleanup EXIT

cpu_ticks(){ awk '{print $14+$15}' /proc/$1/stat; }   # utime+stime
ncpu_ticks(){ awk '{print $20}' /proc/$1/stat; }      # num_threads

HZ=$(getconf CLK_TCK)

mbps(){ awk -v m="$1" -v t="$2" 'BEGIN{ if(t>0) printf "%.0f", m/t; else print "n/a" }'; }

run_write(){ # $1=clients $2=writers_per_client $3=MiB_per_writer $4=label
  local nc=$1 nw=$2 mib=$3 label=$4
  local total=$(( nc*nw*mib ))
  local t0 cpu0
  t0=$(date +%s.%N); cpu0=$(cpu_ticks $DP)
  local files=0 jpids=""
  for c in $(seq 1 $nc); do
    for w in $(seq 1 $nw); do
      dd if=/dev/zero of="$B/mnt$c/f${c}_${w}.img" bs=1M count=$mib status=none 2>/dev/null &
      jpids="$jpids $!"; files=$((files+1))
    done
  done
  wait $jpids
  local t1 cpu1 dt dcpu
  t1=$(date +%s.%N); cpu1=$(cpu_ticks $DP)
  dt=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
  dcpu=$(awk -v a="$cpu0" -v b="$cpu1" -v hz="$HZ" 'BEGIN{printf "%.2f", (b-a)/hz}')
  echo "$label: files=$files total=${total}MiB wall=${dt}s  aggregate=$(mbps $total $dt) MB/s  server_cpu=${dcpu}s  cpu_per_GiB=$(awk -v c="$dcpu" -v m="$total" 'BEGIN{printf "%.2f", c/(m/1024)}')s  server_threads=$(ncpu_ticks $DP)"
}

run_read(){ # $1=clients(reader mounts) $2=writers_per_client $3=MiB_per_file $4=label
  local nc=$1 nw=$2 mib=$3 label=$4
  local total=$(( nc*nw*mib ))
  local t0 cpu0 jpids=""
  t0=$(date +%s.%N); cpu0=$(cpu_ticks $DP)
  for c in $(seq 1 $nc); do
    for w in $(seq 1 $nw); do
      dd if="$B/mnt$c/f${c}_${w}.img" of=/dev/null bs=1M status=none 2>/dev/null &
      jpids="$jpids $!"
    done
  done
  wait $jpids
  local t1 cpu1 dt dcpu
  t1=$(date +%s.%N); cpu1=$(cpu_ticks $DP)
  dt=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
  dcpu=$(awk -v a="$cpu0" -v b="$cpu1" -v hz="$HZ" 'BEGIN{printf "%.2f", (b-a)/hz}')
  echo "$label: total=${total}MiB wall=${dt}s  aggregate=$(mbps $total $dt) MB/s  server_cpu=${dcpu}s  cpu_per_GiB=$(awk -v c="$dcpu" -v m="$total" 'BEGIN{printf "%.2f", c/(m/1024)}')s  server_threads=$(ncpu_ticks $DP)"
}

echo "== data-server pid=$DP  hz=$HZ =="
run_write 1 8 128 "W1  1 client / 8 writers"
run_write 4 8 64  "W4  4 clients / 32 writers"
run_read  1 8 128 "R1  1 client / 8 readers"
run_read  3 8 64  "R3  3 clients / 24 readers"
echo "== data-server totals =="
awk '{printf "  utime+stime=%ds threads=%d rss=%dMB\n", ($14+$15)/100, $20, $24*4096/1048576}' /proc/$DP/stat
