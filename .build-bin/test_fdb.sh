#!/usr/bin/env bash
# FoundationDB metadata backend end-to-end over real FUSE mounts against a
# central meta-server started with `--meta-store fdb://<cluster-file>`.
#
# What it proves (P3's smoke criterion in docs/fdb-keyspace.md):
#   1. serve() starts on an FDB store and BANNERS the backend by name (P2), and
#      GroveFS-native HA (`--role standby`/`--peer`) is refused with the FDB
#      text — the one that talks about the cluster's own replication, not about
#      PG streaming replication;
#   2. a base view is one SUBSPACE, provisioned explicitly by
#      `view create --meta-store fdb://…` and never by a mount;
#   3. the tree surface round-trips through FDB: mkdir / write / read / ls,
#      hard link, symlink, rename, rename-overwrite, EEXIST, ENOTEMPTY, unlink;
#   4. two clients on one view converge on the single authoritative subspace
#      (no local metadata on either side);
#   5. unmount + remount shows byte-identical content — the transactional commit
#      points P3 claims really are durable, with no engine to flush;
#   6. `du --meta-server` totals the same bytes over M_TREE_DUMP, i.e. FDB's
#      dump is the same artifact SQLite's and PG's are;
#   7. `gc` refuses an fdb:// store instead of auditing a directory that is not
#      the authority — a silent "nothing to reclaim" there would delete bytes a
#      subspace names. `view export`/`view import` do NOT refuse: metadata moves
#      as the portable dump rather than as a file, which is what makes the P8
#      round trip in section 12 possible at all;
#   8. the lock surface (§3.7) answers over two mounts of one subspace: R/W
#      conflict, same-inode one-winner-one-loser, cross-inode independence,
#      selective (segmented) unlock, and an implicit release driven by the
#      `'lo` owner key while the lease is nowhere near expiring.
#   9. a derived lens and a pure alias render, invalidate and govern EXACTLY the
#      shape the SQLite backend does — proven against a live SQLite twin of the
#      same deployment (same binary, same rule files, same seeding function) by
#      comparing listings byte-for-byte, with literal expectations asserted on top
#      so "identical" cannot mean "identically wrong"; and `fork`/`graft` answer
#      with an explicit verdict instead of a half-working local tree.

#  10. the P8 migration round trip goes BOTH ways with the existing commands: a
#      SQLite view's dump imports into a subspace nobody provisioned, the mounted
#      import shows the same listing/inodes/links/mode and reads the copied
#      blobs, and its re-export is LINE-FOR-LINE the source (ids and inodes keep
#      their exported values); a create after the import gets an id above every
#      imported one (the allocator's setval), a second import without --replace
#      refuses, a corrupt dump leaves the installed tree untouched, the same dump
#      drops back into a fresh SQLite store, and the source store never changed.
#
# Infrastructure: `.build-bin/fdb_env.sh` (user-space fdbserver, no root, no
# system package). The suite SKIPS cleanly (exit 0, no FAILs) when the cluster
# is unreachable, so the board stays GREEN on hosts without FDB; set
# FDB_TEST=1 to turn absence into a failure (CI).
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"

FDB_HOME="${FDB_HOME:-/data/grovefs/.fdbbin}"
FDB_RUN_DIR="${FDB_RUN_DIR:-/data/grovefs/.fdbtest}"
CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-$FDB_RUN_DIR/test.cluster}"
FDBURI="fdb://$CLUSTER"

# ---- cluster reachability probe (skip cleanly if absent) -------------------
fdb_available(){
    [ -x "$FDB_HOME/fdbcli.x86_64" ] && [ -f "$CLUSTER" ] || return 1
    "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" --exec "status minimal" 2>&1 \
        | grep -q "The database is available"
}
if ! fdb_available; then
    if [ "${FDB_TEST:-0}" = 1 ]; then
        echo "SUMMARY: pass=0 fail=1 SKIP=0 (FDB_TEST=1 but no available cluster at $CLUSTER)"
        exit 1
    fi
    echo "SUMMARY: pass=0 fail=0 SKIP=1 (no FoundationDB cluster; bash .build-bin/fdb_env.sh fetch && ... up)"
    exit 0
fi

# The binary has to link libfdb_c, which the default release build does not, so
# this suite builds its own feature-enabled one (cached after the first run) —
# and a build WITHOUT the feature is exactly the case that must refuse, not
# silently fall back to SQLite, so `$Y` is never the plain target/release binary.
Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
if [ ! -x "$Y" ] || [ -n "${FDB_REBUILD:-}" ]; then
    echo "===== 0) cargo build --release --features fdb (target/fdb) ====="
    FDB_CLIENT_LIB_PATH="${FDB_CLIENT_LIB_PATH:-$FDB_HOME/lib}" \
    LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}" \
        cargo build --release --features fdb --target-dir target/fdb > .fdbtest-build.log 2>&1 \
        || { echo "FAIL: build with --features fdb (see .fdbtest-build.log)"; tail -20 .fdbtest-build.log; exit 1; }
fi
# Fallback only. Since `build.rs` bakes `$ORIGIN/lib`, `$ORIGIN/../lib` and the
# build host's client dir into RUNPATH, a feature binary resolves libfdb_c by
# itself — section 0b below asserts exactly that. The export keeps the suite
# working for a `$GROVEFS_FDB_BIN` that predates that change.
export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"

META=127.0.0.1:15186
META_REFUSE=127.0.0.1:15188
DATA=127.0.0.1:15187
VIEW="fd$(date +%s)"      # unique per run: a fresh subspace, no bleed between runs
S=.fdbtest
rm -rf $S; mkdir -p $S/central/data $S/home $S/h1 $S/h2 $S/mnt_a $S/mnt_b $S/mnt_r $S/mnt_no $S/refuse

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
# Mandatory fail-fast gate (the board's standing lesson): a mount that never came
# up must abort the suite, not let the following writes land in a bare directory
# and produce green numbers about nothing.
mnt_gate(){ local mp="$PWD/$1"; for _ in $(seq 1 60); do
    findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }
# ls_wait: list a mountpoint only once the DAEMON answers a stat of it. `findmnt`
# reports the mount as attached a moment before the first getattr can be replied
# to, and reading that window as "an empty tree" would make the assertion about a
# race instead of about metadata. The VALUE is still asserted verbatim — only the
# moment of asking moves. (Measured on a healthy remote mount: attached at 65ms,
# first getattr answered at 69ms; the loop exists for the window, not for seconds.)
ls_wait(){ local mp="$1" i
    for i in $(seq 1 40); do [ -d "$mp" ] && { T ls "$mp"; return 0; }; sleep 0.25; done
    echo "ls_wait: $mp never became accessible" >&2; return 1
}

# The cluster's tree inventory, one name per line, sorted. `$4` is the KIND column
# ("view" for a live tree, "corpse" for one that kept its keys but lost its root
# row, "ledger" for the shared `refs` subspace); the summary and the trailing note
# line do not have one of those in that column, so they fall out. Corpses are
# candidates too — they are the ones most in need of removal, since nothing can
# mount them while they keep their objects booked.
inv_list(){ "$Y" view --meta-store "$FDBURI" --meta-dir "$S/home" scan 2>/dev/null \
    | awk 'NR>1 && ($4=="view" || $4=="corpse"){print $1}' | sort; }
cleanup(){
    # Every mount of this suite lives under `$S/mnt_*`, so the unmount sweep cannot
    # fall behind a section that adds one.
    for d in "$S"/mnt_*; do [ -d "$d" ] && fusermount -u "$d" 2>/dev/null; done
    # Never `kill 0`: PID 0 means the caller's whole process group, so a suite
    # that zeroes its placeholders before the mounts exist would signal the
    # harness that launched it (and exit 143 on a printed-green summary).
    for p in $BG "${MAP:-}" "${MBP:-}" "${MRP:-}" "$DP" "$MP"; do
        case "$p" in ''|0|*[!0-9]*) continue ;; esac
        kill "$p" 2>/dev/null || true
    done
    wait 2>/dev/null
    # Teardown, now that the command exists: `view rm --base` deletes a subspace
    # AND unwinds the shared ledger in that order, so a run leaves the cluster as
    # it found it instead of +N subspaces per run forever (the state that made an
    # inventory necessary: 169 orphan trees had accumulated).
    #
    # It removes the SET DIFFERENCE against the inventory this run found, not a
    # hand-kept list of names: a section that provisions a view the list does not
    # know about would otherwise leak its subtree forever, and a section that
    # FAILs halfway never reaches its own teardown at all. Silent on purpose — a
    # FAIL printed by a trap would contradict the SUMMARY just printed. The control
    # plane is LOCAL files under fdb:// (C6), and every definition in it belongs to
    # this run, so dropping it is what satisfies the "others resolve through this
    # base" guard rather than working around it.
    if [ -x "$Y" ] && fdb_available 2>/dev/null && [ -f "$S/inv_before.txt" ]; then
        rm -rf "$S/home" "$S/ev/home"
        inv_list > "$S/inv_after.txt" 2>/dev/null
        comm -13 "$S/inv_before.txt" "$S/inv_after.txt" | while read -r v; do
            [ -n "$v" ] || continue
            "$Y" view --meta-store "$FDBURI" --meta-dir "$S/home" rm --name "$v" --base \
                --data-dir "$S/central/data" >/dev/null 2>&1 || true
        done
    fi
}
MAP=0; MBP=0; MRP=0
# Backgrounded processes of the later sections register here as well, so one trap
# owns every child the suite spawns (servers and mounts alike).
BG=""
bg(){ "$@" & BG="$BG $!"; }
trap cleanup EXIT

# What the cluster held before this run. Everything above is build/probe only, so
# this is the baseline a teardown may remove ITSELF down to — no earlier, no later.
inv_list > $S/inv_before.txt
echo "  .. cluster held $(wc -l < $S/inv_before.txt) tree(s) before this run"

echo "===== 0b) the feature binary needs no loader environment ====="
# `libfdb_c` is never a system package here (see .build-bin/fdb_env.sh), so a
# binary that cannot find it on its own turns "run this program" into "run this
# program with someone else's environment". An EMPTY environment is the only
# honest test — LD_LIBRARY_PATH is exported above, so anything inherited proves
# nothing.
if env -i PATH=/usr/bin:/bin ldd "$Y" 2>/dev/null | grep -q 'libfdb_c\.so => /'; then
    ok "resolves libfdb_c with an empty environment (RUNPATH, no LD_LIBRARY_PATH)"
else
    bad "needs LD_LIBRARY_PATH to find libfdb_c — RUNPATH missing, build.rs regressed"
fi

echo "===== 1) serve() refuses GroveFS-native HA under fdb:// (FDB-specific text) ====="
$Y meta-server --listen $META_REFUSE --home $S/refuse --meta-store "$FDBURI" \
    --role standby --peer 127.0.0.1:1 > $S/refuse.log 2>&1
RRC=$?
[ "$RRC" != 0 ] && ok "FDB + native HA refused at startup (rc=$RRC)" \
    || bad "FDB + HA should have failed but returned 0"
grep -qi "fdbbackup" $S/refuse.log \
    && ok "refusal text names the cluster's own backup story" \
    || bad "refusal text unclear: $(cat $S/refuse.log)"
grep -qi "streaming" $S/refuse.log \
    && bad "FDB refusal reused the PG streaming text" \
    || ok "FDB refusal does not reuse the PG wording"

echo "===== 2) provision the base view (a subspace), then start the servers ====="
$Y view --meta-store "$FDBURI" --meta-dir $S/home create --view "$VIEW" > $S/create.log 2>&1 \
    || { bad "view create --meta-store fdb://"; cat $S/create.log; echo "SUMMARY: pass=$pass fail=$fail SKIP=0"; exit 1; }
