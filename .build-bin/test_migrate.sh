#!/usr/bin/env bash
# e2e for METADATA MIGRATION (`view export` / `view import`) and MULTIPLE
# INDEPENDENT BASE VIEWS. The store is moved to a DIFFERENT absolute path (a
# stand-in for "another server"): only the portable JSONL export plus the
# `blocks/` object tree travel, no meta/*.db is copied. Covers:
#   - export while mounted (snapshot read) -> header + one JSON line per node;
#   - the export is host-neutral: no absolute store path, no `blocks/` path,
#     no hostname — blobs are named by `data_ref` only;
#   - export guards: virtual names (alias/derived) refused, missing tree
#     refused, an existing --out needs --force;
#   - import into a fresh store: ids/inodes (hard links), mode, uid/gid,
#     symlink targets and timestamps survive; ledger rebuilt;
#   - the imported view is a normal WRITABLE base view and reads the copied
#     bytes from the new path (no hardware/absolute-path binding);
#   - import guards: no silent --replace, definition-slot conflict, bad
#     header/version, dangling parent, header/body count mismatch;
#   - `--as-fork <base>` restores fork semantics (structure writable, content
#     EROFS) and shares blobs through the rebuilt cross-view ledger;
#   - several base views coexist, each with its own tree and its own bytes.
# Assertion-based; emits a SUMMARY line run_all reads.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
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
mkdir -p .migtest/src .migtest/dst .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab \
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
    # Every mount this suite makes lives under .migtest/mnt_*, so an early abort
    # cannot leave one attached; fdb_up chains its own server reaper onto this.
    trap 'for d in .migtest/mnt_*; do [ -d "$d" ] && fusermount -u "$d" 2>/dev/null; done; fdb_drop_views .migtest/src .migtest/src/data "${VIEW1:-}" "${VIEW2:-}" "${VIEWA:-}" "${VIEWB:-}"' EXIT
    fdb_up .migtest/src "$VIEW1" "$META" "$DATA"
else
    VIEW1=team; VIEW2=team; VIEWA=alpha; VIEWB=beta
    MSRC="--meta-dir .migtest/src"
    MDST="--meta-dir .migtest/dst"
    GHOST="not a physical tree"
fi

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
rofs(){ grep -qE "Read-only file system|只读文件系统" "$1"; }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
# tree_rows <out>: how many nodes the destination tree holds, asked of the
# destination's own authority. On SQLite the equivalent question is "does
# meta/team.db still exist", which is a fact about a file; here the file is the
# cluster, so the suite dumps the subspace and counts its rows instead.
tree_rows(){ $Y view $MDST export --view $VIEW2 --out "$1" --force > /dev/null 2>&1 \
    && tail -n +2 "$1" | wc -l | tr -d ' ' || { echo ERR; return 1; }; }

cat > .migtest/team.manifest <<'EOF'
mkdir docs
touch docs/a.md
mkdir photos
touch photos/x.jpg
touch notes.txt
EOF
if [ "$BACKEND" = fdb ]; then
    # `fdb_up` provisioned the subspace and started the servers; the same
    # manifest file is applied through the mount a few lines below, because
    # `init --manifest` writes a db file and a subspace has no file to write.
    ok "seeded a physical base view as a subspace of the cluster"
else
    $Y init --view $VIEW1 --manifest .migtest/team.manifest > .migtest/init.log 2>&1 \
      && ok "seeded physical base view '$VIEW1' at .migtest/src" || { bad "init failed: $(cat .migtest/init.log)"; exit 1; }
fi

echo "===== 1) fill the source view with content, links and metadata ====="
$Y mount --view $VIEW1 -m .migtest/mnt_src --no-cache $REMOTE --foreground > .migtest/src.log 2>&1 &
SP=$!
sleep 1.5
S=.migtest/mnt_src
if [ "$BACKEND" = fdb ]; then
    # The board's standing fail-fast gate, then the seeding the SQLite `init
    # --manifest` did offline: the identical manifest, applied through the mount.
    mnt_gate .migtest/mnt_src || bad ".migtest/mnt_src never came up"
    seed_through_mount .migtest/mnt_src .migtest/team.manifest \
        && ok "the manifest was applied through the mount (subspace tree seeded)" \
        || bad "seeding the subspace tree failed"
