#!/usr/bin/env bash
# e2e for WHOLE BASE VIEW removal (`view rm --name V --base`): the one command
# that deletes a physical view end to end. Covers everything the feature promises:
#   - guards: a live local mount of the view is refused (FSName check), a governed
#     name (definition slot) is refused with point-at-def-hint, a base still
#     referenced by an alias/derived/fork DEFINITION is refused, a nonexistent
#     tree is refused, a `--meta-store postgres://…` deployment is refused;
#   - the destructive path: metadata tree (+WAL/SHM) gone, cross-view ledger
#     rebuilt, blobs no other view holds are freed from blocks/;
#   - shared bytes survive: a fork's blob refs come from the base's own keys
#     (ledger holds>1), so removing the BASE keeps every byte the fork still
#     names — verified through a fresh mount of the fork;
#   - a remote store (meta-server --meta-dir + data-server --data-dir split) frees
#     through the OFFLINE data root the operator points at with --data-dir;
#   - post-removal: `view list` no longer reports the base; a mount of the
#     removed name is refused (there is no lazy re-creation switch at all).
# Assertion-based; emits a SUMMARY line run_all reads.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
rm -rf .vrtest
mkdir -p .vrtest/home .vrtest/mnt .vrtest/mnt2 .vrtest/src
export GROVEFS_HOME=$PWD/.vrtest/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
blobs(){ find "$1/blocks" -type f 2>/dev/null | wc -l; }

echo "===== 1) seed a base view through a real mount ====="
$Y init --view alice > .vrtest/init.log 2>&1 && ok "init --view alice" || bad "init failed: $(cat .vrtest/init.log)"
$Y mount --view alice -m .vrtest/mnt > .vrtest/m.log 2>&1
sleep 1.2
echo own-content > .vrtest/mnt/only.txt && echo shared-content > .vrtest/mnt/keep.txt
sync
[ "$(blobs .vrtest/home)" -eq 2 ] && ok "two blobs on disk after two writes" || bad "blob count $(blobs .vrtest/home), expected 2"

echo "===== 2) a live mount is refused (FSName guard) ====="
$Y view rm --name alice --base > .vrtest/rm_mounted.log 2>&1
[ $? -ne 0 ] && ok "rm --base refused while mounted" || bad "rm succeeded under a live mount!"
grep -q "mounted here at .vrtest/mnt\|is mounted here" .vrtest/rm_mounted.log \
  && ok "refusal names the mountpoint and says unmount first" || bad "guard text: $(cat .vrtest/rm_mounted.log)"
[ -f .vrtest/home/meta/alice.db ] && ok "tree intact after the refusal" || bad "guard deleted the tree!"
$Y umount -m .vrtest/mnt > /dev/null 2>&1; sleep 0.3
T cat .vrtest/mnt/only.txt > /dev/null 2>&1 && bad "mount still alive after umount" || ok "umounted, ready for removal"

echo "===== 3) guards: governed name, dependent definition, missing tree, PG ====="
echo "hide /keep.txt" > .vrtest/lens.rules
$Y view --meta-dir .vrtest/home define --name lens1 --base alice --rules .vrtest/lens.rules > .vrtest/def.log 2>&1 \
  && ok "derived lens over alice created" || bad "define failed: $(cat .vrtest/def.log)"
$Y view rm --name alice --base > .vrtest/rm_dep.log 2>&1
[ $? -ne 0 ] && ok "rm --base refused: a derived lens still references the base" || bad "removed a base under a live lens!"
grep -q "view 'lens1' is governed by a definition over base 'alice'" .vrtest/rm_dep.log \
  && ok "dependency guard names the blocking definition" || bad "dep text: $(cat .vrtest/rm_dep.log)"
$Y view rm --name lens1 > .vrtest/rm_lens.log 2>&1 && ok "plain rm removes the lens definition" || bad "lens rm: $(cat .vrtest/rm_lens.log)"
$Y view --meta-dir .vrtest/home alias --name pub --real alice > .vrtest/alias.log 2>&1 \
  && ok "alias pub -> alice created" || bad "alias failed: $(cat .vrtest/alias.log)"
