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
rm -rf .migtest
mkdir -p .migtest/src .migtest/dst .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab \
         .migtest/mnt_alpha .migtest/mnt_beta .migtest/mnt_all
export GROVEFS_HOME=$PWD/.migtest/src

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
rofs(){ grep -qE "Read-only file system|只读文件系统" "$1"; }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

cat > .migtest/team.manifest <<'EOF'
mkdir docs
touch docs/a.md
mkdir photos
touch photos/x.jpg
touch notes.txt
EOF
$Y init --view team --manifest .migtest/team.manifest > .migtest/init.log 2>&1 \
  && ok "seeded physical base view 'team' at .migtest/src" || { bad "init failed: $(cat .migtest/init.log)"; exit 1; }

echo "===== 1) fill the source view with content, links and metadata ====="
$Y mount --view team -m .migtest/mnt_src --no-cache --foreground > .migtest/src.log 2>&1 &
SP=$!
sleep 1.5
S=.migtest/mnt_src
T sh -c "printf 'report-body' > $S/docs/a.md" && ok "wrote docs/a.md" || bad "write a.md failed"
T sh -c "printf 'priv' > $S/photos/x.jpg"           && ok "wrote photos/x.jpg" || bad "write x.jpg failed"
T sh -c "printf 'notes-body' > $S/notes.txt"        && ok "wrote notes.txt" || bad "write notes.txt failed"
T chmod 640 $S/docs/a.md                            && ok "chmod 640 docs/a.md" || bad "chmod failed"
T ln $S/docs/a.md $S/docs/b.md && ok "hard-linked docs/b.md -> docs/a.md" || bad "ln failed"
T ln -s docs/a.md $S/link && ok "created symlink 'link' -> docs/a.md" || bad "symlink failed"

echo "===== 2) export WHILE MOUNTED (snapshot read) ====="
$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl > .migtest/exp.log 2>&1 \
  && ok "view export from a live mount" || bad "export failed: $(cat .migtest/exp.log)"
grep -q "exported view 'team' metadata: 8 node(s), 3 blob reference(s)" .migtest/exp.log \
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
$Y view --meta-dir .migtest/src export --view ghost --out .migtest/ghost.jsonl > .migtest/e1.log 2>&1
[ $? -ne 0 ] && grep -q "not a physical tree" .migtest/e1.log \
  && ok "exporting a view with no local tree refused" || bad "ghost export guard: $(cat .migtest/e1.log)"
$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl > .migtest/e2.log 2>&1
[ $? -ne 0 ] && grep -q "already exists" .migtest/e2.log \
  && ok "an existing --out is not silently overwritten" || bad "overwrite guard: $(cat .migtest/e2.log)"
$Y view --meta-dir .migtest/src export --view team --out .migtest/team.jsonl --force > /dev/null 2>&1 \
  && ok "--force allows the overwrite" || bad "--force export failed"
echo "only /docs" > .migtest/docs.layout
$Y view --meta-dir .migtest/src define --name lens --base team --rules .migtest/docs.layout > .migtest/e3.log 2>&1
$Y view --meta-dir .migtest/src export --view lens --out .migtest/lens.jsonl > .migtest/e4.log 2>&1
[ $? -ne 0 ] && grep -q "derived (virtual) view" .migtest/e4.log \
  && ok "derived view refused (no tree of its own; export the base)" || bad "derived export guard: $(cat .migtest/e4.log)"
$Y view --meta-dir .migtest/src alias --name pub --real team > .migtest/e5.log 2>&1
$Y view --meta-dir .migtest/src export --view pub --out .migtest/pub.jsonl > .migtest/e6.log 2>&1
[ $? -ne 0 ] && grep -q "pure alias" .migtest/e6.log \
  && ok "alias refused (export the real view)" || bad "alias export guard: $(cat .migtest/e6.log)"
# An alias makes a direct mount of the real name illegal (existing rule), so drop
# the alias and the lens again: the rest of the suite mounts 'team' by its name.
$Y view --meta-dir .migtest/src rm --name pub > /dev/null 2>&1
$Y view --meta-dir .migtest/src rm --name lens > /dev/null 2>&1

echo "===== 4) a fork exports as kind=fork, with a warning ====="
$Y view --meta-dir .migtest/src fork --name lab --base team > .migtest/fork.log 2>&1 \
  && ok "forked 'lab' from 'team'" || bad "fork failed: $(cat .migtest/fork.log)"