grep -q "subspace" $S/create.log && ok "create reports a subspace, not a db file" \
    || bad "create output unexpected: $(cat $S/create.log)"
chk "$($Y view --meta-dir $S/home list 2>/dev/null | grep -c "^$VIEW\$")" "0" \
    "an FDB view has no local db file for \`view list\` to find"
# The listing command itself must speak the backend, not fall through: a plain
# \`view list --meta-store fdb://…\` that errored into the SQLite arm (or printed a
# stale local tree as if it were the authority) would be the silent-downgrade bug.
$Y view --meta-store "$FDBURI" --meta-dir $S/home list > $S/list.log 2>&1 \
    && ok "view list answers for an fdb:// store" \
    || bad "view list --meta-store fdb:// failed: $(head -3 $S/list.log)"

$Y data-server --listen $DATA --data-dir $S/central/data > $S/data.log 2>&1 & DP=$!
# Long lease ON PURPOSE, and it must be in the meta-server's environment because
# under a remote mount that process is where lock_op runs. test_locks.sh shrinks
# the lease to 3s so that expiry-based cleanup can rescue a dead holder; here the
# opposite is the point: at 60s no assertion in section 9 can pass because a row
# happened to expire, so every "the peer got the lock" below is proof that the
# owner-key implicit release (`'lo` → release_locks_of_owner) really fired.
export GROVEFS_LOCK_LEASE_SECS=60
$Y meta-server --listen $META --home $S/home --meta-store "$FDBURI" > $S/meta.log 2>&1 & MP=$!
wait_port $DATA || { bad "data-server did not bind"; cat $S/data.log; }
wait_port $META || { bad "meta-server did not bind"; cat $S/meta.log; echo "SUMMARY: pass=$pass fail=$fail SKIP=0"; exit 1; }
grep -q "metadata backend foundationdb" $S/meta.log \
    && ok "startup banner names the backend (P2)" \
    || bad "banner missing the backend line: $(head -5 $S/meta.log)"

echo "===== 3) mount A against the subspace ====="
$Y --home $S/h1 mount --view "$VIEW" --mountpoint $S/mnt_a --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_a.log 2>&1 & MAP=$!
mnt_gate $S/mnt_a || { bad "mount A never came up (fail-fast, no fake numbers)"; cat $S/mnt_a.log; echo "SUMMARY: pass=$pass fail=$fail SKIP=0"; exit 1; }
A=$S/mnt_a
ok "mount A up (findmnt gate)"

T mkdir "$A/projects"                    && ok "A mkdir (FDB 'n'/'d' keys)" || bad "A mkdir failed"
T sh -c "printf 'alpha' > $A/hello"      && ok "A write file"               || bad "A write failed"
sync "$A" 2>/dev/null; sleep 1.0
chk "$(T cat $A/hello)" "alpha" "A reads back its own file"
T mkdir -p "$A/sub"                      && ok "A mkdir -p sub"             || bad "A mkdir -p failed"

# The existence gate must be asked of the CLUSTER: a name with no subspace is
# refused (and refused with the hint that actually provisions a subspace), so the
# gate cannot be satisfied by some stale meta/<name>.db lying in --home.
$Y --home $S/h1 mount --view "${VIEW}_ghost" --mountpoint $S/mnt_no --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_no.log 2>&1
GRC=$?
[ "$GRC" != 0 ] && ok "an unprovisioned view is refused at M_OPEN (rc=$GRC)" \
    || bad "an unprovisioned view was ACCEPTED — the gate is not asking the cluster"
grep -qi "subspace of the cluster" $S/mnt_no.log \
    && ok "the refusal names subspace provisioning as the fix" \
    || bad "the refusal gave a non-FDB hint: $(head -3 $S/mnt_no.log)"

echo "===== 4) rename / overwrite / EEXIST / ENOTEMPTY / link / symlink ====="
T sh -c "printf 'beta' > $A/sub/renamed" && ok "A write into sub"           || bad "A write into sub failed"
T mv "$A/sub/renamed" "$A/sub/moved"     && ok "A rename (one FDB tx)"      || bad "A rename failed"
chk "$(T cat $A/sub/moved)" "beta" "content survived rename"
# overwrite onto an existing name: the displaced object must not come back as a
# readable file, and the new content must be what the source held.
T sh -c "printf 'gamma' > $A/over"       && ok "A write overwrite target"   || bad "A write failed"
T mv "$A/sub/moved" "$A/over"            && ok "A rename over an existing file" || bad "A rename-overwrite failed"
chk "$(T cat $A/over)" "beta" "overwrite replaced the content"
T sh -c "printf 'dup' > $A/hello" 2>/dev/null; chk "$?" "0" "second write to an existing name allowed"
# EEXIST the way a shell exposes it: a hard link onto a taken name.
T ln "$A/over" "$A/hardlink"             && ok "A hard link (FDB 'h' key)"   || bad "A hardlink failed"
T ln "$A/over" "$A/hardlink" 2>/dev/null; chk "$?" "1" "hard link onto a taken name fails EEXIST"
chk "$(stat -c %h "$A/over" 2>/dev/null)" "2" "two names share one inode (st_nlink=2)"
T ln -s projects "$A/sym"                && ok "A symlink (create_symlink)" || bad "A symlink failed"
chk "$(T readlink $A/sym)" "projects" "symlink target round-trips"
# ENOTEMPTY: renaming a directory onto a NON-EMPTY directory must fail and leave
# both subtrees alone — the FDB arm reaches it by counting the target's prefix.
# `mv -T` is what makes this a rename(2) at all: plain `mv nd1 nd2` on an
# existing directory destination means "move INTO nd2", which is a different
# (and legal) operation and proved nothing here. And the TARGET has to be the
# non-empty one — renaming onto `nd2` itself is not a rename at all, while an
# empty `nd2/inner` is a legal target (POSIX only refuses a NON-empty one).
T mkdir -p "$A/nd1" "$A/nd2/inner/leaf" 2>/dev/null
T mv -T "$A/nd1" "$A/nd2/inner" 2>/dev/null; rc=$?
[ "$rc" != 0 ] && ok "rename onto non-empty dir refused (ENOTEMPTY)" \
    || bad "rename onto non-empty dir was ALLOWED — children would be orphaned"
[ -d "$A/nd2/inner/leaf" ] && ok "the non-empty target kept its children" || bad "target subtree damaged by a refused rename"
[ -d "$A/nd1" ] && ok "the refused source kept its own subtree" || bad "source vanished on a rename that was refused"
# The complementary case must still WORK: an EMPTY directory target is the one
# rename-onto-directory POSIX allows, and the same FDB arm decides it by the
# same prefix count (0 here).
T mkdir -p "$A/ed1" "$A/ed2" 2>/dev/null
T mv -T "$A/ed1" "$A/ed2" 2>/dev/null; rc=$?
if [ "$rc" = 0 ] && [ ! -e "$A/ed1" ] && [ -d "$A/ed2" ]; then
    ok "rename onto an EMPTY dir succeeds (the same count decides 0 children)"
else
    bad "rename onto an empty dir should succeed (rc=$rc, ed1 still there: $([ -e "$A/ed1" ] && echo yes))"
fi

echo "===== 5) mount B on the same view: one authoritative subspace ====="
$Y --home $S/h2 mount --view "$VIEW" --mountpoint $S/mnt_b --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_b.log 2>&1 & MBP=$!
mnt_gate $S/mnt_b || { bad "mount B never came up"; cat $S/mnt_b.log; }
B=$S/mnt_b
T ls "$B" | grep -qx projects && ok "B sees A's 'projects' dir" || bad "B missing A's dir ($(T ls $B | tr '\n' ' '))"
chk "$(T cat $B/over)" "beta" "B reads A's renamed file (shared blob)"
T sh -c "printf 'from-b' > $B/second" && ok "B writes" || bad "B write failed"
sync "$B" 2>/dev/null; sleep 1.5
chk "$(T cat $A/second)" "from-b" "A sees B's new file (single authoritative tree)"

echo "===== 6) unlink: row gone from both mounts, last name frees the object ====="
T rm "$A/hardlink" && ok "A removes the hard link name" || bad "A unlink failed"
sleep 1.0
[ ! -e "$B/hardlink" ] && ok "hard link gone on B (shared delete)" || bad "hard link still visible on B"
chk "$(stat -c %h "$A/over" 2>/dev/null)" "1" "nlink back to 1 after the sibling unlink"
T rm "$A/second" && ok "A removes B's file" || bad "A unlink of B's file failed"

echo "===== 7) du over M_TREE_DUMP (FDB's dump is the same artifact) ====="
T sh -c "printf '0123456789' > $A/sized" && ok "A writes a known-size file" || bad "A write failed"
sync "$A" 2>/dev/null; sleep 1.5
# The expected number is read off the MOUNT, not guessed in the script: whatever
# the client sees as regular-file bytes is what the server's dump has to account
# for. A hand-written sum would just be a second guess about the tree.
EXPECT=$(find "$A" -type f -printf '%s\n' 2>/dev/null | awk '{s+=$1} END{print s+0}')
DU=$($Y du --view "$VIEW" --meta-server $META --data-servers $DATA -b --total 2>$S/du.err | awk '$2=="/"{print $1}')
chk "$DU" "$EXPECT" "du --meta-server accounts exactly the bytes the mount shows ($DU)"
[ "$DU" -gt 0 ] 2>/dev/null && ok "the FDB dump produced a non-empty total" \
    || bad "du over M_TREE_DUMP gave '$DU' ($(head -2 $S/du.err))"

echo "===== 8) the offline local commands refuse an fdb:// store ====="
# `gc` is an OFFLINE command: it has no `--meta-store` flag of its own and reads
# the backend from GROVEFS_META_STORE, exactly like a deployment's unit file does.
GROVEFS_META_STORE="$FDBURI" $Y gc --meta-dir $S/home --data-dir $S/central/data --apply > $S/gc.log 2>&1
chk "$?" "1" "gc --apply refuses an FDB store"
grep -qi "subspaces in the cluster" $S/gc.log && ok "gc refusal names the reason" \
    || bad "gc refusal text unclear: $(head -3 $S/gc.log)"
# P8 turned this one around: a subspace dumps into the SAME portable format a
# SQLite db or a PG schema does, so `view export` works here instead of refusing
# (the dump carries metadata, not bytes, and asks nothing of a local directory).
$Y view --meta-store "$FDBURI" --meta-dir $S/home export --view "$VIEW" --out $S/exp.jsonl > $S/exp.log 2>&1
chk "$?" "0" "view export dumps an FDB subspace"
grep -q "exported view" $S/exp.log && ok "the FDB export reports its counts" \
    || bad "export output unclear: $(head -3 $S/exp.log)"
head -1 $S/exp.jsonl | grep -q '"format":"grovefs-view-metadata"' \
    && ok "the FDB dump speaks the backend-neutral format" \
    || bad "export header unexpected: $(head -1 $S/exp.jsonl)"
[ "$(wc -l < $S/exp.jsonl)" -ge 2 ] && ok "the dump is a header plus one line per node" \
    || bad "export body empty"

echo "===== 9) lock surface: the four §3.7 methods over two mounts of one subspace ====="
# Two probes, one writer-style like the rest of the board: `flk.py` covers the
# flock(2) shape (whole-file, per-open-file description) and `rng.py` the fcntl
# F_SETLK shape (byte ranges, per-process owner). The holder always locks through
# mount A and the peer always asks through mount B, so a grant on B can only come
# from what the authoritative subspace says — never from a client-side cache.
cat > $S/flk.py <<'PY'
import sys, os, fcntl, time
cmd, path, kind, secs = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
flag = fcntl.LOCK_SH if kind == 's' else fcntl.LOCK_EX
fd = os.open(path, os.O_RDWR)
if cmd == 'hold':
    fcntl.flock(fd, flag)
    print('locked', flush=True)
    time.sleep(secs)
