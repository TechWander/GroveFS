#!/usr/bin/env bash
# e2e for `grovefs gc` — the OFFLINE orphan-object audit / reclaim:
#   - a clean store reports zero orphans and deletes nothing (scan is default);
#   - an out-of-band `rm meta/<view>.db` leaves bytes that the cross-view ledger
#     still books: reported as ledger-held, NEVER deleted, and freed only after
#     `grovefs refs rebuild` releases the stale hold;
#   - --apply honours the grace window (young objects survive), and
#     `--older-than 0` actually unlinks the page;
#   - a file outside the bucket layout is reported as skipped, never deleted;
#   - --apply refuses while THIS store has a live writable mount, but a mount of
#     the same view NAME in an unrelated store does not block it;
#   - a --data-root that shares no key with the trees is refused (wrong-root
#     guard) before anything is deleted, and --force only bypasses the guard;
#   - a PostgreSQL deployment and a malformed duration are refused.
# Assertion-based; emits a SUMMARY line run_all reads.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG=$PWD/.build-bin/pkg-config
Y=./target/release/grovefs
rm -rf .gctest
mkdir -p .gctest/home .gctest/other .gctest/mnt .gctest/mnt2 .gctest/mnt3
export GROVEFS_HOME=$PWD/.gctest/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
blobs(){ find "$1/blocks" -type f 2>/dev/null | wc -l; }
umount_all(){ for d in .gctest/mnt .gctest/mnt2 .gctest/mnt3; do fusermount -u "$d" 2>/dev/null || true; done; }
trap umount_all EXIT

echo "===== 1) a clean store: scan only, nothing to reclaim ====="
$Y init --view alice > .gctest/init.log 2>&1 && ok "init --view alice" || bad "init failed: $(cat .gctest/init.log)"
$Y mount --view alice -m .gctest/mnt > .gctest/m.log 2>&1; sleep 1.2
echo alice-a > .gctest/mnt/a.txt && echo alice-keep > .gctest/mnt/keep.txt
sync
[ "$(blobs .gctest/home)" -eq 2 ] && ok "two objects on disk after two writes" || bad "blobs $(blobs .gctest/home), expected 2"
$Y umount -m .gctest/mnt > /dev/null 2>&1; sleep 0.3
$Y gc --db-path .gctest/home > .gctest/scan1.log 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "gc scan exits 0 on a clean store" || bad "scan rc=$rc: $(cat .gctest/scan1.log)"
grep -q "views: 1 tree(s), 2 referenced key(s)" .gctest/scan1.log \
  && ok "scan sees the tree and both referenced keys" || bad "scan1: $(cat .gctest/scan1.log)"
grep -q "orphans: 0" .gctest/scan1.log && ok "no orphans reported" || bad "scan1 orphans: $(cat .gctest/scan1.log)"
grep -q "dry run: nothing deleted" .gctest/scan1.log && ok "scan is a dry run by default" || bad "dry-run text: $(cat .gctest/scan1.log)"
[ "$(blobs .gctest/home)" -eq 2 ] && ok "scan deleted nothing" || bad "scan touched blocks/"

echo "===== 2) out-of-band tree removal: bytes stay ledger-held, never orphans ====="
$Y init --view bob > /dev/null 2>&1
$Y mount --view bob -m .gctest/mnt2 > .gctest/m2.log 2>&1; sleep 1.2
echo bob-data > .gctest/mnt2/b.txt && sync
$Y umount -m .gctest/mnt2 > /dev/null 2>&1; sleep 0.3
[ "$(blobs .gctest/home)" -eq 3 ] && ok "bob's object is on disk" || bad "blobs $(blobs .gctest/home), expected 3"
rm -f .gctest/home/meta/bob.db .gctest/home/meta/bob.db-wal .gctest/home/meta/bob.db-shm
$Y gc --db-path .gctest/home > .gctest/scan2.log 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "scan survives an out-of-band tree removal" || bad "scan2 rc=$rc: $(cat .gctest/scan2.log)"
grep -q "orphans: 0" .gctest/scan2.log && ok "the ledger still books the bytes, so no orphan yet" || bad "scan2: $(cat .gctest/scan2.log)"
grep -q "ledger-held: 1" .gctest/scan2.log && ok "reported as ledger-held (1)" || bad "held line: $(cat .gctest/scan2.log)"
grep -q "refs --db-path .gctest/home rebuild" .gctest/scan2.log \
  && ok "the report names the rebuild command that releases it" || bad "rebuild hint: $(cat .gctest/scan2.log)"
