#!/usr/bin/env bash
# What happens to a LIVE MOUNT when the metadata cluster dies underneath it?
#
# Every other failure recipe in this repo kills something the mount can route
# around: `fdb3vm_fault.sh` loses fdbserver PROCESSES on a four-zone cluster
# (the database stays available), `probe_*` scripts restart a data-server (the
# client reconnects), and `fdb_backup_mount.sh` takes the cluster away while
# nothing is mounted. None of them answers the question an operator actually
# asks at 3am: "the FDB cluster just went away and I have a mount in use — what
# does it do, and does it lie to me?"
#
# So this script keeps one mount busy, kills the ONLY fdbserver under it, and
# measures instead of asserting a happy path:
#
#   - does the mountpoint stay mounted, or does the process exit and leave the
#     kernel to answer ENOTCONN?
#   - do reads of already-known files keep working (page cache), fail with an
#     errno, or hang?
#   - does a WRITE that reports success stay written once the cluster is back?
#     A filesystem that returns 0 from write() and then loses the file is worse
#     than one that returns EIO, so this is the one claim the script refuses to
#     soften.
#   - does a `dd` already in flight survive? The data half (blobs) lives on the
#     data-server's disk, so it may well keep writing while the metadata half is
#     gone — measured here rather than assumed.
#   - after the cluster returns, is the SAME mount usable again, with every file
#     that existed before the outage intact?
#
# The cluster is a single-node dev cluster, which is the point: one process
# holds every role, so `kill -9` is a total metadata outage, not a degraded one.
# No availability claim may be drawn from this (see R3 / USAGE §15.9); the
# evidence here is about the CLIENT's contract during the window.
#
# MEASURED on this host (two green runs, dev cluster on 127.0.0.1:4500):
#   - the mountpoint stays in the mount table for the whole 8s window, and the
#     FUSE process stays alive; nothing needs remounting afterwards (read from
#     mountinfo, not `findmnt`: findmnt stats the target and hung the sampler);
#   - calls into the mount BLOCK — they do not return EIO. `cat`, `ls`,
#     `create+write` and a new `dd` were all still parked after 8-10s, and the
#     in-flight writer was still parked after 90s. This is FDB's client waiting
#     for a usable version, under the kernel's FUSE wait;
#   - the data half is independent: files the writer had already been told were
#     written (and their bytes) are all there afterwards, byte for byte;
#   - the cluster came back in ~1.6s and every parked call was answered within
#     ~5-110ms — p_cat replayed with the right bytes. Depending on whether its
#     transaction expired, the first readdir/write after recovery may return an
#     error (measured rc=2 on one run, success on the next); ONE retry on the
#     same mount succeeds. No remount, no restart, no lost file.
#
# What this does NOT establish: any availability guarantee. A real cluster has
# more than one fdbserver (see `.bneck/fdb3vm_fault.sh` for that half), and this
# recipe deliberately kills the only one so the outage is total.
#
# usage: bash .bneck/fdb_fault_mount.sh
set -u
cd "$(dirname "$0")/.."

BIN="${FDB_HOME:-/data/grovefs/.fdbbin}"
CLUSTER="${CLUSTER:-/data/grovefs/.fdbtest/test.cluster}"
URI="fdb://$CLUSTER"
Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
WORK="${WORK:-/data/grovefs/.fdbtest/fltmnt}"
V="flt_mnt$$"
M="$WORK/mnt"
META=127.0.0.1:15296
DATA=127.0.0.1:15297
pass=0; fail=0
# Same single-instance rule as fdb_backup_mount.sh: this script KILLS the shared
# dev cluster, so two copies would each blame the other's outage. Every
# long-lived child closes fd 9 (`9>&-`): bash does not set close-on-exec on an
# `exec`-opened descriptor, and an escaped child holding the lock makes every
# later run refuse to start (measured in the backup recipe).
exec 9>"$WORK.lock"
flock -n 9 || { echo "another run holds $WORK.lock"; exit 1; }
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
say(){ echo "    $*"; }
ms(){ date +%s%3N; }

[ -x "$BIN/fdbserver.x86_64" ] || { echo "  SKIP: no fdbserver in $BIN"; exit 0; }
[ -x "$Y" ] || { echo "FAIL: no feature binary at $Y (bash .build-bin/test_fdb.sh builds it)"; exit 1; }

