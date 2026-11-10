#!/usr/bin/env bash
# Metadata backup -> point-in-time restore -> MOUNT READBACK, at the GroveFS
# layer rather than at the key layer.
#
# `.bneck/fdb3vm_backup.sh` already proves the storage half (fdbbackup covers a
# snapshot plus the mutation log; a restore to a version puts the keys back).
# This script proves the layer above it, because a keyspace can be restored
# perfectly and still not be a usable filesystem:
#
#   - the mount gate reads a ROOT ROW, so "the keys came back" has to be
#     re-asked as "the view mounts again";
#   - inodes and link counts live in the tree's own `'i'`/`'n'` rows, so they
#     either survive the round trip or every tool keyed on `st_ino` is broken;
#   - content keys are booked in the shared ledger (a sibling subspace), so a
#     restore that brings back a tree but not its bookings — or books twice — is
#     a metadata pool that leaks or frees the wrong bytes. That is why the last
#     step of each cluster runs `refs rebuild` and demands a NO-OP.
#
# What it proves, in order:
#   1. a continuous backup of a cluster holding real grovefs subspaces;
#   2. batch A (inside the first snapshot) and batch B (written after it, so only
#      the log has it) both come back, in place, after a restore to the stop
#      version — into a root that was wiped first, so no leftover can explain it;
#   3. names, kinds, modes, INODE numbers, link counts, sizes and bytes are
#      identical to what was recorded before the backup;
#   4. the disaster half: the same container restores into a fresh EMPTY cluster,
#      which then mounts on a machine whose control plane is EMPTY too — under
#      `fdb://` a base view IS its subspace, so the cluster is the definition and
#      `view create` correctly refuses (the tree already exists);
#   5. point-in-time, both directions, on that second cluster: the state at the
#      stop version has `late.txt`, the state at the SNAPSHOT version does not —
#      the same tree, the same definition, two versions;
#   6. `refs rebuild` reports 0 corrected / 0 deleted on BOTH clusters: a restore
#      does not need a ledger repair step.
#
# The rule this script had to learn from FDB 7.1 (and why step 6 wipes the whole
# root): a rangefile restore refuses a NON-EMPTY destination range —
# `restore_destination_not_empty`, checked per range with a `getRange(…, 1)` —
# and `-k` does not lift that. Only `--incremental` (log replay) may land on a
# non-empty destination. So "restore over the damaged tree" is not a thing an
# operator can do: the sequence is "clear the range, then restore it", and that
# sequence is what is measured here. It also means the "a write after V is rolled
# back" claim cannot be shown in place (the clear already removed it); it is shown
# at step 10 instead, where the same cluster holds the newer tree and is then
# restored back to the snapshot version.
#
# NOT covered, on purpose: blob bytes live in the data-server's `--data-dir`,
# not in the cluster, so `fdbbackup` never holds them. A cluster restore is only
# usable together with a backup of that directory (USAGE §12.3); this script
# keeps one data-dir across both halves and says so instead of implying that the
# cluster backup alone is a full backup.
#
# usage: bash .bneck/fdb_backup_mount.sh          # single-node dev cluster
#        CLUSTER=/data/grovefs/.fdb3vm/cluster3vm bash .bneck/fdb_backup_mount.sh
set -u
cd "$(dirname "$0")/.."