fi
T sh -c "printf 'report-body' > $S/docs/a.md" && ok "wrote docs/a.md" || bad "write a.md failed"
T sh -c "printf 'priv' > $S/photos/x.jpg"           && ok "wrote photos/x.jpg" || bad "write x.jpg failed"
T sh -c "printf 'notes-body' > $S/notes.txt"        && ok "wrote notes.txt" || bad "write notes.txt failed"
T chmod 640 $S/docs/a.md                            && ok "chmod 640 docs/a.md" || bad "chmod failed"
T ln $S/docs/a.md $S/docs/b.md && ok "hard-linked docs/b.md -> docs/a.md" || bad "ln failed"
T ln -s docs/a.md $S/link && ok "created symlink 'link' -> docs/a.md" || bad "symlink failed"

echo "===== 2) export WHILE MOUNTED (snapshot read) ====="
$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl > .migtest/exp.log 2>&1 \
  && ok "view export from a live mount" || bad "export failed: $(cat .migtest/exp.log)"
grep -q "exported view '$VIEW1' metadata: 8 node(s), 3 blob reference(s)" .migtest/exp.log \
  && ok "export reports 8 nodes / 3 distinct blobs" || bad "export counts: $(cat .migtest/exp.log)"
grep -q "metadata only" .migtest/exp.log \
  && ok "export states it is metadata only (copy blocks/ too)" || bad "metadata-only note missing"
chk "$(wc -l < .migtest/team.jsonl)" "9" "export is header + one line per node"
head -1 .migtest/team.jsonl | grep -q '"format":"grovefs-view-metadata"' \
  && ok "header names the portable format" || bad "format marker missing"
head -1 .migtest/team.jsonl | grep -q '"version":1' \
  && ok "header carries the format version" || bad "version missing"
head -1 .migtest/team.jsonl | grep -q '"kind":"physical"' \
  && ok "a bare base tree exports as kind=physical" || bad "kind missing"
grep -q "\"uid\":$(id -u)" .migtest/team.jsonl \
  && ok "ownership travels in the export (advisory uid/gid)" || bad "uid missing from export"
if grep -qF "$PWD" .migtest/team.jsonl || grep -qF "$(hostname)" .migtest/team.jsonl \
   || grep -qF "/blocks/" .migtest/team.jsonl; then
  bad "export leaked a host path/hostname/blocks path"
else
  ok "export is host-neutral (no absolute path, hostname or blocks path)"
fi

echo "===== 3) export guards: virtual views, missing tree, no silent overwrite ====="
$Y view $MSRC export --view ghost --out .migtest/ghost.jsonl > .migtest/e1.log 2>&1
[ $? -ne 0 ] && grep -qF "$GHOST" .migtest/e1.log \
  && ok "exporting a view with no tree of its own refused" || bad "ghost export guard: $(cat .migtest/e1.log)"
$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl > .migtest/e2.log 2>&1
[ $? -ne 0 ] && grep -q "already exists" .migtest/e2.log \
  && ok "an existing --out is not silently overwritten" || bad "overwrite guard: $(cat .migtest/e2.log)"
$Y view $MSRC export --view $VIEW1 --out .migtest/team.jsonl --force > /dev/null 2>&1 \
  && ok "--force allows the overwrite" || bad "--force export failed"
echo "only /docs" > .migtest/docs.layout
$Y view $MSRC define --name lens --base $VIEW1 --rules .migtest/docs.layout > .migtest/e3.log 2>&1
$Y view $MSRC export --view lens --out .migtest/lens.jsonl > .migtest/e4.log 2>&1
[ $? -ne 0 ] && grep -q "derived (virtual) view" .migtest/e4.log \
  && ok "derived view refused (no tree of its own; export the base)" || bad "derived export guard: $(cat .migtest/e4.log)"
$Y view $MSRC alias --name pub --real $VIEW1 > .migtest/e5.log 2>&1
$Y view $MSRC export --view pub --out .migtest/pub.jsonl > .migtest/e6.log 2>&1
[ $? -ne 0 ] && grep -q "pure alias" .migtest/e6.log \
  && ok "alias refused (export the real view)" || bad "alias export guard: $(cat .migtest/e6.log)"
