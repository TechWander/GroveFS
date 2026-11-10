#!/usr/bin/env python3
# P8 part 1 of the import tier: header, sections 1 and 2.
#
# `grovefs import` is a LOCAL-store tool — its whole point is that an imported
# object is a second name on the SOURCE inode inside that store's own `blocks/`
# tree. That property does not depend on the metadata backend and cannot be
# faked under `fdb://`, so the backend's answer is a refusal (already in main.rs,
# refuse-not-downgrade). What this tier therefore re-runs is the migration path
# the refusal points at: `import` stages the tree in a local store (the shared
# assertions of sections 1-5 stay attached to that store), the dump becomes a
# subspace, and every fidelity claim the SQLite tier makes about the mounted
# result is made again about a mounted FOUNDATIONDB result.
import io

P = '/data/grovefs/grovefs/.build-bin/test_import.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- header ------------------------------------------------------------------
rep('''Y=./target/release/grovefs
rm -rf .itest
mkdir -p .itest/home .itest/src/photos/2026 .itest/src/docs .itest/mnt
export GROVEFS_HOME=$PWD/.itest/home
''',
'''Y=./target/release/grovefs
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
''')

# ---- 1) dry run / refusal ----------------------------------------------------
rep('''echo "===== 1) --dry-run reports without touching anything ====="
$Y import --view legacy --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1 \\
  && ok "dry run exits 0" || bad "dry run failed: $(cat .itest/dry.log)"
grep -q "would mirror 4 dir(s) and 3 file(s)" .itest/dry.log \\
  && ok "dry run counts dirs/files it would import" || bad "dry run tally wrong: $(cat .itest/dry.log)"
[ ! -e .itest/home/meta/legacy.db ] && ok "dry run did NOT even create the view db" || bad "dry run touched the store"
''',
'''echo "===== 1) --dry-run reports without touching anything ====="
if [ "$BACKEND" = fdb ]; then
    # The zero-copy mirror cannot be honoured here: its objects would have to be
    # hard links into a data-server's store, and its metadata rows would have to
    # be a `meta/<view>.db` beside a root whose server serves the cluster. So the
    # command refuses — and refuses BEFORE `--dry-run` is considered, because a
    # promise to touch nothing is not a licence to plan against the wrong backend.
    $Y import $MSARG --view $VIEW --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/dry.log \\
      && ok "an fdb:// deployment refuses `import` instead of faking it" \\
      || bad "import should refuse on fdb://: $(cat .itest/dry.log)"
    grep -q "view create" .itest/dry.log && grep -q "mounted path" .itest/dry.log \\
      && ok "the refusal names the way that does work here" || bad "refusal unclear: $(cat .itest/dry.log)"
    [ ! -e .itest/fdbhome/meta/$VIEW.db ] && [ ! -e .itest/home/meta/$VIEW.db ] \\
      && ok "the refusal created no view db anywhere" || bad "refused import touched a store"
    GROVEFS_META_STORE="$FDBURI" $Y import --view $VIEW --source .itest/src > .itest/dry2.log 2>&1
    [ $? -ne 0 ] && grep -q "GROVEFS_META_STORE" .itest/dry2.log \\
      && ok "the env behind --meta-store refuses too (no bypass by spelling)" \\
      || bad "GROVEFS_META_STORE should refuse: $(cat .itest/dry2.log)"
else
    $Y import --view legacy --source .itest/src --prefix /legacy --dry-run > .itest/dry.log 2>&1 \\
      && ok "dry run exits 0" || bad "dry run failed: $(cat .itest/dry.log)"
    grep -q "would mirror 4 dir(s) and 3 file(s)" .itest/dry.log \\
      && ok "dry run counts dirs/files it would import" || bad "dry run tally wrong: $(cat .itest/dry.log)"
    [ ! -e .itest/home/meta/legacy.db ] && ok "dry run did NOT even create the view db" || bad "dry run touched the store"
fi
''')

# ---- 2) the import, then the move --------------------------------------------
rep('''$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp.log 2>&1 \\
  && ok "import exits 0" || bad "import failed: $(cat .itest/imp.log)"''',
'''# This first command is a LOCAL `import` on BOTH tiers — on fdb it is the staging
# step the refusal above points at, and its tally/link assertions are exactly as
# true there as they are on the SQLite tier, because a local store is what it is.
$Y import --view legacy --source .itest/src --prefix /legacy > .itest/imp.log 2>&1 \\
  && ok "import exits 0" || bad "import failed: $(cat .itest/imp.log)"''')

rep('''$Y mount --view legacy -m .itest/mnt --no-cache --foreground > .itest/mnt.log 2>&1 &
MP=$!
sleep 1.5
cleanup(){ fusermount -u .itest/mnt .itest/mnt_r .itest/mnt_v 2>/dev/null; kill $MP $MSP $DSP $RP $VP 2>/dev/null; }
trap cleanup EXIT
M=.itest/mnt
''',
'''if [ "$BACKEND" = fdb ]; then
    # The second half of the route is the migration itself: the staged tree leaves
    # as a dump, its blobs are handed to the deployment's object store (named by
    # content, so a copy is a rename-free overwrite), and the dump lands in a
    # subspace nothing provisioned. Everything below this line then asks the SAME
    # questions of a FoundationDB mount that the SQLite tier asks of a local one.
    $Y view --meta-dir .itest/home export --view legacy --out .itest/legacy.jsonl > .itest/exp.log 2>&1 \\
      && ok "exported the staged tree" || bad "export failed: $(cat .itest/exp.log)"
    cp -a .itest/home/blocks .itest/fdbhome/data/ \\
      && ok "handed the blobs to the deployment's object store" || bad "blob copy failed"
    $Y view $VDIR import --view $VIEW --in .itest/legacy.jsonl > .itest/vimp.log 2>&1 \\
      && ok "the dump became a subspace no `view create` ever provisioned" \\
      || bad "subspace import failed: $(head -3 .itest/vimp.log)"
fi
$Y mount --view $VIEW -m .itest/mnt --no-cache $REMOTE --foreground > .itest/mnt.log 2>&1 &
MP=$!
sleep 1.5
# `kp`/`wk` rather than a bare `kill`: on the fdb tier the servers belong to the
# run and are named by $FDB_MP/$FDB_DP, while on this tier they are $MSP/$DSP —
# and an empty placeholder handed to `kill` means the whole process group.
cleanup(){ fusermount -u .itest/mnt .itest/mnt_r .itest/mnt_v 2>/dev/null
           kp $MP $MSP $DSP $RP $VP $FDB_MP $FDB_DP; wk $MP $MSP $DSP $RP $VP $FDB_MP $FDB_DP; }
trap cleanup EXIT
M=.itest/mnt
if [ "$BACKEND" = fdb ]; then
    mnt_gate .itest/mnt || bad ".itest/mnt never came up"
    # The moved tree must answer for its own root, by number: an import whose root
    # pointer named the dump's portable "/" instead of the frozen root key would
    # leave every path below it answering ENOENT while all the keys were present.
    chk "$(T stat -c %i $M 2>&1)" "1" "the moved tree answers getattr on its own root inode"
fi
''')

io.open(P, 'w', encoding='utf-8').write(s)
print('import part1 ok, %d -> %d bytes' % (len(orig), len(s)))