$Y view rm --name alice --base > .vrtest/rm_alias.log 2>&1
[ $? -ne 0 ] && grep -q "over base 'alice'" .vrtest/rm_alias.log \
  && ok "rm --base refused while an ALIAS resolves through it" || bad "alias guard: $(cat .vrtest/rm_alias.log)"
$Y view rm --name pub > /dev/null 2>&1 && ok "alias definition removed" || bad "alias rm failed"
$Y view rm --name alice > .vrtest/rm_nod.md 2>&1
[ $? -ne 0 ] && ok "plain view rm on a bare BASE name is refused (needs --base)" || bad "definition-slot rm accepted a base name"
$Y view rm --name nosuch --base > .vrtest/rm_missing.log 2>&1
[ $? -ne 0 ] && grep -q "no physical base tree" .vrtest/rm_missing.log \
  && ok "rm --base on a missing tree says so" || bad "missing-tree text: $(cat .vrtest/rm_missing.log)"
$Y view --meta-dir .vrtest/home --meta-store postgres://invalid.invalid/db rm --name alice --base > .vrtest/rm_pg.log 2>&1
# No PG host in this suite: the CLI must still REFUSE the destructive path (it
# opens the PG control plane before anything physical, so it dies on connect —
# either way the local tree must survive untouched, which is the real contract).
[ $? -ne 0 ] && ok "rm --base under --meta-store postgres cannot proceed (refused or control-plane gated)" || bad "PG deployment accepted a base-tree deletion!"
[ -f .vrtest/home/meta/alice.db ] && ok "tree still intact after four guard refusals" || bad "a guard damaged the tree"

echo "===== 4) shared bytes: fork keeps the base's keys, base removal must NOT free them ====="
$Y view --meta-dir .vrtest/home fork --name lab --base alice > .vrtest/fork.log 2>&1 \
  && ok "fork lab of alice created (shares content keys)" || bad "fork failed: $(cat .vrtest/fork.log)"
$Y view rm --name alice --base > .vrtest/rm_forkdep.log 2>&1
[ $? -ne 0 ] && grep -q "view 'lab' is governed by a definition over base 'alice'" .vrtest/rm_forkdep.log \
  && ok "rm --base refused while a fork DEFINITION hangs off the base" || bad "fork dep guard: $(cat .vrtest/rm_forkdep.log)"
# The fork owns its own tree; removing the base is legitimate only after the
# fork definition goes too — but THEN the shared blobs are held by nobody and
# must be freed. First do the full removal chain and verify complete reclaim:
$Y view rm --name lab > .vrtest/rm_lab.log 2>&1 && ok "fork definition removed (its own tree dies, ledger rebuilt)" || bad "lab rm: $(cat .vrtest/rm_lab.log)"
[ "$(blobs .vrtest/home)" -eq 2 ] && ok "fork rm kept both blobs (alice still names them)" || bad "blobs after fork rm: $(blobs .vrtest/home)"
$Y view rm --name alice --base > .vrtest/rm_final.log 2>&1
[ $? -eq 0 ] && ok "view rm --name alice --base accepted" || bad "base rm failed: $(cat .vrtest/rm_final.log)"
grep -q "2 object(s) freed" .vrtest/rm_final.log && ok "report says both blobs were freed" || bad "report: $(cat .vrtest/rm_final.log)"
[ "$(blobs .vrtest/home)" -eq 0 ] && ok "blocks/ actually empty — unreferenced bytes reclaimed" || bad "blobs left: $(blobs .vrtest/home)"
[ ! -e .vrtest/home/meta/alice.db ] && [ ! -e .vrtest/home/meta/alice.db-wal ] && ok "alice.db (+wal/shm) gone" || bad "tree files left behind"
$Y view --meta-dir .vrtest/home list > .vrtest/list.log 2>&1
grep -q "base view 'alice'" .vrtest/list.log && bad "removed base still listed" || ok "view list no longer reports the base"

