#!/usr/bin/env bash
# Cross-client SHARED-METADATA e2e over real FUSE mounts. This is the headline
# capability of the meta-service: two clients, each with its OWN --home (i.e.
# its own local disk, standing in for two different machines), mounting the SAME
# user through one central `meta-server` + `data-server`. With the old local-view
# model each machine had its own copy of the view and they diverged; here a file
# one client writes is instantly visible to the other, while per-user isolation
# still holds and NOTHING authoritative lives on the clients' local disks.
#
# Assertion-based (pass/fail counters + a real exit code) like the other suites.
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove

META=127.0.0.1:50094
DATA=127.0.0.1:50095
rm -rf .msrv; mkdir -p .msrv/central .msrv/m1 .msrv/m2 .msrv/m3 .msrv/m4 .msrv/m5 \
    .msrv/mnt_a .msrv/mnt_b .msrv/mnt_bob .msrv/mnt_s1 .msrv/mnt_s2

# wait until a TCP port accepts connections
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- central services ----
$Y data-server --listen $DATA --root .msrv/central/data > .msrv/data.log 2>&1 &
DP=$!
$Y --home .msrv/central meta-server --listen $META > .msrv/meta.log 2>&1 &
MP=$!
wait_port $DATA || true
if wait_port $META; then MP_OK=1; else MP_OK=0; fi
[ "$MP_OK" = 1 ] || { echo "servers failed to start"; cat .msrv/data.log .msrv/meta.log; }

# ---- three client mounts, each with its own --home (no local authority) ----
$Y --home .msrv/m1 mount --view alice --mountpoint .msrv/mnt_a --meta $META --data $DATA > .msrv/a.log 2>&1 &
AP=$!
$Y --home .msrv/m2 mount --view alice --mountpoint .msrv/mnt_b --meta $META --data $DATA > .msrv/b.log 2>&1 &
BP=$!
$Y --home .msrv/m3 mount --view bob   --mountpoint .msrv/mnt_bob --meta $META --data $DATA > .msrv/bob.log 2>&1 &
OP=$!
sleep 1.5

cleanup(){ for d in .msrv/mnt_a .msrv/mnt_b .msrv/mnt_bob .msrv/mnt_s1 .msrv/mnt_s2; do fusermount -u "$d" 2>/dev/null; done; kill $AP $BP $OP $SP1 $SP2 $DP $MP 2>/dev/null; }
SP1=0; SP2=0
trap cleanup EXIT

A=.msrv/mnt_a
B=.msrv/mnt_b
BOB=.msrv/mnt_bob
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }

echo "===== 1) server started cleanly ====="
[ -s .msrv/meta.log ] && grep -q 'meta-server listening' .msrv/meta.log && ok "meta-server up" || bad "meta-server not logging"
# a brand-new user gets an authoritative DB created server-side, on demand
[ -f .msrv/central/meta/alice.db ] && ok "central holds alice's authoritative view db" || bad "central alice.db missing"

echo "===== 2) clients keep NO local copy of the view (proves --meta is central) ====="
[ ! -f .msrv/m1/meta/alice.db ] && ok "client m1 has no local view db" || bad "client m1 wrongly created a local copy"
[ ! -f .msrv/m2/meta/alice.db ] && ok "client m2 has no local view db" || bad "client m2 wrongly created a local copy"

echo "===== 3) create on client A, see it on client B (shared tree) ====="
T mkdir "$A/projects"
T sh -c "printf 'hello from A' > $A/notes.txt"
sync "$A" 2>/dev/null; sleep 2.3
if T ls "$B" | grep -qx projects; then ok "client B sees dir created by client A"; else bad "client B missing A's dir ($(T ls $B | tr '\n' ' '))"; fi
chk "$(T cat $B/notes.txt)" "hello from A" "client B reads the bytes client A wrote (via central data)"

echo "===== 4) create on B, read on A (bidirectional, fresh lookup) ====="
# NOTE (honest limitation): a file the peer mount has *already cached* (like
# notes.txt above) is not re-read instantly on append — there is no server-push
# FUSE invalidation yet, so the peer's attr/page cache wins until it expires.
# Structural metadata and *new* objects, however, are shared authoritatively, so
# we prove the B->A direction with a path A has never looked up.
T sh -c "printf 'made on B' > $B/from_b.txt"
sleep 2.3
chk "$(T cat $A/from_b.txt)" "made on B" "client A reads a file client B created"
if T ls "$A" | grep -qx from_b.txt; then ok "B-created file appears in A's listing"; else bad "B-created file missing from A"; fi