# An alias makes a direct mount of the real name illegal (existing rule), so drop
# the alias and the lens again: the rest of the suite mounts 'team' by its name.
$Y view $MSRC rm --name pub > /dev/null 2>&1
$Y view $MSRC rm --name lens > /dev/null 2>&1

echo "===== 4) a fork exports as kind=fork, with a warning ====="
if [ "$BACKEND" = fdb ]; then
    # Fork is the one thing in this suite that has no FoundationDB shape: a fork
    # is a copied snapshot of a base tree, and a view here IS one subspace of the
    # cluster, with nothing to copy from. The answer is the explicit verdict the
    # backend already gives (P5), asserted exactly as section 10d of test_fdb.sh
    # asserts it — this tier exists to re-run the suite's claims, not to invent a
    # quieter second truth.
    $Y view $MSRC fork --name lab --base $VIEW1 > .migtest/fork.log 2>&1
    FRC=$?
    [ "$FRC" != 0 ] && ok "view fork refuses on an fdb:// store (rc=$FRC)" \
        || bad "view fork was ACCEPTED on fdb:// — nothing here copies a subspace"
    grep -qi "not implemented" .migtest/fork.log && grep -qi "FoundationDB" .migtest/fork.log \
        && ok "the refusal says what is missing and on which backend" \
        || bad "refusal unclear: $(head -3 .migtest/fork.log)"
    grep -qiE "alias|derived view" .migtest/fork.log \
        && ok "the refusal names the alternative that does work here" \
        || bad "refusal offers no way forward: $(head -3 .migtest/fork.log)"
    [ ! -e .migtest/src/meta/lab.db ] && ok "the refusal minted no local fork tree" \
        || bad "fork left a tree behind"
else
    $Y view $MSRC fork --name lab --base $VIEW1 > .migtest/fork.log 2>&1 \
      && ok "forked 'lab' from '$VIEW1'" || bad "fork failed: $(cat .migtest/fork.log)"
    $Y view $MSRC export --view lab --out .migtest/lab.jsonl > .migtest/exp_lab.log 2>&1 \
      && ok "fork tree exported" || bad "fork export failed: $(cat .migtest/exp_lab.log)"
    head -1 .migtest/lab.jsonl | grep -q '"kind":"fork"' \
      && ok "header marks the tree as a fork" || bad "fork kind missing"
    grep -q "this is a fork tree" .migtest/exp_lab.log \
      && ok "export warns that import creates a writable base unless --as-fork" || bad "fork warning missing"
fi
fusermount -u .migtest/mnt_src 2>/dev/null; kill $SP 2>/dev/null; wait $SP 2>/dev/null

echo "===== 5) migrate bytes + metadata to a DIFFERENT absolute store path ====="
if [ "$BACKEND" = fdb ]; then
    # The bytes do not travel, and that is the point of a content-addressed
    # object store: one deployment has one data root, the dump names blobs by
    # their content, so the migration moves metadata and zero bytes. Nothing is
    # copied here — section 6 still reads every blob through the imported
    # subspace, which is what makes the claim an assertion instead of a comment.
    ok "no blob copy: the object store belongs to the deployment, not to the store"
    $Y view $MDST export --view $VIEW2 --out .migtest/pre.jsonl > .migtest/e9.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/e9.log \
      && ok "the destination name is not a tree yet (the metadata travels via export)" \
      || bad "destination unexpectedly already provisioned: $(cat .migtest/e9.log)"
else
    cp -a .migtest/src/blocks .migtest/dst/ \
      && ok "copied only blocks/ to the target store" || bad "cp blocks failed"
    [ ! -e .migtest/dst/meta/team.db ] && ok "target has no source database (metadata travels via export)" \
      || bad "target unexpectedly already has a team.db"
fi
$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl > .migtest/imp.log 2>&1 \
  && ok "view import into the new store" || bad "import failed: $(cat .migtest/imp.log)"
grep -q "8 node(s), 3 blob reference(s)" .migtest/imp.log \
  && ok "import reports the same node/blob counts" || bad "import counts: $(cat .migtest/imp.log)"
grep -q "cross-view ledger rebuilt" .migtest/imp.log \
  && ok "cross-view ledger rebuilt after import" || bad "ledger note missing"