$Y gc --db-path .gctest/home --json > .gctest/scan2.json 2>&1
python3 - .gctest/scan2.json <<'PY' && ok "--json: one ledger_held candidate, zero orphans" || bad "--json held shape: $(cat .gctest/scan2.json)"
import json,sys
d=json.load(open(sys.argv[1]))
assert d["applied"] is False and d["deleted"] == 0, d
assert len(d["ledger_held"]) == 1 and d["ledger_held"][0]["kind"] == "plain", d
assert d["orphans"] == [] and d["reclaimable"] == 0, d
PY
[ "$(blobs .gctest/home)" -eq 3 ] && ok "ledger-held bytes were not deleted" || bad "held bytes vanished"

echo "===== 3) refs rebuild -> orphan -> grace-aware apply ====="
$Y refs --db-path .gctest/home rebuild > .gctest/rebuild.log 2>&1 && ok "refs rebuild ran" || bad "rebuild: $(cat .gctest/rebuild.log)"
$Y gc --db-path .gctest/home > .gctest/scan3.log 2>&1
grep -q "orphans: 1" .gctest/scan3.log && ok "the stale hold is gone: 1 orphan" || bad "scan3: $(cat .gctest/scan3.log)"
grep -q "within grace <1d0h" .gctest/scan3.log && ok "default grace window is 24h" || bad "grace text: $(cat .gctest/scan3.log)"
$Y gc --db-path .gctest/home --apply > .gctest/apply_young.log 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "--apply exits 0" || bad "apply rc=$rc: $(cat .gctest/apply_young.log)"
grep -q "reclaimed: 0 object(s).*1 within grace kept" .gctest/apply_young.log \
  && ok "a young orphan is kept by the grace window" || bad "young apply: $(cat .gctest/apply_young.log)"
[ "$(blobs .gctest/home)" -eq 3 ] && ok "young orphan still on disk" || bad "grace window deleted a young object"
$Y gc --db-path .gctest/home --apply --older-than 0 --json > .gctest/apply_old.json 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "--older-than 0 accepted" || bad "apply rc=$rc: $(cat .gctest/apply_old.json)"
python3 - .gctest/apply_old.json <<'PY' && ok "--json: one object deleted, report agrees" || bad "--json apply: $(cat .gctest/apply_old.json)"
import json,sys
d=json.load(open(sys.argv[1]))
assert d["applied"] is True and d["deleted"] == 1 and d["grace_secs"] == 0, d
assert d["reclaimable"] == 1 and d["delete_failed"] == [], d
PY
[ "$(blobs .gctest/home)" -eq 2 ] && ok "reclaim really unlinked the page (2 left)" || bad "blobs $(blobs .gctest/home), expected 2"

echo "===== 4) a loose file outside the bucket layout is skipped, never deleted ====="
printf 'stray' > .gctest/home/blocks/stray
$Y gc --db-path .gctest/home > .gctest/scan4.log 2>&1
grep -q "skipped: 1 entr" .gctest/scan4.log && ok "loose file reported as skipped" || bad "skipped line: $(cat .gctest/scan4.log)"
[ -f .gctest/home/blocks/stray ] && ok "loose file untouched by --apply? (dry run here)" || bad "scan deleted a foreign file"
$Y gc --db-path .gctest/home --apply --older-than 0 > /dev/null 2>&1
[ -f .gctest/home/blocks/stray ] && ok "--apply never deletes a non-object" || bad "--apply deleted a foreign file"
rm -f .gctest/home/blocks/stray

