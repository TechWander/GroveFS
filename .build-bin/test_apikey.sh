#!/usr/bin/env bash
# API-key authentication + MCP call-audit log e2e. Covers BOTH halves:
#   (1) the `grovefs apikey` CLI — create/list/revoke/calls, keys hashed at rest;
#   (2) the gated server `grovefs mcp --require-api-key` — tools/list + tools/call
#       refused until an `initialize`-presented key (or a server-wide
#       GROVEFS_MCP_API_KEY) authenticates, and every call attributed in the log.
# It also pins the DEFAULT behavior: a plain `grovefs mcp` needs no key. Uses the
# file-piped stdio pattern (stdin EOF shuts the server), deterministic for
# request->response assertions. Assertion-based; emits a SUMMARY line run_all reads.
# The metadata BACKEND is a parameter (P5): `APIKEY_BACKEND=fdb` runs the whole
# auth/audit/governance plane against a deployment whose trees live in a
# FoundationDB cluster. What changes is where the base view comes from
# (`view create` + one seeding mount instead of `init --manifest`) and the view's
# NAME (a subspace outlives the run, so it is per-run). Where the KEY RECORDS and
# the AUDIT ROWS live is itself the finding this run pins: under `fdb://` they
# live IN the cluster (the `ctl` subspace) — there is no `meta/api_keys.db` — so a
# key an earlier run minted is still there, "total keys" is a cluster-wide count
# that no assertion may pin, and the at-rest check reads the cluster's registry.
# View DEFINITIONS live in the cluster's `ctl` subspace under `fdb://` (there is
# no local `meta/views/*.layout`), so the "did the define land" probe reads the
# cluster listing and the derived name is per-run like the base — a definition
# outlives the run too. On SQLite the definition stays a local file.
set -u
cd "$(dirname "$0")/.."
Y=./target/release/grovefs
BACKEND="${APIKEY_BACKEND:-sqlite}"
TEAM=team
DEF=apr
. .build-bin/fdb_scaffold.sh
fdb_gate
fdb_binary
if [ "$BACKEND" = fdb ]; then TEAM="ak$(date +%s)"; DEF="${TEAM}_apr"; fi
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
if [ "$BACKEND" != fdb ]; then
    $Y init --view "$TEAM" --manifest .apitest/team.manifest > .apitest/init.log 2>&1 \
      && ok "seeded physical view '$TEAM'" || { bad "init failed: $(cat .apitest/init.log)"; exit 1; }
else
    # A subspace has no db file for `init --manifest` to write, so the seed goes
    # THROUGH one mount of it and the mount goes away again: nothing else in this
    # suite touches the tree, and the MCP server below reaches the cluster on its
    # own (--meta-store), exactly as the meta-server would.
    fdb_up .apitest/home "$TEAM" 127.0.0.1:15402 127.0.0.1:15403
    mkdir -p .apitest/mnt_seed
    # fdb_up chained an EXIT trap that reaps its server pair; extend it (do not
    # replace it) so an abort between here and line 66 also drops the seeding
    # mount, which would otherwise shadow `rm -rf .apitest` on the next run.
    trap 'fusermount -u .apitest/mnt_seed 2>/dev/null; fdb_reap; fdb_drop_views .apitest/home .apitest/home/data "${TEAM:-}"' EXIT
    $Y mount --view "$TEAM" -m .apitest/mnt_seed $REMOTE --foreground > .apitest/seed.log 2>&1 &
    SEEDP=$!
    mnt_gate .apitest/mnt_seed && ok "the subspace mounted for seeding" \
        || { bad ".apitest/mnt_seed never came up: $(cat .apitest/seed.log)"; exit 1; }
    seed_through_mount .apitest/mnt_seed .apitest/team.manifest \
        && ok "seeded physical view '$TEAM' (a subspace, through the mount)" \
        || { bad "seeding the subspace failed"; exit 1; }
    fusermount -u .apitest/mnt_seed 2>/dev/null; kp $SEEDP; wk $SEEDP; rmdir .apitest/mnt_seed 2>/dev/null
fi

