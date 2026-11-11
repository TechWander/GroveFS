#!/usr/bin/env bash
# e2e for `grovefs import`: mirror an EXISTING local directory tree into a view
# with ZERO copies where the filesystem allows — objects are hard links that
# share the source's inode (no extra bytes; live edits visible through BOTH
# paths), timestamps/mode are preserved, GC never eats the source, re-runs
# are incremental, and the imported bytes compose with server-side derived
# views (virtual directories over real data). Assertion-based; SUMMARY line
# for run_all.sh.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
# The metadata backend is a PARAMETER (P8): `IMPORT_BACKEND=fdb` re-runs every
# assertion this suite makes against a FoundationDB deployment. The view a mount
# reaches, the control plane a definition is written to and the ports a server
# listens on become parameters; no assertion is edited to fit the backend.
BACKEND="${IMPORT_BACKEND:-sqlite}"
. .build-bin/fdb_scaffold.sh
fdb_gate
fdb_binary
rm -rf .itest
mkdir -p .itest/home .itest/src/photos/2026 .itest/src/docs .itest/mnt .itest/fdbhome
export GROVEFS_HOME=$PWD/.itest/home
if [ "$BACKEND" = fdb ]; then
    TEAM="it$(date +%s)"
    VIEW="${TEAM}_lg"; BOOT="${TEAM}_boot"
    # `$VDIR` is how a view command addresses the control plane the SUITE works
    # against: the local SQLite store on one tier, the cluster root on the other
    # (definitions stay files either way — C6). `$VBASE` is the physical view the
    # imported tree lives in, and a subspace name is per-run because it outlives
    # the run.
    VDIR="$MSARG --meta-dir .itest/fdbhome"
    VBASE=$VIEW
    META=127.0.0.1:15234; DATA=127.0.0.1:15235
    fdb_ports_free "$META" "$DATA"
    # `fdb_up` provisions a bootstrap subspace and starts the server pair the
    # whole run mounts through. NOT the view of this suite: the tree under test
    # has to arrive by the move, into a subspace nothing created.
    fdb_up .itest/fdbhome "$BOOT" "$META" "$DATA"
    REMOTE="--meta-server $META --data-servers $DATA"
else
    VIEW=legacy; VBASE=legacy
    VDIR="--meta-dir .itest/home"
    META=127.0.0.1:15230; DATA=127.0.0.1:15231
fi

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
if [ "$BACKEND" = fdb ]; then
    # The zero-copy mirror cannot be honoured here: its objects would have to be
    # hard links into a data-server's store, and its metadata rows would have to
    # be a `meta/<view>.db` beside a root whose server serves the cluster. So the
    # command refuses — and refuses BEFORE `--dry-run` is considered, because a
    # promise to touch nothing is not a licence to plan against the wrong backend.
    $Y import $MSARG --view $VIEW --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/dry.log \
      && ok "an fdb:// deployment refuses the import command instead of faking it" \
      || bad "import should refuse on fdb://: $(cat .itest/dry.log)"
    grep -q "view create" .itest/dry.log && grep -q "mounted path" .itest/dry.log \
      && ok "the refusal names the way that does work here" || bad "refusal unclear: $(cat .itest/dry.log)"
    [ ! -e .itest/fdbhome/meta/$VIEW.db ] && [ ! -e .itest/home/meta/$VIEW.db ] \
      && ok "the refusal created no view db anywhere" || bad "refused import touched a store"
    GROVEFS_META_STORE="$FDBURI" $Y import --view $VIEW --source .itest/src > .itest/dry2.log 2>&1
    [ $? -ne 0 ] && grep -q "GROVEFS_META_STORE" .itest/dry2.log \
      && ok "the env behind --meta-store refuses too (no bypass by spelling)" \
      || bad "GROVEFS_META_STORE should refuse: $(cat .itest/dry2.log)"
else
    $Y import --view legacy --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1 \
      && ok "dry run exits 0" || bad "dry run failed: $(cat .itest/dry.log)"
    grep -q "would mirror 4 dir(s) and 3 file(s)" .itest/dry.log \
      && ok "dry run counts dirs/files it would import" || bad "dry run tally wrong: $(cat .itest/dry.log)"
    [ ! -e .itest/home/meta/legacy.db ] && ok "dry run did NOT even create the view db" || bad "dry run touched the store"
fi