else:
    try:
        fcntl.flock(fd, flag | fcntl.LOCK_NB)
        print('granted'); os.close(fd)
    except OSError as ex:
        print('refused errno=%d' % ex.errno); os.close(fd); sys.exit(1)
PY
cat > $S/rng.py <<'PY'
import sys, os, fcntl, struct, signal
cmd, path, kind, s, e = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
extra = sys.argv[6] if len(sys.argv) > 6 else ''
t = {'r': fcntl.F_RDLCK, 'w': fcntl.F_WRLCK}[kind]
def op(fd, ty, a, b):
    fcntl.fcntl(fd, fcntl.F_SETLK, struct.pack('hhqqi', ty, 0, a, b - a, 0))
fd = os.open(path, os.O_RDWR)
if cmd == 'ask':
    try:
        op(fd, t, s, e); print('granted')
    except OSError as ex:
        print('refused errno=%d' % ex.errno); os.close(fd); sys.exit(1)
else:
    op(fd, t, s, e)
    if extra:
        a2, b2 = (int(v) for v in extra.split(','))
        op(fd, t, a2, b2)
    print('locked', flush=True)
    asked_to_quit = False
    for line in sys.stdin:              # "unlock S E" drops ONE segment only
        w = line.split()
        if w[:1] == ['unlock'] and len(w) == 3:
            op(fd, fcntl.F_UNLCK, int(w[1]), int(w[2])); print('unlocked', flush=True)
        elif w[:1] == ['quit']:
            asked_to_quit = True
            break
    # A backgrounded holder whose stdin is NOT the control pipe hits EOF at once.
    # Closing the fd there would release the lock and turn the assertion below it
    # into a probe against a dead owner, so EOF means "keep holding until killed";
    # only an explicit `quit` (or the signal itself) ends the lease.
    if not asked_to_quit:
        signal.pause()
    os.close(fd)
PY
T sh -c "printf 'lockme' > $A/lk1.bin"    && ok "A creates the lock target lk1.bin"   || bad "A lk1 create failed"
T sh -c "printf 'lockme' > $A/lk2.bin"    && ok "A creates the lock target lk2.bin"   || bad "A lk2 create failed"
T sh -c "printf 'lockme' > $A/lk3.bin"    && ok "A creates the lock target lk3.bin"   || bad "A lk3 create failed"
sync "$A" 2>/dev/null; sleep 1.0
# Every "refused" assertion below is only meaningful while the holder is alive:
# a holder that died of its own accord releases its lock legitimately and would
# make the peer's grant look like an implementation bug. Gate on BOTH signals.
held(){ grep -q locked "$1" 2>/dev/null && kill -0 "$2" 2>/dev/null; }

# -- (a) R/W conflict, and its mirror image: S/S must coexist ------------------
python3 $S/flk.py hold "$A/lk1.bin" s 12 > $S/rs.out 2>&1 & RH=$!
for _ in $(seq 1 60); do grep -q locked $S/rs.out 2>/dev/null && break; sleep 0.05; done
grep -q locked $S/rs.out && ok "B-side peer sees a shared holder on lk1" || bad "shared holder never acquired"
rk=$(T python3 $S/flk.py ask "$B/lk1.bin" x 0 2>&1 | tail -1)
[ "$rk" = granted ] && bad "an exclusive request cut in on a shared holder" \
    || ok "R/W conflict: flock -x refused while a shared lock is held ($rk)"
rk=$(T python3 $S/flk.py ask "$B/lk1.bin" s 0 2>&1 | tail -1)
[ "$rk" = granted ] && ok "the conflict is by type: a second shared lock is admitted ($rk)" \
    || bad "S/S should coexist but got '$rk'"
kill $RH 2>/dev/null; wait $RH 2>/dev/null
# Barrier with a voice of its own: `wait` proves the child is gone, not that the
# cluster forgot its row, so the fcntl probes below could otherwise be refused by
# a holder that died two lines earlier. If this fails it is saying so directly —
# a dead flock holder still blocking the inode is a release leak, not a flake.
gone=0
for _ in $(seq 1 50); do
    T python3 $S/flk.py ask "$A/lk1.bin" x 0 >/dev/null 2>&1 && { gone=1; break; }
    sleep 0.1
done
[ "$gone" = 1 ] && ok "the killed shared holder's row is gone (release reaped it)" \
    || bad "a dead shared holder still blocks lk1 — the flock release path leaked its row"

# -- (b) same inode, two writers: exactly one winner ---------------------------
# Ranges live high in the file on purpose: the segments (d) locks on this same
# inode are [0,50)/[60,100), so a row left behind here must not be able to refuse
# them — otherwise (d) would be measuring the previous section, not itself.
python3 $S/rng.py hold "$A/lk1.bin" w 2000 4096 > $S/w1.out 2>&1 & W1=$!
for _ in $(seq 1 60); do held $S/w1.out $W1 && break; sleep 0.05; done
held $S/w1.out $W1 && ok "same-inode winner: A got the fcntl W range [2000,4096)" \
    || bad "same-inode winner not holding: $(cat $S/w1.out)"
rk=$(T python3 $S/rng.py ask "$B/lk1.bin" w 2500 3000 2>&1 | tail -1)
[ "$rk" = granted ] && bad "same-inode loser was granted an overlapping [2500,3000)" \
    || ok "same-inode loser refused (one wins, one loses: $rk)"

# -- (c) cross-inode independence: the other inode is unaffected ---------------
rk=$(T python3 $S/rng.py ask "$B/lk2.bin" w 0 4096 2>&1 | tail -1)
[ "$rk" = granted ] && ok "cross-inode: a lock on lk1 does not touch lk2's own W request" \
    || bad "locks leaked across inodes (lk2 ask got '$rk')"
kill $W1 2>/dev/null; wait $W1 2>/dev/null
# Same barrier as (a), in the fcntl shape: (d) needs lk1 free of THIS holder
# before it can mean anything.
gone=0
for _ in $(seq 1 50); do
    T python3 $S/rng.py ask "$A/lk1.bin" w 3000 3100 >/dev/null 2>&1 && { gone=1; break; }
    sleep 0.1
done
[ "$gone" = 1 ] && ok "the killed writer's range row is gone (release reaped it)" \
    || bad "a dead fcntl holder still blocks lk1 — the release path leaked its row"

# -- (d) segmented unlock: dropping one range leaves its sibling ----------------
# Two DISJOINT rows from one owner. The engine's Unlock deletes the owner's rows
# that OVERLAP the given range (src/meta/engine.rs and src/pg_store.rs both run
# `DELETE ... AND l_start<=e AND l_end>=s` — it does not split a row), so
# "release [0,50) and the peer gets [20,30) but still cannot get [70,80)" is the
# assertion that discriminates: a blanket "unlock clears the owner" would wrongly
# grant [70,80), and a no-op unlock would keep refusing [20,30).
mkfifo $S/ctl 2>/dev/null
python3 $S/rng.py hold "$A/lk1.bin" w 0 50 60,100 < $S/ctl > $S/seg.out 2>&1 & SG=$!
exec 4<>$S/ctl
for _ in $(seq 1 60); do held $S/seg.out $SG && break; sleep 0.05; done
held $S/seg.out $SG && ok "segment holder took [0,50) and [60,100) as two rows" \
    || bad "segment holder not holding: $(cat $S/seg.out)"
rk=$(T python3 $S/rng.py ask "$B/lk1.bin" w 20 30 2>&1 | tail -1)
[ "$rk" = granted ] && bad "[20,30) granted while [0,50) is held" || ok "[20,30) refused while its covering row lives ($rk)"
echo "unlock 0 50" >&4
for _ in $(seq 1 60); do grep -q unlocked $S/seg.out 2>/dev/null && break; sleep 0.05; done
rk=$(T python3 $S/rng.py ask "$B/lk1.bin" w 20 30 2>&1 | tail -1)
[ "$rk" = granted ] && ok "after unlocking [0,50) the peer takes [20,30) (segment really dropped)" \
    || bad "unlock of [0,50) did not free the segment ($rk)"
rk=$(T python3 $S/rng.py ask "$B/lk1.bin" w 70 80 2>&1 | tail -1)
[ "$rk" = granted ] && bad "unlock also cleared the sibling row [60,100) — released more than asked" \
    || ok "the sibling segment [60,100) is still held ($rk)"
echo "quit" >&4; exec 4>&-
wait $SG 2>/dev/null

# -- (e) implicit release by OWNER: SIGKILL the holder, lease is 60s ------------
python3 $S/flk.py hold "$A/lk2.bin" x 120 > $S/kill.out 2>&1 & KH=$!
for _ in $(seq 1 60); do grep -q locked $S/kill.out 2>/dev/null && break; sleep 0.05; done
grep -q locked $S/kill.out && ok "victim holder took flock -x on lk2" || bad "victim holder failed to acquire"
rk=$(T python3 $S/flk.py ask "$B/lk2.bin" x 0 2>&1 | tail -1)
[ "$rk" = granted ] && bad "peer cut in on a LIVE exclusive holder" || ok "lk2 refused while the holder lived ($rk)"
kill -9 $KH 2>/dev/null; wait $KH 2>/dev/null
# Kernel closes the fd on death → FUSE `release` → release_locks_of_owner walks
# the 'lo prefix. Reaped within 10s of a 60s lease = the owner key did it.
reaped=0
for _ in $(seq 1 50); do
    T python3 $S/flk.py ask "$B/lk2.bin" x 0 >/dev/null 2>&1 && { reaped=1; break; }
    sleep 0.2
done
[ "$reaped" = 1 ] && ok "lk2 freed seconds after SIGKILL with a 60s lease ('lo owner release works)" \
    || bad "lk2 still held after SIGKILL — owner-key release did not happen (lease would have to expire)"

# -- (f) the lock rows lived in FDB, not in a client ---------------------------
# A brand-new client process sees the very same answer, which only holds if the
# rows were read back out of the subspace. lk3 is its own inode so nothing any
# earlier section left on lk1/lk2 can colour the answer.
python3 $S/rng.py hold "$A/lk3.bin" w 0 10 > $S/w2.out 2>&1 & W2=$!
for _ in $(seq 1 60); do held $S/w2.out $W2 && break; sleep 0.05; done
held $S/w2.out $W2 || bad "fresh-state holder not holding before the probe: $(cat $S/w2.out)"
rk=$(T python3 $S/rng.py ask "$B/lk3.bin" w 5 15 2>&1 | tail -1)
[ "$rk" = granted ] && bad "a fresh client did not see the subspace's lock row" \
    || ok "a fresh peer process sees the held range (state is in the cluster, not the client)"
kill $W2 2>/dev/null; wait $W2 2>/dev/null

echo "===== 10) derived lens + alias: the shape a mount shows is backend-independent ====="
# P5's criterion read literally: the SAME rules over the SAME tree have to render
# the SAME bytes whoever holds the metadata. The proof is a TWIN, not a
# transcription — a second deployment of this very shape (one meta-server + one
# data-server + one mount per lens) running on the SQLite default, seeded by the
# same function and published from the same rule files, whose listings are then
# compared byte-for-byte against the FDB ones. Literal expectations are asserted
# alongside on the FDB side, because two backends could otherwise agree on the
# same wrong shape and the diff would still read green.
V2="${VIEW}s"
SQL_META=127.0.0.1:15189
SQL_DATA=127.0.0.1:15190
mkdir -p $S/sqlhome $S/sqldata $S/h3 $S/mnt_fb $S/mnt_fb2 $S/mnt_sb $S/mnt_fo $S/mnt_so \
         $S/mnt_fm $S/mnt_sm $S/mnt_fm2 $S/mnt_fa $S/mnt_sa