$Y view --meta-dir .migtest/src export --view lab --out .migtest/lab.jsonl > .migtest/exp_lab.log 2>&1 \
  && ok "fork tree exported" || bad "fork export failed: $(cat .migtest/exp_lab.log)"
head -1 .migtest/lab.jsonl | grep -q '"kind":"fork"' \
  && ok "header marks the tree as a fork" || bad "fork kind missing"
grep -q "this is a fork tree" .migtest/exp_lab.log \
  && ok "export warns that import creates a writable base unless --as-fork" || bad "fork warning missing"
fusermount -u .migtest/mnt_src 2>/dev/null; kill $SP 2>/dev/null; wait $SP 2>/dev/null

echo "===== 5) migrate bytes + metadata to a DIFFERENT absolute store path ====="
cp -a .migtest/src/blocks .migtest/dst/ \
  && ok "copied only blocks/ to the target store" || bad "cp blocks failed"
[ ! -e .migtest/dst/meta/team.db ] && ok "target has no source database (metadata travels via export)" \
  || bad "target unexpectedly already has a team.db"
$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl > .migtest/imp.log 2>&1 \
  && ok "view import into the new store" || bad "import failed: $(cat .migtest/imp.log)"
grep -q "8 node(s), 3 blob reference(s)" .migtest/imp.log \
  && ok "import reports the same node/blob counts" || bad "import counts: $(cat .migtest/imp.log)"
grep -q "cross-view ledger rebuilt" .migtest/imp.log \
  && ok "cross-view ledger rebuilt after import" || bad "ledger note missing"
[ -f .migtest/dst/meta/team.db ] && [ -f .migtest/dst/meta/refs.db ] \
  && ok "target tree + ledger exist" || bad "target tree/ledger missing"
$Y view --meta-dir .migtest/dst list > .migtest/list_dst.log 2>&1
grep -q "base view 'team' -> own tree meta/team.db" .migtest/list_dst.log \
  && ok "view list shows the imported bare base view" || bad "list missing team: $(cat .migtest/list_dst.log)"

echo "===== 6) mount the imported view at the new path: identical tree, writable ====="
$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/dst.log 2>&1 &
DP=$!
sleep 1.5
D=.migtest/mnt_dst
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
$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl > .migtest/i1.log 2>&1
[ $? -ne 0 ] && grep -q "pass --replace" .migtest/i1.log \
  && ok "existing target tree needs --replace" || bad "replace guard: $(cat .migtest/i1.log)"
$Y view --meta-dir .migtest/dst import --view team --in .migtest/team.jsonl --replace > .migtest/i2.log 2>&1
[ $? -eq 0 ] && grep -q "replacing the previous tree" .migtest/i2.log \
  && ok "--replace re-imports over an existing tree" || bad "--replace failed: $(cat .migtest/i2.log)"
$Y view --meta-dir .migtest/dst alias --name pub --real team > /dev/null 2>&1
$Y view --meta-dir .migtest/dst import --view pub --in .migtest/team.jsonl > .migtest/i3.log 2>&1
[ $? -ne 0 ] && grep -q "already has a definition" .migtest/i3.log \
  && ok "importing onto a definition slot refused" || bad "definition guard: $(cat .migtest/i3.log)"
# The alias also makes a direct mount of the real name illegal (existing rule),
# so drop it again before the mount checks below.
$Y view --meta-dir .migtest/dst rm --name pub > /dev/null 2>&1
[ ! -e .migtest/dst/meta/views/pub.layout ] && [ -f .migtest/dst/meta/team.db ] \
  && ok "view rm drops only the alias definition" || bad "alias rm touched the real tree"
printf '{"format":"other","version":1}\n' > .migtest/bad.jsonl
$Y view --meta-dir .migtest/dst import --view bad1 --in .migtest/bad.jsonl > .migtest/i4.log 2>&1
[ $? -ne 0 ] && grep -q "unknown format" .migtest/i4.log \
  && ok "unknown export format refused" || bad "format guard: $(cat .migtest/i4.log)"
printf '{"format":"grovefs-view-metadata","version":99}\n' > .migtest/bad.jsonl
$Y view --meta-dir .migtest/dst import --view bad2 --in .migtest/bad.jsonl > .migtest/i5.log 2>&1
[ $? -ne 0 ] && grep -q "unsupported export version" .migtest/i5.log \
  && ok "future format version refused" || bad "version guard: $(cat .migtest/i5.log)"
printf '%s\n%s\n%s\n' \
  '{"format":"grovefs-view-metadata","version":1,"nodes":2}' \
  '{"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1}' \
  '{"id":2,"ino":2,"parent_id":99,"name":"f","is_dir":0}' > .migtest/bad.jsonl
