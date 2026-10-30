#!/usr/bin/env bash
# Revocable per-caller MOUNT credentials (`--require-api-key` on the meta/data
# planes + `mount --api-key`) end-to-end over real FUSE mounts. A static
# `--token` is ONE shared cluster secret: nobody can tell which client opened a
# view, and rotating it disconnects everyone at once. This suite proves the
# API-key form of the same gate, on the existing `grovefs apikey` surface:
#   - minting is SCOPED (`--for mount|mcp|any`), so a mount credential cannot be
#     replayed against the governance MCP entrance and vice versa;
#   - every connect is AUDITED (`apikey calls`: method meta|data, tool
#     M_OPEN|OP_OPEN), refusals included, with the server's exact reason;
#   - refusals reach the client as TEXT (missing key / unknown id / wrong secret
#     / wrong scope / revoked / malformed), not a bare errno;
#   - `apikey revoke` kills a LIVE mount at its first server round-trip past the
#     1s re-check window: it prints the reason and unmounts itself — never a
#     stale "Transport endpoint is not connected";
#   - only the revoked caller is affected (another key's mount keeps serving);
#   - the gates compose (`--token` AND `--require-api-key` both have to pass),
#     the data plane gates on its own, the credential survives an address
#     failover, and the opt-in default is untouched: an ungated server still
#     admits a key-less mount and ignores a presented key.
# Assertion-based (pass/fail counters + real exit code), like the other suites.
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs

BASE=$PWD/.mktest/central
# key-gated pair / token+key pair / data-plane-only gate / fully ungated pair
META=127.0.0.1:50200;  DATA=127.0.0.1:50201
METAT=127.0.0.1:50203; DATAT=127.0.0.1:50204
METAD=127.0.0.1:50205; DATAD=127.0.0.1:50206
METAP=127.0.0.1:50207; DATAP=127.0.0.1:50208
DEAD=127.0.0.1:50299                 # never bound: forces the failover path
TOK=S3CR3T-TOKEN
rm -rf .mktest
mkdir -p .mktest/central/data .mktest/central/meta .mktest/home
for d in ok any env denied tok dat plain ign fo; do mkdir -p ".mktest/mnt_$d"; done

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }
# A mount that must FAIL: foreground (so the refusal is in the log and the exit
# code is ours), bounded, with no ambient credential.
MFAIL(){ local log=$1; shift; env -u GROVEFS_API_KEY timeout 25 "$@" --foreground >"$log" 2>&1; }
# A mount that must SURVIVE: foreground mode so $! is the mount process itself
# (a detached mount's daemon is re-exec'd out of our reach).
MLIVE(){ local log=$1 var=$2; shift 2
  env -u GROVEFS_API_KEY timeout 600 "$@" --foreground >"$log" 2>&1 &
  eval "$var=\$!"
  PIDS="$PIDS $!"
  sleep 2.5; }