fcli(){ timeout 8 "$BIN/fdbcli.x86_64" -C "$CLUSTER" --timeout 5 --exec "$1" 2>/dev/null; }
avail(){ fcli "status minimal" | grep -q "The database is available"; }
fpid(){ pgrep -f "fdbserver.x86_64 --cluster-file $CLUSTER" | head -1; }
# Mount-table membership is read from /proc/self/mountinfo, NOT from `findmnt
# --mountpoint`: findmnt stats the target to answer, and a stat of a mountpoint
# whose daemon is parked on the metadata plane blocks for the kernel's request
# timeout — measured, that alone hung the sampler for the whole 420s budget.
# mountinfo is kernel data and answers in microseconds no matter what the
# daemon is doing.
is_mounted_at(){ awk -v mp="$1" '$5==mp {f=1} END{exit(f?0:1)}' /proc/self/mountinfo 2>/dev/null; }
is_mounted(){ is_mounted_at "$M"; }
umount_mnt(){ local mp="$1"
    for _ in $(seq 1 40); do
        is_mounted_at "$mp" || return 0
        fusermount -u "$mp" 2>/dev/null || umount "$mp" 2>/dev/null || true; sleep 0.25
    done
    # Last resort, and the reason it is here: a probe with the file still open
    # keeps the mount busy, and the kernel will not give it back while the
    # daemon that would answer it is gone. `-l` detaches the mount from the
    # namespace, which releases the name and lets the teardown finish (the
    # processes holding it then get ENOTCONN and exit).
    umount -l "$mp" 2>/dev/null || true; sleep 0.5
    is_mounted_at "$mp" && return 1 || return 0
}
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
mnt_gate(){ local mp="$1"; for _ in $(seq 1 80); do is_mounted_at "$mp" && return 0; sleep 0.25; done; return 1; }
# `9>&-` on every fdb_env.sh invocation is NOT cosmetic: that script starts
# fdbserver with setsid+nohup and does not close inherited descriptors, so
# without this the long-lived server inherits THIS script's lock fd and holds it
# after the run ends (measured: the very next run refused to start with
# "another run holds ...lock", and `fuser` named fdbserver as the owner).
start_cluster(){ bash .build-bin/fdb_env.sh up > "$1" 2>&1 9>&- || true; }

