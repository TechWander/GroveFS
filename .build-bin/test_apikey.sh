#!/usr/bin/env bash
# API-key authentication + MCP call-audit log e2e. Covers BOTH halves:
#   (1) the `grovefs apikey` CLI — create/list/revoke/calls, keys hashed at rest;
#   (2) the gated server `grovefs mcp --require-api-key` — tools/list + tools/call
#       refused until an `initialize`-presented key (or a server-wide
#       GROVEFS_MCP_API_KEY) authenticates, and every call attributed in the log.
# It also pins the DEFAULT behavior: a plain `grovefs mcp` needs no key. Uses the
# file-piped stdio pattern (stdin EOF shuts the server), deterministic for
# request->response assertions. Assertion-based; emits a SUMMARY line run_all reads.
set -u
cd /data/xy/grovefs
Y=./target/release/grovefs
BASE=$PWD/.apitest/home
rm -rf .apitest
mkdir -p .apitest/home
export GROVEFS_HOME=$BASE

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }
# frame FILE ID  -> the answer object for request id ID (or empty); jq -s slurps
# so an out-of-order/absent id yields "null" rather than a misleading exit code.
frame(){ jq -s -c --argjson id "$2" 'map(select(.id==$id))|.[0]' "$1" 2>/dev/null; }
fget(){ frame "$1" "$2" | jq -r "$3" 2>/dev/null; }

# Seed one physical view so the gated governance calls have a real base.
cat > .apitest/team.manifest <<'EOF'
mkdir docs
mkdir photos
mkdir photos/private
touch docs/a.md
touch photos/private/x.jpg
EOF
$Y init --view team --manifest .apitest/team.manifest > .apitest/init.log 2>&1 \
  && ok "seeded physical view 'team'" || { bad "init failed: $(cat .apitest/init.log)"; exit 1; }

# ---------------------------------------------------------------- CLI: create/list
echo "===== 1) grovefs apikey create + list (keys hashed at rest) ====="
COUT=$($Y apikey --db-path "$BASE" create --label agentA 2>&1)
ID=$(echo "$COUT"   | sed -n 's/^  id:    //p')
KEY=$(echo "$COUT"  | sed -n 's/^  key:   //p')
SECRET=${KEY#*:}
if [ -n "$ID" ] && [ -n "$KEY" ] && [ "$KEY" = "$ID:$SECRET" ]; then
  ok "create minted + printed a full plaintext key once (id + '<id>:<secret>')"
else bad "create output unexpected: $COUT"; fi
LOUT=$($Y apikey --db-path "$BASE" list 2>&1)
echo "$LOUT" | grep -qF "$ID" && echo "$LOUT" | grep -q "agentA" \
  && ok "list shows the new key by id + label" || bad "list missing key: $LOUT"
# The plaintext secret must NOT be recoverable from the on-disk store (hash only).
if [ -f "$BASE/meta/api_keys.db" ] && ! grep -a -q "$SECRET" "$BASE/meta/api_keys.db"; then
  ok "api_keys.db stores only the hash — the plaintext secret is not on disk"
else bad "secret leaked to disk or db missing"; fi

# --------------------------------------------------------- gated server: refused
echo "===== 2) --require-api-key gates tools/list + tools/call until auth ====="
frames(){ # $1 = apiKey to place in initialize ("" = omit the key)
  if [ -n "$1" ]; then
    printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"apiKey":"%s"}}\n' "$1"
  else
    printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}\n'
  fi
  printf '{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n'
  printf '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}\n'
  printf '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"%s","base":"team","rules":"hide /photos/private"}}}\n' "${5:-apr}"
}
# (a) no key at all -> initialize stays open, but tools/list + tools/call refused.
frames "" > .apitest/in_nokey
T $Y mcp --db-path "$BASE" --require-api-key < .apitest/in_nokey > .apitest/out_nokey 2>/dev/null
if [ "$(fget .apitest/out_nokey 1 .result.serverInfo.name)" = "grovefs" ] \
   && [ "$(fget .apitest/out_nokey 2 .error.code)" = "-32001" ] \
   && [ "$(fget .apitest/out_nokey 3 .error.code)" = "-32001" ]; then
  ok "unauthenticated: initialize open, tools/list + tools/call refused (-32001)"
else bad "gate did not refuse: $(cat .apitest/out_nokey)"; fi
[ ! -f "$BASE/meta/views/apr.layout" ] && ok "refused call wrote nothing" || bad "refused define leaked a definition"

# (b) right id, wrong secret -> refused, logged distinctly.
frames "$ID:deadbeefdeadbeef" > .apitest/in_bad
T $Y mcp --db-path "$BASE" --require-api-key < .apitest/in_bad > .apitest/out_bad 2>/dev/null
if [ "$(fget .apitest/out_bad 2 .error.code)" = "-32001" ]; then
  ok "a wrong secret does not authenticate (still refused)"
else bad "wrong secret admitted: $(cat .apitest/out_bad)"; fi

# (c) the real key in initialize -> admitted; a governance write succeeds.
frames "$KEY" > .apitest/in_ok
T $Y mcp --db-path "$BASE" --require-api-key < .apitest/in_ok > .apitest/out_ok 2>/dev/null
if [ "$(fget .apitest/out_ok 2 '.result.tools|length')" = "8" ] \
   && [ "$(fget .apitest/out_ok 4 .result.isError)" != "true" ]; then
  ok "initialize(apiKey) authenticates the session; tools/list + define admitted"
