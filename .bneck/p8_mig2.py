#!/usr/bin/env python3
# P8 part 2 of the migrate tier: sections 2-5.
import io

P = '/data/grovefs/grovefs/.build-bin/test_migrate.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- 2) export while mounted ------------------------------------------------
rep('''$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl > .migtest/exp.log 2>&1 \\
  && ok "view export from a live mount" || bad "export failed: $(cat .migtest/exp.log)"
grep -q "exported view 'team' metadata: 8 node(s), 3 blob reference(s)" .migtest/exp.log \\''',
'''$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl > .migtest/exp.log 2>&1 \\
  && ok "view export from a live mount" || bad "export failed: $(cat .migtest/exp.log)"
grep -q "exported view '$VIEW1' metadata: 8 node(s), 3 blob reference(s)" .migtest/exp.log \\''')

# ---- 3) export guards -------------------------------------------------------
rep('''$Y view --meta-dir .migtest/src export --view ghost --out .migtest/ghost.jsonl > .migtest/e1.log 2>&1
[ $? -ne 0 ] && grep -q "not a physical tree" .migtest/e1.log \\
  && ok "exporting a view with no local tree refused" || bad "ghost export guard: $(cat .migtest/e1.log)"
$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl > .migtest/e2.log 2>&1''',
'''$Y view $MSRC export --view ghost --out .migtest/ghost.jsonl > .migtest/e1.log 2>&1
[ $? -ne 0 ] && grep -qF "$GHOST" .migtest/e1.log \\
  && ok "exporting a view with no tree of its own refused" || bad "ghost export guard: $(cat .migtest/e1.log)"
$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl > .migtest/e2.log 2>&1''')

rep('''$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl --force > /dev/null 2>&1 \\
  && ok "--force allows the overwrite" || bad "--force export failed"
echo "only /docs" > .migtest/docs.layout
$Y view --meta-dir .migtest/src define --name lens --base team --rules .migtest/docs.layout > .migtest/e3.log 2>&1
$Y view --meta-dir .migtest/src export --view lens --out .migtest/lens.jsonl > .migtest/e4.log 2>&1''',
'''$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl --force > /dev/null 2>&1 \\
  && ok "--force allows the overwrite" || bad "--force export failed"
echo "only /docs" > .migtest/docs.layout
$Y view $MSRC define --name lens --base $VIEW1 --rules .migtest/docs.layout > .migtest/e3.log 2>&1
$Y view $MSRC export --view lens --out .migtest/lens.jsonl > .migtest/e4.log 2>&1''')

rep('''$Y view --meta-dir .migtest/src alias --name pub --real team > .migtest/e5.log 2>&1
$Y view --meta-dir .migtest/src export --view pub --out .migtest/pub.jsonl > .migtest/e6.log 2>&1''',
'''$Y view $MSRC alias --name pub --real $VIEW1 > .migtest/e5.log 2>&1
$Y view $MSRC export --view pub --out .migtest/pub.jsonl > .migtest/e6.log 2>&1''')

rep('''$Y view --meta-dir .migtest/src rm --name pub > /dev/null 2>&1
$Y view --meta-dir .migtest/src rm --name lens > /dev/null 2>&1''',
'''$Y view $MSRC rm --name pub > /dev/null 2>&1
$Y view $MSRC rm --name lens > /dev/null 2>&1''')

# ---- 4) fork ----------------------------------------------------------------
rep('''$Y view --meta-dir .migtest/src fork --name lab --base team > .migtest/fork.log 2>&1 \\
  && ok "forked 'lab' from 'team'" || bad "fork failed: $(cat .migtest/fork.log)"
$Y view --meta-dir .migtest/src export --view lab --out .migtest/lab.jsonl > .migtest/exp_lab.log 2>&1 \\
  && ok "fork tree exported" || bad "fork export failed: $(cat .migtest/exp_lab.log)"
head -1 .migtest/lab.jsonl | grep -q '"kind":"fork"' \\
  && ok "header marks the tree as a fork" || bad "fork kind missing"
grep -q "this is a fork tree" .migtest/exp_lab.log \\
  && ok "export warns that import creates a writable base unless --as-fork" || bad "fork warning missing"''',
'''if [ "$BACKEND" = fdb ]; then
    # Fork is the one thing in this suite that has no FoundationDB shape: a fork
    # is a copied snapshot of a base tree, and a view here IS one subspace of the
    # cluster, with nothing to copy from. The answer is the explicit verdict the
    # backend already gives (P5), asserted exactly as section 10d of test_fdb.sh
    # asserts it — this tier exists to re-run the suite's claims, not to invent a
    # quieter second truth.
    $Y view $MSRC fork --name lab --base $VIEW1 > .migtest/fork.log 2>&1
    FRC=$?
    [ "$FRC" != 0 ] && ok "view fork refuses on an fdb:// store (rc=$FRC)" \\
        || bad "view fork was ACCEPTED on fdb:// — nothing here copies a subspace"
    grep -qi "not implemented" .migtest/fork.log && grep -qi "FoundationDB" .migtest/fork.log \\
        && ok "the refusal says what is missing and on which backend" \\
        || bad "refusal unclear: $(head -3 .migtest/fork.log)"
    grep -qiE "alias|derived view" .migtest/fork.log \\
        && ok "the refusal names the alternative that does work here" \\
        || bad "refusal offers no way forward: $(head -3 .migtest/fork.log)"
    [ ! -e .migtest/src/meta/lab.db ] && ok "the refusal minted no local fork tree" \\
        || bad "fork left a tree behind"
else
    $Y view $MSRC fork --name lab --base $VIEW1 > .migtest/fork.log 2>&1 \\
      && ok "forked 'lab' from '$VIEW1'" || bad "fork failed: $(cat .migtest/fork.log)"
    $Y view $MSRC export --view lab --out .migtest/lab.jsonl > .migtest/exp_lab.log 2>&1 \\
      && ok "fork tree exported" || bad "fork export failed: $(cat .migtest/exp_lab.log)"
    head -1 .migtest/lab.jsonl | grep -q '"kind":"fork"' \\
      && ok "header marks the tree as a fork" || bad "fork kind missing"
    grep -q "this is a fork tree" .migtest/exp_lab.log \\
      && ok "export warns that import creates a writable base unless --as-fork" || bad "fork warning missing"
fi''')

