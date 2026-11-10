#!/usr/bin/env python3
# P8: make test_migrate.sh's deployment a PARAMETER (MIGRATE_BACKEND=fdb), so
# every assertion in the file runs against FoundationDB subspaces as well as
# against local SQLite trees. Only how a view comes into existence, how a mount
# reaches it, and where the bytes live change — the claims stay word for word.
import io

P = '/data/grovefs/grovefs/.build-bin/test_migrate.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new)

# ---- header: backend parameter, names, one server pair -----------------------
rep('''Y=./target/release/grovefs
rm -rf .migtest
mkdir -p .migtest/src .migtest/dst .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab \\
         .migtest/mnt_alpha .migtest/mnt_beta .migtest/mnt_all
export GROVEFS_HOME=$PWD/.migtest/src
''',
'''Y=./target/release/grovefs
# The metadata backend is a PARAMETER (P8): `MIGRATE_BACKEND=fdb` re-runs every
# assertion below against FoundationDB subspaces instead of local SQLite trees.
# Three things change, and none of them is an assertion:
#   * how a view comes into existence — `init --manifest` writing a db file, vs.
#     `view create` provisioning a subspace that is then seeded THROUGH the mount;
#   * how a mount reaches the authoritative tree — a local `--home` mount, vs. a
#     mount of one remote view through the meta/data server pair;
#   * where the bytes live — a per-store `blocks/` directory that has to be
#     copied, vs. the deployment's one object store, which needs no copy at all
#     because a blob is named by its content (section 5 says so out loud).
# The destination is a different SUBSPACE rather than a second root on purpose:
# one cluster is one authority and one control plane (C6), so two roots over the
# same cluster would be one deployment pretending to be two.
BACKEND="${MIGRATE_BACKEND:-sqlite}"
. .build-bin/fdb_scaffold.sh
fdb_gate
fdb_binary
rm -rf .migtest
mkdir -p .migtest/src .migtest/dst .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab \\
         .migtest/mnt_alpha .migtest/mnt_beta .migtest/mnt_all
export GROVEFS_HOME=$PWD/.migtest/src

# The view NAMES are a parameter too. On SQLite the story this suite tells is
# "the same name, moved to a different store", and the names can stay `team`.
# A subspace outlives a run (`view rm` of a cluster tree is not implemented), so
# an fdb run uses per-run names and a distinct destination name — the one place
# the two backends cannot share a spelling, because a name that is already a
# subspace cannot also be the target of the import into the same control plane.
if [ "$BACKEND" = fdb ]; then
    TEAM="mt$(date +%s)"
    VIEW1=$TEAM; VIEW2="${TEAM}_b"; VIEWA="${TEAM}_a"; VIEWB="${TEAM}_r"
    # `$MSRC`/`$MDST` are how a view COMMAND addresses its control plane; on
    # SQLite they are the two stores the suite is moving between, on fdb they are
    # the same root twice, because the trees are subspaces and only the names
    # moved. `$GHOST` is what "this name has no tree" is called on each backend.
    MSRC="$MSARG --meta-dir .migtest/src"
    MDST="$MSRC"
    GHOST="not a provisioned subspace"
    META=127.0.0.1:15408
    DATA=127.0.0.1:15409
    REMOTE="--meta-server $META --data-servers $DATA"
    fdb_ports_free "$META" "$DATA"
    fdb_up .migtest/src "$VIEW1" "$META" "$DATA"
else
    VIEW1=team; VIEW2=team; VIEWA=alpha; VIEWB=beta
    MSRC="--meta-dir .migtest/src"
    MDST="--meta-dir .migtest/dst"
    GHOST="not a physical tree"
fi
''')

# ---- section 1: seeding the source -----------------------------------------
rep('''$Y init --view team --manifest .migtest/team.manifest > .migtest/init.log 2>&1 \\
  && ok "seeded physical base view 'team' at .migtest/src" || { bad "init failed: $(cat .migtest/init.log)"; exit 1; }
''',
'''if [ "$BACKEND" = fdb ]; then
    # `fdb_up` provisioned the subspace and started the servers; the same
    # manifest file is applied through the mount a few lines below, because
    # `init --manifest` writes a db file and a subspace has no file to write.
    ok "seeded a physical base view as a subspace of the cluster"
else
    $Y init --view $VIEW1 --manifest .migtest/team.manifest > .migtest/init.log 2>&1 \\
      && ok "seeded physical base view '$VIEW1' at .migtest/src" || { bad "init failed: $(cat .migtest/init.log)"; exit 1; }
fi
''')

rep('''$Y mount --view team -m .migtest/mnt_src --no-cache --foreground > .migtest/src.log 2>&1 &
SP=$!
sleep 1.5
S=.migtest/mnt_src
''',
'''$Y mount --view $VIEW1 -m .migtest/mnt_src --no-cache $REMOTE --foreground > .migtest/src.log 2>&1 &
SP=$!
sleep 1.5
S=.migtest/mnt_src
if [ "$BACKEND" = fdb ]; then
    # The board's standing fail-fast gate, then the seeding the SQLite `init
    # --manifest` did offline: the identical manifest, applied through the mount.
    mnt_gate .migtest/mnt_src || bad ".migtest/mnt_src never came up"
    seed_through_mount .migtest/mnt_src .migtest/team.manifest \\
        && ok "the manifest was applied through the mount (subspace tree seeded)" \\
        || bad "seeding the subspace tree failed"
fi
''')

io.open(P, 'w', encoding='utf-8').write(s)
print('part1 ok, %d -> %d bytes' % (len(orig), len(s)))