LIVE(){ grep -qs " $PWD/$1 " /proc/mounts; }
PIDS=""
UMOUNT_ALL(){ for d in .mktest/mnt_*; do fusermount -u "$d" 2>/dev/null; done; }
cleanup(){ UMOUNT_ALL; for p in $PIDS; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

# Mint one key; the caller asserts on the raw output (a function here would run
# in a subshell and lose the pass/fail counters).
mint(){ $Y apikey --db-path "$BASE" create --label "$1" ${2:+--for "$2"} 2>&1; }

echo "===== 1) apikey create --for <scope>: minting, listing, at-rest hashing ====="
C1=$(mint alice-mnt mount)
ID_MOUNT=$(echo "$C1" | sed -n 's/^  id:    //p')
KEY_MOUNT=$(echo "$C1" | sed -n 's/^  key:   //p')
SEC_MOUNT=${KEY_MOUNT#*:}
if [ -n "$ID_MOUNT" ] && [ "${KEY_MOUNT%%:*}" = "$ID_MOUNT" ] && [ -n "$SEC_MOUNT" ]; then
  ok "mount-scoped key minted as '<id>:<secret>' and printed once"
else bad "unexpected mint output: $C1"; fi
echo "$C1" | grep -q "^  scope: mount$" && ok "--for mount reports its scope" || bad "no scope line: $C1"
C2=$(mint carol-any "")
ID_ANY=$(echo "$C2" | sed -n 's/^  id:    //p')
KEY_ANY=$(echo "$C2" | sed -n 's/^  key:   //p')
echo "$C2" | grep -q "^  scope: any$" && ok "default scope is 'any' (pre-scope deployments keep working)" || bad "default scope wrong: $C2"
C3=$(mint agent-mcp mcp)
KEY_MCP=$(echo "$C3" | sed -n 's/^  key:   //p')
echo "$C3" | grep -q "^  scope: mcp$" && ok "--for mcp reports its scope" || bad "mcp scope line missing: $C3"
LIST=$($Y apikey --db-path "$BASE" list 2>&1)
echo "$LIST" | grep -q "scope" && ok "apikey list carries a scope column" || bad "list has no scope column: $LIST"
echo "$LIST" | grep -F "$ID_MOUNT" | grep -q "mount" && ok "the mount key is listed with scope 'mount'" || bad "mount key not listed with its scope"
if $Y apikey --db-path "$BASE" create --label oops --for everything >/dev/null 2>&1; then
  bad "--for <typo> accepted (a typo must not silently widen a credential)"
else ok "--for <typo> refused"; fi
if [ -f "$BASE/meta/api_keys.db" ] && ! grep -a -q "$SEC_MOUNT" "$BASE/meta/api_keys.db"; then
  ok "only the digest is at rest — the mount secret is not recoverable from api_keys.db"
else bad "plaintext secret found on disk"; fi
echo "$C1" | grep -q -- "--api-key" && ok "the mint prints the exact present-it command for a mount" || bad "mint guidance missing"

# ------------------------------------------------------------------- servers
$Y data-server --listen $DATA  --root "$BASE/data" --require-api-key --keys-from "$BASE" > .mktest/data.log 2>&1 &
PIDS="$PIDS $!"
$Y meta-server --db-path "$BASE" --listen $META --require-api-key > .mktest/meta.log 2>&1 &
PIDS="$PIDS $!"
$Y data-server --listen $DATAD --root "$BASE/data" --require-api-key > .mktest/datad.log 2>&1 &
PIDS="$PIDS $!"
$Y meta-server --db-path "$BASE" --listen $METAD > .mktest/metad.log 2>&1 &
PIDS="$PIDS $!"
$Y data-server --listen $DATAP --root "$BASE/data" > .mktest/datap.log 2>&1 &
PIDS="$PIDS $!"
$Y meta-server --db-path "$BASE" --listen $METAP > .mktest/metap.log 2>&1 &
PIDS="$PIDS $!"
for a in $DATA $META $DATAD $METAD $DATAP $METAP; do wait_port "$a" || echo "  (port $a did not come up)"; done
grep -q "api-key auth REQUIRED" .mktest/meta.log && ok "meta-server announces the key gate at startup" || bad "meta log lacks the gate notice: $(cat .mktest/meta.log)"
grep -q "api-key auth REQUIRED" .mktest/data.log && ok "data-server announces the key gate at startup" || bad "data log lacks the gate notice"
# The data server above with no --keys-from resolves the store under its first
# --root, where no key was ever minted: it must fail closed, not open wide.
MFAIL .mktest/den_empty.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $METAD --data $DATAD --api-key "$KEY_ANY"
grep -qi "no api key presented\|unknown api key id" .mktest/den_empty.log && ok "--keys-from default (first --root) gates too, and an empty store admits nothing" || bad "default keys-from unclear: $(cat .mktest/den_empty.log)"

echo "===== 2) refusals arrive as the server's REASON, not a bare errno ====="
MFAIL .mktest/den_nokey.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA
RC=$?
[ "$RC" != 0 ] && ok "key-less mount refused (rc=$RC)" || bad "key-less mount should fail but returned 0"
grep -qi "no api key presented" .mktest/den_nokey.log && ok "refusal names the missing knob and how to fix it" || bad "unclear refusal: $(cat .mktest/den_nokey.log)"
LIVE .mktest/mnt_denied && bad "refused mount is in /proc/mounts" || ok "refused mount left no live mountpoint behind"

MFAIL .mktest/den_bad.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "${ID_MOUNT}:deadbeefdeadbeef"
[ "$?" != 0 ] && ok "wrong secret refused" || bad "wrong secret admitted"
grep -qi "invalid api key secret" .mktest/den_bad.log && ok "wrong secret reported as such" || bad "unclear: $(cat .mktest/den_bad.log)"

MFAIL .mktest/den_unknown.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "00000000000000000000000000000000:whatever"
grep -qi "unknown api key id" .mktest/den_unknown.log && ok "unknown id reported as such" || bad "unclear: $(cat .mktest/den_unknown.log)"

MFAIL .mktest/den_mal.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "no-colon-here"
grep -qi "malformed api key" .mktest/den_mal.log && ok "a colon-less key reports the format problem" || bad "unclear: $(cat .mktest/den_mal.log)"

MFAIL .mktest/den_scope.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "$KEY_MCP"
[ "$?" != 0 ] && ok "an mcp-scoped key cannot open a mount" || bad "mcp-scoped key admitted to a mount"
grep -qi "scoped to 'mcp', not 'mount'" .mktest/den_scope.log && ok "the scope refusal states both scopes" || bad "unclear: $(cat .mktest/den_scope.log)"

echo "===== 3) admission: --for mount, --for any, and the GROVEFS_API_KEY fallback ====="
MLIVE .mktest/ok.log   PID_OK  $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_ok  --meta $META --data $DATA --api-key "$KEY_MOUNT"
LIVE .mktest/mnt_ok && ok "mount-scoped key admitted" || bad "mount did not come up: $(cat .mktest/ok.log)"
T mkdir .mktest/mnt_ok/projects && ok "mkdir through the keyed mount" || bad "mkdir failed"
T sh -c "printf 'payload' > .mktest/mnt_ok/projects/note.txt" && ok "write through the keyed mount" || bad "write failed"
sync .mktest/mnt_ok 2>/dev/null; sleep 2.2
chk "$(T cat .mktest/mnt_ok/projects/note.txt)" "payload" "content readable back through the keyed mount"

MLIVE .mktest/any.log  PID_ANY  $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_any --meta $META --data $DATA --api-key "$KEY_ANY"
LIVE .mktest/mnt_any && ok "an 'any' key still mounts (scope widening stays backward compatible)" || bad "any-scoped key refused: $(cat .mktest/any.log)"

MLIVE .mktest/env.log  PID_ENV  env GROVEFS_API_KEY="$KEY_ANY" $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_env --meta $META --data $DATA
LIVE .mktest/mnt_env && ok "GROVEFS_API_KEY supplies the credential (secret stays out of argv)" || bad "env fallback failed: $(cat .mktest/env.log)"
LEAK=0
for c in /proc/[0-9]*/cmdline; do
  if tr '\0' ' ' <"$c" 2>/dev/null | grep -q "mnt_env.*--api-key"; then LEAK=1; fi
done
if [ "$LEAK" = 1 ]; then
  bad "the env-fallback mount leaked a --api-key into its command line"
else ok "no key material in the env-fallback mount's argv (what any local user sees in ps)"; fi

echo "===== 4) connects are audited on the existing apikey surface ====="
CALLS=$($Y apikey --db-path "$BASE" calls --limit 200 2>&1)
echo "$CALLS" | grep "M_OPEN" | grep -qF "$ID_ANY" && ok "meta-plane connect audited (tool=M_OPEN) against the key id" || bad "no attributed M_OPEN row: $(echo "$CALLS" | head -3)"
echo "$CALLS" | grep "OP_OPEN" | grep -q "data" && ok "data-plane connect audited (method=data, tool=OP_OPEN)" || bad "the data plane audits nothing: $(echo "$CALLS" | head -3)"
echo "$CALLS" | grep -q "unknown api key id" && ok "the refused unknown-id connect is audited (ok=no) with its reason" || bad "refusal not audited"
echo "$CALLS" | grep "M_OPEN" | grep -q "view='alice'" && ok "the audit row records WHICH view was opened" || bad "audit row lacks the view detail"

echo "===== 5) a mount credential cannot be replayed against the MCP entrance ====="
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"apiKey":"%s"}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}\n' "$KEY_MOUNT" > .mktest/mcp_in
timeout 15 $Y mcp --db-path "$BASE" --require-api-key < .mktest/mcp_in > .mktest/mcp_out 2>/dev/null
if grep -q -- "-32001" .mktest/mcp_out; then
  ok "MCP refuses the mount-scoped key (least privilege holds in both directions)"
