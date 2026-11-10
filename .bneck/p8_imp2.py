#!/usr/bin/env python3
# P8 part 2 of the import tier: sections 3, 4 and 5.
#
# These three sections are where the two tiers genuinely differ, and the
# difference is a property of the deployment rather than of the test:
#   * 3 — sharing an inode with the user's file is what a LOCAL store does. After a
#     cross-backend move the view owns its own objects, so neither side's edit
#     reaches the other, and the tier asserts exactly that (in both directions).
#   * 4 — "GC never destroys the dataset" is asserted as the survival of the user's
#     file on both tiers; the object COUNT is a question about a local blocks/.
#   * 5 — the incremental re-run happens in the staging store on both tiers (same
#     command, same tally shape, one fewer import because the delete happened on
#     the far side), and on fdb the pick-up reaches the view through a RE-MOVE.
import io

P = '/data/grovefs/grovefs/.build-bin/test_import.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- 3) sharing, and its absence after a move --------------------------------
rep('''chk "$(cat .itest/src/docs/quarter.txt)" "edited via grovefs" "source path sees the mount's edit (shared inode)"
echo 'source side change' > .itest/src/photos/2026/y.jpg
sleep 3   # attr/dentry TTL lets the mount converge on the external edit
chk "$(T cat $M/legacy/photos/2026/y.jpg)" "$(cat .itest/src/photos/2026/y.jpg)" "external edit on the source shows through after the TTL"
''',
'''echo 'source side change' > .itest/src/photos/2026/y.jpg
sleep 3   # attr/dentry TTL lets the mount converge on the external edit
if [ "$BACKEND" = fdb ]; then
    # Both directions of the live sharing are checked, and both are ABSENT — which
    # is the honest shape of a cross-backend move, not a regression to hide: the
    # blobs the subspace reads are the deployment's own objects (the copy section 2
    # handed over), so an edit stops on the side it started on. `import`'s zero-copy
    # mirroring is a property of a LOCAL store; the move trades it for a
    # cluster-authoritative tree, and USAGE says so in the same terms.
    [ "$(cat .itest/src/docs/quarter.txt)" != "edited via grovefs" ] \\
      && ok "the mount's edit did NOT reach the source path (the view owns its bytes)" \\
      || bad "the moved tree kept a live link into the user's directory"
    chk "$(T cat $M/legacy/docs/quarter.txt)" "edited via grovefs" "the moved view is writable and reads back its own edit"
    chk "$(T cat $M/legacy/photos/2026/y.jpg)" "beach day" "the source's own edit does not show through: an object-store copy, not a shared inode"
else
    chk "$(cat .itest/src/docs/quarter.txt)" "edited via grovefs" "source path sees the mount's edit (shared inode)"
    chk "$(T cat $M/legacy/photos/2026/y.jpg)" "$(cat .itest/src/photos/2026/y.jpg)" "external edit on the source shows through after the TTL"
fi
''')

# ---- 4) GC ------------------------------------------------------------------
rep('''before=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
T rm $M/legacy/photos/2026/y.jpg && ok "remove an imported name via the mount" || bad "rm failed"
after=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
chk "$after" "$((before-1))" "the blocks/ link was unlinked by GC"
chk "$(cat .itest/src/photos/2026/y.jpg)" "source side change" "the user's source file SURVIVED the view-level delete"
''',
'''# How many objects a delete released is a question about a local `blocks/`
# directory; here the object store belongs to the data-server, so this tier keeps
# the guarantee the section is named for — the user's own file is never the price
# of a view-level delete — and asks it with the same line as the other tier.
T rm $M/legacy/photos/2026/y.jpg && ok "remove an imported name via the mount" || bad "rm failed"
if [ "$BACKEND" = fdb ]; then
    [ ! -e "$M/legacy/photos/2026/y.jpg" ] && ok "the deleted name is gone from the subspace view" \\
      || bad "the delete did not land on the subspace"
    chk "$(T cat $M/legacy/docs/quarter.txt)" "edited via grovefs" "a sibling name still reads after the delete (no collateral unlink)"
else
    after=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
    chk "$after" "$((before-1))" "the blocks/ link was unlinked by GC"
fi
chk "$(cat .itest/src/photos/2026/y.jpg)" "source side change" "the user's source file SURVIVED the view-level delete"
''')

rep('''echo "===== 4) GC never destroys the dataset ====="
''', '''echo "===== 4) GC never destroys the dataset ====="
before=$(find .itest/home/blocks -type f | wc -l | tr -d ' ')
''')

# ---- 5) incremental re-run, then the re-move ---------------------------------
rep('''# The re-run sees: quarter.txt + memo.txt EXIST (skip), fresh.txt is new,
# and y.jpg — whose view entry was deleted in section 4 — comes BACK as a
# fresh hard link onto the still-live source inode; alias/pipe.fifo stay
# skipped as unsupportable => 2 imported, 4 skipped.
echo 'brand new' > .itest/src/docs/fresh.txt
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp2.log 2>&1
grep -q "2 object(s) (2 hardlinked zero-copy, 0 copied), 4 skipped" .itest/imp2.log \\
  && ok "second run imports only fresh.txt + re-links the deleted y.jpg" || bad "incremental tally wrong: $(cat .itest/imp2.log)"
sleep 2
''',
'''# The re-run is the SAME command against the SAME staging store on both tiers. Its
# tally differs by exactly one object, and the reason is where section 4's delete
# happened: on this tier the staging view never lost y.jpg (the delete was on the
# subspace), so only fresh.txt is new => 1 imported, 5 skipped.
echo 'brand new' > .itest/src/docs/fresh.txt
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp2.log 2>&1
if [ "$BACKEND" = fdb ]; then
    grep -q "1 object(s) (1 hardlinked zero-copy, 0 copied), 5 skipped" .itest/imp2.log \\
      && ok "second run imports only fresh.txt (y.jpg was never lost by the staging store)" \\
      || bad "incremental tally wrong: $(cat .itest/imp2.log)"
    # The far side of an incremental re-run is a RE-MOVE, not a TTL: the subspace
    # catches up with the two commands that filled it the first time, and
    # `--replace` is what makes that idempotent. The name section 4 deleted comes
    # back through the move rather than through a re-link.
    $Y view --meta-dir .itest/home export --view legacy --out .itest/legacy2.jsonl --force > .itest/exp2.log 2>&1 \\
      && ok "re-exported the staging store" || bad "re-export failed: $(cat .itest/exp2.log)"
    cp -a .itest/home/blocks .itest/fdbhome/data/ \\
      && ok "refreshed the objects the re-run touched" || bad "blob refresh failed"
    $Y view $VDIR import --view $VIEW --in .itest/legacy2.jsonl --replace > .itest/vimp2.log 2>&1 \\
      && ok "the re-move replaced the subspace with the re-run tree" \\
      || bad "re-move failed: $(head -3 .itest/vimp2.log)"
else
    grep -q "2 object(s) (2 hardlinked zero-copy, 0 copied), 4 skipped" .itest/imp2.log \\
      && ok "second run imports only fresh.txt + re-links the deleted y.jpg" || bad "incremental tally wrong: $(cat .itest/imp2.log)"
fi
sleep 2
''')

io.open(P, 'w', encoding='utf-8').write(s)
print('import part2 ok, %d -> %d bytes' % (len(orig), len(s)))