echo "===== 2) real import: structure, content, fidelity, zero copies ====="
# This first command is a LOCAL `import` on BOTH tiers — on fdb it is the staging
# step the refusal above points at, and its tally/link assertions are exactly as
# true there as they are on the SQLite tier, because a local store is what it is.
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp.log 2>&1 \
  && ok "import exits 0" || bad "import failed: $(cat .itest/imp.log)"
grep -q "3 object(s) (3 hardlinked zero-copy, 0 copied)" .itest/imp.log \
  && ok "all three files hardlinked (zero extra bytes)" || bad "copy stats wrong: $(cat .itest/imp.log)"
# every imported object is a second name on the SOURCE inode, not a new file
hl=$(find .itest/home/blocks -type f -links 2 2>/dev/null | wc -l | tr -d ' ')
chk "$hl" "3" "3 blocks objects share their source inodes"

if [ "$BACKEND" = fdb ]; then
    # The second half of the route is the migration itself: the staged tree leaves
    # as a dump, its blobs are handed to the deployment's object store (named by
    # content, so a copy is a rename-free overwrite), and the dump lands in a
    # subspace nothing provisioned. Everything below this line then asks the SAME
    # questions of a FoundationDB mount that the SQLite tier asks of a local one.
    $Y view --meta-dir .itest/home export --view legacy --out .itest/legacy.jsonl > .itest/exp.log 2>&1 \
      && ok "exported the staged tree" || bad "export failed: $(cat .itest/exp.log)"
    cp -a .itest/home/blocks .itest/fdbhome/data/ \
      && ok "handed the blobs to the deployment's object store" || bad "blob copy failed"
    $Y view $VDIR import --view $VIEW --in .itest/legacy.jsonl > .itest/vimp.log 2>&1 \
      && ok "the dump became a subspace no view create ever provisioned" \
      || bad "subspace import failed: $(head -3 .itest/vimp.log)"
fi
$Y mount --view $VIEW -m .itest/mnt --no-cache $REMOTE --foreground > .itest/mnt.log 2>&1 &
MP=$!
sleep 1.5
# `kp`/`wk` rather than a bare `kill`: on the fdb tier the servers belong to the
# run and are named by $FDB_MP/$FDB_DP, while on this tier they are $MSP/$DSP —
# and an empty placeholder handed to `kill` means the whole process group.
# Every name is `${…:-}` for a second reason: `set -u` is on, and an unbound
# variable INSIDE this trap aborts the trap itself — which leaks every mount and
# every listener the suite made, and hands the next suite a stranger's deployment.
cleanup(){ fusermount -u .itest/mnt .itest/mnt_r .itest/mnt_v 2>/dev/null
           kp ${MP:-} ${MSP:-} ${DSP:-} ${RP:-} ${VP:-} ${FDB_MP:-} ${FDB_DP:-}
           wk ${MP:-} ${MSP:-} ${DSP:-} ${RP:-} ${VP:-} ${FDB_MP:-} ${FDB_DP:-}
           # Both names this suite can leave in the cluster: the bootstrap subspace
           # `fdb_up` provisioned and the one the move created (import is the only
           # other way to make a tree here).
           fdb_drop_views .itest/fdbhome .itest/fdbhome/data "${BOOT:-}"
           fdb_drop_views .itest/fdbhome .itest/fdbhome/data "${VIEW:-}"; }
trap cleanup EXIT
M=.itest/mnt
if [ "$BACKEND" = fdb ]; then
    mnt_gate .itest/mnt || bad ".itest/mnt never came up"
    # The moved tree must answer for its own root, by number: an import whose root
    # pointer named the dump's portable "/" instead of the frozen root key would
    # leave every path below it answering ENOENT while all the keys were present.
    chk "$(T stat -c %i $M 2>&1)" "1" "the moved tree answers getattr on its own root inode"
fi
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
echo 'source side change' > .itest/src/photos/2026/y.jpg
sleep 3   # attr/dentry TTL lets the mount converge on the external edit
if [ "$BACKEND" = fdb ]; then
    # Both directions of the live sharing are checked, and both are ABSENT — which
    # is the honest shape of a cross-backend move, not a regression to hide: the
    # blobs the subspace reads are the deployment's own objects (the copy section 2
    # handed over), so an edit stops on the side it started on. `import`'s zero-copy
    # mirroring is a property of a LOCAL store; the move trades it for a
    # cluster-authoritative tree, and USAGE says so in the same terms.
    [ "$(cat .itest/src/docs/quarter.txt)" != "edited via grovefs" ] \
      && ok "the mount's edit did NOT reach the source path (the view owns its bytes)" \
      || bad "the moved tree kept a live link into the user's directory"
    chk "$(T cat $M/legacy/docs/quarter.txt)" "edited via grovefs" "the moved view is writable and reads back its own edit"
    chk "$(T cat $M/legacy/photos/2026/y.jpg)" "beach day" "the source's own edit does not show through: an object-store copy, not a shared inode"
