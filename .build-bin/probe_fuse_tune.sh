set -u
cd /data/grovefs/grovefs
rm -rf .fstune && mkdir -p .fstune/home .fstune/mnt .fstune/src
export GROVEFS_HOME=$PWD/.fstune/home
Y=./target/release/grovefs
$Y init --view alice >/dev/null 2>&1
$Y mount --view alice --mountpoint .fstune/mnt --foreground > .fstune/mnt.log 2>&1 &
sleep 1.5
grep -qs " $PWD/.fstune/mnt " /proc/mounts || { echo "MOUNT NOT LIVE"; cat .fstune/mnt.log; exit 1; }
M=.fstune/mnt

echo "--- [1] stat -f over the mount (bsize/frsize) ---"
stat -f -c "bsize=%f_frsize bsize_blk=%f_bsize files_max=%f_files" $M
stat -C $M 2>/dev/null | head -1 || true

echo "--- [2] warm/cold read + overwrite of a 1GiB file ---"
dd if=/dev/urandom of=.fstune/src/big.bin bs=1M count=1024 status=none
cp .fstune/src/big.bin $M/big.bin
sync
echo "== cold read (echo drop_caches may need root; skipped if not) =="
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null && echo "cache dropped" || echo "(no root: read is warm)"
t0=$(date +%s.%N); dd if=$M/big.bin of=/dev/null bs=1M status=none; t1=$(date +%s.%N)
echo "read 1G: $(echo "$t1 - $t0" | bc) s"
echo "== overwrite in place (cp truncates+rewrites) =="
t0=$(date +%s.%N); cp .fstune/src/big.bin $M/big.bin; sync; t1=$(date +%s.%N)
echo "overwrite 1G: $(echo "$t1 - $t0" | bc) s"
echo "== fresh create (no stale cache at all) =="
t0=$(date +%s.%N); cp .fstune/src/big.bin $M/big2.bin; sync; t1=$(date +%s.%N)
echo "create   1G: $(echo "$t1 - $t0" | bc) s"

echo "--- [3] small O_TRUNC rewrite cost (per-open round trips) ---"
head -c 4096 /dev/urandom > .fstune/src/small.bin
for i in $(seq 1 200); do cp .fstune/src/small.bin $M/small.bin; done
echo "200x cp-with-truncate: done (rc=$?)"
t0=$(date +%s.%N)
for i in $(seq 1 200); do cp .fstune/src/small.bin $M/small.bin; done
t1=$(date +%s.%N)
echo "200x truncate+rewrite 4KiB: $(echo "$t1 - $t0" | bc) s"
t0=$(date +%s.%N)
for i in $(seq 1 200); do cat .fstune/src/small.bin > $M/small_new_$i.bin; done
t1=$(date +%s.%N)
echo "200x create-new 4KiB:        $(echo "$t1 - $t0" | bc) s"
