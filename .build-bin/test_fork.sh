#!/usr/bin/env bash
# e2e for FORK views: an INDEPENDENT metadata tree seeded from a physical base.
# Covers everything the feature promises, on both metadata paths:
#   - `view fork` creation, definition slot (`fork <base>`), guards
#     (duplicate name, name==base, governed base, missing/PG base);
#   - curated forks through the CLI entrance: `--empty` (root-only tree) then
#     `view graft` with a plan on stdin (rename, directory subtree, whole-plan
#     refusal on a collision, `--skip-existing`), plus the base-view `--src` guard;
#   - `view list` surfacing the fork beside the BARE base view it came from;
#   - `apikey list --view` filtering keys by the view they can open;
#   - LOCAL mount: structure writable (mkdir/rename/unlink/link/symlink/touch),
#     content refused EROFS (write/truncate), deletes touch only the fork;
#   - REMOTE mount via meta-server + data-server: the same split enforced
#     through the M_OPEN ack (byte [2]), so a fork stays content-read-only
#     even when the tree lives behind the governance server;
#   - shared bytes via the cross-view ledger: a fork delete never frees a blob
#     the base still names;
#   - `view rm` deletes the fork's own tree + rebuilds the ledger, base intact.
# Assertion-based; emits a SUMMARY line run_all reads.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
rm -rf .ftest
mkdir -p .ftest/home .ftest/mnt_base .ftest/mnt_lab .ftest/mnt_srv_lab .ftest/mnt_srv_base
export GROVEFS_HOME=$PWD/.ftest/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
rofs(){ grep -qE "Read-only file system|只读文件系统" "$1"; }

cat > .ftest/team.manifest <<'EOF'
mkdir docs
mkdir docs/2026
touch docs/2026/a.md
mkdir photos
mkdir photos/private
touch photos/private/x.jpg
mkdir data
mkdir data/raw
touch data/raw/big.bin
EOF
$Y init --view team --manifest .ftest/team.manifest > .ftest/init.log 2>&1 \
  && ok "seeded physical base view 'team'" || { bad "init failed: $(cat .ftest/init.log)"; exit 1; }

# Real content must exist BEFORE the fork: bytes are what a fork shares.
$Y mount --view team -m .ftest/mnt_base --strict --foreground > .ftest/base.log 2>&1 &
BP=$!
sleep 1.5
T sh -c "printf 'report-body' > .ftest/mnt_base/docs/2026/a.md" \
  && ok "base accepts content (it is the writable tree)" || bad "base write failed"
T sh -c "printf 'priv' > .ftest/mnt_base/photos/private/x.jpg"
T sh -c "printf 'rawdata123' > .ftest/mnt_base/data/raw/big.bin"
fusermount -u .ftest/mnt_base 2>/dev/null; kill $BP 2>/dev/null; wait $BP 2>/dev/null

echo "===== 1) view fork: independent tree, own definition slot ====="
$Y view --db-path .ftest/home fork --name lab --base team > .ftest/fork.log 2>&1 \
  && ok "view fork --name lab --base team" || bad "fork failed: $(cat .ftest/fork.log)"
grep -q "forked view 'lab' from base 'team'" .ftest/fork.log \
  && ok "fork reports entry + shared-object counts" || bad "fork output: $(cat .ftest/fork.log)"
[ -f .ftest/home/meta/lab.db ] && ok "fork owns meta/lab.db" || bad "fork tree missing"
[ "$(cat .ftest/home/meta/views/lab.layout 2>/dev/null)" = "fork team" ] \
  && ok "definition slot records 'fork team'" || bad "lab.layout wrong: $(cat .ftest/home/meta/views/lab.layout 2>/dev/null)"
$Y view --db-path .ftest/home list > .ftest/list.log 2>&1
grep -q "fork 'lab' of base 'team'" .ftest/list.log \
  && ok "view list explains the fork's mount semantics" || bad "fork line missing: $(cat .ftest/list.log)"
