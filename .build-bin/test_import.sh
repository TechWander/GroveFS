#!/usr/bin/env bash
# e2e for `grovefs import`: mirror an EXISTING local directory tree into a view
# with ZERO copies where the filesystem allows — objects are hard links that
# share the source's inode (no extra bytes; live edits visible through BOTH
# paths), timestamps/mode are preserved, GC never eats the source, re-runs
# are incremental, and the imported bytes compose with server-side derived
# views (virtual directories over real data). Assertion-based; SUMMARY line
# for run_all.sh.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
rm -rf .itest
mkdir -p .itest/home .itest/src/photos/2026 .itest/src/docs .itest/mnt
export GROVEFS_HOME=$PWD/.itest/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }

# The legacy dataset sits OUTSIDE the store, with a fixed old clock.
echo 'beach day'      > .itest/src/photos/2026/y.jpg
echo 'q3 report body' > .itest/src/docs/quarter.txt
touch -d '2021-05-06 07:08:09' .itest/src/docs/quarter.txt
mkdir -p .itest/src/docs/archive
echo 'old memo'       > .itest/src/docs/archive/memo.txt
ln -s ../docs        .itest/src/photos/alias      # symlink: skipped, never followed
mkfifo .itest/src/docs/pipe.fifo 2>/dev/null      # special file: skipped

echo "===== 1) --dry-run reports without touching anything ====="
$Y import --view legacy --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1 \
  && ok "dry run exits 0" || bad "dry run failed: $(cat .itest/dry.log)"
grep -q "would mirror 4 dir(s) and 3 file(s)" .itest/dry.log \
  && ok "dry run counts dirs/files it would import" || bad "dry run tally wrong: $(cat .itest/dry.log)"
[ ! -e .itest/home/meta/legacy.db ] && ok "dry run did NOT even create the view db" || bad "dry run touched the store"

echo "===== 2) real import: structure, content, fidelity, zero copies ====="
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp.log 2>&1 \
  && ok "import exits 0" || bad "import failed: $(cat .itest/imp.log)"
grep -q "3 object(s) (3 hardlinked zero-copy, 0 copied)" .itest/imp.log \
  && ok "all three files hardlinked (zero extra bytes)" || bad "copy stats wrong: $(cat .itest/imp.log)"
# every imported object is a second name on the SOURCE inode, not a new file
hl=$(find .itest/home/blocks -type f -links 2 2>/dev/null | wc -l | tr -d ' ')
chk "$hl" "3" "3 blocks objects share their source inodes"

$Y mount --view legacy -m .itest/mnt --strict --foreground > .itest/mnt.log 2>&1 &
MP=$!
sleep 1.5
cleanup(){ fusermount -u .itest/mnt .itest/mnt_r .itest/mnt_v 2>/dev/null; kill $MP $MSP $DSP $RP $VP 2>/dev/null; }
trap cleanup EXIT
M=.itest/mnt
chk "$(T ls $M | tr '\n' ' ')" "legacy " "imported tree appears under the prefix"
chk "$(T ls $M/legacy | tr '\n' ' ')" "docs photos " "directory structure mirrored"
chk "$(T ls $M/legacy/photos | tr '\n' ' ')" "2026 " "symlink was skipped, not imported"
chk "$(T cat $M/legacy/docs/quarter.txt)" "q3 report body" "imported file reads through the view"
m1=$(T stat -c %Y $M/legacy/docs/quarter.txt); m2=$(stat -c %Y .itest/src/docs/quarter.txt)
chk "$m1" "$m2" "mtime preserved from the source (not stamped import time)"
chk "$(T stat -c %a $M/legacy/docs/quarter.txt)" "$(stat -c %a .itest/src/docs/quarter.txt)" "mode preserved"

echo "===== 3) passthrough: one inode, both paths live ====="
T sh -c "printf 'edited via grovefs\n' > $M/legacy/docs/quarter.txt" \
  && ok "write through the mount onto an imported file" || bad "mount-side write failed"
chk "$(cat .itest/src/docs/quarter.txt)" "edited via grovefs" "source path sees the mount's edit (shared inode)"
echo 'source side change' > .itest/src/photos/2026/y.jpg
sleep 3   # attr/dentry TTL lets the mount converge on the external edit
chk "$(T cat $M/legacy/photos/2026/y.jpg)" "$(cat .itest/src/photos/2026/y.jpg)" "external edit on the source shows through after the TTL"

echo "===== 4) GC never destroys the dataset ====="
before=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
T rm $M/legacy/photos/2026/y.jpg && ok "remove an imported name via the mount" || bad "rm failed"
after=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
chk "$after" "$((before-1))" "the blocks/ link was unlinked by GC"
chk "$(cat .itest/src/photos/2026/y.jpg)" "source side change" "the user's source file SURVIVED the view-level delete"

