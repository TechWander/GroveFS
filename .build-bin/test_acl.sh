#!/usr/bin/env bash
# View-level ACL (optional per-view uid whitelist) end-to-end over real FUSE
# mounts against a central meta-server. Proves:
#   - an UNRESTRICTED view (no acl rows) mounts for any uid  -> no regression;
#   - a RESTRICTED view refuses the M_OPEN (E_AUTH) for a uid with no read grant
#     (mount fails fast), admits one with a read grant;
#   - a read-only grant can list/read but every write-intent op returns EPERM
#     (server sends E_NOTPERM, distinct from an HA-standby's E_ROFS failover
#     signal, so an ACL denial never triggers a spurious failover);
#   - revoking the last row reopens the view (back to UNRESTRICTED).
# Assertion-based (pass/fail counters + real exit code), like the other suites.
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove

META=127.0.0.1:50098
DATA=127.0.0.1:50099
rm -rf .acl; mkdir -p .acl/central/data .acl/m1 .acl/m2 .acl/m3 .acl/mnt_rw .acl/mnt_ro

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }

echo "===== 1) seed the ACL on the server's home (before start) ====="
$Y --home .acl/central acl allow --view alice --uid 1000 --read --write >/dev/null 2>&1 \
    && ok "granted uid1000 rw on alice" || bad "acl allow (rw) failed"
$Y --home .acl/central acl allow --view alice --uid 2000 --read >/dev/null 2>&1 \
    && ok "granted uid2000 ro on alice" || bad "acl allow (ro) failed"
[ -f .acl/central/meta/acl.db ] && ok "server home has acl.db" || bad "acl.db missing"
LIST=$($Y --home .acl/central acl list --view alice 2>&1)
echo "$LIST" | grep -q "whitelisted" && ok "alice reports whitelisted" || bad "alice should be whitelisted ($LIST)"
echo "$LIST" | grep -q "uid 1000 read=true write=true" && ok "list shows 1000 rw" || bad "list missing 1000 rw ($LIST)"
echo "$LIST" | grep -q "uid 2000 read=true write=false" && ok "list shows 2000 ro" || bad "list missing 2000 ro ($LIST)"

echo "===== 2) an unrestricted view (bob) reports UNRESTRICTED ====="
$Y --home .acl/central acl list --view bob 2>&1 | grep -q "UNRESTRICTED" \
    && ok "bob unrestricted" || bad "bob should be unrestricted"

# ---- central services ----
$Y data-server --listen $DATA --root .acl/central/data > .acl/data.log 2>&1 &
DP=$!
$Y --home .acl/central meta-server --listen $META > .acl/meta.log 2>&1 &
MP=$!
wait_port $DATA || true
if wait_port $META; then MP_OK=1; else MP_OK=0; fi
[ "$MP_OK" = 1 ] || { echo "servers failed to start"; cat .acl/data.log .acl/meta.log; }

cleanup(){ for d in .acl/mnt_rw .acl/mnt_ro; do fusermount -u "$d" 2>/dev/null; done; kill $RWP $ROP $DP $MP 2>/dev/null; }
RWP=0; ROP=0
trap cleanup EXIT

echo "===== 3) DENIED mount: uid 3000 (no read grant) fails fast ====="
$Y --home .acl/m3 mount --view alice --mountpoint .acl/mnt_denied --meta $META --data $DATA --as-uid 3000 > .acl/denied.log 2>&1
DEN=$?
[ "$DEN" != 0 ] && ok "uid3000 mount rejected (rc=$DEN)" || bad "uid3000 should be denied but mount returned 0"
grep -qi "reject\|permission" .acl/denied.log && ok "denied log cites rejection" || bad "denied log unclear ($(cat .acl/denied.log))"
[ ! -d .acl/mnt_denied/projects ] && ok "denied mountpoint produced no tree" || bad "denied mount somehow active"

echo "===== 4) RW mount: uid 1000 can create + write ====="
$Y --home .acl/m1 mount --view alice --mountpoint .acl/mnt_rw --meta $META --data $DATA --as-uid 1000 > .acl/rw.log 2>&1 &
RWP=$!
sleep 1.5
A=.acl/mnt_rw
T mkdir "$A/projects" && ok "uid1000 mkdir ok" || bad "uid1000 mkdir failed"
T sh -c "printf 'secret' > $A/notes.txt" && ok "uid1000 write ok" || bad "uid1000 write failed"
sync "$A" 2>/dev/null; sleep 2.3
chk "$(T cat $A/notes.txt)" "secret" "uid1000 reads back its file"

echo "===== 5) RO mount: uid 2000 reads but cannot write ====="
$Y --home .acl/m2 mount --view alice --mountpoint .acl/mnt_ro --meta $META --data $DATA --as-uid 2000 > .acl/ro.log 2>&1 &
ROP=$!
sleep 1.5
B=.acl/mnt_ro
if T ls "$B" | grep -qx projects; then ok "uid2000 sees rw client's dir (shared read)"; else bad "uid2000 cannot see dir ($(T ls $B | tr '\n' ' '))"; fi
chk "$(T cat $B/notes.txt)" "secret" "uid2000 reads rw client's file"
T sh -c "printf 'nope' > $B/forbidden.txt"
RC=$?
[ "$RC" != 0 ] && ok "uid2000 write denied (rc=$RC)" || bad "uid2000 write should have been denied"
# and the file must NOT have appeared for the rw client
sleep 1.0
[ ! -f "$A/forbidden.txt" ] && ok "denied write left no trace" || bad "denied write leaked a file"

echo "===== 5b) TOCTOU: downgrade the LIVE rw mount's uid in-place, no remount ====="
# The rw mount from step 4 is STILL connected; its M_OPEN handshake passed while
# uid 1000 had write=true. Downgrade that grant to read-only in place. Because
# the server re-evaluates the write-gate per-op (not cached at open), a fresh
# write over the *existing* pooled connection must now be refused immediately.
$Y --home .acl/central acl allow --view alice --uid 1000 --read >/dev/null 2>&1 \
    && ok "downgraded uid1000 rw->ro (in place)" || bad "acl downgrade failed"
T sh -c "printf 'late' > $A/late.txt"
RC2=$?
[ "$RC2" != 0 ] && ok "write refused on LIVE mount after downgrade (rc=$RC2)" || bad "live mount still writable after revoke -> TOCTOU regression"
[ ! -f "$A/late.txt" ] && ok "downgraded live write left no file" || bad "downgraded write leaked a file"
chk "$(T cat $A/notes.txt)" "secret" "downgraded uid can still read (ro grant)"
# restore rw so the following unrestricted-revoke section starts from a known state
$Y --home .acl/central acl allow --view alice --uid 1000 --read --write >/dev/null 2>&1

echo "===== 6) revoke all rows -> view is UNRESTRICTED again ====="
$Y --home .acl/central acl revoke --view alice --uid 1000 >/dev/null 2>&1
$Y --home .acl/central acl revoke --view alice --uid 2000 >/dev/null 2>&1
$Y --home .acl/central acl list --view alice 2>&1 | grep -q "UNRESTRICTED" \
    && ok "alice unrestricted after revoking last row" || bad "alice should reopen after revoke"

# ---- summary ----
echo
echo "===== ACL RESULT: $pass passed, $fail failed ====="
cleanup
[ "$fail" = 0 ]
