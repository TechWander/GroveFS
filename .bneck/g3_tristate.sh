#!/usr/bin/env bash
# P2's acceptance line, re-run against the current tree: three build shapes, each
# must finish with zero warnings, and the fdb arm must also pass its unit tests
# plus the three-arm differential (P6's "0 diff" criterion).
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
export FDB_CLIENT_LIB_PATH=/data/grovefs/.fdbbin/lib

state(){ # <label> <args...>
  local label=$1; shift
  local log=/tmp/g3build_$label.log
  cargo build --release "$@" > "$log" 2>&1
  local rc=$?
  echo "$label: EXIT=$rc warnings=$(grep -c 'warning' "$log")"
}

state plain
state nodefault --no-default-features --features remote-only
state fdbfeat --features fdb

echo "--- cargo test --release (feature off) ---"
cargo test --release > /tmp/g3test_off.log 2>&1
echo "EXIT=$? warnings=$(grep -c 'warning' /tmp/g3test_off.log)"
grep -E "^test result" /tmp/g3test_off.log | sed 's/^/  /'

echo "--- cargo test --release --features fdb (unit + differential) ---"
GROVEFS_FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
PGHOST=127.0.0.1 PGPORT=5433 cargo test --release --features fdb > /tmp/g3test_fdb.log 2>&1
echo "EXIT=$? warnings=$(grep -c 'warning' /tmp/g3test_fdb.log)"
grep -E "^test result" /tmp/g3test_fdb.log | sed 's/^/  /'
grep -E "three-way differential|arms" /tmp/g3test_fdb.log | sed 's/^/  /'

# leave the tree with the default (feature-off) binary, the shape every suite uses
cargo build --release > /tmp/g3build_final.log 2>&1 && echo "final plain build: EXIT=0"
