#!/usr/bin/env bash
# Post-build.rs re-verification of the P2 criterion: all three build states must
# still compile with ZERO warnings, and the fdb-arm binary must now carry an
# RUNPATH so it no longer needs LD_LIBRARY_PATH at runtime.
set -uo pipefail
cd /data/grovefs/grovefs
eval "$(bash .build-bin/fdb_env.sh vars)" 2>/dev/null || true

count_warn() { grep -c '^warning' "$1" || true; }

run_state() {
  local name="$1"; shift
  local log="/data/grovefs/grovefs/.bneck/vsc_$name.log"
  echo "=== [$name] $*"
  if "$@" >"$log" 2>&1; then
    echo "    EXIT=0 warnings=$(count_warn "$log")"
  else
    echo "    EXIT=$? FAILED (log: $log)"
    tail -20 "$log"
  fi
}

run_state plain      cargo build --release
run_state remoteonly cargo build --release --no-default-features --features remote-only
run_state fdb        cargo build --release --features fdb --target-dir target/fdb
run_state fdbsample  cargo build --release --features fdb --target-dir target/fdb --example fdb_spike

BIN=target/fdb/release/grovefs
echo "=== RUNPATH check ($BIN)"
readelf -d "$BIN" 2>/dev/null | grep -E 'RPATH|RUNPATH|NEEDED.*fdb' || echo "    (no rpath entries found)"
echo "=== default binary must NOT reference fdb_c"
readelf -d target/release/grovefs 2>/dev/null | grep -E 'NEEDED.*(fdb_c)|RPATH|RUNPATH' || echo "    clean: no libfdb_c NEEDED, no RPATH/RUNPATH"
echo "=== done"