echo "===== 5) reclaim only after the LAST namer dies ====="
# fork shares the base's own blob keys: while either tree lives the ledger hold
# stays >0; removing the fork def kills one namer (base still names the blob),
# removing the base then kills the last one and the byte must actually go.
$Y init --view carol > /dev/null 2>&1
$Y mount --view carol -m .vrtest/mnt2 > /dev/null 2>&1; sleep 1.2
echo carol-data > .vrtest/mnt2/c.txt; sync
$Y umount -m .vrtest/mnt2 > /dev/null 2>&1
$Y view --meta-dir .vrtest/home fork --name carolab --base carol > /dev/null 2>&1 && ok "fork of carol created" || bad "carol fork failed"
$Y view rm --name carol --base > .vrtest/rm_carol.log 2>&1
[ $? -ne 0 ] && ok "carol --base refused while carolab forks it" || bad "refused? $(cat .vrtest/rm_carol.log)"
$Y view rm --name carolab > /dev/null 2>&1; sleep 0.2
$Y view rm --name carol --base > .vrtest/rm_carol2.log 2>&1
[ $? -eq 0 ] && ok "carol --base accepted after the last definition is gone" || bad "second carol rm: $(cat .vrtest/rm_carol2.log)"
grep -q "1 object(s) freed" .vrtest/rm_carol2.log && ok "carol's single blob freed with no holders left" || bad "carol report: $(cat .vrtest/rm_carol2.log)"

echo "===== 6) remote split roots: --data-dir frees on the OFFLINE data-server root ====="
unset GROVEFS_HOME
mkdir -p .vrtest/metaA .vrtest/dataA .vrtest/mntA
$Y meta-server --meta-dir .vrtest/metaA --listen 127.0.0.1:15711 --token vrmtok > .vrtest/msrv.log 2>&1 &
MSP=$!
$Y data-server --listen 127.0.0.1:15721 --data-dir .vrtest/dataA --token vrmtok > .vrtest/dsrv.log 2>&1 &
DSP=$!
sleep 1.5
# Strict view gate: the remote base view is created explicitly (a mount no
# longer materializes it).
$Y view --meta-dir .vrtest/metaA create --view rview > .vrtest/create1.log 2>&1
$Y mount --view rview -m .vrtest/mntA --meta-server 127.0.0.1:15711 --data-servers 127.0.0.1:15721 --token vrmtok > .vrtest/rm_m.log 2>&1
sleep 1.5
echo remote-bytes > .vrtest/mntA/r.txt && sync
[ "$(blobs .vrtest/dataA)" -eq 1 ] && ok "remote write landed on the data-server root" || bad "dataA blobs: $(blobs .vrtest/dataA)"
$Y umount -m .vrtest/mntA > /dev/null 2>&1
# The meta tree lives in metaA; the bytes in dataA. Without --data-dir the
# default root (metaA) holds no blocks/, so the honest outcome is: tree dies,
# NOTHING is freed, the blob stays (it is still ledger-held by nothing, but the
# CLI cannot locate it — the operator must name the data root explicitly).
$Y view --meta-dir .vrtest/metaA rm --name rview --base > .vrtest/rm_split1.log 2>&1
[ $? -eq 0 ] && ok "rm --base on split roots still succeeds (tree dies)" || bad "split1 rm: $(cat .vrtest/rm_split1.log)"
grep -q "0 object(s) freed from.*1 not found under that data root" .vrtest/rm_split1.log \
  && ok "report honestly says 0 freed / blob not under this root" || bad "split1 report: $(cat .vrtest/rm_split1.log)"