echo "===== 5) live mounts: THIS store blocks --apply, another store does not ====="
$Y mount --view alice -m .gctest/mnt > .gctest/m3.log 2>&1; sleep 1.2
$Y gc --db-path .gctest/home --apply > .gctest/apply_mounted.log 2>&1; rc=$?
[ "$rc" -ne 0 ] && ok "--apply refused while this store is mounted" || bad "apply ran under a live mount!"
grep -q "live mounts" .gctest/apply_mounted.log && ok "refusal names the live mount" || bad "mount guard text: $(cat .gctest/apply_mounted.log)"
grep -q "unmount them first" .gctest/apply_mounted.log && ok "refusal says to go offline" || bad "mount guard hint: $(cat .gctest/apply_mounted.log)"
$Y umount -m .gctest/mnt > /dev/null 2>&1; sleep 0.3
# A DIFFERENT store that happens to mount the same view name must not block us.
$Y init --home .gctest/other --view alice > /dev/null 2>&1
$Y mount --home .gctest/other --view alice -m .gctest/mnt3 > .gctest/m4.log 2>&1; sleep 1.2
grep -q "grovefs:alice .*\.gctest/mnt3" /proc/mounts && ok "foreign store with the same view name is mounted" || bad "could not mount the foreign store"
$Y gc --db-path .gctest/home --apply --older-than 0 > .gctest/apply_foreign.log 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "same-named view in another store does not block this store's gc" || bad "false positive: $(cat .gctest/apply_foreign.log)"
$Y umount -m .gctest/mnt3 > /dev/null 2>&1; sleep 0.3

echo "===== 6) a --data-root with no shared key is refused before deleting ====="
# Give the foreign store one real object so its blocks/ is not empty.
$Y mount --home .gctest/other --view alice -m .gctest/mnt3 > /dev/null 2>&1; sleep 1.2
echo foreign > .gctest/mnt3/f.txt && sync
$Y umount -m .gctest/mnt3 > /dev/null 2>&1; sleep 0.3
printf 'dead' > .gctest/home/blocks/aa/dead-orphan
before_other=$(blobs .gctest/other)
$Y gc --db-path .gctest/home --data-root .gctest/other --apply > .gctest/wrongroot.log 2>&1; rc=$?
[ "$rc" -ne 0 ] && ok "wrong --data-root refused" || bad "wrong root accepted: $(cat .gctest/wrongroot.log)"
grep -q "share no object with any view tree" .gctest/wrongroot.log && ok "refusal explains the mismatch" || bad "wrong-root text: $(cat .gctest/wrongroot.log)"
grep -q "Nothing was deleted" .gctest/wrongroot.log && ok "refusal promises nothing was deleted" || bad "wrong-root hint: $(cat .gctest/wrongroot.log)"
[ "$(blobs .gctest/other)" -eq "$before_other" ] && ok "foreign bytes intact after the refusal" || bad "wrong-root guard deleted foreign bytes!"
[ -f .gctest/home/blocks/aa/dead-orphan ] && ok "the unexamined orphan is still there" || bad "guard pass deleted something"
$Y gc --db-path .gctest/home --data-root .gctest/other --force > .gctest/force.log 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "--force bypasses only the guard (dry run still)" || bad "force rc=$rc: $(cat .gctest/force.log)"
[ "$(blobs .gctest/other)" -eq "$before_other" ] && ok "--force without --apply deleted nothing" || bad "--force deleted bytes"
rm -f .gctest/home/blocks/aa/dead-orphan

echo "===== 7) unusable inputs are refused with a reason ====="
GROVEFS_STORE=postgres://u:p@127.0.0.1:1/db $Y gc --db-path .gctest/home > .gctest/pg.log 2>&1
[ $? -ne 0 ] && grep -q "PostgreSQL" .gctest/pg.log && ok "PG deployment refused (local page trees only)" || bad "pg: $(cat .gctest/pg.log)"
$Y gc --db-path .gctest/home --older-than 12q > .gctest/age.log 2>&1
[ $? -ne 0 ] && grep -q "unknown duration unit 'q'" .gctest/age.log && ok "malformed duration refused" || bad "age: $(cat .gctest/age.log)"
$Y gc --db-path .gctest/nosuch > .gctest/nometa.log 2>&1
[ $? -ne 0 ] && grep -q "no metadata directory" .gctest/nometa.log && ok "missing metadata root refused" || bad "nometa: $(cat .gctest/nometa.log)"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL GC CHECKS PASSED" || echo "SOME GC CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
