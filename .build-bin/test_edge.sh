#!/usr/bin/env bash
# Boundary + concurrency correctness for grovefs, focused on the WRITEBACK_CACHE
# path (kernel now coalesces/buffers writes) so data integrity is paramount:
#   empty file, random-data round-trip md5, positional overwrite, sparse hole,
#   truncate down/extend, multi-process writers, concurrent O_APPEND to one file,
#   mixed concurrent readers+writers across the multi-threaded session dispatch.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
rm -rf .etest; mkdir -p .etest/home .etest/mnt
export GROVEFS_HOME=$PWD/.etest/home
$Y init --view edge >/dev/null
$Y mount --view edge --mountpoint .etest/mnt --foreground > .etest/mnt.log 2>&1 &
MP=$!
sleep 1.0
cleanup(){ fusermount -u .etest/mnt 2>/dev/null; kill $MP 2>/dev/null; }
trap cleanup EXIT
M=.etest/mnt
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

echo "===== 1) empty file ====="
timeout 5 sh -c "touch $M/empty"
chk "$(stat -c '%s' $M/empty)" "0" "empty file size 0"
chk "$(wc -c < $M/empty)" "0" "reading empty yields 0 bytes"

echo "===== 2) random-data integrity (writeback path) 8MB md5 compare ====="
head -c 8388608 /dev/urandom > .etest/rand.bin
timeout 10 cp .etest/rand.bin $M/rand.bin
sync
src=$(md5sum .etest/rand.bin | awk '{print $1}')
dst=$(md5sum $M/rand.bin | awk '{print $1}')
chk "$dst" "$src" "8MB random round-trip md5 identical"

echo "===== 3) positional overwrite in the middle (pwrite semantics) ====="
timeout 5 sh -c "printf 'AAAAAAAAAA' > $M/mid"          # 10 x A
timeout 5 sh -c "printf 'ZZZ' | dd of=$M/mid bs=1 seek=3 conv=notrunc 2>/dev/null"
chk "$(timeout 5 cat $M/mid)" "AAAZZZAAAA" "overwrite 3 bytes at offset 3"
chk "$(stat -c '%s' $M/mid)" "10" "size unchanged after in-place overwrite"

echo "===== 4) sparse file: write far offset, gap must read as zeros ====="
timeout 15 sh -c "dd if=/dev/zero of=$M/sparse bs=100000 seek=1 count=0 2>/dev/null; printf 'TAIL' | dd of=$M/sparse bs=1 seek=100000 conv=notrunc 2>/dev/null"
chk "$(stat -c '%s' $M/sparse)" "100004" "sparse file size = offset+data"
chk "$(timeout 5 dd if=$M/sparse bs=1 skip=0 count=1 2>/dev/null | od -An -tu1 | tr -d ' ')" "0" "gap byte is zero"
chk "$(timeout 5 dd if=$M/sparse bs=1 skip=100000 count=4 2>/dev/null)" "TAIL" "data at end intact"

echo "===== 5) truncate down then extend ====="
timeout 5 sh -c "head -c 20 /dev/zero | tr '\\0' 'x' > $M/tr.txt"
timeout 5 truncate -s 5 $M/tr.txt
chk "$(stat -c '%s' $M/tr.txt)" "5" "truncate down to 5"
timeout 5 truncate -s 12 $M/tr.txt
chk "$(stat -c '%s' $M/tr.txt)" "12" "extend back to 12"
chk "$(timeout 5 dd if=$M/tr.txt bs=1 skip=5 count=1 2>/dev/null | od -An -tu1 | tr -d ' ')" "0" "extended region is zero"

echo "===== 6) concurrent writers to SEPARATE files (multi-process) ====="
NPROC=${NPROC:-6}; NLINE=${NLINE:-100}
pids=""
for i in $(seq 1 $NPROC); do
  timeout 30 sh -c "for n in \$(seq 1 $NLINE); do echo \"p$i-\$n\"; done > $M/par_$i" &
  pids="$pids $!"
done
wait $pids   # ONLY wait for the writers, not the background mount daemon
tot=$(timeout 20 cat $M/par_* | wc -l)
chk "$tot" "$((NPROC*NLINE))" "$NPROC parallel writers produced $NPROC x $NLINE lines"
l1=$(timeout 5 grep -c "^p1-" $M/par_1); chk "$l1" "$NLINE" "writer 1 file not corrupted by others"

echo "===== 7) concurrent O_APPEND to the SAME file ====="
AP=${AP:-6}; AL=${AL:-50}
timeout 10 sh -c "printf '' > $M/app.log"
pids=""
for i in $(seq 1 $AP); do
  timeout 30 sh -c "for n in \$(seq 1 $AL); do echo a >> $M/app.log; done" &
  pids="$pids $!"
done
wait $pids   # ONLY the append writers
lines=$(timeout 5 wc -l < $M/app.log)
chk "$lines" "$((AP*AL))" "$AP x $AL concurrent appends all persisted (no lost writes)"

echo "===== 8) mixed concurrent readers + writers on one mount (multi-thread dispatch) ====="
# The fuser 0.18 session dispatches callbacks across worker threads (n_threads,
# clone_fd), so readers and writers run genuinely in parallel inside the FUSE
# daemon. Integrity: every reader must always see either nothing or a complete
# md5-valid snapshot of ITS OWN file; writers must never corrupt each other.
timeout 10 sh -c "head -c 4194304 /dev/urandom > $M/mix_src_a; head -c 4194304 /dev/urandom > $M/mix_src_b"
sa=$(md5sum $M/mix_src_a | awk '{print $1}')
sb=$(md5sum $M/mix_src_b | awk '{print $1}')
# Writers keep rewriting their targets; readers keep hashing their completed
# sources — read/write callbacks interleave across session worker threads.
timeout 30 sh -c "for n in \$(seq 1 20); do head -c 1048576 /dev/urandom > $M/mix_t\$n; done" &
wp=$!
timeout 30 sh -c "for n in \$(seq 1 20); do head -c 1048576 /dev/urandom > $M/mix_u\$n; done" &
wq=$!
rr=0
for n in 1 2 3 4 5; do
  ta=$(md5sum $M/mix_src_a | awk '{print $1}')
  tb=$(md5sum $M/mix_src_b | awk '{print $1}')
  [ "$ta" = "$sa" ] && [ "$tb" = "$sb" ] && rr=$((rr+1))
done
wait $wp $wq
[ "$rr" -ge 5 ] && ok "5 concurrent md5 reads stable while 2 writers run ($rr/5 clean)" || bad "concurrent read+write mix disturbed reads ($rr/5 clean)"
# NOTE: count via a fresh readdir of the dir, not shell globs — globs expand
# against the kernel's ~2s dentry cache, which may not list yet another
# process's just-created files (legally stale per POSIX; readdir revalidates).
chk "$(timeout 10 ls $M/ | grep -c 'mix_t[0-9]\|mix_u[0-9]')" "40" "all 40 interleaved write files landed"
chk "$(timeout 10 stat -c '%s' $M/mix_t1)" "1048576" "interleaved writer file size exact"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL EDGE CHECKS PASSED" || echo "SOME EDGE CHECKS FAILED"
# Return a meaningful exit code for the unified runner / CI (echo always 0'd it).
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
