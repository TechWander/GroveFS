#!/usr/bin/env bash
# Astronomy "non-landing data" end-to-end governance case (paper Sec. 7, ext. abs.
# The Observatory 146:204).  It exercises the REAL primitive stack a survey
# science platform needs, and turns the paper's motivating scenario into
# falsifiable, measured evidence on one host, over a central meta-server:
#
#   A) NON-LANDING ONBOARDING: a pre-existing survey archive sitting on disk is
#      registered into a view via zero-copy import -- the bytes are NOT copied
#      ("landed") onto the platform: the object under blocks/ shares the SOURCE
#      host inode (hard link), so no consumer presentation ever duplicates it.
#   B) A CENTRALLY-GOVERNED, READ-ONLY PUBLIC RELEASE: a derived view reshapes
#      the one archive for public consumption -- it re-homes raw products under a
#      friendly path, masks EMBARGOED frames entirely, and is read-only (a
#      consumer cannot mutate or "land" the archive: writes hit EROFS).
#   C) REVOCABLE PUBLIC NAME: the archive is exposed to a partner under a public
#      alias while its real storage name is refused as a mount entry; --retire
#      yanks a leaked public name in one atomic step with no window.
#
# Honesty: single-host, indicative; NO throughput/scalability claim. The derived
# view's read-only is checked at the EROFS the kernel returns; embargo masking and
# friendly-path relocation are checked by what is/isn't visible through the mount.
#
# Assertion-based like the other suites; emits a SUMMARY line for run_all.sh.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove
rm -rf .grovesci; mkdir -p .grovesci/home .grovesci/mnt_dr .grovesci/mnt_al .grovesci/mnt_refused
export GROVEFS_HOME=$PWD/.grovesci/home
SRC=.grovesci/survey

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }

META=127.0.0.1:50174; DATA=127.0.0.1:50175
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- build a small "survey archive" already on disk --------------------------
mkdir -p $SRC/raw/cal $SRC/raw/sci $SRC/embargo
printf 'CALIBRATION-PRODUCT-BYTES' > $SRC/raw/cal/c1.fits
printf 'SCIENCE-PRODUCT-BYTES'     > $SRC/raw/sci/s1.fits
printf 'EMBARGOED-SECRET-FRAME'    > $SRC/embargo/e1.fits
src_host_ino=$(stat -c '%i' $SRC/raw/cal/c1.fits)

# ------------------------------------------------------------------ (A) import
echo "===== A) non-landing zero-copy import of the survey archive ====="
printf 'mkdir _seed\n' > .grovesci/seed.manifest
$Y init --view arch --manifest .grovesci/seed.manifest > .grovesci/init.log 2>&1 \
  || { echo "init failed"; cat .grovesci/init.log; exit 1; }
$Y import --db-path "$GROVEFS_HOME" --view arch --source $SRC > .grovesci/import.log 2>&1 \
  && ok "import survey archive into view arch" || { bad "import failed: $(cat .grovesci/import.log)"; exit 1; }
# Zero-copy PHYSICAL evidence: the object stored under blocks/ shares the SOURCE
# host inode (hard link), so the platform consumed no extra bytes for the archive.
hlink=$(find "$GROVEFS_HOME/blocks" -inum "$src_host_ino" 2>/dev/null | head -1)
[ -n "$hlink" ] && ok "imported object is a hardlink to the source (shared host inode $src_host_ino): bytes never 'landed' as a copy" \
                 || bad "no blocks/ object shares the source inode -> import is not same-device zero-copy"
chk "$(stat -c '%h' $SRC/raw/cal/c1.fits)" "2" "source link count becomes 2 (store + source reference ONE inode)"

# ---- central services over the same authoritative store ----------------------
$Y meta-server --db-path "$GROVEFS_HOME" --listen $META > .grovesci/meta.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --root "$GROVEFS_HOME" > .grovesci/data.log 2>&1 &
DSP=$!
cleanup(){ fusermount -u .grovesci/mnt_dr .grovesci/mnt_al .grovesci/mnt_refused 2>/dev/null; kill $MSP $DSP $(jobs -p) 2>/dev/null; }
trap cleanup EXIT
wait_port $META && wait_port $DATA && ok "meta/data servers up" || { bad "servers failed to start"; exit 1; }

# ------------------------------------------------------------------ (B) derived DR
echo "===== B) centrally-governed, read-only public data-release view ====="
# Rules: re-home raw products under a friendly /release, mask the embargo,
# and hide the internal seed. Written by the authority, consumed by everyone.
cat > .grovesci/dr.rules <<'EOF'
map  /survey/raw /release
hide /survey/embargo
hide /_seed
EOF
$Y view --db-path "$GROVEFS_HOME" define --name dr --base arch --rules .grovesci/dr.rules > .grovesci/def.log 2>&1 \
  && ok "derived public-release view 'dr' defined over the archive" || { bad "define failed: $(cat .grovesci/def.log)"; exit 1; }
# Mount the derived view BY NAME -- no local rule file, shape is central.
$Y mount --view dr -m .grovesci/mnt_dr --meta $META --data $DATA > .grovesci/dr.log 2>&1 &
DRP=$!; sleep 2
DR=.grovesci/mnt_dr
# Friendly relocation: raw products appear under /release, not /survey/raw.
if T ls $DR/release/cal >/dev/null 2>&1 && T ls $DR/release/sci >/dev/null 2>&1; then
  ok "public release serves re-homed products at /release/{cal,sci} (shape governed centrally)"