else bad "key not admitted: $(cat .apitest/out_ok)"; fi
[ -f "$BASE/meta/views/apr.layout" ] && ok "authenticated define wrote the definition" || bad "authenticated define did not land"

# -------------------------------------------------------------- audit-log checks
echo "===== 3) every call is logged + attributed (grovefs apikey calls) ====="
CALLS=$($Y apikey --db-path "$BASE" calls --limit 60 2>&1)
echo "$CALLS" | grep -F "define_derived_view" | grep -qF "$ID" \
  && ok "audit log records the authenticated define, attributed to the key id" \
  || bad "no attributed define row: $(echo "$CALLS" | head -5)"
echo "$CALLS" | grep -q "unauthorized" \
  && ok "the refused (no-key) tools/list is audited as unauthorized" \
  || bad "unauthorized attempt not logged"
echo "$CALLS" | grep -q "invalid api key secret" \
  && ok "the wrong-secret initialize is audited with the exact refusal" \
  || bad "bad-secret attempt not logged"

# ----------------------------------------------------------------- CLI: revoke
echo "===== 4) revoke cuts authentication (row kept for attribution) ====="
ROUT=$($Y apikey --db-path "$BASE" revoke --id "$ID" 2>&1)
echo "$ROUT" | grep -q "revoked id=" && ok "revoke by exact id succeeded" || bad "revoke: $ROUT"
# Revoking again reports already-revoked (idempotent, not an error).
echo "$($Y apikey --db-path "$BASE" revoke --id "$ID" 2>&1)" | grep -q "already revoked" \
  && ok "re-revoking is a no-op reported as such" || bad "double revoke not handled"
# The revoked key can no longer authenticate (still refused, logged 'revoked').
frames "$KEY" apr2 > .apitest/in_rev
T $Y mcp --db-path "$BASE" --require-api-key < .apitest/in_rev > .apitest/out_rev 2>/dev/null
if [ "$(fget .apitest/out_rev 2 .error.code)" = "-32001" ]; then
  ok "revoked key is refused again (authentication fails closed)"
else bad "revoked key still admitted: $(cat .apitest/out_rev)"; fi
$Y apikey --db-path "$BASE" calls --limit 60 2>&1 | grep -q "revoked" \
  && ok "the revoked-key attempt is audited (reason: revoked)" || bad "revoked attempt not logged"
# The row stays listed so historical calls keep attribution.
$Y apikey --db-path "$BASE" list 2>&1 | grep -qF "$ID" \
  && ok "revoked key still appears in list (past calls keep attribution)" || bad "revoked row vanished"
# Unknown id -> a real error (non-zero exit).
if $Y apikey --db-path "$BASE" revoke --id "nosuchkeyid" >/dev/null 2>&1; then
  bad "revoke of an unknown id should fail but exited 0"
else ok "revoke of an unknown id errors (non-zero)"; fi

# ------------------------------------------------------- CLI: id-prefix + env pre-auth
echo "===== 5) revoke by unique prefix + GROVEFS_MCP_API_KEY server pre-auth ====="
C2=$($Y apikey --db-path "$BASE" create --label agentC 2>&1)
ID2=$(echo "$C2" | sed -n 's/^  id:    //p')
KEY2=$(echo "$C2" | sed -n 's/^  key:   //p')
PREFIX=${ID2:0:8}
$Y apikey --db-path "$BASE" revoke --id "$PREFIX" 2>&1 | grep -q "revoked id='$ID2'" \
  && ok "revoke accepts a unique short id prefix (resolved to the full id)" \
  || bad "prefix revoke failed for prefix '$PREFIX'"
# A fresh ACTIVE key pre-authenticates the whole server via env — the client
# need not touch the handshake at all (for MCP clients that cannot customize it).
C3=$($Y apikey --db-path "$BASE" create --label agentD 2>&1)
KEY3=$(echo "$C3" | sed -n 's/^  key:   //p')
frames "" > .apitest/in_env        # initialize WITHOUT a key
GROVEFS_MCP_API_KEY="$KEY3" T $Y mcp --db-path "$BASE" --require-api-key \
  < .apitest/in_env > .apitest/out_env 2>/dev/null
if [ "$(fget .apitest/out_env 2 '.result.tools|length')" = "8" ]; then
  ok "GROVEFS_MCP_API_KEY pre-authenticates the server (handshake needs no key)"
else bad "env pre-auth did not admit tools/list: $(cat .apitest/out_env)"; fi

# ----------------------------------------------- default server is unchanged (no gate)
echo "===== 6) regression: plain 'grovefs mcp' (no --require-api-key) needs no key ====="
frames "" > .apitest/in_plain
T $Y mcp --db-path "$BASE" < .apitest/in_plain > .apitest/out_plain 2>/dev/null
if [ "$(fget .apitest/out_plain 2 '.result.tools|length')" = "8" ]; then
  ok "opt-in honored: without --require-api-key the tool surface stays open (logged, not gated)"
else bad "plain server unexpectedly gated: $(cat .apitest/out_plain)"; fi

echo "SUMMARY: pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