# ---------------------------------------------------------------- CLI: create/list
echo "===== 1) grovefs apikey create + list (keys hashed at rest) ====="
COUT=$($Y apikey $MSARG --meta-dir "$BASE" create --label agentA 2>&1)
ID=$(echo "$COUT"   | sed -n 's/^  id:    //p')
KEY=$(echo "$COUT"  | sed -n 's/^  key:   //p')
SECRET=${KEY#*:}
if [ -n "$ID" ] && [ -n "$KEY" ] && [ "$KEY" = "$ID:$SECRET" ]; then
  ok "create minted + printed a full plaintext key once (id + '<id>:<secret>')"
else bad "create output unexpected: $COUT"; fi
LOUT=$($Y apikey $MSARG --meta-dir "$BASE" list 2>&1)
echo "$LOUT" | grep -qF "$ID" && echo "$LOUT" | grep -q "agentA" \
  && ok "list shows the new key by id + label" || bad "list missing key: $LOUT"
# --view filters by the view a key can OPEN: a team-bound key shows up under
# `--view team`, while the unbound (system) key is summarized separately
# rather than dumped into the per-view answer.
$Y apikey $MSARG --meta-dir "$BASE" create --label team-mount --for mount --view "$TEAM" > .apitest/ak_team.log 2>&1 \
  && ok "minted a view-bound key (--for mount --view of the base view)" || bad "view-bound create: $(cat .apitest/ak_team.log)"
VOUT=$($Y apikey $MSARG --meta-dir "$BASE" list --view "$TEAM" 2>&1)
if echo "$VOUT" | grep -q "team-mount" && ! echo "$VOUT" | grep -q "agentA"; then
  ok "list --view of the base view shows only the view-bound key"
else bad "view-filtered list wrong: $VOUT"; fi
echo "$VOUT" | grep -qE "1 bound key\\(s\\) for view '$TEAM' of [0-9]+ total" \
  && ok "filtered list reports bound/total counts" || bad "count line missing: $VOUT"
# The TOTAL is deliberately not pinned: the registry outlives the run, so a
# shared cluster (and a `--meta-store` store reused across suites) already holds
# every key earlier runs minted. "of 2 total" asserted a fresh store, not the
# filter — the filter itself is proven by the line above showing exactly 1 bound.
echo "$VOUT" | grep -qE "[0-9]+ unbound key\\(s\\) admit every view too" \
  && ok "unbound keys are summarized as admitting every view" || bad "unbound note missing: $VOUT"
$Y apikey $MSARG --meta-dir "$BASE" list > .apitest/ak_all.log 2>&1
grep -q "team-mount" .apitest/ak_all.log && grep -q "agentA" .apitest/ak_all.log \
  && ok "unfiltered list still shows every key" || bad "unfiltered list wrong: $(cat .apitest/ak_all.log)"
# The plaintext secret must NOT be recoverable from the store (hash only).
if [ "$BACKEND" = fdb ]; then
  # There is no api_keys.db to grep: the credential row lives in the cluster's
  # `ctl` subspace, and FDB may compress values, so grepping the data files
  # proves nothing either way. Two things ARE provable here — the local root
  # holds no plaintext copy, and a home that never minted anything still sees
  # the key (so the registry is the CLUSTER, not this root's files).
  if ! grep -rla -F -- "$SECRET" "$BASE" 2>/dev/null | grep -q .; then
    ok "no plaintext secret under the local root — an 'fdb://' store keeps the row in the ctl subspace"
  else bad "the plaintext secret is on disk under $BASE"; fi
  FRESH=$PWD/.apitest/freshhome; mkdir -p "$FRESH"
  $Y apikey $MSARG --meta-dir "$FRESH" list 2>&1 | grep -q "team-mount" \
    && ok "a home that minted nothing still lists the key — the registry is the cluster's, not a local file" \
    || bad "a fresh home does not see the key minted through --meta-store fdb://"
  [ ! -e "$BASE/meta/api_keys.db" ] && ok "no api_keys.db was created under the fdb root" \
    || bad "an api_keys.db appeared under an fdb:// root: $(ls -l "$BASE/meta/api_keys.db")"
else
  if [ -f "$BASE/meta/api_keys.db" ] && ! grep -a -q -F -- "$SECRET" "$BASE/meta/api_keys.db"; then
    ok "api_keys.db stores only the hash — the plaintext secret is not on disk"
  else bad "secret leaked to disk or db missing"; fi
fi

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
  printf '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"%s","base":"%s","rules":"hide /photos/private"}}}\n' "${5:-$DEF}" "$TEAM"
}
# (a) no key at all -> initialize stays open, but tools/list + tools/call refused.
frames "" > .apitest/in_nokey
T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key < .apitest/in_nokey > .apitest/out_nokey 2>/dev/null
if [ "$(fget .apitest/out_nokey 1 .result.serverInfo.name)" = "grovefs" ] \
   && [ "$(fget .apitest/out_nokey 2 .error.code)" = "-32001" ] \
   && [ "$(fget .apitest/out_nokey 3 .error.code)" = "-32001" ]; then
  ok "unauthenticated: initialize open, tools/list + tools/call refused (-32001)"