BIN="${FDB_HOME:-/data/grovefs/.fdbbin}"
CLUSTER="${CLUSTER:-/data/grovefs/.fdbtest/test.cluster}"
URI="fdb://$CLUSTER"
BAK="$BIN/fdbbackup"
RES="$BIN/fdbrestore"
AGENT="$BIN/backup_agent"
Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
WORK="${WORK:-/data/grovefs/.fdbtest/bkmnt}"
TAG="gmnt$$"
V1="bk_mnt_a$$"          # the view that gets wiped and restored
V2="bk_mnt_b$$"          # a second tree in the same cluster: nothing may be lost
META=127.0.0.1:15196
META2=127.0.0.1:15198
DATA=127.0.0.1:15197
A="$WORK/mnt_a"; B="$WORK/mnt_b"; D="$WORK/mnt_d"
# The grovefs root, as the byte range a key-range restore has to name.
ROOTRG='"\x01grovefs" "\x01grovefs\xff"'
pass=0; fail=0
# One run at a time: the whole script wipes `$WORK` and drives the SAME cluster,
# so a second copy started by hand or by a retry would delete the first one's
# data dir under it and produce two contradictory verdicts about one keyspace.
# Every long-lived child closes fd 9 (`9>&-` at each spawn): bash does NOT mark
# an `exec`-opened fd close-on-exec, so an orphaned agent or server would inherit
# the lock and hold it after its parent died — measured: two killed runs left
# agents owning `$WORK.lock`, and every later run refused to start.
exec 9>"$WORK.lock"
flock -n 9 || { echo "another run holds $WORK.lock"; exit 1; }
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
say(){ echo "    $*"; }
# Byte string -> hex, taken through a PIPE on purpose: a shell variable cannot
# hold a NUL byte, and every grovefs key here starts with one
# (`\x01grovefs\x00`), so `$(printf '\x01...')` silently truncates to nothing.
hx(){ od -An -tx1 | tr -d ' \n'; }
# What the cluster holds, one name per line. Used to prove this run did not
# change the count: an aborted run's leftovers are in the BASELINE, not blamed
# on the next run, and a subspace this run leaks shows up as a difference.
inv_names(){ "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan 2>/dev/null \
    | awk 'NR>1 && ($4=="view" || $4=="corpse" || $4=="ledger"){print $1}' | sort; }
fdbget(){ "$BIN/fdbcli.x86_64" -C "$CLUSTER" --exec "$1" 2>/dev/null; }
# `--timeout` is not cosmetic: on an UNCONFIGURED database `status` does not
# answer, it blocks until the timeout — the availability probe below costs
# exactly this many seconds per attempt until the database is configured.
dget(){ timeout 20 "$BIN/fdbcli.x86_64" -C "$DESTCF" --timeout 10 --exec "$1" 2>/dev/null; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
mnt_gate(){ local mp="$1"; for _ in $(seq 1 80); do
    findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }
umount_mnt(){ local mp="$1"
    for _ in $(seq 1 40); do
        findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 || return 0
        fusermount -u "$mp" 2>/dev/null || umount "$mp" 2>/dev/null || true; sleep 0.25
    done; return 1; }
# A restore is only "done" when it says so AND reports no error: `LastError: None`
# contains the word "Error", so the field's VALUE is what gets checked.
noerr(){ ! grep -oE "LastError: +[^ ]+" "$1" | grep -v "None" | grep -q .; }
listing(){ (cd "$1" && find . -mindepth 1 -printf '%y %m %i %n %s %p\n' | sort -k6); }

SERVER_PIDS=""; MOUNT_PIDS=""
cleanup(){
    for m in "$A" "$B" "$D"; do umount_mnt "$m"; done
    # The agents are detached (`setsid`) and are NOT in SERVER_PIDS, so the reap
    # below cannot be a plain `wait`: it would wait for them forever — measured,
    # a finished run sat in its own trap for minutes holding the lock while two
    # agents were still alive. Kill them first, then force what refuses to die.
    pkill -f "backup_agent -C $CLUSTER" 2>/dev/null || true
    [ -n "${DESTCF:-}" ] && pkill -f "backup_agent -C $DESTCF" 2>/dev/null || true
    for p in $MOUNT_PIDS $SERVER_PIDS; do kill "$p" 2>/dev/null || true; done
    sleep 1
    for p in $MOUNT_PIDS $SERVER_PIDS; do kill -9 "$p" 2>/dev/null || true; done
    # Leave both clusters as found: `view rm --base` deletes the subspace AND
    # unwinds the ledger, in that order, so a run does not accumulate a corpse
    # and two trees per invocation. `timeout` is not decoration: a teardown that
    # runs after the cluster is already gone BLOCKS inside the commit (measured:
    # a `view rm` against a killed cluster sat there for minutes and held the
    # whole trap open), and a trap that cannot finish hides the summary.
    for v in "$V1" "$V2"; do
        [ -x "$Y" ] && timeout 25 "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm \
            --name "$v" --base --data-dir "$WORK/data" >/dev/null 2>&1 || true
    done
    if [ -n "${DESTCF:-}" ] && [ -x "$Y" ]; then
        timeout 25 "$Y" view --meta-store "fdb://$DESTCF" --meta-dir "$WORK/home2" rm \
            --name "$V1" --base --data-dir "$WORK/data" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

echo "===== 0. preconditions ====="
[ -x "$BAK" ] && [ -x "$RES" ] && [ -x "$AGENT" ] || {
    echo "  SKIP: fdbbackup/fdbrestore/backup_agent not in $BIN"; exit 0; }
[ -x "$Y" ] || { echo "FAIL: no feature binary at $Y (bash .build-bin/test_fdb.sh builds it)"; exit 1; }
fdbget "status minimal" 2>&1 | grep -q "database is available" \
    && ok "cluster available at $CLUSTER" || { echo "  SKIP: cluster not available"; exit 0; }
rm -rf "$WORK"; mkdir -p "$WORK"/{home,client,data,mnt_a,mnt_b,agent}
baseline=$(inv_names | wc -l)
ok "the cluster holds $baseline subspace(s) before this run (the baseline a clean teardown returns to)"
# The agent is not `fdbbackup agent`: it is the same binary under the name
# `backup_agent`, which is how it decides what to be. It takes no --data-dir.
pkill -f "backup_agent -C $CLUSTER" 2>/dev/null; sleep 1
# A crashed earlier run can leave the second cluster's server holding its port
# and its data dir, which a fresh `rm -rf $WORK` would not dislodge.
pkill -f "fdbserver.x86_64.*127.0.0.1:4600" 2>/dev/null; sleep 1
setsid nohup "$AGENT" -C "$CLUSTER" --log --logdir "$WORK/agent" \
    > "$WORK/agent.out" 2>&1 </dev/null 9>&- &
sleep 6
pgrep -f "backup_agent -C $CLUSTER" >/dev/null \
    && ok "backup_agent running (pid $(pgrep -f "backup_agent -C $CLUSTER" | head -1))" \
    || { echo "  SKIP: no backup agent, nothing would be backed up"; exit 0; }

echo "===== 1. one cluster, two views, one data-dir ====="
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" create --view "$V1" > "$WORK/create1.log" 2>&1 \
    && ok "provisioned $V1 (one subspace)" || { bad "view create $V1"; cat "$WORK/create1.log"; exit 1; }
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" create --view "$V2" > "$WORK/create2.log" 2>&1 \
    && ok "provisioned $V2 (the second tree)" || { bad "view create $V2"; cat "$WORK/create2.log"; exit 1; }
"$Y" data-server --listen "$DATA" --data-dir "$WORK/data" > "$WORK/data.log" 2>&1 9>&- & SERVER_PIDS="$!"
"$Y" meta-server --listen "$META" --home "$WORK/home" --meta-store "$URI" > "$WORK/meta.log" 2>&1 9>&- & SERVER_PIDS="$SERVER_PIDS $!"
wait_port "$DATA" && wait_port "$META" || { bad "servers did not bind"; tail -5 "$WORK/meta.log"; exit 1; }
ok "data-server + meta-server up on the fdb:// store"

mount_v(){ # mount_v <view> <mnt> <log>
    mkdir -p "$2"
    "$Y" --home "$WORK/client" mount --view "$1" --mountpoint "$2" \
        --meta-server "$META" --data-servers "$DATA" --foreground > "$3" 2>&1 9>&- &
    MOUNT_PIDS="$MOUNT_PIDS $!"
    mnt_gate "$2"
}

echo "===== 2. batch A (has to be inside the first snapshot) ====="
mount_v "$V1" "$A" "$WORK/m_a1.log" || { bad "mount $V1 never came up"; tail -5 "$WORK/m_a1.log"; exit 1; }
mkdir -p "$A/dir"
printf 'alpha-payload\n' > "$A/alpha.txt"
head -c 262144 /dev/urandom > "$A/big.bin"      # 256 KiB: spans several blobs
printf 'nested-payload\n' > "$A/dir/nested.txt"
ln "$A/alpha.txt" "$A/hard.txt"                 # hard link: one object, two names
ln -s alpha.txt  "$A/sym.txt"                   # symlink: target stays a string
sync "$A" 2>/dev/null; sleep 0.5
ok "batch A written through the mount (file, 256 KiB file, subdir, hard link, symlink)"
# Read from the MOUNT, not from the blob store: inode numbers, link counts and
# modes are exactly the things a restore can lose.
listing "$A" > "$WORK/treeA.before"
(cd "$A" && sha256sum alpha.txt big.bin dir/nested.txt hard.txt) > "$WORK/bytesA.before"
say "$(wc -l < "$WORK/treeA.before") entries, $(wc -l < "$WORK/bytesA.before") checksums recorded"
umount_mnt "$A" && ok "view $V1 unmounted before the backup" || bad "could not unmount $A"

mount_v "$V2" "$B" "$WORK/m_b1.log" || { bad "mount $V2 never came up"; exit 1; }
printf 'second-tree-content\n' > "$B/keep.txt"
sync "$B" 2>/dev/null; sleep 0.3
umount_mnt "$B" && ok "the second tree seeded and unmounted" || bad "could not unmount $B"

echo "===== 3. continuous backup, then batch B that only the log can hold ====="
"$BAK" start -C "$CLUSTER" -d "file://$WORK/container" -t "$TAG" -s 5 -z > "$WORK/bk_start.log" 2>&1
tail -2 "$WORK/bk_start.log" | sed 's/^/    /'
grep -q "successfully submitted\|in progress" "$WORK/bk_start.log" && ok "backup accepted (-z: continuous)" \
    || bad "backup not running: $(head -3 "$WORK/bk_start.log" | tr '\n' ' ')"
snap=""
for _ in $(seq 1 60); do
    snap=$("$BAK" status -C "$CLUSTER" -t "$TAG" 2>/dev/null | grep "Last complete snapshot version" | grep -oE "[0-9]{6,}" | head -1)
    [ -n "${snap:-}" ] && [ "$snap" != 0 ] && break
    sleep 3
done
[ -n "${snap:-}" ] && [ "$snap" != 0 ] && ok "a snapshot completed at version $snap" \
    || bad "no completed snapshot within 180s"

mount_v "$V1" "$A" "$WORK/m_a2.log" || { bad "second mount of $V1 never came up"; exit 1; }
printf 'log-only-payload\n' > "$A/late.txt"      # after the snapshot: log replay or nothing
sync "$A" 2>/dev/null; sleep 0.3
umount_mnt "$A" && ok "batch B written after the snapshot and unmounted" || bad "could not unmount $A"

echo "===== 4. stop the backup at a known version ====="
"$BAK" discontinue -C "$CLUSTER" -t "$TAG" -w > "$WORK/bk_stop.log" 2>&1
st=""
for _ in $(seq 1 40); do
    st=$("$BAK" status -C "$CLUSTER" -t "$TAG" 2>/dev/null)
    echo "$st" | grep -q "completed at version" && break
    sleep 3
done
V=$(echo "$st" | grep -o "completed at version [0-9]*" | awk '{print $4}' | tail -1)
BU=$(echo "$st" | grep -o "BackupURL: file://[^ ]*" | awk '{print $2}' | tail -1)
[ -n "${V:-}" ] && ok "backup completed at version $V" || bad "could not read the stop version"
[ -n "${BU:-}" ] && ok "restore will read $BU" || bad "no BackupURL; cannot continue"
[ -n "${V:-}" ] && [ -n "${snap:-}" ] && [ "$snap" -lt "$V" ] \
    && ok "the log runs past the snapshot ($snap -> $V), so batch B is log-only" \
    || bad "no version gap between snapshot and stop ($snap / $V)"

echo "===== 5. wipe the whole grovefs keyspace on the source cluster ====="
fdbget "writemode on; clearrange \"\\x01grovefs\" \"\\x01grovefs\\xff\"" > "$WORK/clear.log" 2>&1
grep -q Committed "$WORK/clear.log" && say "$(tail -1 "$WORK/clear.log")" \
    || bad "clearrange of the root failed: $(tail -3 "$WORK/clear.log" | tr '\n' ' ')"
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan > "$WORK/scan_wiped.log" 2>&1
grep -q "no subspaces under the 'grovefs' root" "$WORK/scan_wiped.log" \
    && ok "the inventory is EMPTY: no tree, no corpse, no ledger — nothing to read back from" \
    || bad "the wipe left something behind: $(tail -3 "$WORK/scan_wiped.log" | tr '\n' ' ')"
mkdir -p "$WORK/mnt_dead"
timeout 30 "$Y" --home "$WORK/client" mount --view "$V1" --mountpoint "$WORK/mnt_dead" \
    --meta-server "$META" --data-servers "$DATA" --foreground > "$WORK/m_dead.log" 2>&1
MRC=$?
[ "$MRC" != 0 ] && ok "and the view cannot mount (rc=$MRC) — the gate asked the cluster" \
    || bad "a wiped view still mounted — the gate is not asking the cluster"
grep -qi "subspace of the cluster" "$WORK/m_dead.log" \
    && ok "the refusal names subspace provisioning as the fix" \
    || bad "refusal text changed: $(head -3 "$WORK/m_dead.log")"

echo "===== 6. restore the grovefs keyspace in place, to version $V ====="
# `-k` scopes the restore to the root, so anything else that might live in this
# cluster is left alone; the range has to be empty first (see the header).
"$RES" start --dest-cluster-file "$CLUSTER" -r "$BU" -t "$TAG" -v "$V" -k "$ROOTRG" -w > "$WORK/restore.log" 2>&1
tail -4 "$WORK/restore.log" | sed 's/^/    /'
if noerr "$WORK/restore.log" && grep -q "Restored to version $V" "$WORK/restore.log"; then
    ok "restore completed at version $V with no error reported"
else
    bad "restore did not report success: $(tail -3 "$WORK/restore.log" | tr '\n' ' ')"
fi
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan > "$WORK/scan_back.log" 2>&1
chk "$(awk -v v="$V1" 'NR>1 && $1==v {print $4}' "$WORK/scan_back.log")" "view" \
    "the subspace came back as a live view (root row and all)"
chk "$(awk -v v="$V2" 'NR>1 && $1==v {print $4}' "$WORK/scan_back.log")" "view" \
    "and so did the second tree — the restore was not scoped to the damaged one"

echo "===== 7. mount the restored tree and read it back ====="
if mount_v "$V1" "$A" "$WORK/m_a3.log"; then
    listing "$A" > "$WORK/treeA.after"
    diff -u "$WORK/treeA.before" "$WORK/treeA.after" > "$WORK/treeA.diff"
    added=$(grep -c '^+' "$WORK/treeA.diff"); removed=$(grep -c '^-' "$WORK/treeA.diff")
    # diff's own header contributes one `-` and one `+` line, so an unchanged
    # tree reads 1/1 and "one file appeared" reads 1/2.
    chk "$removed/$added" "1/2" "nothing was lost and exactly one line was added (batch B's file)"
    grep -q '^+.*late\.txt$' "$WORK/treeA.diff" \
        && ok "the added line IS /late.txt — every pre-existing inode, mode and link count is unchanged" \
        || bad "something other than /late.txt changed: $(grep '^[+-]' "$WORK/treeA.diff" | head -4 | tr '\n' ' ')"
    (cd "$A" && sha256sum alpha.txt big.bin dir/nested.txt hard.txt) > "$WORK/bytesA.after"
    diff -q "$WORK/bytesA.before" "$WORK/bytesA.after" > /dev/null \
        && ok "all $(( $(wc -l < "$WORK/bytesA.after") )) recorded files read back byte-identical" \
        || bad "content differs after the restore"
    chk "$(cat "$A/dir/nested.txt" 2>/dev/null)" "nested-payload" "the subdirectory came back with its file"
    chk "$(readlink "$A/sym.txt" 2>/dev/null)" "alpha.txt" "the symlink target is still a string"
    chk "$(cat "$A/late.txt" 2>/dev/null)" "log-only-payload" \
        "batch B (written after the snapshot) came back — the LOG was replayed"
    umount_mnt "$A" || bad "could not unmount $A after the readback"
else
    bad "the restored view will not mount: $(tail -6 "$WORK/m_a3.log" | tr '\n' ' ')"
fi
if mount_v "$V2" "$B" "$WORK/m_b3.log"; then
    chk "$(cat "$B/keep.txt" 2>/dev/null)" "second-tree-content" \
        "the second tree kept what it held before the backup"
    umount_mnt "$B" || bad "could not unmount $B"
else
    bad "the second tree will not mount after the restore"
fi

echo "===== 8. the restored ledger must already agree with the restored trees ====="
RB=$("$Y" refs --meta-store "$URI" --meta-dir "$WORK/home" rebuild 2>&1)
echo "$RB" | grep -q "0 row(s) corrected, 0 row(s) deleted" \
    && ok "refs rebuild is a NO-OP in place (ledger and trees agree after the wipe)" \
    || { bad "the in-place restore left the ledger inconsistent: $(echo "$RB" | head -2)"; say "$RB"; }

echo "===== 9. disaster half: restore the whole database into a FRESH cluster ====="
DESTCF="$WORK/restore.cluster"
URI2="fdb://$DESTCF"
printf 'grovefs:restdev2@127.0.0.1:4600\n' > "$DESTCF"
mkdir -p "$WORK/data2" "$WORK/logs2"
setsid nohup "$BIN/fdbserver.x86_64" --cluster-file "$DESTCF" \
    -d "$WORK/data2" -L "$WORK/logs2" -l 127.0.0.1:4600 -p 127.0.0.1:4600 \
    -m 2000000 > "$WORK/server2.out" 2>&1 9>&- & SERVER_PIDS="$SERVER_PIDS $!"
avail2(){ dget "status minimal" 2>&1 | grep -q "database is available"; }
# Configure FIRST: a brand-new cluster is not "empty and ready", it is
# unconfigured, and every status call on it costs the full timeout. On an
# already-configured destination this fails harmlessly and is ignored.
dget "configure new single ssd" > "$WORK/cfg2.log" 2>&1 || true
for _ in $(seq 1 20); do sleep 1; avail2 && break; done
if avail2; then
    ok "a second, EMPTY cluster is up at $DESTCF"
else
    bad "the second cluster never became available: $(tail -3 "$WORK/server2.out" | tr '\n' ' ')"
fi
# `fdbrestore start` only SUBMITS a task: the task is executed by a backup agent
# connected to the DESTINATION cluster. With no agent there it sits in
# "queued" forever (measured: 2.5 minutes of nothing, which reads exactly like a
# slow restore and is not one — a fresh cluster has no agent by construction).
mkdir -p "$WORK/agent2"
setsid nohup "$AGENT" -C "$DESTCF" --log --logdir "$WORK/agent2" \
    > "$WORK/agent2.out" 2>&1 </dev/null 9>&- &
sleep 6
pgrep -f "backup_agent -C $DESTCF" >/dev/null \
    && ok "a backup_agent on the DESTINATION will execute the restore task" \
    || bad "no agent on the destination — the restore would stay 'queued' forever"
# No `-k` here: an empty destination is exactly what makes the unscoped
# whole-database restore legal — the case an operator hits after losing a machine.
"$RES" start --dest-cluster-file "$DESTCF" -r "$BU" -t "$TAG" -v "$V" -w > "$WORK/restore2.log" 2>&1
tail -3 "$WORK/restore2.log" | sed 's/^/    /'
if noerr "$WORK/restore2.log" && grep -q "Restored to version $V" "$WORK/restore2.log"; then
    ok "the whole database restored into the empty cluster (no key range needed)"
else
    bad "whole-DB restore failed: $(tail -3 "$WORK/restore2.log" | tr '\n' ' ')"
fi
# A fresh `--meta-dir`: no definition exists for this name on this machine.
# `view create` must REFUSE — not because the name is a corpse, but because
# `has_physical` asks the cluster and the root row is already there (the error is
# "already exists"). Recovery therefore needs no provisioning step at all; what
# matters is that the MOUNT works without one.
if out=$("$Y" view --meta-store "$URI2" --meta-dir "$WORK/home2" create --view "$V1" 2>&1); then
    bad "view create re-created a tree that the restore had already put there"
else
    echo "$out" | grep -q "already exists" \
        && ok "view create refuses with 'already exists' — the restored tree is visible to the cluster, not a corpse" \
        || bad "unexpected refusal: $(echo "$out" | head -2 | tr '\n' ' ')"
fi
[ -e "$WORK/home2/meta/$V1.db" ] \
    && bad "the fresh control plane grew a local db file for an fdb:// view" \
    || ok "and nothing was written into the fresh control plane to make it mountable"
"$Y" meta-server --listen "$META2" --home "$WORK/home2" --meta-store "$URI2" > "$WORK/meta2.log" 2>&1 9>&- & SERVER_PIDS="$SERVER_PIDS $!"
wait_port "$META2" || bad "the second meta-server did not bind"
mount_d(){
    mkdir -p "$D"
    "$Y" --home "$WORK/client" mount --view "$V1" --mountpoint "$D" \
        --meta-server "$META2" --data-servers "$DATA" --foreground > "$1" 2>&1 9>&- &
    MOUNT_PIDS="$MOUNT_PIDS $!"
    mnt_gate "$D"
}
if mount_d "$WORK/m_d1.log"; then
    ok "the restored cluster mounts on its own (tree from cluster B, bytes from the one data-dir)"
    listing "$D" > "$WORK/treeD.after"
    diff -q "$WORK/treeA.after" "$WORK/treeD.after" > /dev/null \
        && ok "its listing is exactly the one the source cluster showed after its restore" \
        || bad "the fresh-cluster copy differs: $(diff "$WORK/treeA.after" "$WORK/treeD.after" | head -4 | tr '\n' ' ')"
    (cd "$D" && sha256sum alpha.txt big.bin dir/nested.txt hard.txt) > "$WORK/bytesD.after"
    diff -q "$WORK/bytesA.before" "$WORK/bytesD.after" > /dev/null \
        && ok "and every byte is identical there too" || bad "content differs on the restored cluster"
    chk "$(cat "$D/late.txt" 2>/dev/null)" "log-only-payload" "including the file only the log held"
    umount_mnt "$D" || bad "could not unmount $D"
else
    bad "the restored cluster did not mount: $(tail -6 "$WORK/m_d1.log" | tr '\n' ' ')"
fi

echo "===== 10. point-in-time, both directions, on the second cluster ====="
# Back to the SNAPSHOT version: the same tree, the same definition, one version
# earlier — /late.txt did not exist yet, everything else did. The destination
# range has to be cleared first (FDB refuses to restore over existing rows),
# which is also the honest illustration of what a rollback takes.
dget "writemode on; clearrange \"\\x01grovefs\" \"\\x01grovefs\\xff\"" > "$WORK/clear2.log" 2>&1
grep -q Committed "$WORK/clear2.log" && say "$(tail -1 "$WORK/clear2.log")" \
    || bad "clearrange on the second cluster failed: $(tail -3 "$WORK/clear2.log" | tr '\n' ' ')"
"$RES" start --dest-cluster-file "$DESTCF" -r "$BU" -t "$TAG" -v "$snap" -k "$ROOTRG" -w > "$WORK/restore3.log" 2>&1
tail -3 "$WORK/restore3.log" | sed 's/^/    /'
if noerr "$WORK/restore3.log" && grep -q "Restored to version $snap" "$WORK/restore3.log"; then
    ok "restored the same tree back to the snapshot version $snap"
else
    bad "the rollback restore failed: $(tail -3 "$WORK/restore3.log" | tr '\n' ' ')"
fi
if mount_d "$WORK/m_d2.log"; then
    listing "$D" > "$WORK/treeD.snap"
    diff -q "$WORK/treeA.before" "$WORK/treeD.snap" > /dev/null \
        && ok "at the snapshot version the tree is EXACTLY the pre-backup one" \
        || bad "the snapshot-version tree differs: $(diff "$WORK/treeA.before" "$WORK/treeD.snap" | head -4 | tr '\n' ' ')"
    if [ -e "$D/late.txt" ]; then
        bad "/late.txt exists at version $snap — the restore ignored the requested version"
    else
        ok "/late.txt is GONE at $snap and present at $V: the version parameter decides the contents"
    fi
    umount_mnt "$D" || bad "could not unmount $D"
else
    bad "the snapshot-version tree will not mount: $(tail -6 "$WORK/m_d2.log" | tr '\n' ' ')"
fi
RB2=$("$Y" refs --meta-store "$URI2" --meta-dir "$WORK/home2" rebuild 2>&1)
echo "$RB2" | grep -q "0 row(s) corrected, 0 row(s) deleted" \
    && ok "the ledger restored with the trees is self-consistent on the new cluster too" \
    || { bad "ledger disagrees after a whole-DB restore: $(echo "$RB2" | head -2)"; say "$RB2"; }
"$Y" view --meta-store "$URI2" --meta-dir "$WORK/home2" rm --name "$V1" --base \
    --data-dir "$WORK/data" > /dev/null 2>&1 && ok "the restored copy was removed again" \
    || bad "could not remove the restored copy from the second cluster"

echo "===== 11. the source cluster: this run's two trees are live ====="
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan > "$WORK/scan_final.log" 2>&1
awk 'NR>1 && ($4=="view" || $4=="corpse" || $4=="ledger") {print "    "$1" "$2" keys "$4}' "$WORK/scan_final.log"
chk "$(awk -v v="$V1" 'NR>1 && $1==v {print $4}' "$WORK/scan_final.log")" "view" "$V1 is a live tree again"
chk "$(awk -v v="$V2" 'NR>1 && $1==v {print $4}' "$WORK/scan_final.log")" "view" "$V2 is a live tree"

echo "===== 12. teardown leaves the cluster as this run found it ====="
# The removal is done HERE, visibly, instead of leaving it to the trap: a
# teardown failure that only ever writes to /dev/null is indistinguishable from
# a clean run (exactly how one aborted run left a corpse behind for days).
for v in "$V1" "$V2"; do
    out=$("$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm --name "$v" --base \
        --data-dir "$WORK/data" 2>&1) \
        && ok "removed $v" || bad "could not remove $v: $(echo "$out" | head -2 | tr '\n' ' ')"
done
chk "$(inv_names | wc -l)" "$baseline" \
    "the cluster holds exactly the subspaces it held before this run (no leak, no orphan booking)"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" = 0 ] && echo "FDB_BACKUP_MOUNT_GREEN" || echo "FDB_BACKUP_MOUNT_RED"
[ "$fail" = 0 ]