grep -q "base view 'team' -> own tree meta/team.db" .ftest/list.log \
  && ok "view list also names the BARE base view" || bad "bare base line missing: $(cat .ftest/list.log)"

echo "===== 1b) curated fork: --empty + graft, one transaction, no bytes ====="
$Y view --db-path .ftest/home fork --name cur --base team --empty > .ftest/cur.log 2>&1 \
  && ok "view fork --empty" || bad "empty fork failed: $(cat .ftest/cur.log)"
grep -q "as an EMPTY tree: root only" .ftest/cur.log \
  && ok "empty fork reports a root-only tree" || bad "empty fork output: $(cat .ftest/cur.log)"
[ "$(cat .ftest/home/meta/views/cur.layout 2>/dev/null)" = "fork team" ] \
  && ok "an empty fork records the same 'fork team' definition" || bad "cur.layout wrong"
$Y view --db-path .ftest/home export --view cur --out .ftest/cur0.jsonl > /dev/null 2>&1
grep -q '"nodes":1' .ftest/cur0.jsonl \
  && ok "empty fork exports exactly one row (the root)" || bad "empty fork not empty: $(head -1 .ftest/cur0.jsonl)"
cat > .ftest/graft.tsv <<'EOF'
docs/2026/a.md	reports/2026-annual.md
data/raw	tables/raw
EOF
$Y view --db-path .ftest/home graft --name cur --src team --map - < .ftest/graft.tsv > .ftest/graft.log 2>&1 \
  && ok "view graft --map - (plan piped on stdin)" || bad "graft failed: $(cat .ftest/graft.log)"
grep -q "^grafted .* from 'team' into 'cur'" .ftest/graft.log \
  && ok "graft reports entries, source and destination" || bad "graft output: $(cat .ftest/graft.log)"
$Y view --db-path .ftest/home export --view cur --out .ftest/cur1.jsonl --force > /dev/null 2>&1
grep -q '"name":"reports"' .ftest/cur1.jsonl \
  && ok "a missing destination directory is created" || bad "reports dir missing: $(head -1 .ftest/cur1.jsonl)"
grep -q '"name":"2026-annual.md"' .ftest/cur1.jsonl \
  && ok "the grafted entry landed under its NEW name" || bad "rename did not land"
grep -q '"name":"raw"' .ftest/cur1.jsonl \
  && ok "a directory source brought its subtree" || bad "subtree missing"
$Y view --db-path .ftest/home graft --name cur --src team --map .ftest/graft.tsv > .ftest/graft2.log 2>&1 \
  && bad "re-grafting the same plan was accepted" || ok "a colliding name refuses the WHOLE plan"
grep -q "already exists in the destination" .ftest/graft2.log \
  && ok "the refusal names the collision and the way out" || bad "collision text: $(cat .ftest/graft2.log)"
$Y view --db-path .ftest/home graft --name cur --src team --map .ftest/graft.tsv --skip-existing > .ftest/graft3.log 2>&1 \
  && ok "--skip-existing merges instead of refusing" || bad "skip-existing failed: $(cat .ftest/graft3.log)"
grep -qE "grafted 0 entries .*, [0-9]+ skipped" .ftest/graft3.log \
  && ok "nothing new landed; collisions counted as skipped" || bad "skip count: $(cat .ftest/graft3.log)"
$Y view --db-path .ftest/home graft --name team --map .ftest/graft.tsv > .ftest/graft4.log 2>&1 \
  && bad "a base view accepted a graft without --src" || ok "grafting into a base view without --src refused"
grep -q "is a base view — pass --src" .ftest/graft4.log \
  && ok "the refusal explains how to name the source" || bad "base-target text: $(cat .ftest/graft4.log)"

