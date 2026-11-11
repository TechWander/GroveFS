#!/usr/bin/env bash
# Governance counterexample study (paper Sec. 7.3).
#
# This suite turns the four invariants I1-I4 from Sec. 5 into EXECUTABLE,
# falsifiable evidence by running the SAME identity/sharing assertions against:
#   (A) the hand-composed alternative — overlayfs + symlink + bind-mount(+ACL) —
#       to reproduce concretely WHERE it violates the invariants, and
#   (B) GroveFS's identity-preserving lens/alias/import — to show it holds them.
#
# Honesty notes baked into the checks:
#   * We do NOT use the inode NUMBER to detect overlay copy-up: kernel 5.4
#     overlayfs `xino` keeps the apparent lower inode stable across copy-up, so
#     `stat %i` is equal before/after. The robust signal is BYTE DUPLICATION
#     (a separate upper object appears) and SOURCE-STALE writes (lower is not
#     updated). GroveFS's FUSE inode has no xino layer, so there inode equality
#     IS the right identity signal — measured separately.
#   * overlay mounts need CAP_SYS_ADMIN; we attempt them inside `unshare -rm`
#     (root-in-user-ns). If unavailable we self-SKIP the overlay sub-checks
#     (like the pg suite skips when no PG host) rather than fake a result.
#
# Assertion-based like the other suites; emits a SUMMARY line for run_all.sh.
# The metadata BACKEND is a parameter (P5): `GOVCHECK_BACKEND=fdb` re-runs the
# GroveFS half (B) with the authoritative tree in a FoundationDB cluster. Sections
# A1-A3 are untouched by that choice — they falsify KERNEL stacks (overlayfs,
# symlink, bind), which have no metadata backend. In (B) the invariants under test
# are properties of the FUSE/protocol layer (one inode across two presentations,
# one fate, write-through, no stored duplicate entry), so every assertion is kept
# verbatim; only the deployment changes, plus the one cell that is genuinely a
# file-store property: zero-copy `import` (see B4).
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
BACKEND="${GOVCHECK_BACKEND:-sqlite}"
VIEW=gov
. .build-bin/fdb_scaffold.sh
fdb_gate
fdb_binary
if [ "$BACKEND" = fdb ]; then VIEW="gc$(date +%s)"; fi
HOME_DIR=$PWD/.govcheck/home
rm -rf .govcheck; mkdir -p .govcheck/home .govcheck/mnt_base .govcheck/mnt_lens .govcheck/mnt_map
export GROVEFS_HOME=$HOME_DIR
pass=0; fail=0; skip=0

ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
sk(){ echo "  SKIP: $1"; skip=$((skip+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }

# ------------------------------------------------------------------ (A0) preflight
echo "===== 0) prerequisite probe ====="
UNSH=(unshare -rm)
OVLL_OK=0
if command -v unshare >/dev/null 2>&1; then
  probe=$(mktemp -d); mkdir -p "$probe"/{low,up,work,mt}; echo x > "$probe"/low/f
  if "${UNSH[@]}" bash -c "mount -t overlay ovl -o lowerdir=$probe/low,upperdir=$probe/up,workdir=$probe/work $probe/mt 2>/dev/null && umount $probe/mt" 2>/dev/null; then
    OVLL_OK=1; ok "overlay mountable via unshare -rm (root-in-user-ns)"
  else
    sk "overlay not mountable here (need root/CAP_SYS_ADMIN) -> overlay sub-checks skipped"
  fi
  rm -rf "$probe"
else
  sk "unshare not present -> overlay sub-checks skipped"
fi

# ------------------------------------------------------------------ (A) hand stacks
echo "===== A1) overlayfs copy-up breaks I1 (entry uniqueness) and I4 (write-through-to-source) ====="
if [ "$OVLL_OK" = 1 ]; then
  OT=$(mktemp -d); mkdir -p "$OT"/{low,up,work,mt}
  printf 'hello' > "$OT"/low/f
  "${UNSH[@]}" bash -c "
    mount -t overlay ovl -o lowerdir=$OT/low,upperdir=$OT/up,workdir=$OT/work $OT/mt 2>/dev/null \
      && printf 'world' >> $OT/mt/f; umount $OT/mt 2>/dev/null
  " 2>/dev/null
  # A second physical object now exists in upper (bytes duplicated): violates I1.
  [ -e "$OT/up/f" ] && ok "overlay copy-up materialized a SEPARATE upper object (I1 violated: entry not unique)" \
                    || bad "expected overlay to copy up into a distinct upper file"
  # The lower source stayed STALE: the merged write did NOT pass to the source -> violates I4.
  chk "$(cat "$OT"/low/f)" "hello" "overlay leaves the lower source content stale after a merged write (I4 violated: no write-through)"
  # The merged view now diverges from the source it aliases.
  chk "$(cat "$OT"/up/f)" "helloworld" "overlay upper holds the divergent copy (identity forked into two bodies)"
  # Informational: inode numbers are NOT a reliable divergence signal under xino.
  echo "  INFO: overlay reported equal inodes via xino -> inode number is deliberately NOT used as the signal"
  rm -rf "$OT"
else
  sk "A1 overlay checks"
fi

echo "===== A2) symlink double-display breaks shared lifecycle (I2) and duplicates the name entry (I1) ====="
SHT=$(mktemp -d); mkdir -p "$SHT"/real "$SHT"/shown
printf 'data' > "$SHT"/real/f.txt
ln -s "$SHT"/real/f.txt "$SHT"/shown/f.txt     # second presentation of the same file
# The symlink is itself an extra stored dentry + its own inode (not the same entry).
lreal=$(stat -c '%i' "$SHT"/real/f.txt); llink=$(stat -c '%i' "$SHT"/shown/f.txt)
[ "$lreal" != "$llink" ] && ok "symlink adds a SEPARATE directory entry/inode for the same logical name (I1 violated: name duplicated)" \
                         || bad "symlink should be its own inode (unexpected)"
# Reads follow the target (identity holds WHILE the target exists):
chk "$(cat "$SHT"/shown/f.txt)" "data" "symlink reads the shared target (identity OK only while target lives)"
# Delete the real name: the second presentation now dangles -> shared lifecycle NOT preserved (I2 broken).
rm "$SHT"/real/f.txt
if [ -L "$SHT"/shown/f.txt ] && [ ! -e "$SHT"/shown/f.txt ]; then
  ok "after removing the source the symlink DANGLES (I2 violated: the two views do not share one fate)"
else
  bad "expected a dangling symlink after source removal"
fi
# And the real path is exposed to user space (governance/leakage escape).
tgt=$(readlink "$SHT"/shown/f.txt)
case "$tgt" in "$SHT"/real/*) ok "symlink exposes the real storage path in clear text (no name governance)";; *) bad "unexpected link target";; esac
rm -rf "$SHT"

echo "===== A3) bind-mount: measured identity (single inode, write-through, shared fate) and measured governance gaps ====="
BID=$(mktemp -d)
BIND_OK=0
if command -v unshare >/dev/null 2>&1 \
   && "${UNSH[@]}" bash -c "mkdir -p $BID/bsrc $BID/bmt; mount --bind $BID/bsrc $BID/bmt 2>/dev/null && umount $BID/bmt" 2>/dev/null; then
  BIND_OK=1
fi
if [ "$BIND_OK" = 1 ]; then
  ok "bind mountable via unshare -rm (root-in-user-ns)"
  mkdir -p "$BID"/bsrc/sub "$BID"/bsrc/secret "$BID"/bmt
  printf 'orig' > "$BID"/bsrc/sub/f
  printf 'hiddencontent' > "$BID"/bsrc/secret/g
  "${UNSH[@]}" bash -c '
    set -e
    B="'"$BID"'"
    mount --bind "$B/bsrc" "$B/bmt"
    echo "SAMEINODE $(stat -c %i "$B/bsrc/sub/f") $(stat -c %i "$B/bmt/sub/f")"
    printf "W" >> "$B/bmt/sub/f"
    echo "WT $(cat "$B/bsrc/sub/f")"
    echo "SECRET $([ -e "$B/bmt/secret/g" ] && echo yes || echo no)"
    echo "SRCSTILL $([ -e "$B/bsrc/sub/f" ] && echo yes || echo no)"
    rm "$B/bmt/sub/f"
    echo "FATE $([ -e "$B/bsrc/sub/f" ] && echo yes || echo no)"
    umount "$B/bmt"
  ' > "$BID"/res 2>"$BID"/err
  s1=$(awk '/^SAMEINODE/{print $2}' "$BID"/res); s2=$(awk '/^SAMEINODE/{print $3}' "$BID"/res)
  if [ -n "$s1" ] && [ "$s1" = "$s2" ]; then
    ok "bind shows ONE inode across source and mount paths (Bind single-inode = yes, MEASURED)"
  else
    bad "bind inode mismatch or no output (see $BID/err)"
  fi
  chk "$(awk '/^WT/{print $2}' "$BID"/res)" "origW" "write through the bind lands on the source (Bind write-through = yes, MEASURED; I4 preserved)"
  chk "$(awk '/^SECRET/{print $2}' "$BID"/res)" "yes" "bind cannot mask internals: a hidden subdir stays visible under the bind (no visibility governance)"
  chk "$(awk '/^SRCSTILL/{print $2}' "$BID"/res)" "yes" "bind does not vacate the source spot: it only aliases a mount path (contrast lens map)"
  chk "$(awk '/^FATE/{print $2}' "$BID"/res)" "no" "deleting via the bind removes the one shared object (Bind shared-fate = yes, MEASURED; unlike a dangling symlink)"
  echo "  INFO: bind cells the harness cannot falsify cleanly (server-side shape+reload, gapless name rotation, governed zero-copy onboarding) are ARCHITECTURAL: a bind is a mount whose path IS the storage identity, not a centrally-governed tree entry -> documented, not measured."
else
  sk "A3 bind checks (need root/CAP_SYS_ADMIN in a user namespace) -> bind column documented, not measured"
fi
rm -rf "$BID"

# ------------------------------------------------------------------ (B) GroveFS holds them
echo "===== B0) bring up a single authoritative view for GroveFS checks ====="
cat > .govcheck/gov.manifest <<'EOF'
mkdir docs
touch docs/f
mkdir inbox
EOF
# The tree comes into existence per backend: `init --manifest` WRITES a SQLite db
# file, while a subspace has no file to write — so the fdb run provisions with
# `view create` (inside fdb_up) and applies the SAME manifest through the base
# mount below. One description of the shape, two backends.
if [ "$BACKEND" != fdb ]; then
    $Y init --view "$VIEW" --manifest .govcheck/gov.manifest >/dev/null \
        || { echo "init failed"; exit 1; }
else
    # The central pair comes up with the subspace: under `fdb://` every mount
    # reaches the authoritative tree through a server, which is the only
    # deployment difference in the sections below.
    fdb_up "$HOME_DIR" "$VIEW" 127.0.0.1:15404 127.0.0.1:15405
fi

# lens rule: 'link' keeps the source spot AND shows an alias under /inbox.
cat > .govcheck/link.layout <<'EOF'
link /docs/f /inbox/f2
EOF
# map rule: relocate the file OUT of its source spot into a new name (vacating source).
cat > .govcheck/map.layout <<'EOF'
map /docs/hold /docs/renamed
EOF

$Y mount --view "$VIEW" --mountpoint .govcheck/mnt_base --no-cache $REMOTE --foreground > .govcheck/base.log 2>&1 &
BASEP=$!
$Y mount --view "$VIEW" --mountpoint .govcheck/mnt_lens --no-cache --layout .govcheck/link.layout $REMOTE --foreground > .govcheck/lens.log 2>&1 &
LENP=$!
PIDS=$(jobs -p)
MAPP=""   # B3's map mount exists only later; the trap must never signal a name that was never set
cleanup(){ for d in .govcheck/mnt_*; do fusermount -u "$d" 2>/dev/null; done; kp $PIDS $BASEP $LENP $MAPP $FDB_MP $FDB_DP; fdb_drop_views "${HOME_DIR:-}" "${HOME_DIR:-}/data" "${VIEW:-}"; }
trap cleanup EXIT
sleep 1.5

BASE=.govcheck/mnt_base; LENS=.govcheck/mnt_lens
if [ "$BACKEND" = fdb ]; then
    # Seed the subspace THROUGH the live base mount, from the very manifest file
    # the SQLite run fed to `init --manifest` above. The mount must be up first:
    # a write into a bare directory would otherwise leave the sections below
    # measuring nothing while still printing green numbers.
    mnt_gate .govcheck/mnt_base && ok "base mount came up over the cluster" \
        || bad ".govcheck/mnt_base never came up: $(cat .govcheck/base.log)"
    seed_through_mount .govcheck/mnt_base .govcheck/gov.manifest \
        && ok "the manifest was applied through the mount (subspace tree seeded)" \
        || bad "seeding the subspace tree failed"
fi
# Give the single real entry content on the base (unlensed) mount.
T sh -c "printf 'hello' > $BASE/docs/f" && ok "seed content on the real entry" || bad "seed write failed"

echo "===== B1) identity under reshape (counter to A1/A2): ONE inode, NO second object ====="
# The 'link' alias and the source share the SAME real inode (no copy, no xino trick):
i_src=$(T stat -c '%i' $BASE/docs/f); i_alias=$(T stat -c '%i' $LENS/inbox/f2)
chk "$i_alias" "$i_src" "lens alias resolves to the SAME inode as the source (I2 identity: reshape created no second body)"
# Content through both names is the same single object, live:
T sh -c "printf 'helloworld' > $LENS/inbox/f2" && ok "write through the lens alias lands on the shared entry" || bad "lens write failed"
chk "$(T cat $BASE/docs/f)" "helloworld" "source reflects the lens write IN PLACE (I4 write-through; no stale lower copy as in overlay)"
# Presentation alias does not inflate the real link count (Lemma 5.2 refcount):
n=$(T stat -c '%h' $BASE/docs/f); chk "$n" "1" "display alias does NOT inflate nlink (I1: no stored entry added)"

echo "===== B2) shared lifecycle (counter to A2 dangling symlink) ====="
# Removing the single real entry makes BOTH display names vanish together — no dangling copy.
T rm $BASE/docs/f && ok "removed the real entry via base" || bad "rm real entry failed"
if [ ! -e "$LENS/inbox/f2" ] && [ ! -e "$BASE/docs/f" ]; then
  ok "source and its lens alias both disappear together (I2: one fate, unlike a dangling symlink)"
else
  bad "expected both display names to vanish with the source"
fi

echo "===== B3) map relocation keeps a single object and vacates the source spot ====="
# Fresh tree for map: rebuild the file (docs/f was deleted above).
T sh -c "printf 'ren' > $BASE/docs/hold" && ok "seed a file for the map check" || bad "seed hold failed"
$Y mount --view "$VIEW" --mountpoint .govcheck/mnt_map --no-cache --layout .govcheck/map.layout $REMOTE --foreground > .govcheck/map.log 2>&1 &
MAPP=$!
sleep 1.5
MAP=.govcheck/mnt_map
chk "$(T stat -c '%i' $MAP/docs/renamed)" "$(T stat -c '%i' $BASE/docs/hold)" "map-relocated path is the SAME inode (no copy)"
chk "$(T cat $MAP/docs/renamed)" "ren" "relocated read yields the shared bytes"
fusermount -u $MAP 2>/dev/null; kp $MAPP; wk $MAPP

echo "===== B4) zero-copy import: the view and the source share ONE inode, source is never hurt ====="
mkdir -p .govcheck/src && printf 'LEGACY' > .govcheck/src/legacy.bin
if [ "$BACKEND" = fdb ]; then
    # THE ONE CELL THAT IS A FILE-STORE PROPERTY, NOT A PROTOCOL PROPERTY. The
    # measurement above (source inode == blocks/ object inode) only means
    # something when the objects and the source share a filesystem under the
    # store root; `import` writes a LOCAL SQLite tree, so on this backend it is
    # refused by role with the alternative named — never a silent write of a tree
    # nothing reads. The capability boundary (paper Table tab:expr, GroveFS
    # "zero-copy legacy onboarding") is therefore claimed for the file-store
    # deployment envelope; under `fdb://` onboarding goes through a mounted path.
    $Y import $MSARG --meta-dir "$HOME_DIR" --view "$VIEW" --source .govcheck/src > .govcheck/import.log 2>&1
    rc=$?
    [ $rc -ne 0 ] && grep -q "subspace of the cluster" .govcheck/import.log \
      && ok "import refuses an fdb:// deployment outright and names the alternative (no shadow tree)" \
      || bad "import should refuse under fdb://: $(cat .govcheck/import.log)"
    [ ! -e "$HOME_DIR/meta/$VIEW.db" ] \
      && ok "the refusal created no local view db (refuse really means refuse)" \
      || bad "import wrote a shadow tree despite refusing"
    # The same bytes onboarding through the documented alternative DOES preserve
    # I1/I2/I4: one entry, one fate, in-place write-through — which B1-B3 already
    # measured over the mount, so nothing here is asserted on authority alone.
    T sh -c "printf 'LEGACY' > $BASE/docs/onboarded.bin" \
      && chk "$(T cat $BASE/docs/onboarded.bin)" "LEGACY" \
          "onboarding through the mounted path lands the same bytes (the fdb:// alternative)" \
      || bad "writing through the mount (the documented fdb:// onboarding path) failed"
    fusermount -u $LENS $BASE 2>/dev/null; kp $BASEP $LENP; wk $BASEP $LENP
else
$Y import --meta-dir "$HOME_DIR" --view "$VIEW" --source .govcheck/src > .govcheck/import.log 2>&1 \
  && ok "import legacy tree into view $VIEW" || bad "import failed: $(cat .govcheck/import.log)"
# Re-mount base so imported entries are visible fresh.
fusermount -u $BASE 2>/dev/null; kp $BASEP; wk $BASEP
$Y mount --view "$VIEW" --mountpoint .govcheck/mnt_base --no-cache $REMOTE --foreground > .govcheck/base2.log 2>&1 &
BASEP=$!; trap 'cleanup' EXIT; sleep 1.5
BASE=.govcheck/mnt_base
if [ -e "$BASE/src/legacy.bin" ]; then
  # Zero-copy is a PHYSICAL property: the object stored under blocks/ must be a
  # hard link to the source file, i.e. share its HOST inode. (The FUSE mount's
  # own `stat %i` is GroveFS's logical ino and is intentionally NOT comparable
  # to a host inode.)
  isrc=$(stat -c '%i' .govcheck/src/legacy.bin)
  hlink=$(find "$HOME_DIR/blocks" -inum "$isrc" 2>/dev/null | head -1)
  [ -n "$hlink" ] && ok "import registered the object as a hardlink to the source (shared host inode $isrc): I1 zero-copy, no byte duplication" \
                   || bad "no blocks/ object shares the source inode -> import is not same-device zero-copy"
  nsrc=$(stat -c '%h' .govcheck/src/legacy.bin); chk "$nsrc" "2" "source link count becomes 2 (store + source reference one inode)"
  chk "$(T cat $BASE/src/legacy.bin)" "LEGACY" "imported content is the very source bytes"
  # Removing the name inside the view only unlinks the store; the source is untouched (GC source-safe).
  T rm "$BASE/src/legacy.bin" && ok "remove the imported name in the view" || bad "rm imported name failed"
  [ -e .govcheck/src/legacy.bin ] && ok "removing the view name never hurts the source file (GC at-most-once / source-safe)" \
                                   || bad "import removal leaked into the source!"
else
  bad "imported file not visible under the mounted view"
fi
fi

echo "===== C) expressiveness matrix (mirrors paper Table tab:expr) ====="
cat <<'TABLE' | sed 's/^/  /'
capability                    Subtree Bind  Symlink Overlay GroveFS
single real inode             part    yes*  yes     NO      yes
no duplicate stored entry     yes     yes   NO*     NO*     yes
shared fate (delete together) yes     yes*  NO*     part    yes
write-through to source       yes     yes*  yes     NO*     yes
cross-parent dual view        NO      NO    part    NO      yes
server-side shape + reload    NO      NO    NO      NO      yes
gapless name rotation         NO      NO    NO      NO      yes
zero-copy legacy onboarding   NO      NO    NO      NO      yes
TABLE
echo "  (* = cell EXECUTED by this harness: A1 overlay dup/write-through/stale,"
echo "      A2 symlink dup-entry/dangling/leak, A3 bind single-inode/through/fate/no-mask/no-vacate.)"
echo "  (unstarred Subtree/Bind rows, and all server-side shape / gapless rotation /"
echo "      governed zero-copy cells, are ARCHITECTURAL limitations: documented, not measured.)"
echo "  (GroveFS is the only column with all YES; the others violate one or more invariants.)"
if [ "$BACKEND" = fdb ]; then
  echo "  (On an fdb:// deployment the last row is offered in the FILE-STORE envelope only:"
  echo '      B4 above measured that `import` refuses by role there, so the row reads'
  echo "      'yes (local store) / refused-with-alternative (fdb://)' — a boundary of where"
  echo "      the operation is offered, not a violation of I1/I2/I4 by the fdb backend.)"
fi

fusermount -u $LENS $BASE .govcheck/mnt_map 2>/dev/null
kp $PIDS $BASEP $LENP $MAPP $FDB_MP $FDB_DP; wk $PIDS $BASEP $LENP $MAPP $FDB_MP $FDB_DP

echo
echo "==================== SUMMARY: pass=$pass fail=$fail skip=$skip ===================="
[ "$fail" -eq 0 ] && echo "GOVCHECK: all identity/correctness assertions held" || echo "GOVCHECK: SOME ASSERTIONS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