printf 'only /docs\n'                          > $S/only.rules
printf 'hide /photos/private\n'                > $S/hide.rules
printf 'map /data/raw /team/raw\nmap /docs/2026/a.md /inbox/report.md\n' > $S/map.rules
printf 'link /shared/lib /proj/mylib\n'        > $S/link.rules
ns(){ date +%s%N; }
# poll-until with a deadline, echoing the elapsed ms: the display-target checks
# below need a bound, and 5s is the one test_govtime.sh uses for the same
# property on the other two backends (the lens's own listing cache is 2s).
poll(){ local dl=$1 cmd=$2 t0 t el; t0=$(ns)
  while :; do eval "$cmd" && { el=$(( ($(ns)-t0)/1000000 )); echo "$el"; return 0; }
            el=$(( ($(ns)-t0)/1000000 )); [ "$el" -ge "$dl" ] && { echo "$el"; return 1; }; sleep 0.02; done; }
# One seeding path for both backends, so a divergence can come from the backend
# and from nothing else.
seed_shape(){ local m="$1"
    T mkdir -p "$m/docs/2026" "$m/photos/private" "$m/photos/public" "$m/data/raw" "$m/shared/lib" \
      && T sh -c "printf 'report-body' > '$m/docs/2026/a.md'" \
      && T sh -c "printf 'x' > '$m/photos/private/x.jpg'" \
      && T sh -c "printf 'y' > '$m/photos/public/y.jpg'" \
      && T sh -c "printf 'rawdata' > '$m/data/raw/big.bin'" \
      && T sh -c "printf 'ar' > '$m/shared/lib/lib.a'"
}
# type + path + link count, relative to the mount root — no view name in it, so
# the two sides are comparable as bytes. nlink is in there on purpose: it is where
# the directory-nlink rule (§3.4) and a lens's alias entry (§3.7 link) are visible.
shape(){ ( cd "$1" && find . -mindepth 1 -printf '%y %p %n\n' 2>/dev/null | sort -k2 ); }
same_shape(){ # same_shape <label> <fdb mount> <twin mount>
    shape "$2" > $S/sh_fdb.txt; shape "$3" > $S/sh_sql.txt
    if [ ! -s $S/sh_fdb.txt ]; then bad "$1: the FDB mount listed nothing (nothing to compare)"; return; fi
    if [ ! -s $S/sh_sql.txt ]; then bad "$1: the SQLite twin listed nothing (nothing to compare)"; return; fi
    if cmp -s $S/sh_fdb.txt $S/sh_sql.txt; then
        ok "$1: $(wc -l < $S/sh_fdb.txt | tr -d ' ') entries byte-identical to the SQLite twin (type+path+nlink)"
    else
        bad "$1: diverges from the SQLite twin -> $(diff $S/sh_fdb.txt $S/sh_sql.txt | head -8 | tr '\n' '|')"
    fi
}

$Y view --meta-store "$FDBURI" --meta-dir $S/home create --view "$V2" > $S/v2.log 2>&1 \
    && ok "second subspace '$V2' provisioned for the shape work" \
    || bad "view create $V2: $(head -3 $S/v2.log)"
$Y --home $S/sqlhome init --view twin > $S/sql_init.log 2>&1 \
    && ok "SQLite twin: base view 'twin' provisioned (same binary, no --meta-store)" \
    || bad "twin init: $(head -3 $S/sql_init.log)"
bg $Y data-server --listen $SQL_DATA --data-dir $S/sqldata > $S/sql_data.log 2>&1
wait_port $SQL_DATA || bad "twin data-server did not bind"
bg $Y meta-server --listen $SQL_META --home $S/sqlhome > $S/sql_meta.log 2>&1
wait_port $SQL_META || { bad "twin meta-server did not bind"; echo "SUMMARY: pass=$pass fail=$fail SKIP=0"; exit 1; }
grep -q "metadata backend sqlite" $S/sql_meta.log \
    && ok "the twin banners the SQLite backend, so the pair really is two backends" \
    || bad "twin banner unclear: $(head -3 $S/sql_meta.log)"

bg $Y --home $S/h1 mount --view "$V2" --mountpoint $S/mnt_fb --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fb.log 2>&1
FBP=$!
mnt_gate $S/mnt_fb || { bad "the shape base (fdb) never came up"; cat $S/mnt_fb.log; }
bg $Y --home $S/h3 mount --view twin --mountpoint $S/mnt_sb --meta-server $SQL_META \
    --data-servers $SQL_DATA --foreground > $S/mnt_sb.log 2>&1
SBP=$!
mnt_gate $S/mnt_sb || { bad "the shape base (twin) never came up"; cat $S/mnt_sb.log; }
seed_shape $S/mnt_fb && ok "fdb base seeded" || bad "fdb base seeding failed"
seed_shape $S/mnt_sb && ok "twin base seeded" || bad "twin base seeding failed"
same_shape "base tree" $S/mnt_fb $S/mnt_sb
chk "$(T ls $S/mnt_fb | tr '\n' ' ')" "data docs photos shared " \
    "fdb base root shows the four seeded dirs"

for n in only map; do
    $Y view --meta-store "$FDBURI" --meta-dir $S/home define --name "${VIEW}_${n}" \
        --base "$V2" --rules "$S/$n.rules" > "$S/def_f$n.log" 2>&1 \
        && ok "fdb: derived view '$n' published" || bad "fdb define $n: $(head -3 $S/def_f$n.log)"
    $Y view --meta-dir $S/sqlhome define --name "twin_$n" --base twin \
        --rules "$S/$n.rules" > "$S/def_s$n.log" 2>&1 \
        && ok "twin: derived view '$n' published from the same rules file" \
        || bad "twin define $n: $(head -3 $S/def_s$n.log)"
done
bg $Y --home $S/h1 mount --view "${VIEW}_only" --mountpoint $S/mnt_fo --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fo.log 2>&1
FOP=$!
bg $Y --home $S/h3 mount --view twin_only --mountpoint $S/mnt_so --meta-server $SQL_META \
    --data-servers $SQL_DATA --foreground > $S/mnt_so.log 2>&1
SOP=$!
bg $Y --home $S/h1 mount --view "${VIEW}_map" --mountpoint $S/mnt_fm --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fm.log 2>&1
FMP=$!
bg $Y --home $S/h3 mount --view twin_map --mountpoint $S/mnt_sm --meta-server $SQL_META \
    --data-servers $SQL_DATA --foreground > $S/mnt_sm.log 2>&1
SKP=$!
for mp in mnt_fo mnt_so mnt_fm mnt_sm; do
    mnt_gate $S/$mp || { bad "$mp never came up"; cat $S/$mp.log; }
done
same_shape "only /docs lens" $S/mnt_fo $S/mnt_so
same_shape "map lens" $S/mnt_fm $S/mnt_sm
# The literal side of the same claim, so "identical" cannot mean "identically wrong".
chk "$(T ls $S/mnt_fo | tr '\n' ' ')" "docs " "only /docs trims the fdb lens root to /docs"
[ -e "$S/mnt_fo/photos" ] && bad "fdb lens leaks a hidden subtree" || ok "hidden subtree unreachable through the fdb lens"
chk "$(T ls $S/mnt_fm | tr '\n' ' ')" "data docs inbox photos shared team " \
    "map lens carries the synthesized scaffolding beside the sources"
chk "$(T cat $S/mnt_fm/team/raw/big.bin 2>/dev/null)" "rawdata" "relocated dir reads through its display path"
chk "$(T cat $S/mnt_fm/inbox/report.md 2>/dev/null)" "report-body" "relocated file keeps its bytes at the display path"
chk "$(T ls $S/mnt_fm/data | tr '\n' ' ')" "" "the relocated dir vacated its source spot in the lens"
grep -q "read-only" $S/mnt_fm.log \
    && ok "the fdb derived mount came up read-only (handshake flag, as on the others)" \
    || bad "fdb derived mount should be RO: $(head -5 $S/mnt_fm.log)"

# The two rules that need no mount of their own are compared through the
# introspection entrance instead — which is also the proof that `open_ro_view`
# answers for an fdb:// store: an unpublished `base`+`rules` preview has to walk
# a subspace read-only, and a SQLite-only implementation could not render it at
# all. Paths and kinds only, because the two deployments carry different names.
mcp_shape(){ # mcp_shape <tag> <base view> <rules file> [grovefs mcp options...]
    local tag="$1" view="$2" rules="$3"; shift 3
    local q; q=$(jq -Rs '.' < "$rules")
    printf '%s\n' \
      '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fdb-suite","version":"0"}}}' \
      '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
      > "$S/req_$tag.jsonl"
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"base":"%s","rules":%s,"depth":20}}}\n' \
      "$view" "$q" >> "$S/req_$tag.jsonl"
    "$Y" mcp "$@" < "$S/req_$tag.jsonl" > "$S/resp_$tag.json" 2> "$S/req_$tag.err"
    jq -c 'select(.id==2)|.result.structuredContent.entries[]|"\(.kind) \(.path) \(.size)"' \
        "$S/resp_$tag.json" 2>/dev/null | tr -d '"' | sort
}
mcp_shape hide_f "$V2" $S/hide.rules --meta-dir $S/home --meta-store "$FDBURI"  > $S/hide_fdb.txt
mcp_shape hide_s twin    $S/hide.rules --meta-dir $S/sqlhome                     > $S/hide_sql.txt
mcp_shape link_f "$V2" $S/link.rules --meta-dir $S/home --meta-store "$FDBURI"  > $S/link_fdb.txt
mcp_shape link_s twin    $S/link.rules --meta-dir $S/sqlhome                     > $S/link_sql.txt
if [ -s $S/hide_fdb.txt ] && cmp -s $S/hide_fdb.txt $S/hide_sql.txt; then
    ok "hide preview: $(wc -l < $S/hide_fdb.txt | tr -d ' ') entries byte-identical to the twin"
else
    bad "hide preview diverges or is empty -> fdb:$(head -3 $S/hide_fdb.txt|tr '\n' '|') sql:$(head -3 $S/hide_sql.txt|tr '\n' '|') err:$(head -2 $S/req_hide_f.err|tr '\n' '|')"
fi
if [ -s $S/link_fdb.txt ] && cmp -s $S/link_fdb.txt $S/link_sql.txt; then
    ok "link preview: $(wc -l < $S/link_fdb.txt | tr -d ' ') entries byte-identical to the twin"
else
    bad "link preview diverges or is empty -> fdb:$(head -3 $S/link_fdb.txt|tr '\n' '|') sql:$(head -3 $S/link_sql.txt|tr '\n' '|') err:$(head -2 $S/req_link_f.err|tr '\n' '|')"
fi
# Anchored patterns: the preview lines are `"<kind> <path> <size>"`, so a leading
# space here would match nothing and a "pruned it" assertion on an empty answer
# would read green (which is exactly how the first run of this section lied).
grep -q '^dir /photos/private' $S/hide_fdb.txt \
    && bad "hide preview still renders the hidden dir" || ok "hide preview really pruned /photos/private"
grep -q '^file /photos/public/y.jpg' $S/hide_fdb.txt \
    && ok "hide preview keeps the sibling of the pruned dir (a prune, not a blank)" \
    || bad "hide preview pruned too much: $(cat $S/hide_fdb.txt)"
grep -q '^dir /proj/mylib' $S/link_fdb.txt \
    && ok "link preview shows the aliased dir at its display spot" || bad "link preview lost /proj/mylib: $(cat $S/link_fdb.txt)"