else bad "a mount key opened the governance API: $(cat .mktest/mcp_out)"; fi
$Y apikey --db-path "$BASE" calls --limit 200 2>&1 | grep -q "scoped to 'mount', not 'mcp'" \
  && ok "that cross-entrance attempt is audited with the scope refusal" || bad "cross-entrance refusal not audited"

echo "===== 6) revoke kills the LIVE mount — and only that mount ====="
ROUT=$($Y apikey --db-path "$BASE" revoke --id "$ID_MOUNT" 2>&1)
echo "$ROUT" | grep -q "revoked id='$ID_MOUNT'" && ok "revoke by id" || bad "revoke failed: $ROUT"
echo "$ROUT" | grep -qi "LIVE mount" && ok "the revoke output states the live-connection contract" || bad "revoke output silent about live mounts: $ROUT"
sleep 1.3                                    # past the 1s re-check window
T sh -c "printf 'late' > .mktest/mnt_ok/projects/late.txt"
[ "$?" != 0 ] && ok "write on the revoked live mount refused (the re-check landed)" || bad "revoked key still writing -> TOCTOU regression"
GONE=0
for _ in $(seq 1 60); do LIVE .mktest/mnt_ok || { GONE=1; break; }; sleep 0.25; done
[ "$GONE" = 1 ] && ok "revoked mount tore itself down with no operator action" || bad "mount still live after revoke: $(cat .mktest/ok.log)"
grep -qi "revoked" .mktest/ok.log && ok "the mount printed the server's reason" || bad "no reason printed: $(cat .mktest/ok.log)"
grep -qi "unmounting this view" .mktest/ok.log && ok "the teardown was announced as an auth-loss unmount" || bad "teardown unannounced: $(cat .mktest/ok.log)"
# The endpoint must be GONE, not broken — this is exactly the stale-mount class
# an in-place "just return EACCES forever" design would have left behind. The
# lookup is retried because the kernel needs a moment after the detach before the
# mountpoint resolves to the plain directory underneath it.
STALE=1
for _ in $(seq 1 12); do T ls .mktest/mnt_ok >/dev/null 2>.mktest/stale.err && { STALE=0; break; }; sleep 0.25; done
[ "$STALE" = 0 ] && ok "mountpoint is a clean directory again (no stale endpoint)" || bad "stale endpoint after revoke: $(cat .mktest/stale.err)"
if LIVE .mktest/mnt_any && LIVE .mktest/mnt_env; then
  ok "other callers' mounts untouched (per-caller revoke, unlike rotating one shared token)"