if [ "$BACKEND" = fdb ]; then
    # "tree + ledger exist" has no file to look at here, so the question is asked
    # of the authority: the imported subspace dumps back with the same rows, which
    # is also the evidence that the ledger the import rebuilt is the one §3.6
    # counts (one unit per content key per view) rather than a directory listing.
    $Y view $MDST export --view $VIEW2 --out .migtest/rt.jsonl --force > .migtest/e10.log 2>&1 \
      && chk "$(tail -n +2 .migtest/rt.jsonl | wc -l | tr -d ' ')" "8" \
         "target subspace dumps back the same 8 nodes (tree and ledger exist)" \
      || bad "the imported subspace does not answer: $(cat .migtest/e10.log)"
    # What `view list --meta-store fdb://…` answers, stated rather than assumed:
    # the definitions (local files, C6) — a base tree is a subspace, which this
    # build does not enumerate. Existence is decided by the cluster BY NAME, which
    # the export above and the mount below are the proof of.
    $Y view $MDST list > .migtest/list_dst.log 2>&1
    grep -q "^$VIEW2\$" .migtest/list_dst.log \
      && bad "view list NOW enumerates base subspaces — this assertion and docs.html's 未提供操作 row both need updating" \
      || ok "the documented boundary holds: list answers definitions, not subspaces"
else
    [ -f .migtest/dst/meta/team.db ] && [ -f .migtest/dst/meta/refs.db ] \
      && ok "target tree + ledger exist" || bad "target tree/ledger missing"
    $Y view $MDST list > .migtest/list_dst.log 2>&1
    grep -q "base view '$VIEW2' -> own tree meta/team.db" .migtest/list_dst.log \
      && ok "view list shows the imported bare base view" || bad "list missing $VIEW2: $(cat .migtest/list_dst.log)"
fi

echo "===== 6) mount the imported view at the new path: identical tree, writable ====="
$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/dst.log 2>&1 &
DP=$!
sleep 1.5
D=.migtest/mnt_dst
if [ "$BACKEND" = fdb ]; then
    mnt_gate .migtest/mnt_dst || bad ".migtest/mnt_dst never came up"
    # The root's `('i',1)` pointer is the first thing a mount asks: an imported
    # tree whose `'i` row names the dump's portable "/" instead of the frozen
    # root KEY answers ENOENT for everything, keys and all. Assert the number,
    # not that the path "is there".
    chk "$(T stat -c %i $D 2>&1)" "1" "the imported subspace answers getattr on its own root inode"
fi
chk "$(T ls $D | tr '\n' ' ')" "docs link notes.txt photos " "imported listing matches the source"
chk "$(T cat $D/docs/a.md)" "report-body" "content readable from the copied blocks/ (no path binding)"
chk "$(T cat $D/photos/x.jpg)" "priv" "second blob readable"
chk "$(T readlink $D/link)" "docs/a.md" "symlink target preserved"
chk "$(T stat -c %i $D/docs/a.md)" "$(T stat -c %i $D/docs/b.md)" "hard link preserved (one inode, two names)"
chk "$(T stat -c %a $D/docs/a.md)" "640" "mode preserved"
T sh -c "printf '+more' >> $D/notes.txt" && ok "imported view is a normal WRITABLE base" || bad "imported view refused a write"
T touch $D/new-after-import.txt && ok "new nodes created after import" || bad "touch after import failed"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $DP 2>/dev/null; wait $DP 2>/dev/null

echo "===== 7) import guards: replace, definition slot, corrupt input ====="
$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl > .migtest/i1.log 2>&1
[ $? -ne 0 ] && grep -q "pass --replace" .migtest/i1.log \
  && ok "existing target tree needs --replace" || bad "replace guard: $(cat .migtest/i1.log)"
$Y view $MDST import --view $VIEW2 --in .migtest/team.jsonl --replace > .migtest/i2.log 2>&1
[ $? -eq 0 ] && grep -q "replacing the previous tree" .migtest/i2.log \
  && ok "--replace re-imports over an existing tree" || bad "--replace failed: $(cat .migtest/i2.log)"
$Y view $MDST alias --name pub --real $VIEW2 > /dev/null 2>&1
$Y view $MDST import --view pub --in .migtest/team.jsonl > .migtest/i3.log 2>&1
[ $? -ne 0 ] && grep -q "already has a definition" .migtest/i3.log \
  && ok "importing onto a definition slot refused" || bad "definition guard: $(cat .migtest/i3.log)"