echo "===== 2) guards: duplicates, self-fork, governed/missing bases, alias target ====="
$Y view --db-path .ftest/home fork --name lab --base team > .ftest/g1.log 2>&1
[ $? -ne 0 ] && grep -q "already has a definition" .ftest/g1.log \
  && ok "re-forking an existing name refused" || bad "duplicate fork guard: $(cat .ftest/g1.log)"
$Y view --db-path .ftest/home fork --name team --base team > .ftest/g2.log 2>&1
[ $? -ne 0 ] && grep -q "must differ from its base" .ftest/g2.log \
  && ok "fork name == base refused" || bad "self-fork guard: $(cat .ftest/g2.log)"
echo "only /docs" > .ftest/docs.layout
$Y view --db-path .ftest/home define --name lens --base team --rules .ftest/docs.layout > .ftest/g3.log 2>&1
$Y view --db-path .ftest/home fork --name lab2 --base lens > .ftest/g4.log 2>&1
[ $? -ne 0 ] && grep -q "is governed by a definition" .ftest/g4.log \
  && ok "forking a governed (lens) name refused" || bad "governed-base guard: $(cat .ftest/g4.log)"
$Y view --db-path .ftest/home alias --name labpub --real lab > .ftest/g5.log 2>&1
[ $? -ne 0 ] && grep -q "is a fork" .ftest/g5.log \
  && ok "alias pointing at a fork refused (fork is a tree, not a public name)" || bad "alias-to-fork guard: $(cat .ftest/g5.log)"
$Y view --db-path .ftest/home fork --name lab3 --base ghost > .ftest/g6.log 2>&1
[ $? -ne 0 ] && grep -q "no physical base tree" .ftest/g6.log \
  && ok "fork of a view with no local tree refused (+ PG caveat)" || bad "missing-base guard: $(cat .ftest/g6.log)"
$Y view --db-path .ftest/home rm --name lens > /dev/null 2>&1

echo "===== 3) apikey list --view filters by the view a key can open ====="
$Y apikey --db-path .ftest/home create --label lab-agent --view lab > .ftest/ak1.log 2>&1
$Y apikey --db-path .ftest/home create --label base-agent --view team > .ftest/ak2.log 2>&1
$Y apikey --db-path .ftest/home list --view lab > .ftest/akl.log 2>&1
if grep -q "lab-agent" .ftest/akl.log && ! grep -q "base-agent" .ftest/akl.log; then
  ok "list --view lab shows only the fork-bound key"
else bad "view-filtered list wrong: $(cat .ftest/akl.log)"; fi
grep -q "1 bound key(s) for view 'lab' of 2 total" .ftest/akl.log \
  && ok "filtered list reports the bound/total counts" || bad "count line missing: $(cat .ftest/akl.log)"
$Y apikey --db-path .ftest/home list > .ftest/akall.log 2>&1
grep -q "lab-agent" .ftest/akall.log && grep -q "base-agent" .ftest/akall.log \
  && ok "unfiltered list still shows every key" || bad "unfiltered list wrong: $(cat .ftest/akall.log)"

echo "===== 4) LOCAL fork mount: structure writable, content EROFS ====="
$Y mount --view lab -m .ftest/mnt_lab --strict --foreground > .ftest/lab.log 2>&1 &
LP=$!
sleep 1.5
grep -q "structure writable, content read-only" .ftest/lab.log \
  && ok "local mount announces the fork contract" || bad "fork mount note missing: $(cat .ftest/lab.log)"
L=.ftest/mnt_lab
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
chk "$(T ls $L | tr '\n' ' ')" "data docs photos " "fork starts as a copy of the base listing"
chk "$(T cat $L/docs/2026/a.md)" "report-body" "fork reads the SHARED bytes"
# content writes -> EROFS
T sh -c "printf 'new-body' > $L/docs/2026/a.md" 2>.ftest/w1.err \
  && bad "fork accepted a content write" || { rofs .ftest/w1.err && ok "write refused with EROFS" || bad "wrong write errno: $(cat .ftest/w1.err)"; }
