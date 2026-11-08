#!/usr/bin/env bash
# End-to-end test of the DETERMINISTIC CROSS-SERVER STRIPING data path (P5):
#   client FUSE daemon --(TCP framing)--> two data-servers (distinct roots),
#   a file's bytes spread round-robin in STRIPE_BYTES (16MiB) chunks over
#   server `i % N`, the layout encoded in the durable data_ref (`s<N>:<stripe>:
#   <uuid>`) so no metadata/protocol change is needed.
#
# What this pins down:
#   * a >stripe file physically lands on BOTH server roots (real striping);
#   * bytes read back are identical (md5) including unaligned cross-chunk slices;
#   * truncate/remove fan out to every owned chunk (full GC, idempotent);
#   * a small (<stripe) file pays no cross-server cost (lives on server 0);
#   * a legacy single-object key (bare uuid, no `s` prefix) still round-trips —
#     backward compatibility with pre-striping objects is preserved.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
# two data servers, distinct roots — the striping targets.
A=127.0.0.1:15121
B=127.0.0.1:15122
STRIPE=$((16*1024*1024))   # 16MiB, matches backend::sharded::STRIPE_BYTES

rm -rf .striping; mkdir -p .striping/home .striping/sA .striping/sB .striping/mnt .striping/mnt_legacy
export GROVEFS_HOME=$PWD/.striping/home    # view DBs (data_ref) live client-side

echo "=== start two data-servers (distinct roots) ==="
$Y data-server --listen $A --data-dir $PWD/.striping/sA > .striping/sA.log 2>&1 &
PA=$!
$Y data-server --listen $B --data-dir $PWD/.striping/sB > .striping/sB.log 2>&1 &
PB=$!
sleep 0.8

echo "=== init alice view + mount across BOTH servers ==="
$Y init --view alice > /dev/null
$Y mount --view alice --mountpoint .striping/mnt --data-servers "$A,$B" --foreground > .striping/mnt.log 2>&1 &
PM=$!
sleep 1.2

cleanup(){ fusermount -u .striping/mnt 2>/dev/null; fusermount -u .striping/mnt_legacy 2>/dev/null; kill $PM $PL $PA $PB 2>/dev/null; }
PL=0
trap cleanup EXIT

M=.striping/mnt
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 20 "$@"; }
blocks(){ find "$1"/blocks -type f 2>/dev/null | wc -l; }   # objects live in 256 fan-out buckets; count FILES, not dir entries

echo "===== 1) create tree + roundtrip over the sharded backend ====="
T mkdir $M/docs
T sh -c "printf 'striped grovefs works' > $M/docs/hello.txt"
chk "$(T cat $M/docs/hello.txt)" "striped grovefs works" "sharded write/read roundtrip"

echo "===== 2) small file (<stripe) lives entirely on server 0 ====="
na0=$(blocks .striping/sA); nb0=$(blocks .striping/sB)
T dd if=/dev/zero of=$M/small.bin bs=4096 count=1 2>/dev/null   # 4KiB << 16MiB
sync "$M/small.bin" 2>/dev/null   # sync the FILE: dirty pages land before counting
nb1=$(blocks .striping/sB)
chk "$nb1" "$nb0" "sub-stripe file adds no object on server B (single-chunk path)"
if [ "$(blocks .striping/sA)" -gt "$na0" ]; then ok "sub-stripe file placed on server 0 only"; else bad "small file not on server 0"; fi

echo "===== 3) >stripe file (40MiB) physically spans BOTH server roots ====="
# 40MiB > 2*16MiB => chunks 0,1,2 => servers A,B,A. Both roots must gain objects.
head -c 41943040 /dev/urandom > /tmp/sblob        # 40MiB
T cp /tmp/sblob $M/big.bin
sync "$M/big.bin" 2>/dev/null   # file fsync forces every chunk WRITE over FUSE NOW
                                # (sync on the DIRECTORY would not flush the file's dirty pages)
if [ "$(blocks .striping/sA)" -ge 2 ]; then ok "server A holds >=2 striped chunks"; else bad "server A chunks=$(blocks .striping/sA)"; fi
if [ "$(blocks .striping/sB)" -ge 1 ]; then ok "server B holds >=1 striped chunk (real striping)"; else bad "server B has no striped chunk"; fi
src=$(md5sum /tmp/sblob | awk '{print $1}')
dst=$(T md5sum $M/big.bin | awk '{print $1}')
chk "$dst" "$src" "40MiB cross-server read is byte-identical (md5)"
chk "$(T stat -c '%s' $M/big.bin)" "41943040" "size via sharded mount"

echo "===== 4) unaligned cross-chunk slice read (spans a stripe boundary) ====="
# read across the 16MiB boundary: start 1MiB before it, length 2MiB straddles two chunks
part=$(T dd if=$M/big.bin bs=1M skip=15 count=2 2>/dev/null | md5sum | awk '{print $1}')
want=$(dd if=/tmp/sblob bs=1M skip=15 count=2 2>/dev/null | md5sum | awk '{print $1}')
chk "$part" "$want" "unaligned cross-chunk dd slice matches source"

