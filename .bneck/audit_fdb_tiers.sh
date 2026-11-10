#!/usr/bin/env bash
# P5/P8 parameterized-rerun criteria, re-executed against the current tree: the
# governance/migration suites are the same assertions as the SQLite tier with the
# backend swapped, which needs an fdb-feature binary.  Plain build restored last.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
export FDB_CLIENT_LIB_PATH=/data/grovefs/.fdbbin/lib
export GROVEFS_FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster
export PGHOST=127.0.0.1 PGPORT=5433

cargo build --release --features fdb > /tmp/audit_fdbfeat_build.log 2>&1
echo "feature build: EXIT=$? warnings=$(grep -c warning /tmp/audit_fdbfeat_build.log)"

run(){ # <label> <env-assign> <script>
  local label=$1 env=$2 script=$3
  echo
  echo "===== $label :: $script ====="
  env $env bash "$script" > "/tmp/audit_$label.log" 2>&1
  echo "EXIT=$? $(grep -E 'SUMMARY:' "/tmp/audit_$label.log" | tail -1)"
}

run locks    LOCKS_BACKEND=fdb    .build-bin/test_locks.sh
run layout   LAYOUT_BACKEND=fdb   .build-bin/test_layout.sh
run govcheck GOVCHECK_BACKEND=fdb .build-bin/test_govcheck.sh
run govtime  GOVTIME_BACKEND=fdb  .build-bin/test_govtime.sh
run apikey   APIKEY_BACKEND=fdb   .build-bin/test_apikey.sh
run migrate  MIGRATE_BACKEND=fdb  .build-bin/test_migrate.sh
run import   IMPORT_BACKEND=fdb   .build-bin/test_import.sh

echo
cargo build --release > /tmp/audit_plain_final.log 2>&1
echo "final plain build: EXIT=$? warnings=$(grep -c warning /tmp/audit_plain_final.log)"
echo "leftover grovefs mounts: $(mount | grep -c grovefs)"
echo "leftover meta-servers: $(pgrep -fc 'grovefs meta-server' 2>/dev/null || echo 0)"