else bad "collateral damage: a live mount of a DIFFERENT key died"; fi
chk "$(T cat .mktest/mnt_any/projects/note.txt)" "payload" "the surviving key still reads the shared view"
MFAIL .mktest/den_rev.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "$KEY_MOUNT"
grep -qi "has been revoked" .mktest/den_rev.log && ok "revoked key refused on a fresh mount (fails closed)" || bad "revoked key reconnected: $(cat .mktest/den_rev.log)"
$Y apikey --db-path "$BASE" list 2>&1 | grep -qF "$ID_MOUNT" && ok "the revoked row stays listed (past calls keep attribution)" || bad "revoked row vanished"

echo "===== 7) both gates compose: --token AND --require-api-key both must pass ====="
$Y meta-server --db-path "$BASE" --listen $METAT --token "$TOK" --require-api-key > .mktest/metat.log 2>&1 &
PIDS="$PIDS $!"
$Y data-server --listen $DATAT --root "$BASE/data" --token "$TOK" --require-api-key --keys-from "$BASE" > .mktest/datat.log 2>&1 &
PIDS="$PIDS $!"
wait_port $METAT; wait_port $DATAT
MFAIL .mktest/den_tok.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $METAT --data $DATAT --api-key "$KEY_ANY"
grep -qi "rejected the access token" .mktest/den_tok.log && ok "a valid key without the cluster token is refused (gates AND)" || bad "token gate bypassed by presenting a key: $(cat .mktest/den_tok.log)"
MFAIL .mktest/den_key2.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_denied --meta $METAT --data $DATAT --token "$TOK"
grep -qi "no api key presented" .mktest/den_key2.log && ok "the cluster token alone no longer buys a mount" || bad "key gate bypassed by the token: $(cat .mktest/den_key2.log)"
MLIVE .mktest/tok.log PID_TOK $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_tok --meta $METAT --data $DATAT --token "$TOK" --api-key "$KEY_ANY"
T mkdir .mktest/mnt_tok/x && ok "token + key together admitted (both gates satisfied)" || bad "double-gated mount unusable: $(cat .mktest/tok.log)"

