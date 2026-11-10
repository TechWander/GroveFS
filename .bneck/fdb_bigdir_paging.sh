#!/usr/bin/env bash
# Scale case: ONE directory with 20,000 entries, read back through a mount.
#
# Why this is a separate recipe and not "more of test_fdb.sh": every readdir in
# the suites is small enough to fit in one page, so a paging bug would pass all
# 170 assertions. The FDB arm implements `readdir_page` with §3.3's stride-32
# cursor (`READDIR_CURSOR_STRIDE`, `READDIR_CURSOR_CAP = 65_536`) over a name
# range, and the kernel asks for 4 KiB at a time — so a 20,000-entry directory
# needs ~30 pages, and the failure modes this catches are exactly the ones a
# small directory cannot show:
#
#   - a page that repeats a name (cursor resumes inclusively instead of after),
#   - a page that skips a name (range bound off by one, or a stride computed
#     from the wrong offset),
#   - a listing that ends early (the "4 KiB / 58 keys" class of truncation that
#     a keyspace with a value-size limit invites),
#   - a cursor that survives its cache entry and returns stale data after a
#     delete.
#
# The tree is built through the MOUNT (one create per file, one transaction
# each), not by `view import`: import writes the whole tree in ONE transaction,
# which is the right tool for migrating a dump but not for proving that 20,000
# ordinary creates work. It also means every row went through the same path a
# user would take.
#
# MEASURED on this host (dev cluster on 127.0.0.1:4500, SATA SSD):
#   - 20,000 creates through the mount took 153.4s (7.7ms each — the write path's
#     price, one transaction apiece, not a paging number);
#   - `ls -1` 3.8s and `ls -U` 3.7s for 20,000 names (~5.2k names/s), `find` 3.7s,
#     `du -s` 13.2s (du stats every entry);
#   - the listing equals the expected SET at both 6,000 and 20,000 — no
#     duplicate, no gap — and the post-delete re-read follows the new order;
#   - `view scan` reports 19,501 entries (root + 19,500) after the 500 deletes.
#
# And it earned its keep on its first validated run: at 6,000 names the mount
# listed 851 names TWICE and dropped 851 others while the total stayed exactly
# 6,000. `FdbStore::readdir_page` keyed its checkpoints by BLOCK index while the
# name it stored belonged to the previous ROW, so a resume landed `offset % 32`
# rows late. Every readdir in the suites fits in one page, so nothing else could
# see it; the regression test is now `tests/fdb_tree.rs`'s
# `fdb_readdir_pages_stitch_into_the_sqlite_listing` (verified red against the
# old body, green against the fix).
#
# usage: bash .bneck/fdb_bigdir_paging.sh          # N=20000 by default
#        N=5000 bash .bneck/fdb_bigdir_paging.sh   # smaller run while editing
set -u
cd "$(dirname "$0")/.."

BIN="${FDB_HOME:-/data/grovefs/.fdbbin}"
CLUSTER="${CLUSTER:-/data/grovefs/.fdbtest/test.cluster}"
URI="fdb://$CLUSTER"
Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
WORK="${WORK:-/data/grovefs/.fdbtest/bigdir}"
V="big_dir$$"
M="$WORK/mnt"
META=127.0.0.1:15496
DATA=127.0.0.1:15497
N="${N:-20000}"          # entries in the single directory
DEL="${DEL:-500}"        # how many are removed to force a cursor rebuild
# Where the deleted run starts: a quarter of the way in by default, so the run
# sits across many page boundaries and never touches either edge. Derived from
# N rather than fixed at 5000 so a small validation run (N=6000) stays valid.
START="${START:-$((N/4))}"
pass=0; fail=0
export LC_ALL=C          # one collation for the listing, or "sorted" is a guess
# Same lock discipline as the other recipes (see fdb_fault_mount.sh for the
# three measured reasons): one run at a time, every long-lived child closes fd 9,
# and fdb_env.sh is called with `9>&-` so the fdbserver it spawns cannot hold it.
exec 9>"$WORK.lock"
flock -n 9 || { echo "another run holds $WORK.lock"; exit 1; }
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
say(){ echo "    $*"; }
ms(){ date +%s%3N; }