# The alias also makes a direct mount of the real name illegal (existing rule),
# so drop it again before the mount checks below.
$Y view $MDST rm --name pub > /dev/null 2>&1
if [ "$BACKEND" = fdb ]; then
    # A definition IS a local file on this backend too (C6), so the alias half of
    # the check is identical; what changes is the other half — "the real tree is
    # still there" is a question about the cluster.
    [ ! -e .migtest/src/meta/views/pub.layout ] && [ "$(tree_rows .migtest/keep0.jsonl)" != "ERR" ] \
      && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"
else
    [ ! -e .migtest/dst/meta/views/pub.layout ] && [ -f .migtest/dst/meta/team.db ] \
      && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"
fi
printf '{"format":"other","version":1}\n' > .migtest/bad.jsonl
$Y view $MDST import --view bad1 --in .migtest/bad.jsonl > .migtest/i4.log 2>&1
[ $? -ne 0 ] && grep -q "unknown format" .migtest/i4.log \
  && ok "unknown export format refused" || bad "format guard: $(cat .migtest/i4.log)"
printf '{"format":"grovefs-view-metadata","version":99}\n' > .migtest/bad.jsonl
$Y view $MDST import --view bad2 --in .migtest/bad.jsonl > .migtest/i5.log 2>&1
[ $? -ne 0 ] && grep -q "unsupported export version" .migtest/i5.log \
  && ok "future format version refused" || bad "version guard: $(cat .migtest/i5.log)"
printf '%s\n%s\n%s\n' \
  '{"format":"grovefs-view-metadata","version":1,"nodes":2}' \
  '{"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1}' \
  '{"id":2,"ino":2,"parent_id":99,"name":"f","is_dir":0}' > .migtest/bad.jsonl
$Y view $MDST import --view bad3 --in .migtest/bad.jsonl > .migtest/i6.log 2>&1
[ $? -ne 0 ] && grep -q "missing or non-directory parent" .migtest/i6.log \
  && ok "dangling parent refused before the target is touched" || bad "dangling guard: $(cat .migtest/i6.log)"
if [ "$BACKEND" = fdb ]; then
    # SQLite gets this guarantee by building beside the destination and renaming
    # only at the end; the cluster gets it by never provisioning the name in the
    # first place (the import opens the subspace READ-ONLY, so a dump that is
    # refused leaves not one key behind).
    $Y view $MDST export --view bad3 --out .migtest/bad3.jsonl > .migtest/i6b.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/i6b.log \
      && ok "a rejected import leaves no keys behind" || bad "bad3 subspace left behind"
else
    [ ! -e .migtest/dst/meta/bad3.db ] \
      && ok "a rejected import leaves no half tree" || bad "bad3 tree left behind"
fi
# A failed --replace must leave the PREVIOUS tree untouched (build-beside-rename).
$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/keep.log 2>&1 &
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
    [ "$AFTER" = "$BEFORE" ] && [ "$AFTER" != "ERR" ] \
      && ok "the old tree is still installed after the failed replace ($AFTER nodes)" \
      || bad "failed replace damaged the tree ($BEFORE -> $AFTER)"
else
    $Y view $MDST import --view $VIEW2 --in .migtest/bad.jsonl --replace > .migtest/i7.log 2>&1
    [ $? -ne 0 ] && ok "a corrupt --replace is refused" || bad "corrupt --replace unexpectedly succeeded"
    [ -f .migtest/dst/meta/team.db ] && ok "the old tree is still installed after the failed replace" \
      || bad "failed replace destroyed the old tree"
fi
$Y --home .migtest/dst mount --view $VIEW2 -m .migtest/mnt_dst --no-cache $REMOTE --foreground > .migtest/keep2.log 2>&1 &
KP2=$!
sleep 1.5
[ "$BACKEND" = fdb ] && mnt_gate .migtest/mnt_dst
[ -e .migtest/mnt_dst/keep.txt ] && ok "old tree contents survived the failed replace (no window of loss)" \
  || bad "old tree contents lost after failed replace"
chk "$(T cat .migtest/mnt_dst/docs/a.md)" "report-body" "old blob names still resolve after the failed replace"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $KP2 2>/dev/null; wait $KP2 2>/dev/null