echo "===== 8) the data plane gates content on its own ====="
MFAIL .mktest/den_dat.log $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_dat --meta $METAD --data $DATAD
RC=$?
[ "$RC" != 0 ] && ok "an open meta-plane still reads no content: the data server refused (rc=$RC)" || bad "data plane served without a credential"
grep -qi "no api key presented" .mktest/den_dat.log && ok "the data refusal carries the same reason text" || bad "unclear data refusal: $(cat .mktest/den_dat.log)"
MLIVE .mktest/dat.log PID_DAT $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_dat --meta $METAD --data $DATAD --api-key "$KEY_ANY"
T sh -c "printf 'content' > .mktest/mnt_dat/blob.txt" && chk "$(T cat .mktest/mnt_dat/blob.txt)" "content" "keyed content path works against a key-gated data server" || bad "keyed mount cannot write to the gated data server"

echo "===== 9) the credential rides a failover address list ====="
MLIVE .mktest/fo.log PID_FO $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_fo --meta "$DEAD,$META" --data $DATA --api-key "$KEY_ANY"
sleep 2                                     # the dead addr burns the retry budget first
LIVE .mktest/mnt_fo && ok "mount fell over to the live meta addr with its key intact" || bad "failover lost the credential: $(cat .mktest/fo.log)"
chk "$(T cat .mktest/mnt_fo/projects/note.txt)" "payload" "the failover mount reads the shared view"

echo "===== 10) regression: the gate is opt-in, the default path is untouched ====="
MLIVE .mktest/plain.log PID_PLAIN $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_plain --meta $METAP --data $DATAP
T mkdir .mktest/mnt_plain/p && ok "key-less mount still works against an ungated server" || bad "default path regressed: $(cat .mktest/plain.log)"
MLIVE .mktest/ign.log PID_IGN $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_ign --meta $METAP --data $DATAP --api-key "$KEY_ANY"
LIVE .mktest/mnt_ign && ok "an ungated server simply ignores a presented key (no accidental lockout)" || bad "key against an open server failed: $(cat .mktest/ign.log)"

echo "===== 11) view-bound (--view) and read-only (--ro) credentials ====="
mkdir -p .mktest/mnt_bound .mktest/mnt_ro
# A key minted with `--view alice` opens alice, and is refused on ANY other view
# — per-view isolation carried by the credential itself.
CB=$($Y apikey --db-path "$BASE" create --label alice-only --for mount --view alice 2>&1)
KEY_BOUND=$(echo "$CB" | sed -n 's/^  key:   //p')
echo "$CB" | grep -q "^  view:  alice$" && ok "a --view-bound key reports its bound view at mint" || bad "mint lacks the view line: $CB"
MLIVE .mktest/bound.log PID_BOUND $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_bound --meta $META --data $DATA --api-key "$KEY_BOUND"
T mkdir .mktest/mnt_bound/bx && ok "a view-bound read-write key writes its own view" || bad "bound rw mount not writable: $(cat .mktest/bound.log)"
MFAIL .mktest/den_view.log $Y --home .mktest/home mount --view bob --mountpoint .mktest/mnt_denied --meta $META --data $DATA --api-key "$KEY_BOUND"
grep -qi "bound to view 'alice'" .mktest/den_view.log && ok "a view-bound key is refused on a different view (credential-level isolation)" || bad "view binding not enforced: $(cat .mktest/den_view.log)"
$Y apikey --db-path "$BASE" calls --limit 200 2>&1 | grep "M_OPEN" | grep -q "bound to view 'alice'" \
  && ok "the cross-view refusal is audited with its reason" || bad "view-binding refusal not audited"

# A read-only key opens the view for reading but the server refuses every write,
# and the client is told to mount read-only so it never even tries.
CR=$($Y apikey --db-path "$BASE" create --label reader --for mount --view alice --ro 2>&1)
KEY_RO=$(echo "$CR" | sed -n 's/^  key:   //p')
echo "$CR" | grep -q "^  rw:    ro (read-only)$" && ok "an --ro key reports read-only at mint" || bad "mint lacks the rw line: $CR"
MLIVE .mktest/ro.log PID_RO $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_ro --meta $META --data $DATA --api-key "$KEY_RO"
LIVE .mktest/mnt_ro && ok "a read-only key still mounts (reads allowed)" || bad "ro mount did not come up: $(cat .mktest/ro.log)"
chk "$(T cat .mktest/mnt_ro/projects/note.txt)" "payload" "the read-only key reads the shared view content"
if T mkdir .mktest/mnt_ro/should_fail 2>/dev/null; then
  bad "a write through a read-only key mount succeeded (ro not enforced)"
else ok "writes refused through a read-only key mount (server-side ro cap, not just a client flag)"; fi
if T sh -c "printf 'x' > .mktest/mnt_ro/projects/note.txt" 2>/dev/null; then
  bad "a read-only key overwrote existing content"
