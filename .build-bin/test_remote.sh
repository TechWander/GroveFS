#!/usr/bin/env bash
# End-to-end test of the REMOTE (no-NFS) data path:
#   client FUSE daemon  --(TCP framing)-->  data-server  -->  server-local disk
# Assertion-based pass/fail + real exit code so run_all.sh / CI can trust it.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
ADDR=127.0.0.1:15099

rm -rf .test; mkdir -p .test/rhome .test/rsrv .test/mnt_r
export GROVEFS_HOME=$PWD/.test/rhome      # meta (view DBs) live client-side
                                          # physical objects live server-side (.test/rsrv)

echo "=== start data-server (bg) ==="
$Y data-server --listen $ADDR --root $PWD/.test/rsrv > .test/srv.log 2>&1 &
SP=$!
sleep 0.8

echo "=== init alice view (empty tree) ==="
$Y init --view alice > /dev/null

echo "=== mount alice against REMOTE backend ==="
$Y mount --view alice --mountpoint .test/mnt_r --data $ADDR --foreground > .test/mnt_r.log 2>&1 &
MP=$!
sleep 1.0

cleanup(){ fusermount -u .test/mnt_r 2>/dev/null; kill $MP $SP 2>/dev/null; }
trap cleanup EXIT

M=.test/mnt_r
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

echo "===== 1) create tree + roundtrip over remote backend ====="
T mkdir $M/docs
T sh -c "printf 'remote grovefs works' > $M/docs/hello.txt"
chk "$(T cat $M/docs/hello.txt)" "remote grovefs works" "remote write/read roundtrip"

echo "===== 2) 4MB write + integrity via network (md5) ====="
head -c 4194304 /dev/urandom > /tmp/rblob
T cp /tmp/rblob $M/blob.bin
src=$(md5sum /tmp/rblob | awk '{print $1}')
dst=$(T md5sum $M/blob.bin | awk '{print $1}')
chk "$dst" "$src" "4MB over network md5 identical"
chk "$(T stat -c '%s' $M/blob.bin)" "4194304" "size via mount over network"

echo "===== 3) objects physically live on SERVER disk, not client ====="
chk "$(find .test/rhome/blocks -type f 2>/dev/null | wc -l)" "0" "client keeps no physical objects"
srv=$(find .test/rsrv/blocks -type f 2>/dev/null | wc -l)
if [ "$srv" -ge 1 ]; then ok "server holds object(s) ($srv)"; else bad "server has no objects"; fi

echo "===== 4) append + truncate roundtrip over network ====="
T sh -c "printf ' MORE' >> $M/docs/hello.txt"
chk "$(T cat $M/docs/hello.txt)" "remote grovefs works MORE" "append over network"
T truncate -s 6 $M/docs/hello.txt
chk "$(T stat -c '%s' $M/docs/hello.txt)" "6" "truncate over network"

echo "===== 5) GC over network: rm deletes server-side object ====="
before=$(find .test/rsrv/blocks -type f | wc -l)
T rm $M/blob.bin
sleep 0.3
after=$(find .test/rsrv/blocks -type f | wc -l)
if [ "$after" -lt "$before" ]; then ok "GC freed server object ($before -> $after)"; else bad "GC did not free ($before -> $after)"; fi

echo "===== 6) rename + rmdir over remote ====="
T sh -c "printf 'x' > $M/mv.txt"
T mv $M/mv.txt $M/docs/renamed.txt
if T ls $M/docs | grep -qx renamed.txt; then ok "rename reflected in remote listing"; else bad "rename not reflected"; fi
T rm $M/docs/renamed.txt
# `docs` still holds hello.txt (non-empty), so verify rmdir on a fresh empty dir.
T mkdir $M/emptydir
T rmdir $M/emptydir
if [ ! -d $M/emptydir ]; then ok "rmdir removed empty dir"; else bad "rmdir failed"; fi
# and confirm rmdir on a NON-empty dir is correctly refused
if T rmdir $M/docs 2>/dev/null; then bad "rmdir should refuse non-empty dir"; else ok "rmdir refuses non-empty dir"; fi

echo "===== 7) hardlink shared inode + symlink over remote data path ====="
T sh -c "printf 'hl-base\n' > $M/hlf.txt"
T ln $M/hlf.txt $M/hlf_alias.txt
i1=$(T stat -c '%i' $M/hlf.txt); i2=$(T stat -c '%i' $M/hlf_alias.txt)
chk "$i1" "$i2" "hardlink names share one inode"
chk "$(T stat -c '%h' $M/hlf.txt)" "2" "nlink is 2 after hardlink"
T sh -c "printf 'via-hardlink\n' >> $M/hlf_alias.txt"
chk "$(T grep -c via-hardlink $M/hlf.txt)" "1" "append via alias visible on orig name"
T sh -c "ln -s hlf.txt $M/rlink"
chk "$(T readlink $M/rlink)" "hlf.txt" "readlink over remote"

echo "===== 8) chunked parallel read path (P3): 8MB at bs=2M + unaligned slice ====="
# 2MiB dd requests exceed the 1MiB READ_CHUNK threshold, so RemoteBackend
# strips each read across parallel pooled connections; md5 must still match.
head -c 8388608 /dev/urandom > /tmp/rblob8
T cp /tmp/rblob8 $M/blob8.bin
sync
src8=$(md5sum /tmp/rblob8 | awk '{print $1}')
dst8=$(T md5sum $M/blob8.bin | awk '{print $1}')
chk "$dst8" "$src8" "8MB md5 via chunked whole-file read"
dst8b=$(T dd if=$M/blob8.bin bs=2M 2>/dev/null | md5sum | awk '{print $1}')
chk "$dst8b" "$src8" "8MB md5 via dd bs=2M (2MiB chunked reads)"
# Unaligned cross-slice read: offset 1MiB-4096, 3 full 1MiB chunks + ragged ends.
part=$(T dd if=$M/blob8.bin bs=4096 skip=255 count=768 2>/dev/null | md5sum | awk '{print $1}')
want=$(dd if=/tmp/rblob8 bs=4096 skip=255 count=768 2>/dev/null | md5sum | awk '{print $1}')
chk "$part" "$want" "unaligned 3MiB chunked read matches source"
T rm $M/blob8.bin

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL REMOTE CHECKS PASSED" || echo "SOME REMOTE CHECKS FAILED"
if [ "$fail" -ne 0 ]; then echo "--- srv.log tail ---"; tail -3 .test/srv.log; fi
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