T sh -c ": > $L/docs/2026/a.md" 2>.ftest/w2.err \
  && bad "fork accepted a truncate" || { rofs .ftest/w2.err && ok "truncate refused with EROFS" || bad "wrong truncate errno: $(cat .ftest/w2.err)"; }
chk "$(T cat $L/docs/2026/a.md)" "report-body" "refused writes left the shared bytes intact"
# structure writes -> allowed
T touch $L/docs/2026/empty.md && ok "touch (empty create) allowed" || bad "fork refused an empty create"
T mkdir $L/docs/newdir && ok "mkdir allowed" || bad "fork refused mkdir"
T mv $L/docs/2026/a.md $L/docs/newdir/moved.md && ok "rename allowed (organize the tree)" || bad "fork refused rename"
T ln -s ../newdir $L/dirlink && ok "symlink allowed" || bad "fork refused symlink"
T ln $L/data/raw/big.bin $L/data/raw/big2.bin && ok "hard link allowed (same shared blob)" || bad "fork refused hard link"
chk "$(T ls $L/docs/2026 | tr '\n' ' ')" "empty.md " "rename moved the name inside the fork"
chk "$(T cat $L/docs/newdir/moved.md)" "report-body" "renamed name still reads the shared blob"
T rm $L/photos/private/x.jpg && ok "unlink allowed" || bad "fork refused unlink"
[ ! -e "$L/photos/private/x.jpg" ] && ok "fork no longer sees the deleted name" || bad "fork delete did not take effect"
T chmod 600 $L/docs/2026/empty.md && ok "chmod allowed (attribute update)" || bad "fork refused chmod"

echo "===== 5) the base is untouched: deletes/renames stay inside the fork ====="
fusermount -u .ftest/mnt_lab 2>/dev/null; kill $LP 2>/dev/null; wait $LP 2>/dev/null
$Y mount --view team -m .ftest/mnt_base --strict --foreground > .ftest/base2.log 2>&1 &
BP2=$!
sleep 1.5
B=.ftest/mnt_base
chk "$(T cat $B/docs/2026/a.md)" "report-body" "base name still reads a.md (fork rename was metadata-only)"
chk "$(T cat $B/photos/private/x.jpg)" "priv" "base name still reads x.jpg (fork delete did not free it)"
[ ! -e "$B/docs/2026/empty.md" ] && [ ! -e "$B/docs/newdir" ] \
  && ok "fork-created names never appear in the base" || bad "fork writes leaked into the base tree"

echo "===== 6) REMOTE fork mount: same split through the governance server ====="
META=127.0.0.1:15362; DATA=127.0.0.1:15363
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
$Y meta-server --db-path .ftest/home --listen $META > .ftest/meta_srv.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --root .ftest/home > .ftest/data_srv.log 2>&1 &
DSP=$!
wait_port $META && wait_port $DATA || bad "fork servers failed to start"
$Y mount --view lab -m .ftest/mnt_srv_lab --meta $META --data $DATA --foreground > .ftest/srv_lab.log 2>&1 &
SLP=$!
$Y mount --view team -m .ftest/mnt_srv_base --meta $META --data $DATA --foreground > .ftest/srv_base.log 2>&1 &
SBP=$!
sleep 2
SL=.ftest/mnt_srv_lab; SB=.ftest/mnt_srv_base
chk "$(T ls $SL | tr '\n' ' ')" "data dirlink docs photos " "remote fork listing matches the base plus the fork's own names"
chk "$(T cat $SL/docs/newdir/moved.md)" "report-body" "remote fork reads the shared blob"
# A NEW file: create is allowed but the content write is refused, so the file
# exists with zero bytes. `dd` surfaces the refusal (a shell builtin may not).
T dd if=/dev/zero of=$SL/docs/2026/remote.md bs=4 count=1 2>.ftest/w3.err
if [ $? -ne 0 ] && [ ! -s "$SL/docs/2026/remote.md" ]; then
  ok "remote fork refuses content writes (ack byte; zero bytes stored)"