$Y view --meta-dir .migtest/dst import --view bad3 --in .migtest/bad.jsonl > .migtest/i6.log 2>&1
[ $? -ne 0 ] && grep -q "missing or non-directory parent" .migtest/i6.log \
  && ok "dangling parent refused before the target is touched" || bad "dangling guard: $(cat .migtest/i6.log)"
[ ! -e .migtest/dst/meta/bad3.db ] \
  && ok "a rejected import leaves no half tree" || bad "bad3 tree left behind"
# A failed --replace must leave the PREVIOUS tree untouched (build-beside-rename).
$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/keep.log 2>&1 &
KP=$!
sleep 1.5
T touch .migtest/mnt_dst/keep.txt && ok "marked the installed tree before a bad --replace" || bad "touch keep.txt failed"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $KP 2>/dev/null; wait $KP 2>/dev/null
$Y view --meta-dir .migtest/dst import --view team --in .migtest/bad.jsonl --replace > .migtest/i7.log 2>&1
[ $? -ne 0 ] && ok "a corrupt --replace is refused" || bad "corrupt --replace unexpectedly succeeded"
[ -f .migtest/dst/meta/team.db ] && ok "the old tree is still installed after the failed replace" \
  || bad "failed replace destroyed the old tree"
$Y --home .migtest/dst mount --view team -m .migtest/mnt_dst --no-cache --foreground > .migtest/keep2.log 2>&1 &
KP2=$!
sleep 1.5
[ -e .migtest/mnt_dst/keep.txt ] && ok "old tree contents survived the failed replace (no window of loss)" \
  || bad "old tree contents lost after failed replace"
chk "$(T cat .migtest/mnt_dst/docs/a.md)" "report-body" "old blob names still resolve after the failed replace"
fusermount -u .migtest/mnt_dst 2>/dev/null; kill $KP2 2>/dev/null; wait $KP2 2>/dev/null

echo "===== 8) --as-fork restores fork semantics plus ledger sharing ====="
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

echo "===== 9) several base views, each managing its own tree ====="
$Y init --view alpha > /dev/null 2>&1 && ok "created base view 'alpha'" || bad "init alpha failed"
$Y init --view beta  > /dev/null 2>&1 && ok "created base view 'beta'"  || bad "init beta failed"
$Y mount --view alpha -m .migtest/mnt_alpha --no-cache --foreground > .migtest/alpha.log 2>&1 &
AP=$!
$Y mount --view beta -m .migtest/mnt_beta --no-cache --foreground > .migtest/beta.log 2>&1 &
BP=$!
sleep 1.5
T sh -c "printf 'alpha-only' > .migtest/mnt_alpha/alpha.txt" && ok "alpha accepts its own content" || bad "alpha write failed"
T sh -c "printf 'beta-only' > .migtest/mnt_beta/beta.txt"   && ok "beta accepts its own content"  || bad "beta write failed"
chk "$(T ls .migtest/mnt_alpha | tr '\n' ' ')" "alpha.txt " "alpha sees only its own tree"
chk "$(T ls .migtest/mnt_beta | tr '\n' ' ')" "beta.txt " "beta sees only its own tree"
fusermount -u .migtest/mnt_alpha .migtest/mnt_beta 2>/dev/null
kill $AP $BP 2>/dev/null; wait $AP $BP 2>/dev/null
$Y view --meta-dir .migtest/src list > .migtest/list_all.log 2>&1
grep -q "base view 'team'" .migtest/list_all.log \
  && grep -q "base view 'alpha'" .migtest/list_all.log \
  && grep -q "base view 'beta'" .migtest/list_all.log \
  && ok "view list names every base view" || bad "multi-base list wrong: $(cat .migtest/list_all.log)"
$Y mount --view team -m .migtest/mnt_all --no-cache --foreground > .migtest/all.log 2>&1 &
TP=$!
sleep 1.5
chk "$(T cat .migtest/mnt_all/docs/a.md)" "report-body" "team still reads its own content beside alpha/beta"
[ ! -e .migtest/mnt_all/alpha.txt ] && [ ! -e .migtest/mnt_all/beta.txt ] \
  && ok "alpha/beta content never leaks into team" || bad "views bled into each other"
fusermount -u .migtest/mnt_all 2>/dev/null; kill $TP 2>/dev/null; wait $TP 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL MIGRATE CHECKS PASSED" || echo "SOME MIGRATE CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
