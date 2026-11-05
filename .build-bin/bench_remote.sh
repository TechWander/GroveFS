#!/usr/bin/env bash
# Throughput bench for the REMOTE data path (client FUSE --TCP--> data-server),
# focused on the P3 chunked-parallel read: whole-file md5 reads issued as 2MiB
# dd requests exceed the 1MiB READ_CHUNK threshold and are striped across
# parallel pooled connections. Numbers here are the honest measurement for the
# P3 claim; run alongside (not inside) run_all.sh's bench suite.
#
# Measured on this machine (2026-10-13, SIZE_MB=512, loopback TCP):
#   write bs=1M: 398.1 MB/s
#   read  bs=1M: 1302.8 MB/s   bs=2M (chunked): 1261.1 MB/s   bs=4M (chunked): 1185.2 MB/s
# Honest conclusion: NO gain on loopback — a single localhost TCP socket already
# saturates at ~1.3 GB/s here, so splitting a request into parallel slices only
# adds framing/stitching overhead (slightly negative, and reproducible). The
# chunked path is expected to pay off only when one socket does NOT saturate the
# path (real LAN with slower NICs or a busy server); re-measure on such a link
# before claiming a win. Correctness of the chunked path is gated by test_remote
# section 8 and tests/remote_roundtrip.rs instead.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
ADDR=127.0.0.1:15098
SIZE_MB=${SIZE_MB:-512}

rm -rf .br; mkdir -p .br/home .br/srv .br/mnt
export GROVEFS_HOME=$PWD/.br/home
$Y data-server --listen $ADDR --root $PWD/.br/srv > .br/srv.log 2>&1 &
SP=$!
$Y init --view perf >/dev/null
$Y mount --view perf --mountpoint .br/mnt --data $ADDR --foreground > .br/mnt.log 2>&1 &
MP=$!
sleep 1.0
cleanup(){ fusermount -u .br/mnt 2>/dev/null; kill $MP $SP 2>/dev/null; }
trap cleanup EXIT

secs(){ local t0 t1 rc e; t0=$(date +%s.%N); "$@" >/dev/null 2>&1; rc=$?; t1=$(date +%s.%N)
  e=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }'); echo "$e"; return $rc; }
rate(){ awk -v s="$1" -v t="$2" 'BEGIN{ if(t>0) printf "%.1f", s/t; else print "n/a" }'; }

M=.br/mnt
echo "=================================================="
echo " grovefs REMOTE-path throughput (size=${SIZE_MB}MB)"
echo "=================================================="
w_t=$(secs dd if=/dev/zero of=$M/r.img bs=1M count=$SIZE_MB conv=fsync)
echo "  remote write  bs=1M: ${w_t}s  ($(rate $SIZE_MB $w_t) MB/s)"
sync
r1_t=$(secs dd if=$M/r.img of=/dev/null bs=1M)
echo "  remote read   bs=1M: ${r1_t}s  ($(rate $SIZE_MB $r1_t) MB/s)"
r2_t=$(secs dd if=$M/r.img of=/dev/null bs=2M)
echo "  remote read   bs=2M (chunked-parallel): ${r2_t}s  ($(rate $SIZE_MB $r2_t) MB/s)"
r4_t=$(secs dd if=$M/r.img of=/dev/null bs=4M)
echo "  remote read   bs=4M (chunked-parallel): ${r4_t}s  ($(rate $SIZE_MB $r4_t) MB/s)"
echo DONE