grep -q '^dir /shared/lib' $S/link_fdb.txt \
    && ok "link preview keeps the source spot too (an alias, not a move)" || bad "link preview dropped the source"

echo "===== 10b) display_targets: a base edit lands on the DISPLAY coordinate of a second lens client ====="
# The one coordinate a source event cannot reach on its own is a leaf hanging
# under a SYNTHESIZED dir — `/inbox` exists only in this lens, so `report.md`
# under it has no source (parent, name) that the kernel already knows. A second
# client that has already listed `/inbox` (cache warm) can therefore only converge
# if the provider reports the display coordinate with the event. This is the
# LayoutMeta::display_targets path, and the FDB store is what has to prove it.
bg $Y --home $S/h2 mount --view "${VIEW}_map" --mountpoint $S/mnt_fm2 --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fm2.log 2>&1
FMP2=$!
mnt_gate $S/mnt_fm2 || { bad "second map-lens client never came up"; cat $S/mnt_fm2.log; }
chk "$(T ls $S/mnt_fm2/inbox | tr '\n' ' ')" "report.md " "second lens client sees the display entry (cache now warm)"
T rm "$S/mnt_fb/docs/2026/a.md" || bad "base removal of the mapped source failed"
el=$(poll 5000 "[ ! -e '$S/mnt_fm2/inbox/report.md' ]"); rc=$?
[ "$rc" = 0 ] && ok "removal reached the display coordinate (=${el}ms, cache TTL is 2s)" \
              || bad "display entry survived the source removal for ${el}ms (invalidate leaked the synthesized parent)"
T sh -c "printf 'report-body' > '$S/mnt_fb/docs/2026/a.md'" || bad "base rewrite of the mapped source failed"
el=$(poll 5000 "T ls '$S/mnt_fm2/inbox' | grep -q report.md"); rc=$?
[ "$rc" = 0 ] && ok "creation reached the display coordinate (=${el}ms)" \
              || bad "display entry never reappeared after the source create (${el}ms)"
chk "$(T cat $S/mnt_fm2/inbox/report.md 2>/dev/null)" "report-body" "the display entry carries the source's bytes on the second client"
T sh -c "printf 'n' > '$S/mnt_fb/data/raw/new.bin'" || bad "base create through the relocated dir failed"
el=$(poll 5000 "T ls '$S/mnt_fm2/team/raw' | grep -q new.bin"); rc=$?
[ "$rc" = 0 ] && ok "a leaf under the relocated dir shows at its display path (=${el}ms)" \
              || bad "relocated subtree did not converge on the display path (${el}ms)"
# The same property measured on the twin, so "it converged" is not an FDB-only
# story that could hide a second, backend-specific bug.
T sh -c "printf 'n' > '$S/mnt_sb/data/raw/new.bin'" || bad "twin base create through the relocated dir failed"
el=$(poll 5000 "T ls '$S/mnt_sm/team/raw' | grep -q new.bin"); rc=$?
[ "$rc" = 0 ] && ok "the twin's lens client converges the same way (=${el}ms) — one behavior, two backends" \
              || bad "twin lens never converged (${el}ms)"

echo "===== 10c) pure alias on fdb://: name governance, writable, and the real name steps aside ====="
$Y view --meta-store "$FDBURI" --meta-dir $S/home alias --name "${VIEW}_al" --real "$V2" \
    > $S/al_f.log 2>&1 && ok "fdb: alias '${VIEW}_al' -> '$V2'" || bad "fdb alias: $(head -3 $S/al_f.log)"
$Y view --meta-dir $S/sqlhome alias --name twin_al --real twin > $S/al_s.log 2>&1 \
    && ok "twin: the same alias shape published on SQLite" || bad "twin alias: $(head -3 $S/al_s.log)"
bg $Y --home $S/h1 mount --view "${VIEW}_al" --mountpoint $S/mnt_fa --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fa.log 2>&1
FALP=$!
bg $Y --home $S/h3 mount --view twin_al --mountpoint $S/mnt_sa --meta-server $SQL_META \
    --data-servers $SQL_DATA --foreground > $S/mnt_sa.log 2>&1
SALP=$!
mnt_gate $S/mnt_fa || { bad "alias mount (fdb) never came up"; cat $S/mnt_fa.log; }
mnt_gate $S/mnt_sa || bad "alias mount (twin) never came up"
same_shape "alias mount" $S/mnt_fa $S/mnt_sa
# An alias carries NO rules, so it must show exactly what the base shows — that
# is the difference from a lens, stated as an assertion rather than in prose.
same_shape "alias mount vs its own base" $S/mnt_fa $S/mnt_fb
T sh -c "printf 'aw' > $S/mnt_fa/alias_note" && ok "alias mount is WRITABLE (name governance, not shape)" \
    || bad "alias mount refused writes"
el=$(poll 5000 "T cat '$S/mnt_fb/alias_note' | grep -q aw"); rc=$?
[ "$rc" = 0 ] && ok "the alias write landed on the real subspace tree (=${el}ms)" \
              || bad "alias write never reached the base mount (${el}ms)"
$Y --home $S/h1 mount --view "$V2" --mountpoint $S/mnt_fb2 --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_fb2.log 2>&1
ARC=$?
[ "$ARC" != 0 ] && ok "the governed real name is refused while the alias owns the entry point" \
    || bad "real name mounted while an alias governs it — the alias is not enforced"
$Y view --meta-store "$FDBURI" --meta-dir $S/home list > $S/vlist.log 2>&1 \
    && grep -q "alias '${VIEW}_al' -> real view '$V2'" $S/vlist.log \
    && ok "view list answers alias governance for an fdb:// store" \
    || bad "alias missing from the fdb listing: $(head -5 $S/vlist.log)"
grep -q "view '${VIEW}_map' -> base '$V2'" $S/vlist.log \
    && ok "view list answers the derived lens too (one listing, both kinds)" \
    || bad "derived missing from the fdb listing: $(cat $S/vlist.log)"

echo "===== 10d) fork on fdb://: an explicit verdict, never a half-working tree ====="
$Y view --meta-store "$FDBURI" --meta-dir $S/home fork --name "${V2}_fk" --base "$V2" \
    > $S/fork.log 2>&1
FRC=$?
[ "$FRC" != 0 ] && ok "view fork refuses on an fdb:// store (rc=$FRC)" \
    || bad "view fork was ACCEPTED on fdb:// — nothing here copies a subspace"
grep -qi "not implemented" $S/fork.log && grep -qi "FoundationDB" $S/fork.log \
    && ok "the refusal says what is missing and on which backend" \
    || bad "refusal unclear: $(head -3 $S/fork.log)"
grep -qiE "alias|derived view" $S/fork.log \
    && ok "the refusal names the alternative that does work here" \
    || bad "refusal offers no way forward: $(head -3 $S/fork.log)"
grep -qiE "No such file|os error 2" $S/fork.log \
    && bad "the refusal leaked a raw io error instead of a verdict" \
    || ok "no raw io error leaked from the wrong layer"
[ ! -e "$S/home/meta/${V2}_fk.db" ] && ok "the refusal minted no local fork tree" \
    || bad "a LOCAL SQLite fork db appeared for an fdb:// view (the silent half-usable shape)"
$Y view --meta-store "$FDBURI" --meta-dir $S/home list 2>/dev/null | grep -q "${V2}_fk" \
    && bad "a refused fork still left a definition behind" || ok "no definition left behind"
# Grafting needs a destination that OWNS a tree, so this grafts FROM the run's
# first subspace INTO the shape subspace — past every guard, straight into the
# store-level verdict (a refusal that never reached `graft_tree` would be the
# wrong-layer answer this criterion is about).
printf '/docs\tgrafted_docs\n' | $Y view --meta-store "$FDBURI" --meta-dir $S/home graft \
    --name "$V2" --src "$VIEW" --map - > $S/graft.log 2>&1
GRC=$?
[ "$GRC" != 0 ] && grep -qi "not implemented" $S/graft.log && grep -qi "FoundationDB" $S/graft.log \
    && ok "graft answers with the same explicit verdict" \
    || bad "graft on fdb:// did not refuse clearly (rc=$GRC): $(head -3 $S/graft.log)"

echo "===== 11) unmount + remount: content identical (durability without an engine) ====="
SNAP_A=$(cd "$A" 2>/dev/null && find . -type f | sort | while read -r f; do printf '%s %s\n' "$f" "$(cat "$f")"; done)
fusermount -u $S/mnt_a 2>/dev/null; kill $MAP 2>/dev/null; wait $MAP 2>/dev/null
fusermount -u $S/mnt_b 2>/dev/null; kill $MBP 2>/dev/null; wait $MBP 2>/dev/null
$Y --home $S/h1 mount --view "$VIEW" --mountpoint $S/mnt_r --meta-server $META \
    --data-servers $DATA --foreground > $S/mnt_r.log 2>&1 & MRP=$!
mnt_gate $S/mnt_r || { bad "remount never came up"; cat $S/mnt_r.log; }
R=$S/mnt_r
SNAP_R=$(cd "$R" 2>/dev/null && find . -type f | sort | while read -r f; do printf '%s %s\n' "$f" "$(cat "$f")"; done)
chk "$SNAP_R" "$SNAP_A" "remounted tree is byte-identical to the pre-unmount one"
T ls "$R/projects" >/dev/null 2>&1 && ok "remount can traverse the directory the first mount made" \
    || bad "remount cannot traverse projects"
[ ! -e "$R/hardlink" ] && ok "the unlinked name is still gone after a remount" || bad "unlinked name came back"

echo "===== 12) P8 migration: SQLite -> FDB subspace -> back to SQLite ====="
# One pair of existing commands, no new subcommand: `m_team` is a SQLite store
# seeded offline, its dump is imported into a subspace that `view create` never
# provisioned, mounted through the fdb:// meta-server running this suite, and
# dumped again. The re-export being line-for-line the source is the evidence that
# ids and inodes travelled verbatim — a re-issued id would show up here as a
# rewritten row, and a collapsed hard link as two different inodes.
SQLH=$S/mig_sql
BACK=$S/mig_back
mkdir -p $SQLH $BACK $S/mnt_m1 $S/mnt_m2 $S/mnt_m3
cat > $S/m.manifest <<MAN
mkdir docs
touch docs/a.md
touch notes.txt
MAN
GROVEFS_HOME="$PWD/$SQLH" $Y init --view m_team --manifest $S/m.manifest > $S/m_init.log 2>&1 \
    && ok "seeded a SQLite source view offline" || bad "init failed: $(cat $S/m_init.log)"
$Y --home $SQLH mount --view m_team -m $S/mnt_m1 --no-cache --foreground > $S/m_mnt1.log 2>&1 &
MP1=$!; BG="$BG $MP1"
mnt_gate $S/mnt_m1 || { bad "the SQLite source never mounted"; cat $S/m_mnt1.log; }
MS=$S/mnt_m1
T sh -c "printf 'report-body' > $MS/docs/a.md" && ok "source: wrote docs/a.md" || bad "source write failed"
T sh -c "printf 'notes-body' > $MS/notes.txt"  && ok "source: wrote notes.txt"  || bad "source write failed"
T chmod 640 $MS/docs/a.md                      && ok "source: chmod 640"        || bad "source chmod failed"
T ln $MS/docs/a.md $MS/docs/b.md               && ok "source: hard-linked docs/b.md" || bad "source ln failed"
T ln -s docs/a.md $MS/link                     && ok "source: symlink link"     || bad "source symlink failed"
sync "$MS" 2>/dev/null; sleep 1.5
$Y view --meta-dir $SQLH export --view m_team --out $S/m_src.jsonl > $S/m_exp1.log 2>&1 \
    && ok "exported the SQLite source WHILE IT WAS STILL MOUNTED" || bad "source export failed: $(cat $S/m_exp1.log)"
