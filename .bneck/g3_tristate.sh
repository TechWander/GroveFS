#!/usr/bin/env bash
# P2's acceptance line, re-run against the current tree: every build shape must
# finish with zero warnings, and the default shape must pass its unit tests plus
# the two-backend differential (P6's "0 diff" criterion).
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"

state(){ # <label> <args...>
  local label=$1; shift
  local log=/tmp/g3build_$label.log
  cargo build --release "$@" > "$log" 2>&1
  local rc=$?
  echo "$label: EXIT=$rc warnings=$(grep -c 'warning' "$log")"
}

state plain
state nodefault --no-default-features --features remote-only

echo "--- cargo test --release (default features) ---"
PGHOST=127.0.0.1 PGPORT=5433 cargo test --release > /tmp/g3test_off.log 2>&1
echo "EXIT=$? warnings=$(grep -c 'warning' /tmp/g3test_off.log)"
grep -E "^test result" /tmp/g3test_off.log | sed 's/^/  /'
grep -E "differential|arms" /tmp/g3test_off.log | sed 's/^/  /'

# leave the tree with the default binary, the shape every suite uses
cargo build --release > /tmp/g3build_final.log 2>&1 && echo "final plain build: EXIT=0"