else ok "a read-only key cannot overwrite existing content"; fi

# ---------------------------------------------------------------------------
echo "===== 12) set-ro: flip rw <-> ro IN PLACE, hot on the live mount ====="
mkdir -p .mktest/mnt_tog
# Start from a READ-WRITE key so the mount is fully writable at the OS level:
# every refusal below can only come from the SERVER-side gate, which is what
# `set-ro` moves. (A mount minted `--ro` is kernel read-only from the handshake;
# that cap needs a remount and is NOT what this asserts.)
CT=$($Y apikey --db-path "$BASE" create --label toggler --for mount --view alice 2>&1)
ID_TOG=$(echo "$CT" | sed -n 's/^  id:    //p')
KEY_TOG=$(echo "$CT" | sed -n 's/^  key:   //p')
MLIVE .mktest/tog.log PID_TOG $Y --home .mktest/home mount --view alice --mountpoint .mktest/mnt_tog --meta $META --data $DATA --api-key "$KEY_TOG"
T mkdir .mktest/mnt_tog/pre_rw && ok "baseline: the rw key's live mount writes" || bad "toggle mount not writable: $(cat .mktest/tog.log)"
# rw -> ro over the CLI (a unique short id prefix, like revoke accepts).
SROUT=$($Y apikey --db-path "$BASE" set-ro --id "${ID_TOG:0:8}" 2>&1)
echo "$SROUT" | grep -q "is now ro (read-only)" && ok "set-ro flips rw->ro by id prefix" || bad "set-ro output: $SROUT"
echo "$SROUT" | grep -qi "re-check window" && ok "set-ro states the hot-effect contract (live mounts, ≤1s, no reconnect)" || bad "set-ro output silent on the live contract: $SROUT"
LIVE .mktest/mnt_tog && ok "the flip did NOT tear the mount down (a permission change, not an auth loss)" || bad "mount died on set-ro: $(cat .mktest/tog.log)"
sleep 1.3                                   # past the 1s re-check window
if T mkdir .mktest/mnt_tog/should_fail 2>/dev/null; then
  bad "write admitted after set-ro -> ro (hot re-check not reading the flag)"
else ok "writes refused on the SAME live mount within the re-check window (meta gate EPERM)"; fi
if T sh -c "printf 'x' > .mktest/mnt_tog/pre_rw/f.txt" 2>/dev/null; then
  bad "content write admitted after set-ro -> ro (data-plane gate not hot)"
else ok "content writes refused too (data plane refreshes ro on its own tick)"; fi
chk "$(T cat .mktest/mnt_tog/projects/note.txt)" "payload" "reads keep flowing while ro"
# ro -> rw with --rw, same live mount, no reconnect.
SROUT2=$($Y apikey --db-path "$BASE" set-ro --id "$ID_TOG" --rw 2>&1)
echo "$SROUT2" | grep -q "is now rw (read-write)" && ok "set-ro --rw flips ro->rw in place" || bad "set-ro --rw output: $SROUT2"
sleep 1.3
T mkdir .mktest/mnt_tog/post_rw && ok "writes admitted again on the live mount — no reconnect, no remount" || bad "rw not restored hot: $(cat .mktest/tog.log)"
# The rw COLUMN (field 6 of the list row) flipped, not just some "rw" text.
ROWCOL=$($Y apikey --db-path "$BASE" list 2>&1 | grep -F "$ID_TOG" | awk '{print $6}')
chk "$ROWCOL" "rw" "list reflects the flipped rw column"
# The guard cases: unknown id, and a revoked row that set-ro must not resurrect.
if $Y apikey --db-path "$BASE" set-ro --id nosuchkeyid --rw >/dev/null 2>&1; then
  bad "set-ro of an unknown id should fail but exited 0"
else ok "set-ro of an unknown id errors (non-zero)"; fi
$Y apikey --db-path "$BASE" revoke --id "$ID_TOG" >/dev/null 2>&1
if $Y apikey --db-path "$BASE" set-ro --id "$ID_TOG" --rw >/dev/null 2>&1; then
  bad "set-ro flipped a REVOKED row (a revoke is permanent)"
else ok "set-ro refuses a revoked id (never half-undoes a revoke)"; fi

# ---- summary ----
echo
echo "SUMMARY: pass=$pass fail=$fail"
echo "===== MOUNTKEY RESULT: $pass passed, $fail failed ====="
cleanup
[ "$fail" = 0 ]