else bad "gate did not refuse: $(cat .apitest/out_nokey)"; fi
if [ "$BACKEND" = fdb ]; then
  # The refused define must not have reached the cluster's ctl subspace either.
  $Y view $MSARG --meta-dir "$BASE" list 2>/dev/null | grep -q "view '$DEF'" \
    && bad "refused define leaked a definition into the cluster" || ok "refused call wrote nothing (no '$DEF' in the cluster listing)"
else
  [ ! -f "$BASE/meta/views/$DEF.layout" ] && ok "refused call wrote nothing" || bad "refused define leaked a definition"
fi

# (b) right id, wrong secret -> refused, logged distinctly.
frames "$ID:deadbeefdeadbeef" > .apitest/in_bad
T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key < .apitest/in_bad > .apitest/out_bad 2>/dev/null
if [ "$(fget .apitest/out_bad 2 .error.code)" = "-32001" ]; then
  ok "a wrong secret does not authenticate (still refused)"
else bad "wrong secret admitted: $(cat .apitest/out_bad)"; fi

# (c) the real key in initialize -> admitted; a governance write succeeds.
# The admission checks below ask only "did tools/list come back with the
# registry" (length >= 10), never an exact count: the tool SET is pinned
# name-for-name by the `mcp` suite, so adding a tool must not silently break
# this suite — while a shrunken registry is still a failure here.
frames "$KEY" > .apitest/in_ok
T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key < .apitest/in_ok > .apitest/out_ok 2>/dev/null
if [ "$(fget .apitest/out_ok 2 '.result.tools|length>=10')" = "true" ] \
   && [ "$(fget .apitest/out_ok 4 .result.isError)" != "true" ]; then
  ok "initialize(apiKey) authenticates the session; tools/list + define admitted"
else bad "key not admitted: $(cat .apitest/out_ok)"; fi
if [ "$BACKEND" = fdb ]; then
  # The define lands in the cluster, not a local slot: ask the cluster listing
  # (a fresh home would see it too — the registry is the cluster's, not this root).
  $Y view $MSARG --meta-dir "$BASE" list 2>/dev/null | grep -q "view '$DEF'" \
    && ok "authenticated define wrote the definition (into the cluster's ctl subspace)" || bad "authenticated define did not land in the cluster"
else
  [ -f "$BASE/meta/views/$DEF.layout" ] && ok "authenticated define wrote the definition" || bad "authenticated define did not land"
fi

# -------------------------------------------------------------- audit-log checks
echo "===== 3) every call is logged + attributed (grovefs apikey calls) ====="
CALLS=$($Y apikey $MSARG --meta-dir "$BASE" calls --limit 60 2>&1)
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
ROUT=$($Y apikey $MSARG --meta-dir "$BASE" revoke --id "$ID" 2>&1)
echo "$ROUT" | grep -q "revoked id=" && ok "revoke by exact id succeeded" || bad "revoke: $ROUT"
# Revoking again reports already-revoked (idempotent, not an error).
echo "$($Y apikey $MSARG --meta-dir "$BASE" revoke --id "$ID" 2>&1)" | grep -q "already revoked" \
  && ok "re-revoking is a no-op reported as such" || bad "double revoke not handled"
# The revoked key can no longer authenticate (still refused, logged 'revoked').
frames "$KEY" apr2 > .apitest/in_rev
T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key < .apitest/in_rev > .apitest/out_rev 2>/dev/null
if [ "$(fget .apitest/out_rev 2 .error.code)" = "-32001" ]; then
  ok "revoked key is refused again (authentication fails closed)"
else bad "revoked key still admitted: $(cat .apitest/out_rev)"; fi
$Y apikey $MSARG --meta-dir "$BASE" calls --limit 60 2>&1 | grep -q "revoked" \
  && ok "the revoked-key attempt is audited (reason: revoked)" || bad "revoked attempt not logged"
# The row stays listed so historical calls keep attribution.
$Y apikey $MSARG --meta-dir "$BASE" list 2>&1 | grep -qF "$ID" \
  && ok "revoked key still appears in list (past calls keep attribution)" || bad "revoked row vanished"
# Unknown id -> a real error (non-zero exit).
if $Y apikey $MSARG --meta-dir "$BASE" revoke --id "nosuchkeyid" >/dev/null 2>&1; then
  bad "revoke of an unknown id should fail but exited 0"
else ok "revoke of an unknown id errors (non-zero)"; fi

