#!/usr/bin/env bash
# A/B throughput bench for the fuser 0.18 multi-threaded session dispatch:
#   --threads 1  (legacy single worker, n_threads=None)   vs
#   --threads 4  (4 worker event loops + clone_fd)
# Same payloads, same order of operations; only the session config differs.
# This quantifies the P1 claim with measured numbers instead of asserting it.
#
# Measured on this machine (2026-10-13, SIZE_MB=512 bs=1M, local backend):
#   threads=1  write=328.4  read=1882.4  read(warm)=1861.8  4kx8192=313.7  4xparallel=594.0  MB/s
#   threads=4  write=487.2  read=1861.8  read(warm)=1868.6  4kx8192=313.7  4xparallel=1094.0 MB/s
# Honest reading: single-stream seq write +48% (writeback flushes dispatched in
# parallel), 4-file concurrent write +84% (the multi-worker win); reads unchanged
# (already page-cache/throughput-bound on one stream). Run-to-run variance on the
# single-stream write was large (an earlier run: 407 -> 534), the concurrent and
# read numbers were stable.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove
SIZE_MB=${SIZE_MB:-512}
BS=${BS:-1M}

secs(){ # run command, echo elapsed seconds (float)
  local t0 t1 rc e
  t0=$(date +%s.%N)
  "$@" >/dev/null 2>&1; rc=$?
  t1=$(date +%s.%N)
  e=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }')
  echo "$e"
  return $rc
}
rate(){ awk -v s="$1" -v t="$2" 'BEGIN{ if(t>0) printf "%.1f", s/t; else print "n/a" }'; }

run_case(){ # $1 = --threads value; echoes 3 lines of results
  local TH=$1 TAG="t$1"
  rm -rf .bt-$TAG; mkdir -p .bt-$TAG/home .bt-$TAG/mnt
  GROVEFS_HOME=$PWD/.bt-$TAG/home $Y init --view perf >/dev/null
  GROVEFS_HOME=$PWD/.bt-$TAG/home $Y mount --view perf --mountpoint .bt-$TAG/mnt --threads $TH > .bt-$TAG/mnt.log 2>&1 &
  local MP=$!
  sleep 1.0
  local M=.bt-$TAG/mnt
  local w r rw sm c4 t0 t1 cs
  w=$(secs dd if=/dev/zero of=$M/fuse.img bs=$BS count=$SIZE_MB conv=fsync 2>/dev/null)
  r=$(secs dd if=$M/fuse.img of=/dev/null bs=$BS 2>/dev/null)
  rw=$(secs dd if=$M/fuse.img of=/dev/null bs=$BS 2>/dev/null)
  sm=$(secs dd if=/dev/zero of=$M/small.img bs=4k count=8192 conv=fsync 2>/dev/null)
  # [F] 4 concurrent dd writers to 4 separate files (each SIZE_MB/4): the
  # workload multi-threaded dispatch exists for — independent request streams.
  t0=$(date +%s.%N)
  local pids=""
  for i in 1 2 3 4; do
    dd if=/dev/zero of=$M/c$i.img bs=$BS count=$((SIZE_MB/4)) conv=fsync >/dev/null 2>&1 &
    pids="$pids $!"
  done
  wait $pids
  t1=$(date +%s.%N)
  cs=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }')
  echo "threads=$TH  write=$(rate $SIZE_MB $w) MB/s  read=$(rate $SIZE_MB $r) MB/s  read(warm)=$(rate $SIZE_MB $rw) MB/s  4kx8192=$(rate 32 $sm) MB/s  4xparallel=$(rate $SIZE_MB $cs) MB/s"
  fusermount -u $M 2>/dev/null; kill $MP 2>/dev/null
  wait $MP 2>/dev/null
  rm -rf .bt-$TAG
}

echo "=================================================="
echo " grovefs multi-thread dispatch A/B  (size=${SIZE_MB}MB bs=${BS})"
echo "=================================================="
run_case 1
run_case 4
echo "DONE"
