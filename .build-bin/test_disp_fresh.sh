#!/usr/bin/env bash
# Display-coordinate freshness e2e: proves that a relocated entry under a
# SYNTHESIZED parent dir converges within the push-invalidation latency (~ms),
# NOT the ≤2s attr TTL. This exercises the `display_targets` enrichment path:
#   - the server translates source-coordinate events into derived display
#     coordinates and flushes the parent's cached listing.
# Scenario:
#   map /data/raw /team/raw  (derived view 'dview' over base 'alice')
#   B mounts dview, warms its /team listing (sees "raw" → dentry + dir page cached)
#   A removes source /data/raw
#   B re-lists /team within 500ms → must NOT show "raw" (test would FAIL if
#   the enrichment were missing, because without it the synthetic parent's
#   listing page remains stale until the 2s TTL expires).
#   A recreates /data/raw (dir)
#   B re-lists /team within 500ms → must show "raw" again.
# Assertion-based.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

META=127.0.0.1:15232
DATA=127.0.0.1:15233
rm -rf .dfresh; mkdir -p .dfresh/central .dfresh/mA .dfresh/mB .dfresh/mntA .dfresh/mntB

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- central services ----
$Y data-server --listen $DATA --data-dir .dfresh/central/data > .dfresh/data.log 2>&1 &
DP=$!
$Y meta-server --meta-dir .dfresh/central --listen $META > .dfresh/meta.log 2>&1 &
MP=$!
wait_port $DATA && wait_port $META || { echo "servers failed to start"; exit 1; }

# ---- seed the base view 'alice' with /data/raw as a real dir ----
# Strict view gate is the default: create the base view explicitly first (the
# derived definition below also requires its base tree to exist).
$Y view --meta-dir .dfresh/central create --view alice > .dfresh/create.log 2>&1
$Y --home .dfresh/mA mount --view alice --mountpoint .dfresh/mntA --meta-server $META --data-servers $DATA --foreground > .dfresh/a.log 2>&1 &
AP=$!
sleep 1
mkdir -p .dfresh/mntA/data/raw
printf 'payload' > .dfresh/mntA/data/raw/big.bin
sync .dfresh/mntA 2>/dev/null

# ---- define the derived view: map /data/raw -> /team/raw ----
mkdir -p .dfresh/central/meta/views
cat > .dfresh/central/meta/views/dview.layout <<'EOF'
base alice
map /data/raw /team/raw
EOF
# Give meta-server time to reload view definitions (mtime check per op)
sleep 0.5

# ---- B mounts the derived view 'dview' ----
$Y --home .dfresh/mB mount --view dview --mountpoint .dfresh/mntB --meta-server $META --data-servers $DATA --foreground > .dfresh/b.log 2>&1 &
BP=$!
sleep 2

cleanup(){ fusermount -u .dfresh/mntA 2>/dev/null; fusermount -u .dfresh/mntB 2>/dev/null; kill $AP $BP $DP $MP 2>/dev/null; }
trap cleanup EXIT

A=.dfresh/mntA
B=.dfresh/mntB
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

# ---------------------------------------------------------------------------
echo "===== 1) B's /team shows 'raw' initially ====="
ls_out=$(T ls "$B/team/" 2>/dev/null || echo "ERR")
if echo "$ls_out" | grep -qw "raw"; then
  ok "display listing of /team shows 'raw'"
else
  bad "display listing of /team should show 'raw' (got: $ls_out)"
fi

# Warm the page cache: B lists /team again so the kernel caches it.
T ls "$B/team/" >/dev/null 2>&1
sleep 0.3

# ---------------------------------------------------------------------------
echo "===== 2) A removes /data/raw (source) -> B /team loses 'raw' <500ms ====="
# rm -rf removes the dir AND its children
rm -rf "$A/data/raw"
sync "$A" 2>/dev/null
# Give enriched invalidation ≤500 ms (far less than the 2s TTL)
sleep 0.5

ls_out2=$(T ls "$B/team/" 2>/dev/null || echo "EMPTY")
if echo "$ls_out2" | grep -qw "raw"; then
  bad "B still sees 'raw' in /team after source removal (display enrichment NOT working)"
else
  ok "B's /team no longer shows 'raw' within 500ms (display-coordinate push works)"
fi

# ---------------------------------------------------------------------------
echo "===== 3) A recreates /data/raw -> B /team gains 'raw' <500ms ====="
mkdir -p "$A/data/raw"
printf 'new' > "$A/data/raw/small.bin"
sync "$A" 2>/dev/null
sleep 0.5

ls_out3=$(T ls "$B/team/" 2>/dev/null || echo "ERR")
if echo "$ls_out3" | grep -qw "raw"; then
  ok "B's /team shows 'raw' again within 500ms after source recreation"
else
  bad "B's /team does NOT show recreated 'raw' (got: $ls_out3)"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL DISPLAY-FRESHNESS CHECKS PASSED" || echo "SOME DISPLAY-FRESHNESS CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
