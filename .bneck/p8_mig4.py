#!/usr/bin/env python3
# P8 part 4 of the migrate tier: sections 8 and 9.
#
# Section 8 is the import-side half of the fork story, and on FoundationDB fork
# has already been given one verdict (P5, and section 4 above): refuse, say why,
# name the alternative, leave nothing behind. So the fdb branch asserts THAT
# verdict, and the SQLite-only branch keeps the fork semantics it has always
# checked. Section 9 is backend-neutral except for how a base view comes to
# exist and how a mount reaches it, so it is patched in five places rather than
# branched wholesale.
import io

P = '/data/grovefs/grovefs/.build-bin/test_migrate.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- 8) --as-fork ------------------------------------------------------------
i8 = s.index('echo "===== 8)')
i9 = s.index('echo "===== 9)')
head, body = s[i8:i9].split('\n', 1)
body = body.rstrip('\n')
fdb8 = '''    # `--as-fork` is the import-side half of section 4's fork, and it meets the
    # same verdict for the same reason: a fork is a copied snapshot of a base
    # tree, and there is no snapshot of a subspace to copy. So the claim this
    # section makes on SQLite — fork semantics, a shared ledger, an EROFS content
    # write — has no fdb counterpart to re-run, and what runs instead is the
    # refusal the backend actually gives, asserted with the same four checks
    # section 4 uses: it refuses, it says on which backend and why, it names the
    # way forward, and it provisions neither a subspace nor a definition.
    $Y view $MDST import --view labx --in .migtest/team.jsonl --as-fork $VIEW1 > .migtest/f1.log 2>&1
    [ $? -ne 0 ] && ok "import --as-fork refuses on an fdb:// store" \\
        || bad "--as-fork was ACCEPTED on fdb://: $(cat .migtest/f1.log)"
    grep -q "as-fork" .migtest/f1.log && grep -qi "not implemented" .migtest/f1.log \\
        && ok "the refusal names the flag it refused and what is missing" \\
        || bad "refusal unclear: $(cat .migtest/f1.log)"
    grep -qiE "alias|derived view" .migtest/f1.log \\
        && ok "the refusal names the alternative that works on every backend" \\
        || bad "refusal offers no way forward: $(cat .migtest/f1.log)"
    $Y view $MDST export --view labx --out .migtest/labx.jsonl > .migtest/f1b.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/f1b.log \\
        && ok "the refusal provisioned no subspace" || bad "labx answers after the refusal"
    [ ! -e .migtest/src/meta/views/labx.layout ] && ok "the refusal wrote no definition slot" \\
        || bad "labx.layout left behind"'''
s = s[:i8] + head + '\nif [ "$BACKEND" = fdb ]; then\n' + fdb8 + '\nelse\n' \
    + '\n'.join('    ' + l if l.strip() else l for l in body.split('\n')) + '\nfi\n\n' + s[i9:]

# The fork body below the `else` is now SQLite-only, so it keeps its own
# spellings (`team`, `--meta-dir .migtest/dst`) exactly as the suite always had
# them — parameterizing a branch that can never see another backend would only
# make the two backends look more alike than they are.