echo "===== 8) --as-fork restores fork semantics plus ledger sharing ====="
if [ "$BACKEND" = fdb ]; then
    # `--as-fork` is the import-side half of section 4's fork, and it meets the
    # same verdict for the same reason: a fork is a copied snapshot of a base
    # tree, and there is no snapshot of a subspace to copy. So the claim this
    # section makes on SQLite — fork semantics, a shared ledger, an EROFS content
    # write — has no fdb counterpart to re-run, and what runs instead is the
    # refusal the backend actually gives, asserted with the same four checks
    # section 4 uses: it refuses, it says on which backend and why, it names the
    # way forward, and it provisions neither a subspace nor a definition.
    $Y view $MDST import --view labx --in .migtest/team.jsonl --as-fork $VIEW1 > .migtest/f1.log 2>&1
    [ $? -ne 0 ] && ok "import --as-fork refuses on an fdb:// store" \
        || bad "--as-fork was ACCEPTED on fdb://: $(cat .migtest/f1.log)"
    grep -q "as-fork" .migtest/f1.log && grep -qi "not implemented" .migtest/f1.log \
        && ok "the refusal names the flag it refused and what is missing" \
        || bad "refusal unclear: $(cat .migtest/f1.log)"
    grep -qiE "alias|derived view" .migtest/f1.log \
        && ok "the refusal names the alternative that works on every backend" \
        || bad "refusal offers no way forward: $(cat .migtest/f1.log)"
    $Y view $MDST export --view labx --out .migtest/labx.jsonl > .migtest/f1b.log 2>&1
    [ $? -ne 0 ] && grep -qF "$GHOST" .migtest/f1b.log \
        && ok "the refusal provisioned no subspace" || bad "labx answers after the refusal"
    [ ! -e .migtest/src/meta/views/labx.layout ] && ok "the refusal wrote no definition slot" \
        || bad "labx.layout left behind"
else
    $Y view --meta-dir .migtest/dst import --view labx --in .migtest/lab.jsonl --as-fork team > .migtest/f1.log 2>&1 \
      && ok "import --as-fork team" || bad "as-fork import failed: $(cat .migtest/f1.log)"
    grep -q 'recorded `fork team`' .migtest/f1.log \
      && ok "fork definition recorded" || bad "fork definition note missing"
    chk "$(cat .migtest/dst/meta/views/labx.layout)" "fork team" "definition slot says 'fork team'"
    $Y --home .migtest/dst mount --view labx -m .migtest/mnt_lab --no-cache --foreground > .migtest/lab.log 2>&1 &
    LP=$!
    sleep 1.5
    L=.migtest/mnt_lab
    grep -q "structure writable, content read-only" .migtest/lab.log \
      && ok "imported fork announces its contract" || bad "fork contract note missing: $(cat .migtest/lab.log)"
    T sh -c "printf 'new-body' > $L/docs/a.md" 2>.migtest/fw.err \
      && bad "imported fork accepted a content write" \
      || { rofs .migtest/fw.err && ok "content write refused with EROFS" || bad "wrong errno: $(cat .migtest/fw.err)"; }
    T mkdir $L/org && ok "imported fork accepts structure writes" || bad "fork mkdir failed"
    T rm $L/photos/x.jpg && ok "imported fork can drop one of its own names" || bad "fork unlink failed"
    fusermount -u .migtest/mnt_lab 2>/dev/null; kill $LP 2>/dev/null; wait $LP 2>/dev/null
    $Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/dst2.log 2>&1 &
    DP2=$!
    sleep 1.5
    chk "$(T cat .migtest/mnt_dst/photos/x.jpg)" "priv" "fork delete did not free the base's shared blob (ledger)"
    chk "$(T cat .migtest/mnt_dst/docs/a.md)" "report-body" "base content still intact after the fork session"
    fusermount -u .migtest/mnt_dst 2>/dev/null; kill $DP2 2>/dev/null; wait $DP2 2>/dev/null
    $Y view --meta-dir .migtest/dst import --view laby --in .migtest/lab.jsonl --as-fork ghost > .migtest/f2.log 2>&1
    [ $? -ne 0 ] && grep -q "no physical base tree" .migtest/f2.log \
      && ok "--as-fork requires the base tree in the target store" || bad "missing-base guard: $(cat .migtest/f2.log)"
    $Y view --meta-dir .migtest/dst import --view labz --in .migtest/lab.jsonl --as-fork labz > .migtest/f3.log 2>&1
    [ $? -ne 0 ] && grep -q "must differ from its base" .migtest/f3.log \
      && ok "a view cannot be its own fork base" || bad "self-base guard: $(cat .migtest/f3.log)"
    $Y view --meta-dir .migtest/dst rm --name labx > /dev/null 2>&1
    $Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/dst3.log 2>&1 &
    DP3=$!
    sleep 1.5
    chk "$(T cat .migtest/mnt_dst/photos/x.jpg)" "priv" "base blob survives removing the imported fork"
    fusermount -u .migtest/mnt_dst 2>/dev/null; kill $DP3 2>/dev/null; wait $DP3 2>/dev/null