# ------------------------------------------------------- CLI: id-prefix + env pre-auth
echo "===== 5) revoke by unique prefix + GROVEFS_MCP_API_KEY server pre-auth ====="
C2=$($Y apikey $MSARG --meta-dir "$BASE" create --label agentC 2>&1)
ID2=$(echo "$C2" | sed -n 's/^  id:    //p')
KEY2=$(echo "$C2" | sed -n 's/^  key:   //p')
PREFIX=${ID2:0:8}
$Y apikey $MSARG --meta-dir "$BASE" revoke --id "$PREFIX" 2>&1 | grep -q "revoked id='$ID2'" \
  && ok "revoke accepts a unique short id prefix (resolved to the full id)" \
  || bad "prefix revoke failed for prefix '$PREFIX'"
# A fresh ACTIVE key pre-authenticates the whole server via env — the client
# need not touch the handshake at all (for MCP clients that cannot customize it).
C3=$($Y apikey $MSARG --meta-dir "$BASE" create --label agentD 2>&1)
KEY3=$(echo "$C3" | sed -n 's/^  key:   //p')
frames "" > .apitest/in_env        # initialize WITHOUT a key
GROVEFS_MCP_API_KEY="$KEY3" T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key \
  < .apitest/in_env > .apitest/out_env 2>/dev/null
if [ "$(fget .apitest/out_env 2 '.result.tools|length>=10')" = "true" ]; then
  ok "GROVEFS_MCP_API_KEY pre-authenticates the server (handshake needs no key)"
else bad "env pre-auth did not admit tools/list: $(cat .apitest/out_env)"; fi

# ----------------------------------------------- default server is unchanged (no gate)
echo "===== 6) regression: plain 'grovefs mcp' (no --require-api-key) needs no key ====="
frames "" > .apitest/in_plain
T $Y mcp $MSARG --meta-dir "$BASE" < .apitest/in_plain > .apitest/out_plain 2>/dev/null
if [ "$(fget .apitest/out_plain 2 '.result.tools|length>=10')" = "true" ]; then
  ok "opt-in honored: without --require-api-key the tool surface stays open (logged, not gated)"
else bad "plain server unexpectedly gated: $(cat .apitest/out_plain)"; fi

# ---------------------------------------------------------------- CLI: set-ro
echo "===== 7) set-ro flips ro <-> rw in place (CLI plumbing) ====="
# Mint an `any`-scoped --ro key (this suite is the MCP plane; `--for mount` would
# be refused by SCOPE, which is orthogonal to the ro flag set-ro moves).
C4=$($Y apikey $MSARG --meta-dir "$BASE" create --label toggler --ro 2>&1)
ID4=$(echo "$C4" | sed -n 's/^  id:    //p')
KEY4=$(echo "$C4" | sed -n 's/^  key:   //p')
echo "$C4" | grep -q "^  rw:    ro (read-only)$" && ok "an --ro key (any scope) mints read-only" || bad "mint rw line: $C4"
$Y apikey $MSARG --meta-dir "$BASE" set-ro --id "$ID4" --rw 2>&1 | grep -q "is now rw (read-write)" \
  && ok "set-ro --rw reports the flip to read-write" || bad "set-ro --rw output wrong"
COL4=$($Y apikey $MSARG --meta-dir "$BASE" list 2>&1 | grep -F "$ID4" | awk '{print $7}')
[ "$COL4" = "rw" ] && ok "list's rw column now reads 'rw'" || bad "rw column: '$COL4'"
# The flipped key still authenticates the MCP plane (set-ro never touches
# liveness/scope; `ro` itself is a mount-plane dimension the MCP gate ignores).
frames "$KEY4" tog > .apitest/in_tog
T $Y mcp $MSARG --meta-dir "$BASE" --require-api-key < .apitest/in_tog > .apitest/out_tog 2>/dev/null
[ "$(fget .apitest/out_tog 2 '.result.tools|length>=10')" = "true" ] \
  && ok "the flipped key still authenticates (set-ro never breaks liveness)" || bad "flipped key lost auth: $(cat .apitest/out_tog)"
$Y apikey $MSARG --meta-dir "$BASE" set-ro --id "$ID4" --ro 2>&1 | grep -q "is now ro (read-only)" \
  && ok "explicit --ro flips back to read-only" || bad "set-ro --ro output wrong"
COL4B=$($Y apikey $MSARG --meta-dir "$BASE" list 2>&1 | grep -F "$ID4" | awk '{print $7}')
[ "$COL4B" = "ro" ] && ok "list's rw column reads 'ro' again" || bad "ro column: '$COL4B'"
if $Y apikey $MSARG --meta-dir "$BASE" set-ro --id "nosuchkeyid" >/dev/null 2>&1; then
  bad "set-ro of an unknown id should fail but exited 0"
else ok "set-ro of an unknown id errors (non-zero)"; fi

echo "SUMMARY: pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