echo "===== 5) re-run is incremental; specials are skipped ====="
# The re-run sees: quarter.txt + memo.txt EXIST (skip), fresh.txt is new,
# and y.jpg — whose view entry was deleted in section 4 — comes BACK as a
# fresh hard link onto the still-live source inode; alias/pipe.fifo stay
# skipped as unsupportable => 2 imported, 4 skipped.
echo 'brand new' > .itest/src/docs/fresh.txt
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp2.log 2>&1
grep -q "2 object(s) (2 hardlinked zero-copy, 0 copied), 4 skipped" .itest/imp2.log \
  && ok "second run imports only fresh.txt + re-links the deleted y.jpg" || bad "incremental tally wrong: $(cat .itest/imp2.log)"
sleep 2
T ls $M/legacy/docs | grep -q fresh.txt && ok "incremental pick-up visible on the live mount (TTL)" || bad "live mount missed the new file"
# symlinks/fifos were skipped on run one — check they never landed as objects
[ -e "$M/legacy/docs/pipe.fifo" ] && bad "fifo leaked into the view" || ok "special file not imported (skipped as documented)"

echo "===== 6) server topology: import + meta-server + derived view over imported bytes ====="
META=127.0.0.1:15230; DATA=127.0.0.1:15231
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
$Y meta-server --db-path .itest/home --listen $META > .itest/meta_srv.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --root .itest/home > .itest/data_srv.log 2>&1 &
DSP=$!
wait_port $META && wait_port $DATA || bad "import-suite servers failed to start"

# a virtual-directory shape OVER the imported tree: album aliases the photos
cat > .itest/album.layout <<'EOF'
# one logical album dir, physically living under /legacy/photos
map /legacy/photos /album
EOF
$Y view --db-path .itest/home define --name albumview --base legacy --rules .itest/album.layout > .itest/def.log 2>&1 \
  && ok "define derived view over the imported base" || bad "define failed: $(cat .itest/def.log)"

mkdir -p .itest/mnt_r .itest/mnt_v
$Y mount --view legacy   -m .itest/mnt_r --meta $META --data $DATA --foreground > .itest/r.log 2>&1 &
RP=$!
$Y mount --view albumview -m .itest/mnt_v --meta $META --data $DATA --foreground > .itest/v.log 2>&1 &
VP=$!
sleep 2
R=.itest/mnt_r; V=.itest/mnt_v
chk "$(T ls $R/legacy | tr '\n' ' ')" "docs photos " "remote mount sees the imported tree"
chk "$(T ls $V | tr '\n' ' ')" "album legacy " "derived view synthesizes the virtual /album dir"
chk "$(T cat $V/album/2026/y.jpg)" "source side change" "virtual dir reads the imported (re-linked) bytes"
# the virtual view is READ-ONLY: clients consume the shape; writes belong to
# the original (base) view — and never touch the source tree either way
grep -q "read-only" .itest/v.log \
  && ok "virtual view mounted read-only (server handshake flag)" || bad "virtual mount should be RO: $(cat .itest/v.log)"
T sh -c "printf 'via lens' > $V/album/notes.txt" 2>/dev/null \
  && bad "write through the virtual dir should be refused" || ok "write through the virtual dir refused (EROFS)"
[ ! -e .itest/src/photos/notes.txt ] && ok "source tree untouched by the refused write" || bad "source leaked a new file"
T sh -c "printf 'via base' > $R/legacy/photos/note2.txt" && ok "base (original) view still takes writes" || bad "base write failed"
chk "$(T cat $V/album/note2.txt)" "via base" "base write shows through the virtual dir"

echo "===== 7) guards ====="
$Y import --view legacy --source .itest/does-not-exist > .itest/g1.log 2>&1
[ $? -ne 0 ] && grep -qi "no such file" .itest/g1.log && ok "missing source rejected" || bad "missing source should fail: $(cat .itest/g1.log)"
$Y import --view legacy --source .itest/src/docs/quarter.txt > .itest/g2.log 2>&1
[ $? -ne 0 ] && grep -q "must be a directory" .itest/g2.log && ok "file source rejected" || bad "file source should fail: $(cat .itest/g2.log)"
# default prefix = source dir name
$Y import --view legacy --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1 \
  && [ -e .itest/home/meta/legacy.db ] && ok "explicit alternate prefix imports" || bad "alt prefix import failed: $(cat .itest/g3.log)"
# importing INTO a derived (virtual) name would write a shadow db nothing reads
$Y import --view albumview --source .itest/src > .itest/g4.log 2>&1
[ $? -ne 0 ] && grep -qF "derived (virtual) view" .itest/g4.log \
  && ok "import refuses a derived (virtual) view name" || bad "import should refuse derived name: $(cat .itest/g4.log)"

echo
echo "SUMMARY import: $pass passed, $fail failed"
[ $fail -eq 0 ]