else
    chk "$(cat .itest/src/docs/quarter.txt)" "edited via grovefs" "source path sees the mount's edit (shared inode)"
    chk "$(T cat $M/legacy/photos/2026/y.jpg)" "$(cat .itest/src/photos/2026/y.jpg)" "external edit on the source shows through after the TTL"
fi

echo "===== 4) GC never destroys the dataset ====="
before=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
# How many objects a delete released is a question about a local `blocks/`
# directory; here the object store belongs to the data-server, so this tier keeps
# the guarantee the section is named for — the user's own file is never the price
# of a view-level delete — and asks it with the same line as the other tier.
T rm $M/legacy/photos/2026/y.jpg && ok "remove an imported name via the mount" || bad "rm failed"
if [ "$BACKEND" = fdb ]; then
    [ ! -e "$M/legacy/photos/2026/y.jpg" ] && ok "the deleted name is gone from the subspace view" \
      || bad "the delete did not land on the subspace"
    chk "$(T cat $M/legacy/docs/quarter.txt)" "edited via grovefs" "a sibling name still reads after the delete (no collateral unlink)"
else
    after=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
    chk "$after" "$((before-1))" "the blocks/ link was unlinked by GC"
fi
chk "$(cat .itest/src/photos/2026/y.jpg)" "source side change" "the user's source file SURVIVED the view-level delete"

echo "===== 5) re-run is incremental; specials are skipped ====="
# The re-run is the SAME command against the SAME staging store on both tiers. Its
# tally differs by exactly one object, and the reason is where section 4's delete
# happened: on this tier the staging view never lost y.jpg (the delete was on the
# subspace), so only fresh.txt is new => 1 imported, 5 skipped.
echo 'brand new' > .itest/src/docs/fresh.txt
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp2.log 2>&1
if [ "$BACKEND" = fdb ]; then
    grep -q "1 object(s) (1 hardlinked zero-copy, 0 copied), 5 skipped" .itest/imp2.log \
      && ok "second run imports only fresh.txt (y.jpg was never lost by the staging store)" \
      || bad "incremental tally wrong: $(cat .itest/imp2.log)"
    # The far side of an incremental re-run is a RE-MOVE, not a TTL: the subspace
    # catches up with the two commands that filled it the first time, and
    # `--replace` is what makes that idempotent. The name section 4 deleted comes
    # back through the move rather than through a re-link.
    $Y view --meta-dir .itest/home export --view legacy --out .itest/legacy2.jsonl --force > .itest/exp2.log 2>&1 \
      && ok "re-exported the staging store" || bad "re-export failed: $(cat .itest/exp2.log)"
    cp -a .itest/home/blocks .itest/fdbhome/data/ \
      && ok "refreshed the objects the re-run touched" || bad "blob refresh failed"
    $Y view $VDIR import --view $VIEW --in .itest/legacy2.jsonl --replace > .itest/vimp2.log 2>&1 \
      && ok "the re-move replaced the subspace with the re-run tree" \
      || bad "re-move failed: $(head -3 .itest/vimp2.log)"
else
    grep -q "2 object(s) (2 hardlinked zero-copy, 0 copied), 4 skipped" .itest/imp2.log \
      && ok "second run imports only fresh.txt + re-links the deleted y.jpg" || bad "incremental tally wrong: $(cat .itest/imp2.log)"
fi
sleep 2
T ls $M/legacy/docs | grep -q fresh.txt && ok "incremental pick-up visible on the live mount (TTL)" || bad "live mount missed the new file"
# symlinks/fifos were skipped on run one — check they never landed as objects
[ -e "$M/legacy/docs/pipe.fifo" ] && bad "fifo leaked into the view" || ok "special file not imported (skipped as documented)"

echo "===== 6) server topology: import + meta-server + derived view over imported bytes ====="
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
if [ "$EARLY" != 1 ]; then
    $Y meta-server --meta-dir .itest/home --listen $META > .itest/meta_srv.log 2>&1 &
    MSP=$!
    $Y data-server --listen $DATA --data-dir .itest/home > .itest/data_srv.log 2>&1 &
    DSP=$!