echo "===== 5) per-user isolation still holds over the shared server ====="
if T ls "$BOB" | grep -qx projects; then bad "leak: bob sees alice's projects"; else ok "bob's view has no 'projects'"; fi
[ -e "$BOB/notes.txt" ] && bad "leak: bob sees alice's notes.txt" || ok "bob has no notes.txt"

echo "===== 6) delete on B removes it from A ====="
T rm "$B/notes.txt"
sleep 2.3
[ -e "$A/notes.txt" ] && bad "alice's file still visible after B deleted it" || ok "removal propagates A<->B"

echo "===== 7) data lives centrally, not on the clients ====="
# leave a fresh object so the central store is guaranteed non-empty after the
# step-6 deletion GC'd the earlier one
T sh -c "printf 'central-bytes' > $A/keep.bin"; sync "$A" 2>/dev/null; sleep 0.3
blk=$(ls .msrv/central/data/blocks 2>/dev/null | wc -l)
[ "$blk" -ge 1 ] && ok "objects stored on central data-server ($blk)" || bad "no objects in central blocks"
[ ! -d .msrv/m1/blocks ] && ok "client m1 has no local blocks dir" || bad "client m1 unexpectedly holds blocks"

echo "===== 8) --strict coherence: peer overwrite visible on an already-read file ====="
# This is exactly the case the default (cached) mounts get wrong (see step 4
# note): a file the peer already looked up once is not re-read until the TTL.
# Two MORE clients of the same user, mounted --strict, bypass that cache.
$Y --home .msrv/m4 mount --view alice --mountpoint .msrv/mnt_s1 --meta $META --data $DATA --strict > .msrv/s1.log 2>&1 &
SP1=$!
$Y --home .msrv/m5 mount --view alice --mountpoint .msrv/mnt_s2 --meta $META --data $DATA --strict > .msrv/s2.log 2>&1 &
SP2=$!
sleep 1.5
S1=.msrv/mnt_s1; S2=.msrv/mnt_s2
T sh -c "printf 'v1' > $S1/doc.txt"; sleep 0.2
chk "$(T cat $S2/doc.txt)" "v1" "strict S2 reads S1's new file"
T cat $S2/doc.txt >/dev/null          # S2 now holds the inode
T sh -c "printf 'v2-overwritten' > $S1/doc.txt"; sleep 0.2
chk "$(T cat $S2/doc.txt)" "v2-overwritten" "strict: S1 overwrite immediately visible on S2's cached path"

echo "===== 9) real-time invalidation push: NON-strict cached mounts converge ====="
# The default cached mounts A,B used to serve a stale read of an already-opened
# file after a peer overwrote it (the very reason --strict existed). The
# meta-server now pushes inval_inode/inval_entry the instant a peer mutates, so
# the CACHED mounts converge on their own -- no --strict, no waiting a full TTL.
T sh -c "printf 'one' > $A/live.txt"; sleep 0.3
chk "$(T cat $B/live.txt)" "one" "B reads A's new file (cached mount)"
T cat $B/live.txt >/dev/null                 # B now caches inode live.txt
T sh -c "printf 'two-overwritten' > $A/live.txt"; sleep 0.5   # A overwrite -> push
chk "$(T cat $B/live.txt)" "two-overwritten" "B sees A's overwrite on a cached file WITHOUT --strict (inval_inode push)"
# structural push: a new file A creates appears in B's listing with no TTL wait
T sh -c "printf x > $A/pushed.txt"; sleep 0.5
if T ls "$B" | grep -qx pushed.txt; then ok "B lists A's newly created file immediately (inval_entry push)"; else bad "pushed.txt missing from B"; fi
# delete push
T rm "$A/pushed.txt"; sleep 0.5
[ -e "$B/pushed.txt" ] && bad "B still shows a file A deleted" || ok "B's view drops A's deletion immediately"


echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL META-SERVICE CHECKS PASSED" || echo "SOME META-SERVICE CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
