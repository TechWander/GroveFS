set -u
cd /data/grovefs/grovefs
Y=./target/release/grovefs
A=127.0.0.1:15151
B=127.0.0.1:15152
rm -rf .rgap; mkdir -p .rgap/home .rgap/sA .rgap/sB .rgap/mnt
export GROVEFS_HOME=$PWD/.rgap/home
$Y data-server --listen $A --data-dir $PWD/.rgap/sA > .rgap/sA.log 2>&1 &
PA=$!
$Y data-server --listen $B --data-dir $PWD/.rgap/sB > .rgap/sB.log 2>&1 &
PB=$!
sleep 0.8
$Y init --view alice > /dev/null
$Y mount --view alice --mountpoint .rgap/mnt --data-servers "$A,$B" --foreground > .rgap/mnt.log 2>&1 &
PM=$!
sleep 1.2
echo "=== write 40MiB, record md5 ==="
dd if=/dev/urandom of=.rgap/mnt/big.bin bs=1M count=40 2>/dev/null
sync .rgap/mnt/big.bin
md5sum .rgap/mnt/big.bin > .rgap/md5.good; cat .rgap/md5.good
echo "=== stop mount+servers, delete ONE interior chunk (#1 lives on B) ==="
fusermount -u .rgap/mnt; kill $PM $PA $PB; wait 2>/dev/null
find .rgap/sB/blocks -type f | head -1
rm -f $(find .rgap/sB/blocks -type f | head -1)
if ss -ltn | grep -q "15152"; then echo "port B still listening!"; exit 1; fi
$Y data-server --listen $A --data-dir .rgap/sA > /dev/null 2>&1 &
PA=$!
$Y data-server --listen $B --data-dir .rgap/sB > /dev/null 2>&1 &
PB=$!
sleep 0.8
$Y mount --view alice --mountpoint .rgap/mnt --data-servers "$A,$B" --foreground > .rgap/mnt2.log 2>&1 &
PM=$!
sleep 1.2
echo "=== stat (metadata says?) ==="
stat -c "size=%s" .rgap/mnt/big.bin
echo "=== full read through mount: rc, bytes landed, md5 vs original ==="
md5sum .rgap/mnt/big.bin > .rgap/md5.read
rc=$?
echo "rc=$rc"
cat .rgap/md5.read
echo "byte count actually returned by read:"
wc -c < .rgap/mnt/big.bin
echo "=== du still announces damage (control) ==="
$Y du --view alice --home $PWD/.rgap/home --data-servers "$A,$B" --bytes --total 2>&1 | head -3
fusermount -u .rgap/mnt 2>/dev/null; kill $PM $PA $PB 2>/dev/null; wait 2>/dev/null
