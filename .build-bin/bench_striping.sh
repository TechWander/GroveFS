#!/usr/bin/env bash
# Throughput bench for the P5 STRIPING data path: writes/reads a large file
# across ONE data server vs TWO striped data servers (client --data a,b), to see
# whether splitting a file's chunks over multiple server sockets raises aggregate
# bandwidth. Run alongside (not inside) run_all.sh's bench suite.
#
# Honest framing: striping parallelizes at the *server* granularity (chunk i ->
# server i%N). Its payoff requires the per-socket path to be the bottleneck. On
# loopback TCP a single socket already saturates, so — like the P3 chunked-IO
# finding — no gain is expected here; real multi-NIC / multi-node links must be
# re-measured before claiming a win. Correctness is gated by test_striping.sh and
# the backend::sharded unit tests, not by these numbers.
#
# Mount-level numbers below are noisy: writing large images to two servers plus
# their FUSE daemons thrashes page cache, so a striped read can show a large
# apparent regression that does NOT reproduce in isolation. The authoritative
# signal is the isolated backend probe (no FUSE, no client cache):
#   cargo test --release --test stripe_probe -- --ignored --nocapture
# Measured there on loopback (128MiB, 2 servers):
#   single-threaded: stripe is ~0.73-0.82x of single (pure wrapper cost)
#   n=4 concurrent (emulates the mount's n_threads): single 3736 vs stripe 3407
#     MB/s = 0.91x — i.e. near parity, no scaling bug.
# Honest conclusion: striping yields NO throughput gain on a loopback / single
# saturated link and adds a small per-op cost; its purpose is to spread a file
# across the *aggregate* capacity of multiple nodes, which must be measured on
# genuinely independent links before any win is claimed.
#
# Measured on this machine (2026-10-13, SIZE_MB=512, loopback TCP, 2 servers):
#   1-server  write ~310 MB/s   read ~1250 MB/s
#   2-stripe  write ~200 MB/s   read ~360 MB/s   (mount-level, cache-thrashed;
#     superseded by the isolated probe above for interpretation)
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
S1=127.0.0.1:50141
S2=127.0.0.1:50142
SIZE_MB=${SIZE_MB:-512}

rm -rf .bs; mkdir -p .bs/home .bs/r1 .bs/r2 .bs/mnt1 .bs/mnt2
export GROVEFS_HOME=$PWD/.bs/home

secs(){ local t0 t1 rc e; t0=$(date +%s.%N); "$@" >/dev/null 2>&1; rc=$?; t1=$(date +%s.%N)
  e=$(awk -v a="$t0" -v b="$t1" 'BEGIN{ printf "%.3f", b-a }'); echo "$e"; return $rc; }
rate(){ awk -v s="$1" -v t="$2" 'BEGIN{ if(t>0) printf "%.1f", s/t; else print "n/a" }'; }

# ---- single-server mount ----
$Y data-server --listen $S1 --root $PWD/.bs/r1 > .bs/s1.log 2>&1 & P1=$!
$Y init --view perf1 >/dev/null
$Y mount --view perf1 --mountpoint .bs/mnt1 --data $S1 --foreground > .bs/mnt1.log 2>&1 & M1=$!
sleep 1.0
# ---- two-server striped mount ----
$Y data-server --listen $S2 --root $PWD/.bs/r2 > .bs/s2.log 2>&1 & P2=$!
$Y init --view perf2 >/dev/null
$Y mount --view perf2 --mountpoint .bs/mnt2 --data "$S1,$S2" --foreground > .bs/mnt2.log 2>&1 & M2=$!
sleep 1.0

cleanup(){ fusermount -u .bs/mnt1 .bs/mnt2 2>/dev/null; kill $M1 $M2 $P1 $P2 2>/dev/null; }
trap cleanup EXIT

echo "=================================================="
echo " grovefs STRIPING throughput (size=${SIZE_MB}MB, 1 vs 2 servers)"
echo "=================================================="
w1=$(secs dd if=/dev/zero of=.bs/mnt1/s.img bs=1M count=$SIZE_MB conv=fsync)
echo "  1-server  write  bs=1M: ${w1}s  ($(rate $SIZE_MB $w1) MB/s)"
sync .bs/mnt1 2>/dev/null
r1=$(secs dd if=.bs/mnt1/s.img of=/dev/null bs=1M)
echo "  1-server  read   bs=1M: ${r1}s  ($(rate $SIZE_MB $r1) MB/s)"

w2=$(secs dd if=/dev/zero of=.bs/mnt2/t.img bs=1M count=$SIZE_MB conv=fsync)
echo "  2-stripe  write  bs=1M: ${w2}s  ($(rate $SIZE_MB $w2) MB/s)"
sync .bs/mnt2 2>/dev/null
r2=$(secs dd if=.bs/mnt2/t.img of=/dev/null bs=1M)
echo "  2-stripe  read   bs=1M: ${r2}s  ($(rate $SIZE_MB $r2) MB/s)"

echo "  --- striped file really spans both roots ---"
echo "    r1 blocks=$(find .bs/r1/blocks -type f 2>/dev/null | wc -l) r2 blocks=$(find .bs/r2/blocks -type f 2>/dev/null | wc -l)"
echo "  speedup write: $(awk -v a=$w1 -v b=$w2 'BEGIN{if(b>0)printf "%.2fx", a/b}')  read: $(awk -v a=$r1 -v b=$r2 'BEGIN{if(b>0)printf "%.2fx", a/b}')"
echo DONE