# ---- 9) several base views ---------------------------------------------------
rep('''$Y init --view alpha > /dev/null 2>&1 && ok "created base view 'alpha'" || bad "init alpha failed"
$Y init --view beta  > /dev/null 2>&1 && ok "created base view 'beta'"  || bad "init beta failed"
$Y mount --view alpha -m .migtest/mnt_alpha --no-cache --foreground > .migtest/alpha.log 2>&1 &
AP=$!
$Y mount --view beta -m .migtest/mnt_beta --no-cache --foreground > .migtest/beta.log 2>&1 &
BP=$!
sleep 1.5
''',
'''if [ "$BACKEND" = fdb ]; then
    # Two more BASE views, which here means two more subspaces of the same cluster
    # under the same control plane: `view create` is the only verb that makes a
    # tree on this backend, and it is the very one the mounts below read through.
    $Y view $MSRC create --view $VIEWA > /dev/null 2>&1 && ok "created base view '$VIEWA' as a subspace" || bad "create $VIEWA failed"
    $Y view $MSRC create --view $VIEWB > /dev/null 2>&1 && ok "created base view '$VIEWB' as a subspace" || bad "create $VIEWB failed"
else
    $Y init --view alpha > /dev/null 2>&1 && ok "created base view 'alpha'" || bad "init alpha failed"
    $Y init --view beta  > /dev/null 2>&1 && ok "created base view 'beta'"  || bad "init beta failed"
fi
$Y mount --view $VIEWA -m .migtest/mnt_alpha --no-cache $REMOTE --foreground > .migtest/alpha.log 2>&1 &
AP=$!
$Y mount --view $VIEWB -m .migtest/mnt_beta --no-cache $REMOTE --foreground > .migtest/beta.log 2>&1 &
BP=$!
sleep 1.5
if [ "$BACKEND" = fdb ]; then
    # Three views live under one control plane and one cluster here, so "each one
    # manages its own tree" is the claim this section exists to make — it needs
    # both mounts to be real before a single byte of it means anything.
    mnt_gate .migtest/mnt_alpha || bad ".migtest/mnt_alpha never came up"
    mnt_gate .migtest/mnt_beta  || bad ".migtest/mnt_beta never came up"
fi
''')

rep('''$Y view --meta-dir .migtest/src list > .migtest/list_all.log 2>&1
grep -q "base view 'team'" .migtest/list_all.log \\
  && grep -q "base view 'alpha'" .migtest/list_all.log \\
  && grep -q "base view 'beta'" .migtest/list_all.log \\
  && ok "view list names every base view" || bad "multi-base list wrong: $(cat .migtest/list_all.log)"
$Y mount --view team -m .migtest/mnt_all --no-cache --foreground > .migtest/all.log 2>&1 &
TP=$!
sleep 1.5
''',
'''$Y view $MSRC list > .migtest/list_all.log 2>&1
if [ "$BACKEND" = fdb ]; then
    # The same boundary section 5 stated, now with four bases in the control plane
    # instead of one: `view list` answers the definitions, which are files (C6),
    # and does not enumerate subspaces. "Every base view is known" is therefore
    # asked of the authority that decides it — each name dumps its OWN rows, and
    # alpha's dump names alpha.txt without ever having seen beta's.
    grep -q "^$VIEWA\$" .migtest/list_all.log \\
      && bad "`view list` NOW enumerates base subspaces — this assertion and docs.html's 未提供操作 row both need updating" \\
      || ok "the documented boundary holds over a four-base control plane too"
    $Y view $MSRC export --view $VIEWA --out .migtest/la.jsonl --force > .migtest/l_a.log 2>&1 \\
      && grep -q '"name":"alpha.txt"' .migtest/la.jsonl && ! grep -q '"name":"beta.txt"' .migtest/la.jsonl \\
      && ok "'$VIEWA' is a base subspace holding its own tree" || bad "alpha subspace dump wrong: $(cat .migtest/l_a.log)"
    $Y view $MSRC export --view $VIEWB --out .migtest/lb.jsonl --force > .migtest/l_b.log 2>&1 \\
      && grep -q '"name":"beta.txt"' .migtest/lb.jsonl && ! grep -q '"name":"alpha.txt"' .migtest/lb.jsonl \\
      && ok "'$VIEWB' is a base subspace holding its own tree" || bad "beta subspace dump wrong: $(cat .migtest/l_b.log)"
else
    grep -q "base view 'team'" .migtest/list_all.log \\
      && grep -q "base view 'alpha'" .migtest/list_all.log \\
      && grep -q "base view 'beta'" .migtest/list_all.log \\
      && ok "view list names every base view" || bad "multi-base list wrong: $(cat .migtest/list_all.log)"
fi
$Y mount --view $VIEW1 -m .migtest/mnt_all --no-cache $REMOTE --foreground > .migtest/all.log 2>&1 &
TP=$!
sleep 1.5
[ "$BACKEND" = fdb ] && mnt_gate .migtest/mnt_all || true
''')

io.open(P, 'w', encoding='utf-8').write(s)
print('part4 ok, %d -> %d bytes' % (len(orig), len(s)))