# ---- 5) the move itself -----------------------------------------------------
rep('''cp -a .migtest/src/blocks .migtest/dst/ \\
  && ok "copied only blocks/ to the target store" || bad "cp blocks failed"
[ ! -e .migtest/dst/meta/team.db ] && ok "target has no source database (metadata travels via export)" \\
  || bad "target unexpectedly already has a team.db"
$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl > .migtest/imp.log 2>&1 \\
  && ok "view import into the new store" || bad "import failed: $(cat .migtest/imp.log)"''',
'''if [ "$BACKEND" = fdb ]; then
    # The bytes do not travel, and that is the point of a content-addressed
    # object store: one deployment has one data root, the dump names blobs by
    # their content, so the migration moves metadata and zero bytes. Nothing is
    # copied here — section 6 still reads every blob through the imported
    # subspace, which is what makes the claim an assertion instead of a comment.
    ok "no blob copy: the object store belongs to the deployment, not to the store"
    $Y view $MDST export --view $VIEW2 --out .migtest/pre.jsonl > .migtest/e9.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/e9.log \\
      && ok "the destination name is not a tree yet (the metadata travels via export)" \\
      || bad "destination unexpectedly already provisioned: $(cat .migtest/e9.log)"
else
    cp -a .migtest/src/blocks .migtest/dst/ \\
      && ok "copied only blocks/ to the target store" || bad "cp blocks failed"
    [ ! -e .migtest/dst/meta/team.db ] && ok "target has no source database (metadata travels via export)" \\
      || bad "target unexpectedly already has a team.db"
fi
$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl > .migtest/imp.log 2>&1 \\
  && ok "view import into the new store" || bad "import failed: $(cat .migtest/imp.log)"''')

rep('''[ -f .migtest/dst/meta/team.db ] && [ -f .migtest/dst/meta/refs.db ] \\
  && ok "target tree + ledger exist" || bad "target tree/ledger missing"
$Y view --meta-dir .migtest/dst list > .migtest/list_dst.log 2>&1
grep -q "base view 'team' -> own tree meta/team.db" .migtest/list_dst.log \\
  && ok "view list shows the imported bare base view" || bad "list missing team: $(cat .migtest/list_dst.log)"''',
'''if [ "$BACKEND" = fdb ]; then
    # "tree + ledger exist" has no file to look at here, so the question is asked
    # of the authority: the imported subspace dumps back with the same rows, which
    # is also the evidence that the ledger the import rebuilt is the one §3.6
    # counts (one unit per content key per view) rather than a directory listing.
    $Y view $MDST export --view $VIEW2 --out .migtest/rt.jsonl --force > .migtest/e10.log 2>&1 \\
      && chk "$(tail -n +2 .migtest/rt.jsonl | wc -l | tr -d ' ')" "8" \\
         "target subspace dumps back the same 8 nodes (tree and ledger exist)" \\
      || bad "the imported subspace does not answer: $(cat .migtest/e10.log)"
    # What `view list --meta-store fdb://…` answers, stated rather than assumed:
    # the definitions (local files, C6) — a base tree is a subspace, which this
    # build does not enumerate. Existence is decided by the cluster BY NAME, which
    # the export above and the mount below are the proof of.
    $Y view $MDST list > .migtest/list_dst.log 2>&1
    grep -q "^$VIEW2\$" .migtest/list_dst.log \\
      && bad "`view list` NOW enumerates base subspaces — this assertion and docs.html's 未提供操作 row both need updating" \\
      || ok "the documented boundary holds: list answers definitions, not subspaces"
else
    [ -f .migtest/dst/meta/team.db ] && [ -f .migtest/dst/meta/refs.db ] \\
      && ok "target tree + ledger exist" || bad "target tree/ledger missing"
    $Y view $MDST list > .migtest/list_dst.log 2>&1
    grep -q "base view '$VIEW2' -> own tree meta/team.db" .migtest/list_dst.log \\
      && ok "view list shows the imported bare base view" || bad "list missing $VIEW2: $(cat .migtest/list_dst.log)"
fi''')

io.open(P, 'w', encoding='utf-8').write(s)
print('part2 ok, %d -> %d bytes' % (len(orig), len(s)))