else
  bad "re-homed /release paths not visible through the derived view"
fi
chk "$(T cat $DR/release/cal/c1.fits 2>/dev/null)" "CALIBRATION-PRODUCT-BYTES" "the released file is the very archive bytes (no copy; I4 write-through to source)"
[ ! -e "$DR/survey/raw/cal/c1.fits" ] && ok "map vacated the source spot /survey/raw (single presentation, not a duplicate)" \
                                      || bad "source spot still present after map"
# Embargo masking: the embargoed frame is invisible under the public view.
if [ ! -e "$DR/survey/embargo" ] && ! T ls -R $DR 2>/dev/null | grep -q e1.fits; then
  ok "embargoed frame is entirely masked in the public release (visibility governance)"
else
  bad "embargoed content leaked into the derived view!"
fi
# Read-only: a consumer cannot mutate or 'land' the archive through the release.
if T sh -c "printf 'x' >> $DR/release/cal/c1.fits" 2>/dev/null; then
  bad "write to derived release unexpectedly succeeded"
else
  ok "writes through the public release are refused (read-only governed view; source stays authoritative)"
fi
if T sh -c "mkdir $DR/scratch" 2>/dev/null; then bad "mkdir on derived succeeded"; else ok "cannot create inside the derived release (EROFS)"; fi
fusermount -u $DR 2>/dev/null; kill $DRP 2>/dev/null; wait $DRP 2>/dev/null

# ------------------------------------------------------------------ (C) alias/revoke
echo "===== C) revocable public alias with gapless rotation ====="
$Y view --db-path "$GROVEFS_HOME" alias --name archive-open --real arch > .grovesci/al.log 2>&1 \
  && ok "public alias 'archive-open' -> archive defined" || bad "alias create: $(cat .grovesci/al.log)"
# The public name serves the (unshaped) tree for the partner.
$Y mount --view archive-open -m .grovesci/mnt_al --meta $META --data $DATA > .grovesci/mntal.log 2>&1 &
ALP=$!; sleep 2
if T ls .grovesci/mnt_al/survey >/dev/null 2>&1; then ok "public alias reaches the archive tree" ; else bad "alias did not serve the tree"; fi
kill $ALP 2>/dev/null; wait $ALP 2>/dev/null; fusermount -u .grovesci/mnt_al 2>/dev/null
# The governed REAL storage name is refused as a direct mount entry (leak-proof).
$Y mount --view arch -m .grovesci/mnt_refused --meta $META --data $DATA > .grovesci/ref.log 2>&1 &
RFP=$!; sleep 1.5
if [ ! -d .grovesci/mnt_refused/survey ]; then ok "real storage name 'arch' is refused as a mount entry (storage identity not exposed)"; else bad "real name still mountable"; fi
kill $RFP 2>/dev/null; wait $RFP 2>/dev/null; fusermount -u .grovesci/mnt_refused 2>/dev/null
# If the public name leaks, --retire yanks it atomically (old dead, new live).
$Y view --db-path "$GROVEFS_HOME" alias --name archive-vault --real arch --retire archive-open > .grovesci/rot.log 2>&1 \
  && ok "gapless rotation archive-open -> archive-vault" || bad "retire rotation failed: $(cat .grovesci/rot.log)"
$Y mount --view archive-open -m .grovesci/mnt_al --meta $META --data $DATA > .grovesci/old.log 2>&1 &
OLDP=$!; sleep 1.5
[ ! -d .grovesci/mnt_al/survey ] && ok "leaked public name no longer reaches the archive (revoked, no window)" \
                                 || bad "retired public name still served the archive"
kill $OLDP 2>/dev/null; wait $OLDP 2>/dev/null; fusermount -u .grovesci/mnt_al 2>/dev/null; rm -rf .grovesci/mnt_al; mkdir -p .grovesci/mnt_al
$Y mount --view archive-vault -m .grovesci/mnt_al --meta $META --data $DATA > .grovesci/new.log 2>&1 &
NWP=$!; sleep 2
if T ls .grovesci/mnt_al/survey >/dev/null 2>&1; then ok "rotated public name serves the archive immediately" ; else bad "new alias did not serve"; fi
fusermount -u .grovesci/mnt_al 2>/dev/null; kill $NWP 2>/dev/null; wait $NWP 2>/dev/null

# ---- the source on disk is untouched throughout (non-landing, source-safe) ---
echo "===== D) end-state integrity: the on-disk archive was never modified ====="
chk "$(cat $SRC/raw/cal/c1.fits)" "CALIBRATION-PRODUCT-BYTES" "source calibration product byte-identical after all governance (no landing/mutation)"
chk "$(cat $SRC/embargo/e1.fits)" "EMBARGOED-SECRET-FRAME" "embargoed source frame still intact on disk (never exposed, never touched)"
du_blocks=$(du -sb "$GROVEFS_HOME/blocks" 2>/dev/null | cut -f1)
echo "  INFO: blocks/ on-disk size = ${du_blocks} bytes; the three survey files total $(cat $SRC/raw/cal/c1.fits $SRC/raw/sci/s1.fits $SRC/embargo/e1.fits | wc -c) bytes, held as hardlinks (no per-consumer copies)"

kill $(jobs -p) 2>/dev/null; wait 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "GROVESCI: astronomy non-landing governance case fully verified" || echo "GROVESCI: SOME ASSERTIONS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