inv_names(){ "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan 2>/dev/null \
    | awk 'NR>1 && ($4=="view" || $4=="corpse" || $4=="ledger"){print $1}' | sort; }

MOUNT_PID=""; SERVER_PIDS=""; STREAM_PID=""; PROBE_PIDS=""

# A probe is started in the background and NEVER waited on synchronously: when
# the metadata plane is gone, a filesystem call does not fail fast — it parks in
# the kernel until the daemon answers, and the daemon is waiting on an FDB
# transaction that has its own (60s default) timeout. `timeout N cmd` does not
# bound that: the process sits in the FUSE wait and the SIGTERM does not land
# until the kernel abandons the request (~65s, measured). So the window probes
# are background jobs whose verdict is read from a file, and "has not answered
# in N seconds" is itself a recorded result rather than a hang in this script.
probe_start(){ local label=$1; shift
    ( "$@" > "$WORK/p.$label.out" 2>&1; echo $? > "$WORK/p.$label.rc" ) 9>&- &
    eval "PP_$label=$!"
    PROBE_PIDS="$PROBE_PIDS $!"
}
probe_done(){ [ -f "$WORK/p.$1.rc" ]; }
probe_wait(){ local label=$1 to=$2 s
    s=$(ms)
    while ! probe_done "$label" && [ $(( $(ms) - s )) -lt $((to*1000)) ]; do sleep 0.1; done
    if probe_done "$label"; then
        echo "done rc=$(cat "$WORK/p.$label.rc") after $(( $(ms) - s ))ms"
    else
        echo "pending after ${to}s"
    fi
}
probe_rc(){ cat "$WORK/p.$1.rc" 2>/dev/null; }

SERVER_PIDS=""
cleanup(){
    [ -n "$STREAM_PID" ] && kill -9 "$STREAM_PID" 2>/dev/null || true
    for p in $PROBE_PIDS; do kill -9 "$p" 2>/dev/null || true; done
    # Kill the daemon BEFORE unmounting: a request waiting on the metadata plane
    # keeps the mount busy, and a busy mount would hold this trap open for the
    # kernel's full request timeout (~65s per pending request, measured).
    [ -n "$MOUNT_PID" ] && kill -9 "$MOUNT_PID" 2>/dev/null || true
    sleep 0.5
    umount_mnt "$M" 2>/dev/null || true
    for p in $SERVER_PIDS; do kill "$p" 2>/dev/null || true; done
    sleep 1
    for p in $SERVER_PIDS; do kill -9 "$p" 2>/dev/null || true; done
    # The cluster must be left RUNNING: it is the shared dev cluster, and leaving
    # it dead would make the next recipe's "cluster not available" look like a
    # regression in the code rather than a script that killed it.
    if ! avail; then start_cluster "$WORK/up_cleanup.log"; fi
    # `timeout` around `view rm`: against a still-dead cluster the commit retries
    # for minutes and would hold the trap open (measured in the backup recipe).
    [ -x "$Y" ] && timeout -k 10 40 "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm \
        --name "$V" --base --data-dir "$WORK/data" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "===== 0. preconditions ====="
if ! avail; then
    echo "  starting the dev cluster (it was down)"
    # $WORK does not exist yet on this path, so the log goes nowhere durable.
    start_cluster /dev/null
fi
avail && ok "cluster available at $CLUSTER" || { echo "  SKIP: cluster not available"; exit 0; }
rm -rf "$WORK"; mkdir -p "$WORK"/{home,client,data,mnt}
baseline=$(inv_names | wc -l)
ok "the cluster holds $baseline subspace(s) before this run (the baseline teardown returns to)"
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" create --view "$V" > "$WORK/create.log" 2>&1 \
    && ok "provisioned $V" || { bad "view create $V"; cat "$WORK/create.log"; exit 1; }
"$Y" data-server --listen "$DATA" --data-dir "$WORK/data" > "$WORK/data.log" 2>&1 9>&- & SERVER_PIDS="$!"
"$Y" meta-server --listen "$META" --home "$WORK/home" --meta-store "$URI" > "$WORK/meta.log" 2>&1 9>&- & SERVER_PIDS="$SERVER_PIDS $!"
wait_port "$DATA" && wait_port "$META" || { bad "servers did not bind"; tail -5 "$WORK/meta.log"; exit 1; }
ok "data-server + meta-server up (they stay up for the whole outage: it is the METADATA plane that dies)"

mount_m(){
    is_mounted && return 0
    "$Y" --home "$WORK/client" mount --view "$V" --mountpoint "$M" \
        --meta-server "$META" --data-servers "$DATA" --foreground > "$WORK/mount.log" 2>&1 9>&- &
    MOUNT_PID=$!
    mnt_gate "$M"
}
mount_m || { bad "mount never came up"; tail -8 "$WORK/mount.log"; exit 1; }
ok "mounted $V at $M (pid $MOUNT_PID)"

echo "===== 1. baseline content, then a writer that is still running when the cluster dies ====="
printf 'survives-the-outage\n' > "$M/keep.txt"
head -c 4194304 /dev/urandom > "$M/blob.bin"   # 4 MiB: several blobs through the ledger
sync "$M" 2>/dev/null; sleep 0.3
sha256sum "$M/keep.txt" "$M/blob.bin" 2>/dev/null | awk '{print $1}' > "$WORK/pre.sha"
KEEP_SHA=$(sed -n 1p "$WORK/pre.sha"); BLOB_SHA=$(sed -n 2p "$WORK/pre.sha")
[ -n "${KEEP_SHA:-}" ] && [ -n "${BLOB_SHA:-}" ] && ok "keep.txt + 4 MiB blob.bin written and hashed before the outage" \
    || bad "could not write the baseline files"
# The in-flight writer: unique 64 KiB files, each one a create+write+close, so
# every iteration needs the metadata plane. It stops at the first failure and
# records what it had already been told was durable — the list this script later
# holds the filesystem to.
( i=0; while [ "$i" -lt 120 ]; do
      f="$M/stream.$(printf '%04d' "$i")"
      dd if=/dev/zero of="$f" bs=65536 count=1 status=none 2>/dev/null || break
      echo "$i" >> "$WORK/stream.ok"
      i=$((i+1))
  done; echo "writer stopped after $i file(s)" > "$WORK/stream.end" ) 9>&- &
STREAM_PID=$!
for _ in $(seq 1 100); do [ -s "$WORK/stream.ok" ] && break; sleep 0.05; done
n0=$(wc -l < "$WORK/stream.ok" 2>/dev/null || echo 0)
[ "$n0" -ge 1 ] && ok "the writer is live (${n0} file(s) accepted so far)" || bad "the writer never got going"

echo "===== 2. kill the only fdbserver: total metadata outage ====="
P=$(fpid)
[ -n "${P:-}" ] && kill -9 "$P" 2>/dev/null && ok "SIGKILLed the only fdbserver (pid $P)" \
    || { bad "no fdbserver to kill"; exit 1; }
T_KILL=$(ms)
sleep 1
avail && bad "the database still answers after killing its only server — the probe is measuring nothing" \
      || ok "the database no longer answers (single-node cluster: no failover to hide behind)"

echo "===== 3. watch the mount table across the outage window (8s) ====="
# Deliberately nothing here touches the INSIDE of the mountpoint: a stat/readdir
# would park for the kernel's whole request timeout and turn the sampler into
# the very hang it is supposed to measure (the first version of this script did
# exactly that and spent 65s per sample).
: > "$WORK/samples"
for _ in $(seq 1 16); do
    mnt=$(is_mounted && echo mounted || echo gone)
    mstate="-"; [ -n "$STREAM_PID" ] && kill -0 "$STREAM_PID" 2>/dev/null && \
        mstate=$(awk '{print $3}' "/proc/$STREAM_PID/stat" 2>/dev/null || echo "?")
    printf '%s mount=%s writer=%s\n' "$(( $(ms) - T_KILL ))" "$mnt" "$mstate" >> "$WORK/samples"
    sleep 0.5
done
say "samples (mount / writer state): $(awk '{print $2, $3}' "$WORK/samples" | sort | uniq -c | tr '\n' ' ')"
grep -q "mount=gone" "$WORK/samples" \
    && say "the mountpoint left the mount table during the outage (the FUSE process exited)" \
    || ok "the mountpoint stayed in the mount table for the whole window"

echo "===== 4. probes INSIDE the outage window (background: 'pending' is a result) ====="
probe_start p_cat cat "$M/keep.txt"
r=$(probe_wait p_cat 8); echo "$r" > "$WORK/window.p_cat"; say "read keep.txt   -> $r"
probe_start p_ls ls -1 "$M"
r=$(probe_wait p_ls 8); echo "$r" > "$WORK/window.p_ls"; say "readdir         -> $r"
probe_start p_write sh -c "printf 'written-during-the-outage\n' > '$M/probe.txt'"
r=$(probe_wait p_write 10); echo "$r" > "$WORK/window.p_write"; say "create+write    -> $r"
probe_start p_dd dd if=/dev/zero of="$M/during.bin" bs=1M count=8 status=none
r=$(probe_wait p_dd 10); echo "$r" > "$WORK/window.p_dd"; say "new 8 MiB file  -> $r"

echo "===== 5. did the in-flight writer stop? ====="
s=$(ms)
while [ ! -f "$WORK/stream.end" ] && [ $(( $(ms) - s )) -lt 90000 ]; do sleep 0.2; done
if [ -f "$WORK/stream.end" ]; then
    nok=$(wc -l < "$WORK/stream.ok" 2>/dev/null || echo 0)
    ok "the writer finished inside the outage: $(cat "$WORK/stream.end"); ${nok} file(s) had been accepted"
else
    kill -9 "$STREAM_PID" 2>/dev/null || true; STREAM_PID=""
    nok=$(wc -l < "$WORK/stream.ok" 2>/dev/null || echo 0)
    say "the writer was still parked after 90s (its create is waiting on the metadata plane); ${nok} file(s) had been accepted before"
    ok "the writer did not report success and exit — it parked, which step 7 re-checks after recovery"
fi

echo "===== 6. bring the cluster back ====="
T_UP=$(ms)
start_cluster "$WORK/up.log"
s=$(ms)
while [ $(( $(ms) - s )) -lt 120000 ]; do avail && break; sleep 1; done
RECOVER=$(( $(ms) - T_UP ))
avail && ok "cluster available again after ${RECOVER}ms (fdbserver pid $(fpid))" \
      || { bad "cluster did not come back within 120s"; tail -5 "$WORK/up.log"; }

# The clock that matters for the pending requests: from the cluster being usable
# to the parked calls being answered. This is the point of the whole recipe.
echo "===== 7. the same mount, after the outage ====="
if is_mounted; then
    ok "the mount is still mounted (the FUSE process survived the outage)"
elif kill -0 "$MOUNT_PID" 2>/dev/null; then
    say "the mount process is alive but the kernel dropped the mount"
else
    say "the mount process exited during the outage; remounting the same view"
fi
if ! is_mounted; then
    mount_m || { bad "the view will not remount: $(tail -6 "$WORK/mount.log" | tr '\n' ' ')"; }
fi
say "the probes that were parked during the window, now that the cluster answers:"
for l in p_cat p_ls p_write p_dd; do say "  $l -> $(probe_wait "$l" 10)"; done
# `timeout` alone does NOT bound a call into the mount: the process parks in the
# FUSE wait and does not act on SIGTERM until the kernel abandons the request.
# Only `-k` (SIGKILL) is reliable there — measured, a plain `timeout 20 cat`
# stayed for minutes while the daemon was still waiting on the cluster.
if is_mounted; then
    chk "$(timeout -k 5 20 cat "$M/keep.txt" 2>/dev/null)" "survives-the-outage" \
        "keep.txt still reads back with its pre-outage bytes"
    now_blob=$(timeout -k 5 30 sha256sum "$M/blob.bin" 2>/dev/null | awk '{print $1}')
    chk "${now_blob:-missing}" "${BLOB_SHA:-expected}" \
        "the 4 MiB blob still hashes to its pre-outage value (content plane untouched)"
    if timeout -k 5 15 bash -c "printf 'written-after-recovery\n' > '$M/after.txt'" 2>/dev/null; then
        chk "$(timeout -k 5 10 cat "$M/after.txt" 2>/dev/null)" "written-after-recovery" \
            "a fresh create+write+read works on the same view after recovery"
    else
        bad "the view is not writable after recovery"
    fi
    # Measured: the first readdir after the cluster returned failed with 2 (the
    # request the kernel/transaction dropped is not retried FOR the caller), and
    # the very next one on the SAME mount succeeds. That is the contract worth
    # writing down: one error, then a retry works, no remount.
    if [ "$(probe_rc p_ls)" != 0 ]; then
        if timeout -k 5 20 ls -1 "$M" > "$WORK/ls_retry.out" 2>&1; then
            grep -qx "keep.txt" "$WORK/ls_retry.out" \
                && ok "a retried readdir on the SAME mount succeeds — one error, then no remount needed" \
                || bad "the retried readdir came back without keep.txt"
        else
            bad "the mount is not usable even for a retried readdir"
        fi
    fi
else
    bad "no usable mount after recovery; nothing further can be claimed"
fi

echo "===== 8. the outage's accounting: no success was invented, no file was lost ====="
# Every index the writer was TOLD was durable must be on disk now. This is the
# assertion the whole recipe exists for.
missing=0; wrong=0
while read -r i; do
    f="$M/stream.$(printf '%04d' "$i")"
    # stat through the mount can block like every other call, so even the
    # accounting is bounded (and a stat that cannot answer counts as missing
    # rather than hanging the receipt).
    szf=$(timeout -k 5 15 stat -c %s "$f" 2>/dev/null)
    if [ -n "$szf" ]; then
        [ "$szf" = "65536" ] || wrong=$((wrong+1))
    else
        missing=$((missing+1))
    fi
done < "$WORK/stream.ok" 2>/dev/null
if [ "${nok:-0}" -eq 0 ]; then
    bad "the writer never recorded a single accepted file"
elif [ "$missing" -eq 0 ] && [ "$wrong" -eq 0 ]; then
    ok "all ${nok} file(s) the writer was told were written are present and 65536 bytes"
else
    bad "${missing} of ${nok} accepted file(s) are MISSING and ${wrong} have the wrong size — a write reported success and was lost"
    say "first missing: $(while read -r i; do [ -n "$(timeout -k 5 10 stat -c %s "$M/stream.$(printf '%04d' "$i")" 2>/dev/null)" ] || { echo "stream.$i"; break; }; done < "$WORK/stream.ok")"
fi
# The cached-read probes: a success with wrong bytes would be the worst outcome.
CRC=$(probe_rc p_cat); CWIN=$(cat "$WORK/window.p_cat" 2>/dev/null)
if [ "${CRC:-pending}" = 0 ]; then
    if [ "$CWIN" = "done rc=0" ]; then
        ok "the keep.txt read that answered during the outage returned the right bytes"
    else
        ok "the parked keep.txt read completed with the right bytes once the cluster answered — cache was not stale"
    fi
    chk "$(cat "$WORK/p.p_cat.out")" "survives-the-outage" "and its content is the pre-outage one"
elif [ "${CRC:-pending}" = pending ]; then
    say "the keep.txt read was still parked at the end of the window (it finished later, see step 7)"
else
    ok "the keep.txt read was refused with rc=$CRC during the outage — no bytes invented"
fi
LRC=$(probe_rc p_ls); LWIN=$(cat "$WORK/window.p_ls" 2>/dev/null)
if [ "${LRC:-pending}" = 0 ]; then
    grep -qx "keep.txt" "$WORK/p.p_ls.out" \
        && ok "readdir answered and still listed keep.txt (not an empty directory)" \
        || bad "readdir answered WITHOUT keep.txt — a partial success"
    [ "$LWIN" = "done rc=0" ] || say "  (that listing was parked during the window and completed after recovery)"
else
    say "readdir was ${LRC:-parked} when the cluster came back — it did not pretend the directory was empty"
fi
# The write probe's verdict, checked against what it reported at the time.
WRC=$(probe_rc p_write)
WIN=$(cat "$WORK/window.p_write" 2>/dev/null)
if [ "${WRC:-pending}" = 0 ]; then
    if [ "$WIN" = "done rc=0" ]; then
        ok "the write that reported success DURING the outage is still there, byte for byte"
    else
        ok "the write was parked during the outage and completed after the cluster returned — retried, not lost"
    fi
    [ "$(timeout -k 5 20 cat "$M/probe.txt" 2>/dev/null)" = "written-during-the-outage" ] \
        || bad "probe.txt reported success but its bytes are wrong — silent corruption"
elif [ "${WRC:-pending}" = pending ]; then
    say "the write was still parked at the end of the window; its final verdict is in step 7"
else
    if [ -e "$M/probe.txt" ]; then
        say "probe.txt exists although the write reported rc=$WRC (partial landing, not a success claim)"
    else
        ok "the refused write left nothing behind — a refusal, not a half-file"
    fi
fi
DRC=$(probe_rc p_dd)
if [ "${DRC:-pending}" = 0 ]; then
    chk "$(timeout -k 5 10 stat -c %s "$M/during.bin" 2>/dev/null)" "8388608" \
        "the 8 MiB file created during the outage is complete"
else
    say "the 8 MiB create did not report success during the window (rc=${DRC:-parked})"
fi

echo "===== 9. no crash, and the evidence is readable ====="
if grep -qiE "panic|RUST_BACKTRACE|Segmentation" "$WORK/mount.log" 2>/dev/null; then
    bad "the mount process logged a panic: $(grep -iE 'panic|Segmentation' "$WORK/mount.log" | head -1)"
else
    ok "no panic in the mount log across the outage"
fi
for p in $SERVER_PIDS; do
    kill -0 "$p" 2>/dev/null || { bad "a server (pid $p) exited during the outage"; continue; }
done
ok "the data-server and meta-server were never touched (the outage was metadata-only)"

echo "===== 10. teardown leaves the cluster as this run found it ====="
# Stop everything holding the mount before unmounting: a dd that is still
# writing keeps the mount busy, and `view rm` refuses to delete a view that is
# mounted here (measured: the first green-logic run ended with 3 FAILs here for
# exactly this reason).
for p in $PROBE_PIDS; do kill -9 "$p" 2>/dev/null || true; done
[ -n "$STREAM_PID" ] && kill -9 "$STREAM_PID" 2>/dev/null || true
# Same order as cleanup: the daemon first, because a request parked on the
# metadata plane would keep the mount busy and make `umount` wait out the
# kernel's request timeout.
[ -n "$MOUNT_PID" ] && kill -9 "$MOUNT_PID" 2>/dev/null || true
sleep 0.5
umount_mnt "$M" && ok "unmounted" || bad "could not unmount"
timeout -k 10 60 "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm \
    --name "$V" --base --data-dir "$WORK/data" > "$WORK/rm.log" 2>&1
grep -q "removed base view" "$WORK/rm.log" && ok "removed $V (and its bookings)" \
    || bad "view rm failed: $(tail -2 "$WORK/rm.log" | tr '\n' ' ')"
after=$(inv_names | wc -l)
chk "$after" "$baseline" "the cluster holds exactly the subspaces it held before this run (no leak, no orphan booking)"

echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "FDB_FAULT_MOUNT_GREEN"; else echo "FDB_FAULT_MOUNT_RED"; exit 1; fi