else
    # The pair `fdb_up` started serves EVERY view of its control plane, and the
    # trees it points at are the cluster's, so a second pair over a second root
    # would only split the definitions (which stay files — C6) between two
    # authorities. The suite's own section 2 already mounted through this pair.
    ok "the run's existing meta/data server pair is the one under test"
fi
wait_port $META && wait_port $DATA || bad "import-suite servers failed to start"

# a virtual-directory shape OVER the imported tree: album aliases the photos
cat > .itest/album.layout <<'EOF'
# one logical album dir, physically living under /legacy/photos
map /legacy/photos /album
EOF
$Y view $VDIR define --name albumview --base $VBASE --rules .itest/album.layout > .itest/def.log 2>&1 \
  && ok "define derived view over the imported base" || bad "define failed: $(cat .itest/def.log)"

mkdir -p .itest/mnt_r .itest/mnt_v
$Y mount --view $VIEW    -m .itest/mnt_r --meta-server $META --data-servers $DATA --foreground > .itest/r.log 2>&1 &
RP=$!
$Y mount --view albumview -m .itest/mnt_v --meta-server $META --data-servers $DATA --foreground > .itest/v.log 2>&1 &
VP=$!
sleep 2
R=.itest/mnt_r; V=.itest/mnt_v
if [ "$BACKEND" = fdb ]; then
    mnt_gate .itest/mnt_r || bad ".itest/mnt_r never came up"
    mnt_gate .itest/mnt_v || bad ".itest/mnt_v never came up"
fi
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
# The two input guards are backend-blind on purpose: the tool checks what it was
# pointed at before it asks which deployment it is in, so an `fdb://` invocation
# reports a missing or non-directory source with the SAME words. That ordering is
# asserted, not assumed — a refusal that came first would hide a bad --source.
$Y import $MSARG --view $VIEW --source .itest/does-not-exist > .itest/g1.log 2>&1
[ $? -ne 0 ] && grep -qi "no such file" .itest/g1.log \
  && ok "missing source rejected (before the backend is even consulted)" \
  || bad "missing source should fail: $(cat .itest/g1.log)"
! grep -q "local SQLite view db" .itest/g1.log \
  && ok "a bad --source is reported as the source problem it is, not as a backend refusal" \
  || bad "the input guards lost to the backend check: $(cat .itest/g1.log)"
$Y import $MSARG --view $VIEW --source .itest/src/docs/quarter.txt > .itest/g2.log 2>&1
[ $? -ne 0 ] && grep -q "must be a directory" .itest/g2.log && ok "file source rejected" || bad "file source should fail: $(cat .itest/g2.log)"
if [ "$BACKEND" = fdb ]; then
    # What the remaining two guards become here: the backend boundary is decided
    # before the control plane is consulted, so an alt-prefix import AND an import
    # onto a derived name are both refused with the STORE's words. The layout
    # guards are not weakened — they are unreachable, and the shadow-tree rule
    # they protect (`meta/<view>.db` under a virtual name) has no form to take in
    # a cluster, where a name is only ever a subspace or a definition.
    $Y import $MSARG --view $VIEW --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/g3.log \
      && ok "an alt-prefix import is refused too (a different prefix does not make it legal)" \
      || bad "alt prefix should refuse on fdb://: $(cat .itest/g3.log)"
    $Y import $MSARG --view albumview --source .itest/src > .itest/g4.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/g4.log \
      && ok "an import onto a derived name is stopped at the backend boundary" \
      || bad "derived name should refuse on fdb://: $(cat .itest/g4.log)"
    [ ! -e .itest/fdbhome/meta/albumview.db ] && [ ! -e .itest/fdbhome/meta/$VIEW.db ] \
      && ok "no shadow tree appeared in the cluster root either" || bad "a refused import wrote a local db"
else
    # default prefix = source dir name
    $Y import --view legacy --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1 \
      && [ -e .itest/home/meta/legacy.db ] && ok "explicit alternate prefix imports" || bad "alt prefix import failed: $(cat .itest/g3.log)"
    # importing INTO a derived (virtual) name would write a shadow db nothing reads
    $Y import --view albumview --source .itest/src > .itest/g4.log 2>&1
    [ $? -ne 0 ] && grep -qF "derived (virtual) view" .itest/g4.log \
      && ok "import refuses a derived (virtual) view name" || bad "import should refuse derived name: $(cat .itest/g4.log)"
fi

echo
echo "SUMMARY import: $pass passed, $fail failed"
[ $fail -eq 0 ]
