#!/usr/bin/env bash
# End-to-end functional test for grovefs: multi-user view isolation + POSIX
# read/write/mkdir/rename/rm/GC. Now assertion-based (pass/fail counters + a
# real exit code) so the unified runner (run_all.sh) and CI can trust it, while
# still printing the human-readable steps.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove
rm -rf .test; mkdir -p .test/home .test/mnt_alice .test/mnt_bob
export GROVEFS_HOME=$PWD/.test/home

cat > .test/alice.manifest <<'EOF'
mkdir projects
mkdir projects/alpha
touch projects/alpha/notes.txt
touch readme.md
EOF
cat > .test/bob.manifest <<'EOF'
mkdir shared
mkdir private
touch shared/data.bin
EOF

$Y init --view alice --manifest .test/alice.manifest >/dev/null
$Y init --view bob   --manifest .test/bob.manifest   >/dev/null

$Y mount --view alice --mountpoint .test/mnt_alice > .test/alice.log 2>&1 &
AP=$!
$Y mount --view bob   --mountpoint .test/mnt_bob   > .test/bob.log   2>&1 &
BP=$!
sleep 1.2

cleanup(){ fusermount -u .test/mnt_alice 2>/dev/null; fusermount -u .test/mnt_bob 2>/dev/null; kill $AP $BP 2>/dev/null; }
trap cleanup EXIT

A=.test/mnt_alice
B=.test/mnt_bob
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }

echo "===== 1) per-user view trees ====="
chk "$(T ls $A | tr '\n' ' ')" "projects readme.md " "alice root listing"
chk "$(T ls $B | tr '\n' ' ')" "private shared "    "bob root listing"

echo "===== 2) isolation: bob must NOT see alice's files ====="
[ -e "$B/readme.md" ] && bad "leak: bob sees alice's readme.md" || ok "bob has no readme.md"
[ -e "$B/projects"  ] && bad "leak: bob sees alice's projects" || ok "bob has no projects dir"

echo "===== 3) write+read+append roundtrip in alice view ====="
T sh -c "printf 'hello grovefs' > $A/readme.md"
chk "$(T cat $A/readme.md)" "hello grovefs" "write then read readme.md"
T sh -c "printf ' MORE' >> $A/readme.md"
chk "$(T cat $A/readme.md)" "hello grovefs MORE" "O_APPEND round-trip"
chk "$(T stat -c '%s' $A/readme.md)" "18" "size after append = 18"

echo "===== 4) mkdir / touch / rename / rm ====="
T mkdir $A/newdir
T sh -c "printf 'abc' > $A/newdir/a.txt"
T mv $A/newdir/a.txt $A/newdir/b.txt
if T ls $A/newdir | grep -qx b.txt; then ok "rename a.txt->b.txt reflected"; else bad "rename not reflected"; fi
chk "$(T cat $A/newdir/b.txt)" "abc" "content survives rename"
T rm $A/newdir/b.txt
chk "$(T ls $A/newdir | wc -l)" "0" "rm leaves dir empty"

echo "===== 5) 4MB write + integrity (pwrite/pread path) ====="
head -c 4194304 /dev/urandom > .test/src4.bin
T cp .test/src4.bin $A/blob.bin
src=$(md5sum .test/src4.bin | awk '{print $1}')
dst=$(T md5sum $A/blob.bin | awk '{print $1}')
chk "$dst" "$src" "4MB copy integrity (md5 identical)"
chk "$(T stat -c '%s' $A/blob.bin)" "4194304" "4MB file size via mount"

echo "===== 6) GC: after rm, orphan block is reclaimed ====="
before=$(ls .test/home/blocks | wc -l)
T rm $A/blob.bin
sleep 0.3
after=$(ls .test/home/blocks | wc -l)
if [ "$after" -lt "$before" ]; then ok "GC reclaimed orphan block ($before -> $after)"; else bad "GC did not reclaim block ($before -> $after)"; fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL E2E CHECKS PASSED" || echo "SOME E2E CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