fi

echo "===== 9) several base views, each managing its own tree ====="
if [ "$BACKEND" = fdb ]; then
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
T sh -c "printf 'alpha-only' > .migtest/mnt_alpha/alpha.txt" && ok "alpha accepts its own content" || bad "alpha write failed"
T sh -c "printf 'beta-only' > .migtest/mnt_beta/beta.txt"   && ok "beta accepts its own content"  || bad "beta write failed"
chk "$(T ls .migtest/mnt_alpha | tr '\n' ' ')" "alpha.txt " "alpha sees only its own tree"
chk "$(T ls .migtest/mnt_beta | tr '\n' ' ')" "beta.txt " "beta sees only its own tree"
fusermount -u .migtest/mnt_alpha .migtest/mnt_beta 2>/dev/null
kill $AP $BP 2>/dev/null; wait $AP $BP 2>/dev/null
$Y view $MSRC list > .migtest/list_all.log 2>&1
if [ "$BACKEND" = fdb ]; then
    # The same boundary section 5 stated, now with four bases in the control plane
    # instead of one: `view list` answers the definitions, which are files (C6),
    # and does not enumerate subspaces. "Every base view is known" is therefore
    # asked of the authority that decides it — each name dumps its OWN rows, and
    # alpha's dump names alpha.txt without ever having seen beta's.
    grep -q "^$VIEWA\$" .migtest/list_all.log \
      && bad "view list NOW enumerates base subspaces — this assertion and docs.html's 未提供操作 row both need updating" \
      || ok "the documented boundary holds over a four-base control plane too"
    $Y view $MSRC export --view $VIEWA --out .migtest/la.jsonl --force > .migtest/l_a.log 2>&1 \
      && grep -q '"name":"alpha.txt"' .migtest/la.jsonl && ! grep -q '"name":"beta.txt"' .migtest/la.jsonl \
      && ok "'$VIEWA' is a base subspace holding its own tree" || bad "alpha subspace dump wrong: $(cat .migtest/l_a.log)"
    $Y view $MSRC export --view $VIEWB --out .migtest/lb.jsonl --force > .migtest/l_b.log 2>&1 \
      && grep -q '"name":"beta.txt"' .migtest/lb.jsonl && ! grep -q '"name":"alpha.txt"' .migtest/lb.jsonl \
      && ok "'$VIEWB' is a base subspace holding its own tree" || bad "beta subspace dump wrong: $(cat .migtest/l_b.log)"
else
    grep -q "base view 'team'" .migtest/list_all.log \
      && grep -q "base view 'alpha'" .migtest/list_all.log \
      && grep -q "base view 'beta'" .migtest/list_all.log \
      && ok "view list names every base view" || bad "multi-base list wrong: $(cat .migtest/list_all.log)"
fi
$Y mount --view $VIEW1 -m .migtest/mnt_all --no-cache $REMOTE --foreground > .migtest/all.log 2>&1 &
TP=$!
sleep 1.5
[ "$BACKEND" = fdb ] && mnt_gate .migtest/mnt_all || true
chk "$(T cat .migtest/mnt_all/docs/a.md)" "report-body" "team still reads its own content beside alpha/beta"
[ ! -e .migtest/mnt_all/alpha.txt ] && [ ! -e .migtest/mnt_all/beta.txt ] \
  && ok "alpha/beta content never leaks into team" || bad "views bled into each other"
fusermount -u .migtest/mnt_all 2>/dev/null; kill $TP 2>/dev/null; wait $TP 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL MIGRATE CHECKS PASSED" || echo "SOME MIGRATE CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
