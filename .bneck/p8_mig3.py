#!/usr/bin/env python3
# P8 part 3 of the migrate tier: sections 6-9, plus the unmount sweep and the
# one helper the fdb branches use to ask a tree "how many nodes do you hold".
import io

P = '/data/grovefs/grovefs/.build-bin/test_migrate.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- unmount sweep + the row-count helper ------------------------------------
rep('''    fdb_ports_free "$META" "$DATA"
    fdb_up .migtest/src "$VIEW1" "$META" "$DATA"''',
'''    fdb_ports_free "$META" "$DATA"
    # Every mount this suite makes lives under .migtest/mnt_*, so an early abort
    # cannot leave one attached; fdb_up chains its own server reaper onto this.
    trap 'for d in .migtest/mnt_*; do [ -d "$d" ] && fusermount -u "$d" 2>/dev/null; done' EXIT
    fdb_up .migtest/src "$VIEW1" "$META" "$DATA"''')

rep('''chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
''',
'''chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
# tree_rows <out>: how many nodes the destination tree holds, asked of the
# destination's own authority. On SQLite the equivalent question is "does
# meta/team.db still exist", which is a fact about a file; here the file is the
# cluster, so the suite dumps the subspace and counts its rows instead.
tree_rows(){ $Y view $MDST export --view $VIEW2 --out "$1" --force > /dev/null 2>&1 \\
    && tail -n +2 "$1" | wc -l | tr -d ' ' || { echo ERR; return 1; }; }
''')

# ---- 6) mount the imported view ---------------------------------------------
rep('''$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/dst.log 2>&1 &
DP=$!
sleep 1.5
D=.migtest/mnt_dst
chk "$(T ls $D | tr '\\n' ' ')" "docs link notes.txt photos " "imported listing matches the source"''',
'''$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/dst.log 2>&1 &
DP=$!
sleep 1.5
D=.migtest/mnt_dst'''
    + '''
if [ "$BACKEND" = fdb ]; then
    mnt_gate .migtest/mnt_dst || bad ".migtest/mnt_dst never came up"
    # The root's `('i',1)` pointer is the first thing a mount asks: an imported
    # tree whose `'i` row names the dump's portable "/" instead of the frozen
    # root KEY answers ENOENT for everything, keys and all. Assert the number,
    # not that the path "is there".
    chk "$(T stat -c %i $D 2>&1)" "1" "the imported subspace answers getattr on its own root inode"
fi
chk "$(T ls $D | tr '\\n' ' ')" "docs link notes.txt photos " "imported listing matches the source"''')

# ---- 7) import guards -------------------------------------------------------
rep('''$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl > .migtest/i1.log 2>&1
[ $? -ne 0 ] && grep -q "pass --replace" .migtest/i1.log \\
  && ok "existing target tree needs --replace" || bad "replace guard: $(cat .migtest/i1.log)"
$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl --replace > .migtest/i2.log 2>&1''',
'''$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl > .migtest/i1.log 2>&1
[ $? -ne 0 ] && grep -q "pass --replace" .migtest/i1.log \\
  && ok "existing target tree needs --replace" || bad "replace guard: $(cat .migtest/i1.log)"
$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl --replace > .migtest/i2.log 2>&1''')

rep('''$Y view --meta-dir .migtest/dst alias --name pub --real team > /dev/null 2>&1
$Y view --meta-dir .migtest/dst import --view pub --in .migtest/team.jsonl > .migtest/i3.log 2>&1''',
'''$Y view $MDST alias --name pub --real $VIEW2 > /dev/null 2>&1
$Y view $MDST import --view pub --in .migtest/team.jsonl > .migtest/i3.log 2>&1''')

rep('''$Y view --meta-dir .migtest/dst rm --name pub > /dev/null 2>&1
[ ! -e .migtest/dst/meta/views/pub.layout ] && [ -f .migtest/dst/meta/team.db ] \\
  && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"''',
'''$Y view $MDST rm --name pub > /dev/null 2>&1
if [ "$BACKEND" = fdb ]; then
    # A definition IS a local file on this backend too (C6), so the alias half of
    # the check is identical; what changes is the other half — "the real tree is
    # still there" is a question about the cluster.
    [ ! -e .migtest/src/meta/views/pub.layout ] && [ "$(tree_rows .migtest/keep0.jsonl)" != "ERR" ] \\
      && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"
else
    [ ! -e .migtest/dst/meta/views/pub.layout ] && [ -f .migtest/dst/meta/team.db ] \\
      && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"
fi''')

rep('''printf '{"format":"other","version":1}\\n' > .migtest/bad.jsonl
$Y view --meta-dir .migtest/dst import --view bad1 --in .migtest/bad.jsonl > .migtest/i4.log 2>&1''',
'''printf '{"format":"other","version":1}\\n' > .migtest/bad.jsonl
$Y view $MDST import --view bad1 --in .migtest/bad.jsonl > .migtest/i4.log 2>&1''')