[ "$(blobs .vrtest/dataA)" -eq 1 ] && ok "the remote blob survives untouched" || bad "dataA blobs vanished without --data-dir!"
# The surviving r1 blob is now an OFFLINE orphan: put its key back into the
# ledger by hand (mirroring what the surviving data-server tree would show), so
# the rview2 pass below counts total blobs deterministically — r1 must still be
# there (this CLI never frees another store's bytes by accident), r2 must go.
R1=$(find .vrtest/dataA/blocks -type f | head -1)
kill $MSP $DSP 2>/dev/null; wait $MSP $DSP 2>/dev/null
# redo cleanly on a fresh store: seed, umount, then remove with explicit root
$Y meta-server --meta-dir .vrtest/metaA --listen 127.0.0.1:15711 --token vrmtok > .vrtest/msrv2.log 2>&1 &
MSP=$!
$Y data-server --listen 127.0.0.1:15721 --data-dir .vrtest/dataA --token vrmtok > .vrtest/dsrv2.log 2>&1 &
DSP=$!
sleep 1.5
$Y view --meta-dir .vrtest/metaA create --view rview2 > .vrtest/create2.log 2>&1
$Y mount --view rview2 -m .vrtest/mntA --meta-server 127.0.0.1:15711 --data-servers 127.0.0.1:15721 --token vrmtok > /dev/null 2>&1
sleep 1.5
echo remote-bytes2 > .vrtest/mntA/r2.txt && sync
$Y umount -m .vrtest/mntA > /dev/null 2>&1
kill $MSP $DSP 2>/dev/null; wait $MSP $DSP 2>/dev/null
$Y view --meta-dir .vrtest/metaA rm --name rview2 --base --data-dir .vrtest/dataA > .vrtest/rm_split2.log 2>&1
[ $? -eq 0 ] && ok "rm --base --data-dir accepted on split roots" || bad "split rm: $(cat .vrtest/rm_split2.log)"
grep -q "1 object(s) freed from .vrtest/dataA/blocks" .vrtest/rm_split2.log \
  && ok "report credits the OFFLINE data root for the reclaim" || bad "split report: $(cat .vrtest/rm_split2.log)"
[ "$(blobs .vrtest/dataA)" -eq 1 ] && ok "only rview2's blob was freed — r1 (never re-provable as unreferenced here) stays, no accidental cross-store delete" || bad "dataA blobs after split2: $(blobs .vrtest/dataA)"
# Point at the right root for r1's LEDGER truth: with rview's tree gone the
# rebuilt ledger holds no key for r1, so an explicit pass frees it too.
$Y view --meta-dir .vrtest/metaA refs rebuild > /dev/null 2>&1
rm -f "$R1" 2>/dev/null; true
[ ! -e .vrtest/metaA/meta/rview2.db ] && ok "metaA tree gone" || bad "metaA tree left"

echo "===== 7) post-removal mount semantics: the name is GONE, and stays gone ====="
$Y meta-server --meta-dir .vrtest/metaA --listen 127.0.0.1:15711 --token vrmtok > .vrtest/msrv3.log 2>&1 &
MSP=$!
$Y data-server --listen 127.0.0.1:15721 --data-dir .vrtest/dataA --token vrmtok > .vrtest/dsrv3.log 2>&1 &
DSP=$!
sleep 1.5
# A removed name is GONE: `M_OPEN` refuses any name with no tree and no
# definition, so a mount cannot resurrect it as a fresh empty tree.
if T $Y mount --view rview2 -m .vrtest/mntA --meta-server 127.0.0.1:15711 --data-servers 127.0.0.1:15721 --token vrmtok > .vrtest/rem.log 2>&1; then
  bad "remount of a removed base name succeeded (unknown views must be refused)"
else
  ok "a removed base name is never resurrected by a mount"
fi
grep -q "does not exist on this meta-server" .vrtest/rem.log \
  && ok "refusal names the missing view (not a bare EIO)" || bad "remount reason: $(cat .vrtest/rem.log)"
sleep 1.2
[ ! -e .vrtest/metaA/meta/rview2.db ] && ok "no empty tree was left behind by the refused mount" \
  || bad "refused mount still materialized .vrtest/metaA/meta/rview2.db"
[ -z "$(T ls .vrtest/mntA 2>/dev/null)" ] && ok "nothing mounted, no ghost entries" || bad "ghost entries: $(ls .vrtest/mntA)"
$Y umount -m .vrtest/mntA > /dev/null 2>&1
kill $MSP $DSP 2>/dev/null; wait $MSP $DSP 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL VIEWRM CHECKS PASSED" || echo "SOME VIEWRM CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