NODES=$(( $(wc -l < $S/m_src.jsonl) - 1 ))
fusermount -u $S/mnt_m1 2>/dev/null; kill $MP1 2>/dev/null; wait $MP1 2>/dev/null

echo "-- 12a) forward: the dump becomes a subspace"
cp -a $SQLH/blocks $S/central/data/ && ok "copied the blobs into the data server's store" || bad "blob copy failed"
V3="${VIEW}_mig"
$Y view --meta-store "$FDBURI" --meta-dir $S/home import --view "$V3" --in $S/m_src.jsonl > $S/m_imp1.log 2>&1 \
    && ok "view import wrote the dump into a subspace nobody created" || bad "FDB import failed: $(head -3 $S/m_imp1.log)"
grep -q "$NODES node(s)" $S/m_imp1.log && ok "import reports the source's node count ($NODES)" \
    || bad "import output unexpected: $(cat $S/m_imp1.log)"
# What `view list --meta-store fdb://…` answers, stated rather than assumed:
# definitions come from the local files (C6), while a BASE tree is a subspace — and
# the two questions are answered by two different commands on purpose. `view list`
# keeps answering the DEFINITIONS (that is what a definition slot is for: the
# derived/alias/fork names, which have no subspace of their own); `view scan`
# (section 14) walks the cluster and answers what TREES exist. Collapsing them
# would make a base view and a definition look alike in one listing, and it would
# hide the case both commands exist for: a subspace nothing here defines (an
# abandoned run, an import nobody mounted).
#
# Existence is still decided by the cluster — by name, through
# `has_physical`/`subspace_exists` — which is what the mount below proves.
$Y view --meta-store "$FDBURI" --meta-dir $S/home list 2>/dev/null | grep -q "^$V3\$" \
    && bad "view list NOW enumerates base subspaces — this assertion, and docs.html's 未提供操作 row, both need updating" \
    || ok "the documented boundary holds: \`view list\` answers the definitions (local files, C6) and \`view scan\` answers the subspaces"
$Y --home $S/h1 mount --view "$V3" --mountpoint $S/mnt_m2 --meta-server $META \
    --data-servers $DATA --foreground > $S/m_mnt2.log 2>&1 &
MP2=$!; BG="$BG $MP2"
mnt_gate $S/mnt_m2 || { bad "the imported subspace never mounted"; cat $S/m_mnt2.log; }
MF=$S/mnt_m2
# The property the round trip used to break: `('i',1)` must point at the ROOT
# KEY `(ROOT_PARENT,"")`, not at the dump's portable name "/". A `'i` pointer
# that names a key which does not exist makes `getattr` of the mount root answer
# ENOENT for the whole tree, so this is asserted before the listing — and with
# the exact number, not "the path is there".
chk "$(T stat -c %i $MF 2>&1)" "1" "the imported subspace answers getattr on its own root inode"
chk "$(ls_wait $MF | tr '\n' ' ')" "docs link notes.txt " "the mounted import shows the source's listing"
chk "$(T cat $MF/docs/a.md)" "report-body" "imported metadata resolves the copied blobs (no host-path binding)"
chk "$(T cat $MF/notes.txt)" "notes-body" "the second blob reads through the subspace"
chk "$(T readlink $MF/link)" "docs/a.md" "symlink target travelled"
chk "$(T stat -c %i $MF/docs/a.md)" "$(T stat -c %i $MF/docs/b.md)" "the hard link is still ONE inode in the subspace"
chk "$(T stat -c %a $MF/docs/a.md)" "640" "mode travelled"

# The inode numbers are pinned to the DUMP's own "ino" values, not merely to each
# other. That is the difference between "the hard link stayed one inode" (which a
# re-issued pair would also satisfy) and "the client keeps the inode it already
# cached handles for" — Spec P8's reason for preserving id/ino at all. The dump is
# written by an offline CLI call, so no mount's attribute cache can influence it:
# it is the authority for what the source numbers were.
AINO=$(grep '"name":"a.md"' $S/m_src.jsonl | grep -o '"ino":[0-9]*' | cut -d: -f2)
NINO=$(grep '"name":"notes.txt"' $S/m_src.jsonl | grep -o '"ino":[0-9]*' | cut -d: -f2)
[ -n "$AINO" ] && [ -n "$NINO" ] && ok "the source dump carries an ino for both names ($AINO/$NINO)" \
    || bad "could not read inos out of the source dump — the assertion below would be vacuous"
chk "$(T stat -c %i $MF/docs/a.md)" "$AINO" "the mount answers the SOURCE's inode for docs/a.md, not a re-issued one"
chk "$(T stat -c %i $MF/notes.txt)" "$NINO" "the mount answers the SOURCE's inode for notes.txt"

echo "-- 12b) fidelity: re-export equals the source row for row"
$Y view --meta-store "$FDBURI" --meta-dir $S/home export --view "$V3" --out $S/m_fd.jsonl > $S/m_exp2.log 2>&1 \
    && ok "dumped the subspace back" || bad "FDB export failed: $(cat $S/m_exp2.log)"
if diff <(tail -n +2 $S/m_src.jsonl) <(tail -n +2 $S/m_fd.jsonl) > $S/m_diff.txt 2>&1; then
    ok "the FDB dump is LINE-FOR-LINE the SQLite source: ids, inodes and every attribute kept their exported values"
else
    bad "the round trip rewrote rows: $(head -4 $S/m_diff.txt | tr '\n' ' ')"
fi

echo "-- 12c) the allocator continues past the imported maximum (setval parity)"
MAXID=$(tail -n +2 $S/m_fd.jsonl | grep -o '"id":[0-9]*' | cut -d: -f2 | sort -n | tail -1)
T sh -c "printf 'fresh' > $MF/after_import.txt" && ok "the imported subspace is writable" || bad "write into the import failed"
sync "$MF" 2>/dev/null; sleep 1.5

# Cache discipline (Spec P8 names the pitfall directly: a cached page of
# `.grove-stats` / a cached attribute can make an A/B measurement read as
# unchanged). This mount answers from the cluster and caches attrs for <=2s, so the
# SAME two numbers are re-read after waiting past that bound. An inode that only
# LOOKED preserved because the client still held the old answer would move here;
# the SQLite side of the round trip is mounted --no-cache for the same reason.
sleep 1.5
chk "$(T stat -c %i $MF/docs/a.md)" "$AINO" "docs/a.md still answers the source inode after the attr TTL expired"
chk "$(T stat -c %i $MF/notes.txt)" "$NINO" "notes.txt likewise — the number is the cluster's, not a cache hit"
$Y view --meta-store "$FDBURI" --meta-dir $S/home export --view "$V3" --out $S/m_fd2.jsonl --force > $S/m_exp3.log 2>&1 \
    && ok "dumped the subspace after a post-import create" || bad "second FDB export failed: $(cat $S/m_exp3.log)"
NEWID=$(grep '"name":"after_import.txt"' $S/m_fd2.jsonl | grep -o '"id":[0-9]*' | cut -d: -f2)
[ -n "$NEWID" ] && [ "$NEWID" -gt "$MAXID" ] 2>/dev/null \
    && ok "the new node took id $NEWID, above every imported id (max $MAXID) — the water mark was raised on import" \
    || bad "the import left the allocator behind (new id '$NEWID', imported max $MAXID)"
DUP=$(tail -n +2 $S/m_fd2.jsonl | grep -o '"id":[0-9]*' | cut -d: -f2 | sort | uniq -d)
[ -z "$DUP" ] && ok "no id in the imported tree is duplicated" || bad "duplicate ids: $DUP"

echo "-- 12d) import guards on this backend"
$Y view --meta-store "$FDBURI" --meta-dir $S/home import --view "$V3" --in $S/m_src.jsonl > $S/m_imp2.log 2>&1
[ $? -ne 0 ] && grep -q "pass --replace" $S/m_imp2.log \
    && ok "a second import onto an occupied subspace refuses without --replace" \
    || bad "double-import guard wrong: $(head -3 $S/m_imp2.log)"
$Y view --meta-store "$FDBURI" --meta-dir $S/home export --view "${VIEW}_ghost" --out $S/m_ghost.jsonl > $S/m_exp5.log 2>&1
[ $? -ne 0 ] && grep -q "not a provisioned subspace" $S/m_exp5.log \
    && ok "exporting a name the cluster does not have is refused" \
    || bad "ghost export guard: $(head -3 $S/m_exp5.log)"
[ ! -e $S/m_ghost.jsonl ] && ok "a refused export leaves no empty dump behind" \
    || bad "an empty dump was written for a view that does not exist"
printf '{"format":"other","version":1}\n' > $S/m_badfmt.jsonl
$Y view --meta-store "$FDBURI" --meta-dir $S/home import --view "${VIEW}_bf" --in $S/m_badfmt.jsonl > $S/m_imp3.log 2>&1
[ $? -ne 0 ] && grep -q "unknown format" $S/m_imp3.log \
    && ok "unknown export format refused" || bad "format guard: $(head -3 $S/m_imp3.log)"
$Y view --meta-store "$FDBURI" --meta-dir $S/home list 2>/dev/null | grep -q "${VIEW}_bf" \
    && bad "a refused import still provisioned the name" \
    || ok "a rejected dump leaves no keys behind (the build-beside-and-rename guarantee)"
printf '%s\n%s\n%s\n' \
  '{"format":"grovefs-view-metadata","version":1,"nodes":2}' \
  '{"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1,"data_ref":null,"link_target":null,"mode":493,"uid":0,"gid":0,"mtime":1,"atime":1,"ctime":1}' \
  '{"id":2,"ino":2,"parent_id":99,"name":"f","is_dir":0,"data_ref":null,"link_target":null,"mode":420,"uid":0,"gid":0,"mtime":1,"atime":1,"ctime":1}' \
  > $S/m_bad.jsonl
$Y view --meta-store "$FDBURI" --meta-dir $S/home import --view "$V3" --in $S/m_bad.jsonl --replace > $S/m_imp4.log 2>&1
[ $? -ne 0 ] && grep -q "missing or non-directory parent" $S/m_imp4.log \
    && ok "dangling parent refused before the target is touched" || bad "dangling guard: $(head -3 $S/m_imp4.log)"
$Y view --meta-store "$FDBURI" --meta-dir $S/home export --view "$V3" --out $S/m_fd3.jsonl --force > $S/m_exp4.log 2>&1
diff <(tail -n +2 $S/m_fd2.jsonl) <(tail -n +2 $S/m_fd3.jsonl) > /dev/null 2>&1 \
    && ok "the rejected --replace left the installed tree exactly as it was (one transaction, never a mixture)" \
    || bad "a rejected import damaged the subspace"
chk "$(T cat $MF/docs/a.md)" "report-body" "the live mount kept serving the old tree through the failed replace"

echo "-- 12e) rollback direction: the same dump back into a SQLite store"
cp -a $SQLH/blocks $BACK/ && ok "copied the blobs into the rollback store" || bad "rollback blob copy failed"
$Y view --meta-dir $BACK import --view m_team --in $S/m_fd.jsonl > $S/m_imp5.log 2>&1 \
    && ok "the FDB dump imports into a fresh SQLite store" || bad "rollback import failed: $(head -3 $S/m_imp5.log)"
