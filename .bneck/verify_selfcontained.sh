#!/usr/bin/env bash
# Re-verification of the P2 criterion: every build state must still compile with
# ZERO warnings, and the released binary must carry no engine library of its own
# (no RPATH/RUNPATH, no third-party client .so) so it runs from a plain clone.
set -uo pipefail
cd /data/grovefs/grovefs

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

echo "=== default binary must carry no RPATH/RUNPATH (nothing outside the base OS to locate)"
readelf -d target/release/grovefs 2>/dev/null | grep -E 'RPATH|RUNPATH' || echo "    clean: no RPATH/RUNPATH"
echo "=== done"