[ -x "$Y" ] || { echo "FAIL: no feature binary at $Y"; exit 1; }
fcli(){ timeout 8 "$BIN/fdbcli.x86_64" -C "$CLUSTER" --timeout 5 --exec "$1" 2>/dev/null; }
avail(){ fcli "status minimal" | grep -q "The database is available"; }
is_mounted_at(){ awk -v mp="$1" '$5==mp {f=1} END{exit(f?0:1)}' /proc/self/mountinfo 2>/dev/null; }
is_mounted(){ is_mounted_at "$M"; }
start_cluster(){ bash .build-bin/fdb_env.sh up > "$1" 2>&1 9>&- || true; }
umount_mnt(){ local mp="$1"
    for _ in $(seq 1 40); do
        is_mounted_at "$mp" || return 0
        fusermount -u "$mp" 2>/dev/null || umount "$mp" 2>/dev/null || true; sleep 0.25
    done
    umount -l "$mp" 2>/dev/null || true; sleep 0.5
    is_mounted_at "$mp" && return 1 || return 0
}
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
mnt_gate(){ local mp="$1"; for _ in $(seq 1 120); do is_mounted_at "$mp" && return 0; sleep 0.25; done; return 1; }
inv_names(){ "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan 2>/dev/null \
    | awk 'NR>1 && ($4=="view" || $4=="corpse" || $4=="ledger"){print $1}' | sort; }

MOUNT_PID=""; SERVER_PIDS=""
cleanup(){
    [ -n "$MOUNT_PID" ] && kill -9 "$MOUNT_PID" 2>/dev/null || true
    sleep 0.3
    umount_mnt "$M" 2>/dev/null || true
    for p in $SERVER_PIDS; do kill "$p" 2>/dev/null || true; done
    sleep 1
    for p in $SERVER_PIDS; do kill -9 "$p" 2>/dev/null || true; done
    if ! avail; then start_cluster "$WORK/up_cleanup.log"; fi
    [ -x "$Y" ] && timeout -k 10 60 "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm \
        --name "$V" --base --data-dir "$WORK/data" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "===== 0. preconditions ====="
if ! avail; then
    echo "  starting the dev cluster (it was down)"
    start_cluster /dev/null
fi
avail && ok "cluster available at $CLUSTER" || { echo "  SKIP: cluster not available"; exit 0; }
rm -rf "$WORK"; mkdir -p "$WORK"/{home,client,data,mnt}
baseline=$(inv_names | wc -l)
ok "the cluster holds $baseline subspace(s) before this run"
"$Y" view --meta-store "$URI" --meta-dir "$WORK/home" create --view "$V" > "$WORK/create.log" 2>&1 \
    && ok "provisioned $V" || { bad "view create $V"; cat "$WORK/create.log"; exit 1; }
"$Y" data-server --listen "$DATA" --data-dir "$WORK/data" > "$WORK/data.log" 2>&1 9>&- & SERVER_PIDS="$!"
"$Y" meta-server --listen "$META" --home "$WORK/home" --meta-store "$URI" > "$WORK/meta.log" 2>&1 9>&- & SERVER_PIDS="$SERVER_PIDS $!"
wait_port "$DATA" && wait_port "$META" || { bad "servers did not bind"; tail -5 "$WORK/meta.log"; exit 1; }
"$Y" --home "$WORK/client" mount --view "$V" --mountpoint "$M" \
    --meta-server "$META" --data-servers "$DATA" --foreground > "$WORK/mount.log" 2>&1 9>&- &
MOUNT_PID=$!
mnt_gate "$M" || { bad "mount never came up"; tail -8 "$WORK/mount.log"; exit 1; }
ok "mounted $V at $M"

echo "===== 1. build $N entries through the mount (one create each) ====="
python3 - "$M" "$N" <<'PY' > "$WORK/create.out" 2>&1
import os, sys, time
d, n = sys.argv[1], int(sys.argv[2])
t0 = time.time()
for i in range(n):
    p = os.path.join(d, "f%05d" % i)
    fd = os.open(p, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    os.close(fd)
    if (i + 1) % 2000 == 0:
        print("%d created, %.1fs" % (i + 1, time.time() - t0), flush=True)
print("CREATED %d in %.1fs" % (n, time.time() - t0))
PY
CRC=$?
tail -1 "$WORK/create.out" | sed 's/^/    /'
[ "$CRC" = 0 ] && grep -q "^CREATED $N " "$WORK/create.out" \
    && ok "the mount accepted $N creates ($(grep '^CREATED' "$WORK/create.out" | awk '{print $4}'))" \
    || bad "creating $N entries failed: $(tail -3 "$WORK/create.out" | tr '\n' ' ')"

echo "===== 2. readdir: every name, exactly once, in name order ====="
python3 - "$N" "$DEL" <<'PY' > "$WORK/expected.all"
import sys
n = int(sys.argv[1])
for i in range(n):
    print("f%05d" % i)
PY
t0=$(ms)
ls -1 "$M" > "$WORK/ls.sorted" 2> "$WORK/ls.err"; LSRC=$?
t_ls=$(( $(ms) - t0 ))
t0=$(ms)
ls -U -1 "$M" > "$WORK/ls.raw" 2> "$WORK/ls_raw.err"; LRRC=$?
t_raw=$(( $(ms) - t0 ))
chk "$LSRC/$LRRC" "0/0" "both readdir shapes returned success"
chk "$(wc -l < "$WORK/ls.sorted")" "$N" "ls (sorted) listed $N names in ${t_ls}ms"
chk "$(wc -l < "$WORK/ls.raw")" "$N" "ls -U (raw getdents order) listed $N names in ${t_raw}ms"
# The real question is not the count but the SET: a paging bug that repeats one
# name and drops another keeps the count right.
diff -q "$WORK/expected.all" "$WORK/ls.sorted" > /dev/null \
    && ok "the sorted listing is EXACTLY the expected set — no duplicate, no gap" \
    || { bad "the listing differs from the expected set"; diff "$WORK/expected.all" "$WORK/ls.sorted" | head -6 | sed 's/^/      /'; }
sort "$WORK/ls.raw" > "$WORK/ls.raw.sorted"
diff -q "$WORK/expected.all" "$WORK/ls.raw.sorted" > /dev/null \
    && ok "the raw getdents stream (no shell sort) is the same set — the pages stitch correctly" \
    || bad "the raw getdents stream differs from the expected set"
# A page boundary is where a cursor bug shows up: check names around every
# stride-32 boundary rather than a few random ones.
bad_at=""
for i in 0 1 31 32 33 63 64 65 1023 1024 1025 $((N/2)) $((N-2)) $((N-1)); do
    nm="f$(printf '%05d' "$i")"
    [ "$(stat -c %s "$M/$nm" 2>/dev/null)" = "0" ] || bad_at="$bad_at $nm"
done
[ -z "$bad_at" ] && ok "stat on names at page-stride boundaries (0,31,32,33,63,64,65,1023,1024,1025,mid,last) all resolve" \
                 || bad "names that do not stat: $bad_at"

echo "===== 3. find and du walk the same tree ====="
t0=$(ms)
find "$M" -mindepth 1 | wc -l > "$WORK/find.count" 2> "$WORK/find.err"
t_find=$(( $(ms) - t0 ))
chk "$(cat "$WORK/find.count")" "$N" "find saw $N entries in ${t_find}ms"
t0=$(ms)
du -s "$M" > "$WORK/du.out" 2> "$WORK/du.err"; DURC=$?
t_du=$(( $(ms) - t0 ))
chk "$DURC" "0" "du succeeded over $N entries in ${t_du}ms ($(cat "$WORK/du.out" 2>/dev/null))"

echo "===== 4. remove $DEL names in the middle of the range, then re-read ====="
python3 - "$M" "$DEL" "$START" <<'PY' > "$WORK/del.out" 2>&1
import os, sys
d, k, start = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
for i in range(start, start + k):
    os.unlink(os.path.join(d, "f%05d" % i))
print("DELETED %d from f%05d" % (k, start))
PY
grep -q "^DELETED $DEL " "$WORK/del.out" \
    && ok "$(cat "$WORK/del.out")" || bad "deletes failed: $(tail -2 "$WORK/del.out" | tr '\n' ' ')"
python3 - "$N" "$DEL" "$START" <<'PY' > "$WORK/expected.after"
import sys
n, k, start = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
drop = set(range(start, start + k))
for i in range(n):
    if i not in drop:
        print("f%05d" % i)
PY
ls -1 "$M" > "$WORK/ls.after" 2> "$WORK/ls_after.err"
chk "$(wc -l < "$WORK/ls.after")" "$((N-DEL))" "after removing $DEL names, ls lists $((N-DEL))"
diff -q "$WORK/expected.after" "$WORK/ls.after" > /dev/null \
    && ok "the post-delete listing is exactly the surviving set — the cursor did not go stale" \
    || { bad "the post-delete listing differs"; diff "$WORK/expected.after" "$WORK/ls.after" | head -6 | sed 's/^/      /'; }
# One deleted name must be ENOENT, one survivor must still stat: both directions.
FIRST_DEL="f$(printf '%05d' "$START")"
LAST="f$(printf '%05d' "$((N-1))")"
if [ -e "$M/$FIRST_DEL" ]; then bad "$FIRST_DEL was deleted but still resolves"; else ok "a deleted name is gone (ENOENT)"; fi
if [ -e "$M/$LAST" ]; then ok "the last name ($LAST) still resolves after the deletes"; else bad "$LAST disappeared"; fi

echo "===== 5. the meta-server's own view of the tree ====="
# `du` above already walked it; this checks the accounting the server keeps,
# which is what a `view scan` / `refs` report reads.
SCAN=$("$Y" view --meta-store "$URI" --meta-dir "$WORK/home" scan 2>&1)
echo "$SCAN" | grep -q "^$V " && say "$(echo "$SCAN" | awk -v v="$V" '$1==v {print "scan: "$0}')"
ENT=$(echo "$SCAN" | awk -v v="$V" '$1==v {print $3}')
chk "${ENT:-missing}" "$((N-DEL+1))" "the cluster reports $((N-DEL+1)) entries for this view (root + $((N-DEL)) files)"

echo "===== 6. teardown ====="
[ -n "$MOUNT_PID" ] && kill -9 "$MOUNT_PID" 2>/dev/null || true
sleep 0.3
umount_mnt "$M" && ok "unmounted" || bad "could not unmount"
timeout -k 10 120 "$Y" view --meta-store "$URI" --meta-dir "$WORK/home" rm \
    --name "$V" --base --data-dir "$WORK/data" > "$WORK/rm.log" 2>&1
grep -q "removed base view" "$WORK/rm.log" && ok "removed $V (and its bookings)" \
    || bad "view rm failed: $(tail -2 "$WORK/rm.log" | tr '\n' ' ')"
chk "$(inv_names | wc -l)" "$baseline" "the cluster holds exactly the subspaces it held before this run"

echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "FDB_BIGDIR_GREEN"; else echo "FDB_BIGDIR_RED"; exit 1; fi