$Y --home $BACK mount --view m_team -m $S/mnt_m3 --no-cache --foreground > $S/m_mnt3.log 2>&1 &
MP3=$!; BG="$BG $MP3"
mnt_gate $S/mnt_m3 || { bad "the rolled-back view never mounted"; cat $S/m_mnt3.log; }
MB=$S/mnt_m3
chk "$(ls_wait $MB | tr '\n' ' ')" "docs link notes.txt " "the rolled-back store shows the same listing"
chk "$(T cat $MB/docs/a.md)" "report-body" "the rolled-back store reads the same bytes"
chk "$(T stat -c %i $MB/docs/a.md)" "$(T stat -c %i $MB/docs/b.md)" "the hard link survived the second hop too"
$Y view --meta-dir $BACK export --view m_team --out $S/m_back.jsonl > $S/m_exp5.log 2>&1
diff <(tail -n +2 $S/m_fd.jsonl) <(tail -n +2 $S/m_back.jsonl) > /dev/null 2>&1 \
    && ok "the rolled-back dump equals the FDB dump row for row — three backends, one artifact" \
    || bad "the second hop lost or rewrote rows"

echo "-- 12f) the source of the forward migration is untouched (rollback window)"
$Y view --meta-dir $SQLH export --view m_team --out $S/m_src2.jsonl --force > $S/m_exp6.log 2>&1 \
    && ok "the source store still exports after both hops" || bad "source re-export failed: $(cat $S/m_exp6.log)"
diff <(tail -n +2 $S/m_src.jsonl) <(tail -n +2 $S/m_src2.jsonl) > /dev/null 2>&1 \
    && ok "migration never wrote to the source: its rows are exactly what they were" \
    || bad "the source tree changed under migration"
[ -f $SQLH/meta/m_team.db ] && ok "the source metadata tree is still installed (a rollback can go back)" \
    || bad "the source db disappeared"
fusermount -u $S/mnt_m2 $S/mnt_m3 2>/dev/null
for q in $MP2 $MP3; do kill $q 2>/dev/null; wait $q 2>/dev/null; done