else bad "remote fork accepted a content write"; fi
# Truncating an EXISTING shared file fails at open time with EROFS.
T sh -c "printf 'x' > $SL/docs/newdir/moved.md" 2>.ftest/w4.err \
  && bad "remote fork accepted a truncate" \
  || { rofs .ftest/w4.err && ok "remote fork refuses truncate with EROFS" || bad "remote truncate errno: $(cat .ftest/w4.err)"; }
chk "$(T cat $SL/docs/newdir/moved.md)" "report-body" "refused truncate left the shared bytes intact"
T mkdir $SL/docs/remote_dir && ok "remote fork allows mkdir" || bad "remote fork refused mkdir"
T sh -c "printf 'base-body' > $SB/docs/2026/frombase.md" \
  && ok "base stays writable on the same server" || bad "base write failed remotely"
T ls $SL/docs/2026 | grep -q frombase.md \
  && bad "post-fork base write leaked into the fork's own tree" \
  || ok "fork keeps its own snapshot (the base's later names are not injected)"
T rmdir $SL/docs/remote_dir && ok "remote fork allows rmdir (structure)" || bad "remote fork refused rmdir"
fusermount -u .ftest/mnt_srv_lab .ftest/mnt_srv_base 2>/dev/null
kill $SLP $SBP $MSP $DSP 2>/dev/null; wait $SLP $SBP $MSP $DSP 2>/dev/null
chk "$(T cat $B/docs/2026/a.md)" "report-body" "base still intact after the remote fork session"
[ -e "$B/docs/2026/frombase.md" ] && ok "remote base write is visible on the local base mount" || bad "remote base write missing"
fusermount -u .ftest/mnt_base 2>/dev/null; kill $BP2 2>/dev/null; wait $BP2 2>/dev/null

echo "===== 7) view rm deletes the fork tree only ====="
$Y view --db-path .ftest/home rm --name lab > .ftest/rm.log 2>&1 \
  && ok "view rm --name lab" || bad "fork rm failed: $(cat .ftest/rm.log)"
grep -q "removed fork view 'lab' of base 'team'" .ftest/rm.log \
  && ok "removal says the fork's own tree is gone" || bad "rm output: $(cat .ftest/rm.log)"
[ ! -e .ftest/home/meta/lab.db ] && ok "meta/lab.db deleted (with its wal/shm)" || bad "fork tree left behind"
[ -f .ftest/home/meta/team.db ] && ok "base tree untouched" || bad "base tree vanished"
$Y view --db-path .ftest/home list > .ftest/list2.log 2>&1
grep -q "fork 'lab'" .ftest/list2.log && bad "removed fork still listed" || ok "removed fork no longer listed"
grep -q "base view 'team' -> own tree meta/team.db" .ftest/list2.log \
  && ok "base view remains the only entry" || bad "base view missing after rm: $(cat .ftest/list2.log)"
# Base content survives the fork removal (the ledger rebuild must not free it).
$Y mount --view team -m .ftest/mnt_base --strict --foreground > .ftest/base3.log 2>&1 &
BP3=$!
sleep 1.5
chk "$(T cat .ftest/mnt_base/docs/2026/a.md)" "report-body" "base content readable after the fork is gone"
chk "$(T cat .ftest/mnt_base/photos/private/x.jpg)" "priv" "base blob the fork deleted still lives (ledger hold kept)"
$Y view --db-path .ftest/home rm --name lab > .ftest/rm2.log 2>&1
[ $? -ne 0 ] && ok "removing the fork twice is refused" || bad "second rm should fail"
fusermount -u .ftest/mnt_base 2>/dev/null; kill $BP3 2>/dev/null; wait $BP3 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL FORK CHECKS PASSED" || echo "SOME FORK CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
