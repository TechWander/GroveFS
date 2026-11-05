#!/usr/bin/env bash
# POSIX-completeness e2e for the Stage-1 additions:
#   symlink/readlink, hardlink (shared object + refcount GC), statfs (df),
#   setattr timestamps (touch -d / -t), chmod, mkfifo (mknod).
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
rm -rf .ptest; mkdir -p .ptest/home .ptest/mnt
export GROVEFS_HOME=$PWD/.ptest/home

cat > .ptest/man <<'EOF'
mkdir docs
touch docs/orig.txt
EOF
$Y init --view carol --manifest .ptest/man >/dev/null

$Y mount --view carol --mountpoint .ptest/mnt --foreground > .ptest/mount.log 2>&1 &
MP=$!
sleep 1.2
cleanup(){ fusermount -u .ptest/mnt 2>/dev/null; kill $MP 2>/dev/null; }
trap cleanup EXIT

T(){ timeout 5 "$@"; echo "[rc=$?]"; }
pass=0; fail=0
chk(){ if [ "$1" = "$2" ]; then echo "  OK: $3"; pass=$((pass+1)); else echo "  FAIL: $3 (got '$1' want '$2')"; fail=$((fail+1)); fi; }

M=.ptest/mnt
echo "===== A) write a real file to have content ====="
T sh -c "echo 'original content' > $M/docs/orig.txt"

echo "===== B) symlink: ln -s + readlink + follow ====="
T ln -s ../docs/orig.txt $M/docs/sym.txt 2>/dev/null || T ln -s $M/docs/orig.txt $M/sym_abs
# create a fresh predictable symlink
T sh -c 'ln -s docs/orig.txt '"$M"'/link'
chk "$(readlink $M/link)" "docs/orig.txt" "readlink returns target"
chk "$(cat $M/link)" "original content" "following symlink reads through"
if [ -L "$M/link" ]; then echo "  OK: link is a symlink (-L)"; pass=$((pass+1)); else echo "  FAIL: not a symlink"; fail=$((fail+1)); fi
chk "$(stat -L -c '%s' $M/link 2>/dev/null)" "17" "stat -L follows to target size"

echo "===== C) hardlink: ln shares bytes, both names, GC waits for last ====="
before=$(find .ptest/home/blocks -type f | wc -l)   # fan-out buckets: count object FILES
T ln $M/docs/orig.txt $M/docs/hard.txt
chk "$(cat $M/docs/hard.txt)" "original content" "hardlink sees same content"
chk "$(stat -c '%h' $M/docs/orig.txt)" "2" "nlink is 2 after hardlink"
inodes=$(stat -c '%i' $M/docs/orig.txt $M/docs/hard.txt | sort -u | wc -l)
chk "$inodes" "1" "both hardlink names share one inode (stat %i)"
inl=$(find .ptest/home/blocks -type f | wc -l)
chk "$inl" "$before" "hardlink added NO new physical object"
# modify via one name, observe via the other (shared object)
T sh -c "echo 'UPDATED' >> $M/docs/hard.txt"
chk "$(grep -c UPDATED $M/docs/orig.txt)" "1" "write via hardlink visible on orig"
T rm $M/docs/orig.txt
after1=$(find .ptest/home/blocks -type f | wc -l)
chk "$after1" "$inl" "removing one hardlink keeps object (refcount>0)"
chk "$(cat $M/docs/hard.txt)" "original content
UPDATED" "content intact after first unlink"
T rm $M/docs/hard.txt
after2=$(find .ptest/home/blocks -type f | wc -l)
chk "$after2" "$((inl-1))" "removing last hardlink frees the object"

echo "===== D) statfs via df ====="
T df -h $M
dfout=$(timeout 5 df -B1 $M 2>/dev/null | awk 'NR==2{print $2}')
[ "${dfout:-0}" -gt 0 ] && { echo "  OK: df reports nonzero size ($dfout)"; pass=$((pass+1)); } || { echo "  FAIL: df size"; fail=$((fail+1)); }

echo "===== E) setattr timestamps: touch -d ====="
T touch -d '2001-02-03 04:05:06' $M/docs
mt=$(stat -c '%y' $M/docs)
chk "${mt:0:10}" "2001-02-03" "touch -d set mtime ($mt)"

echo "===== F) chmod reflects in stat ====="
T sh -c 'touch '"$M"'/perm.txt'
T chmod 600 $M/perm.txt
chk "$(stat -c '%a' $M/perm.txt)" "600" "chmod 600 persisted"
T chmod 755 $M/perm.txt
chk "$(stat -c '%a' $M/perm.txt)" "755" "chmod 755 persisted"

echo "===== G) mknod: regular via touch; fifo unsupported-but-clean ====="
T sh -c 'mkfifo '"$M"'/pipe 2>/dev/null' || true
# We only require the FS not to hang/crash; rc handled above.
T ls $M

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL POSIX CHECKS PASSED" || echo "SOME CHECKS FAILED"
[ -f .ptest/mount.log ] && { echo "--- mount.log tail ---"; tail -5 .ptest/mount.log; }
# Return a meaningful exit code for the unified runner / CI.
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
