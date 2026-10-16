#!/usr/bin/env bash
# Throughput micro-benchmark: sequential write + read through the FUSE mount,
# compared against a direct write to the same underlying filesystem (baseline).
# Quantifies the userspace-FUSE overhead so the "high efficiency" claim is measured,
# not asserted. Uses only coreutils `dd` + `time` (no fio dependency).
set -u
cd /data/xy/yourfs
export PKG_CONFIG=/data/xy/yourfs/.build-bin/pkg-config
Y=./target/release/yourfs
rm -rf .bench; mkdir -p .bench/home .bench/mnt
export YOURFS_HOME=$PWD/.bench/home

$Y init --user perf >/dev/null
$Y mount --user perf --mountpoint .bench/mnt > .bench/mnt.log 2>&1 &
MP=$!
sleep 1.0
cleanup(){ fusermount -u .bench/mnt 2>/dev/null; kill $MP 2>/dev/null; rm -f .bench/direct.img; }
trap cleanup EXIT

SIZE_MB=${SIZE_MB:-512}          # total bytes to move
BS=${BS:-1M}                      # dd block size (FUSE max write is typically 1M+)
M=.bench/mnt

secs(){ # run command, echo elapsed seconds (float)
  local t0 t1 rc e
  t0=$(date +%s.%N)
  "$@" >/dev/null 2>&1; rc=$?
  t1=$(date +%s.%N)
  e=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }')
  echo "$e"
  return $rc
}
rate(){ # $1=size_mb $2=seconds -> MB/s
  awk -v s="$1" -v t="$2" 'BEGIN{ if(t>0) printf "%.1f", s/t; else print "n/a" }';
}

echo "=================================================="
echo " yourfs FUSE throughput  (size=${SIZE_MB}MB bs=${BS})"
echo "=================================================="

echo "--- [A] direct write to underlying fs (baseline) ---"
base_t=$(secs dd if=/dev/zero of=.bench/direct.img bs=$BS count=$SIZE_MB conv=fsync 2>/dev/null)
sync
echo "  baseline write: ${base_t}s  ($(rate $SIZE_MB $base_t) MB/s)"

echo "--- [B] write through yourfs mount ---"
fuse_t=$(secs dd if=/dev/zero of=$M/fuse.img bs=$BS count=$SIZE_MB conv=fsync 2>/dev/null)
echo "  yourfs   write: ${fuse_t}s  ($(rate $SIZE_MB $fuse_t) MB/s)"

echo "--- [C] drop caches-ish: read back through yourfs ---"
rd_t=$(secs dd if=$M/fuse.img of=/dev/null bs=$BS 2>/dev/null)
echo "  yourfs    read: ${rd_t}s  ($(rate $SIZE_MB $rd_t) MB/s)"

echo "--- [D] re-read (warm) through yourfs ---"
rd2_t=$(secs dd if=$M/fuse.img of=/dev/null bs=$BS 2>/dev/null)
echo "  yourfs read(warm): ${rd2_t}s  ($(rate $SIZE_MB $rd2_t) MB/s)"

echo "--- [E] small random-ish 4k writes (latency-sensitive path) ---"
sm_t=$(secs dd if=/dev/zero of=$M/small.img bs=4k count=8192 conv=fsync 2>/dev/null)
echo "  4k x8192 write: ${sm_t}s  ($(rate 32 $sm_t) MB/s over 32MB)"

echo "--- [F] 2M-block read (local backend has no connection pool; the"
echo "     RemoteBackend chunked-read path is measured by bench_remote.sh) ---"
b2_t=$(secs dd if=$M/fuse.img of=/dev/null bs=2M 2>/dev/null)
echo "  yourfs read bs=2M: ${b2_t}s  ($(rate $SIZE_MB $b2_t) MB/s)"

echo
echo "integrity: md5 of source vs yourfs copy"
sha256sum $M/fuse.img | cut -c1-16
echo "DONE"