echo "===== 13) --engine-idle-secs here: memory reclaimed, session gate held, view reopens ====="
# Spec P2 re-adjudicated this knob for the third backend: with no per-view
# connection pool there is nothing to check back in, so eviction retires an
# in-process engine (block allocator, readdir checkpoints, last-touch stamp) and
# the next op reopens it. Three assertions, on a dedicated server pair so the
# 3s threshold cannot be confused with the main run's default:
#   a) a MOUNTED view is never evicted (the `subs` gate is not backend-specific);
#   b) after the unmount the idle engine IS retired;
#   c) the same view serves its data again through a fresh mount — the property
#      that makes the knob invisible to clients.
META_EV=127.0.0.1:15196
DATA_EV=127.0.0.1:15197
for a in $META_EV $DATA_EV; do
    (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null \
        && { bad "port $a is already listening; section 13 would talk to a stranger's server"; }
done
V_EV="e$(date +%s)"          # its own subspace: nothing here touches sections 1-12
mkdir -p $S/ev/data $S/ev/home $S/mnt_ev
$Y view --meta-store "$FDBURI" --meta-dir $S/ev/home create --view "$V_EV" > $S/ev/create.log 2>&1 \
    && ok "provisioned a scratch subspace for the eviction check" \
    || { bad "scratch subspace failed"; cat $S/ev/create.log; }
$Y data-server --listen $DATA_EV --data-dir $S/ev/data > $S/ev/data.log 2>&1 & DPE=$!; BG="$BG $DPE"
$Y meta-server --listen $META_EV --home $S/ev/home --meta-store "$FDBURI" \
    --engine-idle-secs 3 > $S/ev/meta.log 2>&1 & MPE=$!; BG="$BG $MPE"
wait_port $META_EV || { bad "section 13's meta-server never bound"; tail -3 $S/ev/meta.log; }
wait_port $DATA_EV || { bad "section 13's data-server never bound"; tail -3 $S/ev/data.log; }
grep -q "metadata backend foundationdb" $S/ev/meta.log \
    && ok "the probe server banners the foundationdb backend" || bad "probe server banner missing: $(head -3 $S/ev/meta.log)"
$Y --home $S/ev/home mount --view "$V_EV" -m $S/mnt_ev --meta-server $META_EV \
    --data-servers $DATA_EV --foreground > $S/ev/mnt.log 2>&1 & MNE=$!; BG="$BG $MNE"
mnt_gate $S/mnt_ev || { bad "the probe view never mounted"; cat $S/ev/mnt.log; }
T sh -c "printf x > $S/mnt_ev/f" && ok "wrote through the probe mount" || bad "probe mount not writable"
sleep 8                      # 3s threshold, comfortably exceeded, WHILE the session is held
chk "$(grep -c 'evicted' $S/ev/meta.log)" "0" "a mounted view is not evicted on this backend either — the session gate is not backend-specific"
fusermount -u $S/mnt_ev 2>/dev/null; kill $MNE 2>/dev/null; wait $MNE 2>/dev/null
for _ in $(seq 1 40); do grep -q 'evicted' $S/ev/meta.log && break; sleep 0.5; done
grep -q 'evicted 1 idle engine' $S/ev/meta.log \
    && ok "once the session is gone the idle engine IS retired (what is reclaimed here is memory, not a connection pool)" \
    || bad "no eviction line after the idle threshold: $(tail -3 $S/ev/meta.log)"
$Y --home $S/ev/home mount --view "$V_EV" -m $S/mnt_ev --meta-server $META_EV \
    --data-servers $DATA_EV --foreground > $S/ev/mnt2.log 2>&1 & MNE2=$!; BG="$BG $MNE2"
mnt_gate $S/mnt_ev || { bad "the evicted view did not reopen on the next mount"; cat $S/ev/mnt2.log; }
chk "$(T cat $S/mnt_ev/f)" "x" "the reopened view serves the same object — eviction is invisible to clients"
fusermount -u $S/mnt_ev 2>/dev/null; kill $MNE2 2>/dev/null; wait $MNE2 2>/dev/null
kill $MPE $DPE 2>/dev/null; wait $MPE 2>/dev/null; wait $DPE 2>/dev/null

echo "===== 14) view scan / view rm --base: the cluster inventory and the real teardown ====="
# C6's split, made operational: definitions are LOCAL files under `--meta-store
# fdb://…`, trees are SUBSPACES in the cluster. `view list` answers the first and
# `view scan` the second; neither can answer the other. This section pins both the
# listing (including that the prefix column addresses the keys it counted) and the
# removal — tree first is WRONG, so what is asserted here is the order: unwind the
# shared ledger, then drop the subspace, and a second run is a no-op.
#
# Everything below runs against the run's own subspaces, so the assertions hold on
# a cluster other suites are also using.
for d in "$S"/mnt_*; do [ -d "$d" ] && fusermount -u "$d" 2>/dev/null; done
VCLI="$Y view --meta-store $FDBURI --meta-dir $S/home"
$VCLI scan > $S/scan.txt 2>&1 && ok "view scan reads the cluster inventory" \
    || bad "view scan failed: $(head -3 $S/scan.txt)"
head -1 $S/scan.txt | grep -q "SUBSPACE" && ok "the inventory names its columns" \
    || bad "no header line: $(head -2 $S/scan.txt)"
grep -qE "^refs +[0-9]+ +0 +ledger" $S/scan.txt \
    && ok "the shared ledger is listed once, as kind=ledger rather than as a tree" \
    || bad "refs is missing or mis-typed in the inventory: $(grep refs $S/scan.txt | head -1)"
for v in "$VIEW" "$V2" "$V3"; do
    grep -qE "^$v +[0-9]+ +[0-9]+ +view" $S/scan.txt \
        && ok "run subspace $v is in the inventory with a key and entry count" \
        || bad "$v missing from the inventory"
done
grep -q "subspace(s) under the 'grovefs' root" $S/scan.txt \
    && ok "the inventory sums trees and ledgers separately" || bad "no summary line"
# The PREFIX column is the reason an operator can act on this listing without
# re-deriving the tuple encoding: it must address exactly the keys the KEYS column
# counted. One `\xNN` per byte, because `fdbcli` reads `\x` as ONE byte.
PX=$(awk -v v="$VIEW" '$1==v {print $5}' $S/scan.txt)
PXKEYS=$(awk -v v="$VIEW" '$1==v {print $2}' $S/scan.txt)
if [ -n "$PX" ]; then
    PFX=$(echo "$PX" | sed 's/../\\x&/g')
    RAW=$("$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" \
        --exec "getrangekeys \"$PFX\" \"${PFX}\\xff\" 100000" 2>/dev/null | grep -c '^`')
    chk "$RAW" "$PXKEYS" "the prefix hex addresses exactly the keys the inventory counted for $VIEW"
else
    bad "no prefix hex printed for $VIEW"
fi
$Y view --meta-dir $S/home scan > $S/scan_nostore.log 2>&1 \
    && bad "view scan without a cluster reported success" \
    || ok "view scan without --meta-store is refused (it reads a cluster, not a directory)"
$Y view --meta-store postgres://nobody@127.0.0.1:1/none --meta-dir $S/home scan > $S/scan_pg.log 2>&1 \
    && bad "view scan accepted a non-fdb store" \
    || ok "view scan refuses a store that is not fdb://"
$VCLI create --view refs > $S/reserved.log 2>&1 \
    && bad "view create accepted the reserved ledger name 'refs'" \
    || ok "'refs' is refused as a view name before any key is written (it is the shared ledger)"
grep -q "reserved" $S/reserved.log && ok "the refusal says why: 'refs' is reserved, not merely taken" \
    || bad "unexpected refusal text: $(head -2 $S/reserved.log)"

echo "-- 14b) the teardown: ledger unwound, then the subspace, and a re-run is a no-op"
RM1=$($VCLI rm --name "$V3" --base --data-dir $S/central/data 2>&1)
echo "$RM1" | grep -q "removed base view '$V3' from $FDBURI" \
    && ok "view rm --base removed the imported subspace $V3" || bad "rm --base failed: $(echo "$RM1" | head -3)"
echo "$RM1" | grep -qE "[0-9]+ key\(s\) gone" && ok "the removal reports the key count it removed" \
    || bad "no key count in the report: $(echo "$RM1" | head -2)"
$VCLI scan > $S/scan2.txt 2>&1
grep -qE "^$V3 " $S/scan2.txt && bad "$V3 is still in the inventory after its removal" \
    || ok "the removed subspace is gone from the inventory"
$VCLI rm --name "$V3" --base > $S/rm_twice.log 2>&1 \
    && bad "a second rm --base on a removed view reported success" \
    || ok "a second rm --base is refused, not a cheerful zero (a typo and a removed view must look different)"
grep -q "no key under a subspace named '$V3'" $S/rm_twice.log \
    && ok "the refusal names the missing subspace and points at view scan" \
    || bad "unexpected re-run text: $(head -2 $S/rm_twice.log)"
# The guard set is backend-independent: $V2 still has derived/alias definitions
# resolving through it, so the removal has to be refused for the same reason it is
# on the SQLite arm. This is the assertion that the fdb branch did not jump the
# guards by being new.
$VCLI rm --name "$V2" --base > $S/rm_governed.log 2>&1 \
    && bad "rm --base removed a base that definitions still resolve through" \
    || ok "rm --base under fdb:// honours the same dependency guard the SQLite arm does"
grep -q "governed by a definition over base '$V2'" $S/rm_governed.log \
    && ok "the guarded refusal names the dependent definition" \
    || bad "unexpected guard text: $(head -2 $S/rm_governed.log)"

# The ledger half, with two views sharing the SAME content keys: remove one and
# nothing may be released, remove the other and it must be. This is the part a
# plain `clear_range` gets wrong (it leaves every blob booked forever, and an FDB
# deployment has no `refs rebuild` to repair it).
mkdir -p $S/zb/data
for v in zb1 zb2; do
    $Y view --meta-store "$FDBURI" --meta-dir $S/zb/home import --view "$v" --in $S/m_src.jsonl \
        > $S/zb/$v.log 2>&1 \
        && ok "imported the same dump into $v (same content keys, so the bookings are shared)" \
        || bad "import into $v failed: $(head -3 $S/zb/$v.log)"
done
ZA=$($Y view --meta-store "$FDBURI" --meta-dir $S/zb/home rm --name zb1 --base --data-dir $S/zb/data 2>&1)
echo "$ZA" | grep -qE "[1-9][0-9]* still held by other views" \
    && ok "removing one of two holders releases nothing — it books down and keeps the bytes" \
    || bad "the shared booking was not preserved: $(echo "$ZA" | head -3)"
# The report always prints both counts, so "no longer held" alone would match the
# zero. Only a NON-ZERO release may be read as "the bytes are free" — that is the
# number which, if it were 1 here, would mean this removal freed a blob the other
# holder still names.
echo "$ZA" | grep -qE "[1-9][0-9]* object\(s\) no longer held by any view" \
    && bad "a view holding a shared content key claimed the bytes were free: $(echo "$ZA" | head -2)" \
    || ok "and it did NOT report the shared bytes as free (the ledger went 2 -> 1)"
ZB=$($Y view --meta-store "$FDBURI" --meta-dir $S/zb/home rm --name zb2 --base --data-dir $S/zb/data 2>&1)
echo "$ZB" | grep -qE "[1-9][0-9]* object\(s\) no longer held by any view" \
    && ok "the last holder's removal DOES release the content keys (the ledger reaches zero and the row goes)" \
    || bad "the last holder released nothing: $(echo "$ZB" | head -3)"
$Y view --meta-store "$FDBURI" --meta-dir $S/zb/home scan > $S/scan3.txt 2>&1
for v in zb1 zb2; do
    grep -qE "^$v " $S/scan3.txt && bad "$v survived its removal" \
        || ok "$v is gone from the cluster inventory"
done


# 14c) The shape that proved the first teardown asked the wrong question.
# `subspace_exists` means "there is a ROOT ROW", i.e. a tree a mount can open. A
# subspace that lost its root row but kept the rest answers NO there while still
# booking every content key it names — the cluster held `spike` in exactly that
# state (7065 keys, 2001 entries, no root row; a partial `clearrange` of the `'n'
# family leaves it), and the first version of the sweep REFUSED to remove it, which
# would have stranded its objects forever with no `refs rebuild` in this backend.
# So: `view scan` must call it a corpse, `view create` must refuse to seed a root
# on top of it, and `rm --base` must both clear it and unwind its bookings.
echo "-- 14c) a corpse (keys, no root row) is removable and its bookings are unwound"
ZC="zc$$"; mkdir -p $S/zc/data
$VCLI import --view "$ZC" --in $S/m_src.jsonl > $S/zc_imp.log 2>&1 \
    && ok "seeded a tree to damage ($ZC)" || bad "import into $ZC failed: $(head -3 $S/zc_imp.log)"
$VCLI scan > $S/scan4.txt 2>&1
ZPX=$(awk -v v="$ZC" '$1==v {print $5}' $S/scan4.txt)
LKB=$(awk '$4=="ledger"{print $2}' $S/scan4.txt)
chk "$(awk -v v="$ZC" '$1==v {print $4}' $S/scan4.txt)" "view" "before damage the subspace is an ordinary tree"
# Clear ONLY the `'n'` family (the packed tag is `\x02n\x00`), which takes the
# root row with it and leaves every other family alone.
if [ -n "$ZPX" ]; then
    ZPFX=$(echo "$ZPX" | sed 's/../\\x&/g')
    # `writemode on;` is required or fdbcli answers "writemode must be enabled"
    # and clears nothing (which would make the next three assertions lie).
    "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" \
        --exec "writemode on; clearrange \"${ZPFX}\\x02\\x6e\\x00\" \"${ZPFX}\\x02\\x6e\\x01\"" > $S/zc_cc.log 2>&1 \
        && ok "cleared the 'n' family out of band (the root row goes with it)" \
        || bad "clearrange failed: $(tail -2 $S/zc_cc.log)"
else
    bad "no prefix hex for $ZC"
fi
$VCLI scan > $S/scan5.txt 2>&1
ZK=$(awk -v v="$ZC" '$1==v {print $2}' $S/scan5.txt)
grep -qE "^$ZC +[1-9][0-9]* +0 +corpse" $S/scan5.txt \
    && ok "the inventory calls it a corpse: keys left, no entries, no root row" \
    || bad "$ZC is not reported as a corpse: $(grep "^$ZC " $S/scan5.txt | head -1)"
$VCLI create --view "$ZC" > $S/zc_create.log 2>&1 \
    && bad "view create seeded a root on top of a corpse's leftovers" \
    || ok "view create refuses to resurrect a corpse instead of writing a root over its keys"
grep -q "no root row" $S/zc_create.log \
    && ok "the refusal says what is wrong and what to run" || bad "unexpected create text: $(head -2 $S/zc_create.log)"
ZR=$($VCLI rm --name "$ZC" --base --data-dir $S/zc/data 2>&1)
echo "$ZR" | grep -q "removed base view '$ZC'" \
    && ok "rm --base removes a corpse (the refusal that started this section is gone)" \
    || bad "rm --base refused a corpse: $(echo "$ZR" | head -3)"
echo "$ZR" | grep -qE "$ZK key\\(s\\) gone" \
    && ok "it reports the $ZK key(s) it cleared, not just the 0 entries the 'n' family still had" \
    || bad "unexpected key count: $(echo "$ZR" | head -2)"
echo "$ZR" | grep -qE "[1-9][0-9]* object\\(s\\) no longer held by any view" \
    && ok "the bookings it held in the shared ledger were unwound by the removal" \
    || bad "the corpse's objects stayed booked: $(echo "$ZR" | head -2)"
$VCLI scan > $S/scan6.txt 2>&1
LKA=$(awk '$4=="ledger"{print $2}' $S/scan6.txt)
[ -n "$LKB" ] && [ -n "$LKA" ] && [ "$LKA" -lt "$LKB" ] \
    && ok "the shared ledger actually shrank ($LKB -> $LKA keys), which is the part clearrange alone gets wrong" \
    || bad "ledger did not shrink (before '$LKB', after '$LKA')"
grep -qE "^$ZC " $S/scan6.txt && bad "$ZC survived its removal" || ok "$ZC is gone from the inventory"

# 14d) `refs rebuild` under fdb://: the repair path this backend did not have.
# The recount reads the TREES and never trusts the ledger it rewrites, which is the
# only way both drift directions can be repaired: a LOST row (under-count — the
# next unlink would free bytes a live name still reads) comes back, and an
# over-count row that no tree names goes away. Neither is reachable with
# `fdbcli clearrange`, and neither has any other CLI path in this backend.
echo "-- 14d) refs rebuild: the recount comes from the trees, so both drift directions repair"
VCLIR="$Y refs --meta-store $FDBURI"
RB0=$($VCLIR rebuild 2>&1)
echo "$RB0" | grep -q "0 row(s) corrected, 0 row(s) deleted" \
    && ok "a rebuild on a healthy cluster corrects nothing (and does not churn the ledger)" \
    || bad "clean rebuild was not a no-op: $(echo "$RB0" | head -2)"
$VCLI scan > $S/scan7.txt 2>&1
LK0=$(awk '$4=="ledger"{print $2}' $S/scan7.txt)
REFSPFX=$(printf '\x01grovefs\x00\x02refs\x00' | od -An -tx1 | tr -d ' \n')
REFHEX=$(echo "$REFSPFX" | sed 's/../\\x&/g')
# fdbcli prints a key with its PRINTABLE bytes as themselves and the rest as
# `\xNN`, so the hex has to be rebuilt by decoding those escapes: a plain
# `sed 's/\\x//g'` turns `\x67` (the letter "g") into `67`, i.e. it fabricates a
# key that does not exist (which is exactly how the first version of this section
# cleared nothing and then reported a healthy ledger). `printf %b` decodes them.
tohex(){ printf '%b' "$1" | od -An -tx1 | tr -d ' \n'; }
# The delimiters are never part of a key, so dropping the characters is enough —
# and it avoids putting a backtick through sed, where `\`` means "start of line".
unquote(){ tr -d "\`'"; }
fdbget(){ "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" --exec "$1" 2>/dev/null; }
# The row is taken from the LEDGER side rather than from one view's 'r' family:
# what this section proves is that a lost row comes back, and the recount finds
# the owning tree by itself — so choosing "a row some tree names" is the rebuild's
# job, not the test's.
LKLINE=$(fdbget "getrangekeys \"$REFHEX\" \"${REFHEX}\\xff\" 1" | grep '^`' | head -1 | unquote)
LKEY=$(tohex "$LKLINE")
if [ -n "$LKEY" ]; then
    LHEX=$(echo "$LKEY" | sed 's/../\\x&/g')
    # The value the trees currently agree on — captured, not assumed: the no-op
    # rebuild above is what makes "before" the truth, so the repair must land on
    # exactly this number rather than on a copied constant.
    VAL0=$(tohex "$(fdbget "get \"$LHEX\"" | sed -n 's/.* is //p' | unquote)")
    fdbget "writemode on; clear \"$LHEX\"" >/dev/null 2>&1 \
        && ok "simulated a LOST booking: one ledger row deleted out of band" \
        || bad "could not clear the ledger row"
    $VCLI scan > $S/scan8.txt 2>&1
    chk "$(awk '$4=="ledger"{print $2}' $S/scan8.txt)" "$((LK0-1))" \
        "the ledger is now short by exactly the row that was removed"
    RB1=$($VCLIR rebuild 2>&1)
    echo "$RB1" | grep -q "1 row(s) corrected, 0 row(s) deleted" \
        && ok "the rebuild RESTORES a booking the ledger had lost (the unsafe direction)" \
        || bad "lost row not restored: $(echo "$RB1" | head -2)"
    $VCLI scan > $S/scan9.txt 2>&1
    chk "$(awk '$4=="ledger"{print $2}' $S/scan9.txt)" "$LK0" \
        "the ledger is back to its true size"
    VAL1=$(tohex "$(fdbget "get \"$LHEX\"" | sed -n 's/.* is //p' | unquote)")
    chk "$VAL1" "$VAL0" "and the restored count is the one the trees agree on, not a guess"
    # Over-count: a row that no tree names at all.
    GHOST="${REFSPFX}0267686f73742d72656600"   # \x02"ghost-ref"\x00
    GHEX=$(echo "$GHOST" | sed 's/../\\x&/g')
    fdbget "writemode on; set \"$GHEX\" \"\\x15\\x09\"" >/dev/null 2>&1 \
        && ok "simulated an OVER-count: a ledger row for a content key no tree names" \
        || bad "could not write the ghost row"
    RB2=$($VCLIR rebuild 2>&1)
    echo "$RB2" | grep -q "[1-9][0-9]* row(s) deleted" \
        && ok "the rebuild DROPS a row that no surviving tree names" \
        || bad "ghost row not dropped: $(echo "$RB2" | head -2)"
    $VCLI scan > $S/scan10.txt 2>&1
    chk "$(awk '$4=="ledger"{print $2}' $S/scan10.txt)" "$LK0" "the ledger is exactly its true size again, with the ghost gone"
else
    bad "could not read a ledger row to damage (refs at '$S/scan7.txt' says '$LK0' keys)"
fi


if [ "$fail" = 0 ]; then echo "ALL FDB-BACKEND CHECKS PASSED"; exit 0; else echo "SOME FDB CHECKS FAILED"; exit 1; fi
