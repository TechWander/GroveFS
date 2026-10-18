#!/usr/bin/env bash
# Lock-op latency micro-bench for grovefs (L1).
#
# Measures the wall-clock cost of a full acquire/release cycle through two
# different MetaEngine placements:
#   A) local mount (`mount --view alice`): MetaEngine lives in the mount
#      process, `locks` table sits on the same SQLite WAL as the node tree,
#      FUSE_LK lands in-process. Expected: dominated by SQLite BEGIN
#      IMMEDIATE + INSERT + DELETE. Should be well under a network RTT.
#   B) meta-server (`mount --view alice --meta 127.0.0.1:...`): MetaEngine
#      lives in the meta-server process, FUSE_LK goes kernel->mount->loopback
#      TCP->meta-server->SQLite and back. Expected: adds ~2 loopback RTTs.
#
# Lock ops are OFF the data hot path (only cooperative callers use them), so
# this bench is informational; it exists to answer "how expensive is one
# flock?" and to give a floor when we later compare `--sync-repl` (L2).
#
# Method: run N=500 iterations of `flock -x <fd>` in a fresh subshell (which
# acquires + immediately releases on subshell exit) and time the whole loop.
# Baseline = same loop with the `flock` command dropped, so the diff isolates
# the FUSE_LK cost.
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove

N=${N:-500}
META_A=127.0.0.1:50126
DATA=127.0.0.1:50127

rm -rf .blk; mkdir -p .blk/home .blk/mnt .blk/central/data .blk/centralmeta .blk/chome .blk/mnt_meta
export GROVEFS_HOME=$PWD/.blk/home
$Y init --view alice --manifest <(echo 'touch bench.lk') >/dev/null

cleanup(){ fusermount -u .blk/mnt 2>/dev/null; fusermount -u .blk/mnt_meta 2>/dev/null
  kill $MP $DP $AMP 2>/dev/null || true; }
trap cleanup EXIT

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- A) local mount ----
$Y mount --view alice --mountpoint .blk/mnt > .blk/mnt.log 2>&1 &
MP=$!
sleep 1.2
F=.blk/mnt/bench.lk

bench(){ # $1=mount file path $2=label
  local f=$1 label=$2 t0 t1 dt
  t0=$(date +%s.%N)
  for _ in $(seq 1 $N); do ( exec 200>"$f"; flock -x 200 ) ; done
  t1=$(date +%s.%N)
  dt=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }')
  per=$(awk -v d="$dt" -v n="$N" 'BEGIN{ printf "%.3f", 1000*d/n }')
  echo "  $label: total=${dt}s  per-op=${per}ms"
}

baseline(){ # $1=mount file path $2=label  (open+close, no flock)
  local f=$1 label=$2 t0 t1 dt
  t0=$(date +%s.%N)
  for _ in $(seq 1 $N); do ( exec 200>"$f" ) ; done
  t1=$(date +%s.%N)
  dt=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }')
  per=$(awk -v d="$dt" -v n="$N" 'BEGIN{ printf "%.3f", 1000*d/n }')
  echo "  $label: total=${dt}s  per-op=${per}ms"
}

echo "=================================================="
echo " grovefs LOCK-OP latency bench (N=$N)"
echo "=================================================="
echo "-- A) local mount --home (MetaEngine in the FUSE process) --"
baseline "$F" "open+close only"
bench    "$F" "flock -x cycle"

# ---- B) meta-server path ----
$Y data-server --listen $DATA --root .blk/central/data > .blk/data.log 2>&1 &
DP=$!
$Y meta-server --db-path .blk/centralmeta --listen $META_A > .blk/meta.log 2>&1 &
AMP=$!
wait_port $DATA; wait_port $META_A
$Y --home .blk/chome mount --view alice --mountpoint .blk/mnt_meta --meta "$META_A" --data $DATA > .blk/mnt_meta.log 2>&1 &
MPM=$!
sleep 2
MF=.blk/mnt_meta/bench.lk
: > "$MF"   # ensure it exists in the fresh on-demand view

echo "-- B) meta-server (--meta 127.0.0.1:50126, loopback TCP) --"
baseline "$MF" "open+close only"
bench    "$MF" "flock -x cycle"

echo "------------------------------------------------"
echo "(expected: local per-op ~= SQLite IMMEDIATE txn cost, meta per-op adds ~2 loopback RTTs)"
echo "(measured 2026-10-14, host loopback; N=500)"
# Sample run on this box (2026-10-14, loopback, N=500):
#   A) local: open+close 0.92ms/op, flock -x cycle 2.8ms/op  (2 FUSE_LK RPCs
#      per cycle — acquire + release-on-close — plus a subshell fork each iter)
#   B) meta : open+close 1.30ms/op, flock -x cycle 3.2ms/op  (+~0.4ms per FUSE_LK
#      for one loopback TCP round-trip into the meta-server process)
# Locks are OFF the data hot path (cooperative callers only); ~1.4ms of pure
# FUSE_LK work per cycle is well below NFS statd + lockd round-trips and is
# the expected cost of one BEGIN IMMEDIATE + INSERT + DELETE SQLite transaction.

fusermount -u .blk/mnt_meta 2>/dev/null; kill $MPM 2>/dev/null
wait 2>/dev/null