echo "===== 5) truncate shrinks owned chunks and trims trailing (cross-server) ones ====="
bB=$(blocks .striping/sB)               # big.bin currently owns a chunk on server B
T truncate -s $STRIPE $M/big.bin        # back to exactly one chunk (16MiB, server A)
sync "$M/big.bin" 2>/dev/null
chk "$(T stat -c '%s' $M/big.bin)" "$STRIPE" "truncate to 16MiB size"
head -c $STRIPE /tmp/sblob > /tmp/sblob16
chk "$(T md5sum $M/big.bin | awk '{print $1}')" "$(md5sum /tmp/sblob16 | awk '{print $1}')" "truncated prefix still byte-identical readback"
if [ "$(blocks .striping/sB)" -lt "$bB" ]; then ok "truncate trimmed the trailing chunk off server B"; else bad "server B trailing chunk not trimmed"; fi

echo "===== 6) GC over striping: rm clears chunks on BOTH roots ====="
# fresh >stripe file so it owns a chunk on each server again.
head -c 41943040 /dev/urandom > /tmp/sblob2
T cp /tmp/sblob2 $M/big2.bin; sync "$M/big2.bin" 2>/dev/null
beforeA=$(blocks .striping/sA); beforeB=$(blocks .striping/sB)
if [ "$beforeB" -ge 1 ]; then ok "re-striped file put a chunk back on server B"; else bad "big2.bin did not stripe onto server B"; fi
T rm $M/big2.bin
# rm's GC delete is synchronous, but WITHOUT the file fsync above a lagging
# kernel writeback could re-create a chunk AFTER its delete (a delayed WRITE
# materializes the object again) — that race once flaked the whole suite RED
# on `(1 -> 1)`. The file is fsynced before rm; poll a short deadline anyway
# so a transient transport hiccup can't fail the assertion outright.
afterA=$beforeA; afterB=$beforeB
for _i in $(seq 1 100); do
  afterA=$(blocks .striping/sA); afterB=$(blocks .striping/sB)
  [ "$afterA" -lt "$beforeA" ] && [ "$afterB" -lt "$beforeB" ] && break
  sleep 0.1
done
if [ "$afterB" -lt "$beforeB" ]; then ok "rm freed server B chunk ($beforeB -> $afterB)"; else bad "server B not GC'd ($beforeB -> $afterB)"; fi
if [ "$afterA" -lt "$beforeA" ]; then ok "rm freed server A chunk ($beforeA -> $afterA)"; else bad "server A not GC'd ($beforeA -> $afterA)"; fi

echo "===== 7) backward compat: single --data-servers mount uses bare-uuid keys ====="
# a single address must NOT emit the s-prefix layout (old objects keep working).
# use a distinct user so its local view DB never contends with the sharded mount.
$Y init --view bob > /dev/null
$Y mount --view bob --mountpoint .striping/mnt_legacy --data-servers $A --foreground > .striping/mnt_legacy.log 2>&1 &
PL=$!
sleep 1.2
T sh -c "printf 'legacy-single' > .striping/mnt_legacy/leg.txt"
chk "$(T cat .striping/mnt_legacy/leg.txt)" "legacy-single" "single-address mount round-trips (unchanged path)"
# its object key must be a bare uuid (striped keys carry a '#' chunk marker; a
# single-address RemoteBackend never emits one) on server A
if find .striping/sA/blocks -type f 2>/dev/null | grep -qv '#'; then ok "legacy object stored under a bare uuid key (no '#' stripe marker)"; else bad "single-address mount wrote a striped key"; fi

echo "===== 8) du across a striped store ====="
# The tree's data_refs carry the s<N> layout, so sizes only come back when
# every server is named: one address must refuse, the list must total exactly
# what the mounted tree reports as file sizes.
sync "$M" 2>/dev/null
exp=$(T sh -c "find $M -type f -printf '%s\\n'" | T awk '{s+=$1} END{print s+0}')
T fusermount -u .striping/mnt_legacy 2>/dev/null
got=$($Y du --view alice --data-servers "$A,$B" -b --total 2>/dev/null | awk '$2=="/"{print $1}')
chk "$got" "$exp" "du --data-servers a,b totals every striped object ($exp bytes)"
if $Y du --view alice --data-servers $A -b --total >.striping/du_one.log 2>&1; then
  bad "single-address du returned success instead of refusing"
else
  if grep -q 'written across 2 data-servers but --data-servers names 1' .striping/du_one.log && grep -q 'host1:port,host2:port' .striping/du_one.log; then
    ok "single-address du refuses and names the list to pass"
  else
    bad "single-address du refused without guidance: $(tail -2 .striping/du_one.log)"
  fi
fi
# a pre-striping (bare uuid) tree is still measurable from the one server it lives on
got_bob=$($Y du --view bob --data-servers $A -b --total 2>/dev/null | awk '$2=="/"{print $1}')
chk "$got_bob" "$(T printf 'legacy-single' | wc -c)" "single-address du reads a bare-uuid store"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL STRIPING CHECKS PASSED" || echo "SOME STRIPING CHECKS FAILED"
if [ "$fail" -ne 0 ]; then echo "--- sA.log ---"; tail -5 .striping/sA.log; echo "--- sB.log ---"; tail -5 .striping/sB.log; echo "--- mnt.log ---"; tail -8 .striping/mnt.log; fi
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