rep('''printf '{"format":"grovefs-view-metadata","version":99}\\n' > .migtest/bad.jsonl
$Y view --meta-dir .migtest/dst import --view bad2 --in .migtest/bad.jsonl > .migtest/i5.log 2>&1''',
'''printf '{"format":"grovefs-view-metadata","version":99}\\n' > .migtest/bad.jsonl
$Y view $MDST import --view bad2 --in .migtest/bad.jsonl > .migtest/i5.log 2>&1''')

rep('''$Y view --meta-dir .migtest/dst import --view bad3 --in .migtest/bad.jsonl > .migtest/i6.log 2>&1
[ $? -ne 0 ] && grep -q "missing or non-directory parent" .migtest/i6.log \\
  && ok "dangling parent refused before the target is touched" || bad "dangling guard: $(cat .migtest/i6.log)"
[ ! -e .migtest/dst/meta/bad3.db ] \\
  && ok "a rejected import leaves no half tree" || bad "bad3 tree left behind"''',
'''$Y view $MDST import --view bad3 --in .migtest/bad.jsonl > .migtest/i6.log 2>&1
[ $? -ne 0 ] && grep -q "missing or non-directory parent" .migtest/i6.log \\
  && ok "dangling parent refused before the target is touched" || bad "dangling guard: $(cat .migtest/i6.log)"
if [ "$BACKEND" = fdb ]; then
    # SQLite gets this guarantee by building beside the destination and renaming
    # only at the end; the cluster gets it by never provisioning the name in the
    # first place (the import opens the subspace READ-ONLY, so a dump that is
    # refused leaves not one key behind).
    $Y view $MDST export --view bad3 --out .migtest/bad3.jsonl > .migtest/i6b.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/i6b.log \\
      && ok "a rejected import leaves no keys behind" || bad "bad3 subspace left behind"
else
    [ ! -e .migtest/dst/meta/bad3.db ] \\
      && ok "a rejected import leaves no half tree" || bad "bad3 tree left behind"
fi''')

rep('''$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/keep.log 2>&1 &
KP=$!
sleep 1.5
T touch .migtest/mnt_dst/keep.txt && ok "marked the installed tree before a bad --replace" || bad "touch keep.txt failed"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $KP 2>/dev/null; wait $KP 2>/dev/null
$Y view --meta-dir .migtest/dst import --view team --in .migtest/bad.jsonl --replace > .migtest/i7.log 2>&1
[ $? -ne 0 ] && ok "a corrupt --replace is refused" || bad "corrupt --replace unexpectedly succeeded"
[ -f .migtest/dst/meta/team.db ] && ok "the old tree is still installed after the failed replace" \\
  || bad "failed replace destroyed the old tree"
$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/keep2.log 2>&1 &
KP2=$!
sleep 1.5''',
'''$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/keep.log 2>&1 &
KP=$!
sleep 1.5
[ "$BACKEND" = fdb ] && mnt_gate .migtest/mnt_dst
T touch .migtest/mnt_dst/keep.txt && ok "marked the installed tree before a bad --replace" || bad "touch keep.txt failed"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $KP 2>/dev/null; wait $KP 2>/dev/null
if [ "$BACKEND" = fdb ]; then
    BEFORE=$(tree_rows .migtest/keepA.jsonl)
    $Y view $MDST import --view $VIEW2 --in .migtest/bad.jsonl --replace > .migtest/i7.log 2>&1
    [ $? -ne 0 ] && ok "a corrupt --replace is refused" || bad "corrupt --replace unexpectedly succeeded"
    AFTER=$(tree_rows .migtest/keepB.jsonl)
    [ "$AFTER" = "$BEFORE" ] && [ "$AFTER" != "ERR" ] \\
      && ok "the old tree is still installed after the failed replace ($AFTER nodes)" \\
      || bad "failed replace damaged the tree ($BEFORE -> $AFTER)"
else
    $Y view $MDST import --view $VIEW2 --in .migtest/bad.jsonl --replace > .migtest/i7.log 2>&1
    [ $? -ne 0 ] && ok "a corrupt --replace is refused" || bad "corrupt --replace unexpectedly succeeded"
    [ -f .migtest/dst/meta/team.db ] && ok "the old tree is still installed after the failed replace" \\
      || bad "failed replace destroyed the old tree"
fi
$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/keep2.log 2>&1 &
KP2=$!
sleep 1.5
[ "$BACKEND" = fdb ] && mnt_gate .migtest/mnt_dst''')

io.open(P, 'w', encoding='utf-8').write(s)
print('part3 ok, %d -> %d bytes' % (len(orig), len(s)))
