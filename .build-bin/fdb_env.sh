#!/usr/bin/env bash
# FoundationDB single-node dev/test cluster for the GroveFS FDB metadata backend.
#
# Why this script exists instead of `apt install foundationdb-*`:
#   this host has no FDB apt source, no reachable docker daemon, and
#   pkg.foundationdb.org is blocked. What IS reachable is the GitHub release
#   asset endpoint, which publishes the very same official build artifacts
#   (fdbserver / fdbcli / fdbmonitor / libfdb_c) per-version, each with a
#   published .sha256. So the environment path chosen for P0 is:
#       download release assets -> verify sha256 -> run fdbserver in user space
#   No root, no system package, and the C header comes from the Rust crate
#   itself (`embedded-fdb-include`), so nothing is installed under /usr.
#
# Usage:
#   bash .build-bin/fdb_env.sh fetch     # download + sha256-verify the assets
#   bash .build-bin/fdb_env.sh up        # start fdbserver, init db if new
#   bash .build-bin/fdb_env.sh status    # `status minimal`
#   bash .build-bin/fdb_env.sh down      # stop the server
#   bash .build-bin/fdb_env.sh vars      # export lines for cargo / grovefs
#   eval "$(bash .build-bin/fdb_env.sh vars)"   # apply them to this shell
#
# Everything lives under $FDB_HOME (default /data/grovefs/.fdbbin) and
# $FDB_RUN_DIR (default /data/grovefs/.fdbtest); both are gitignored.
set -euo pipefail

FDB_VERSION="${FDB_VERSION:-7.1.67}"
FDB_API="${FDB_API:-710}"
FDB_HOME="${FDB_HOME:-/data/grovefs/.fdbbin}"
FDB_RUN_DIR="${FDB_RUN_DIR:-/data/grovefs/.fdbtest}"
FDB_PORT="${FDB_PORT:-4500}"
CLUSTER="$FDB_RUN_DIR/test.cluster"
BASE="https://github.com/apple/foundationdb/releases/download/$FDB_VERSION"
# `single` = one coordinator, replication 1; `ssd` = the production storage
# engine (redwood). This is the "兜底档" from the plan: functionally complete,
# but with NO redundancy, so no durability/HA/performance claim may be written
# into user docs from a cluster started here.
FDB_ENGINE="${FDB_ENGINE:-ssd}"

SERVER="$FDB_HOME/fdbserver.x86_64"
CLI="$FDB_HOME/fdbcli.x86_64"
MON="$FDB_HOME/fdbmonitor.x86_64"
LIB="$FDB_HOME/libfdb_c.x86_64.so"

log() { printf '=== %s\n' "$*"; }

# fetch: download each asset with resume, then require a sha256 match. A missing
# or mismatched checksum is fatal — this feeds both the test cluster and the
# linked client library, so a silently truncated .so would be very confusing.
cmd_fetch() {
  mkdir -p "$FDB_HOME/lib"
  local f rc want got
  for f in libfdb_c.x86_64.so fdbserver.x86_64 fdbcli.x86_64 fdbmonitor.x86_64; do
    rc=1
    for _ in 1 2 3 4 5 6 7 8; do
      if [ -s "$FDB_HOME/$f" ]; then
        curl -fL --retry 3 --retry-delay 2 -C - -o "$FDB_HOME/$f" "$BASE/$f" && { rc=0; break; }
      else
        curl -fL --retry 3 --retry-delay 2 -o "$FDB_HOME/$f" "$BASE/$f" && { rc=0; break; }
      fi
      sleep 2
    done
    [ "$rc" = 0 ] || { echo "fetch failed: $f" >&2; return 1; }
    curl -fsL -o "$FDB_HOME/$f.sha256" "$BASE/$f.sha256" || true
    if [ -s "$FDB_HOME/$f.sha256" ]; then
      want=$(awk '{print $1}' "$FDB_HOME/$f.sha256")
      got=$(sha256sum "$FDB_HOME/$f" | awk '{print $1}')
      [ "$want" = "$got" ] || { echo "SHA256 MISMATCH $f want=$want got=$got" >&2; return 1; }
      printf 'ok  %-24s %s\n' "$f" "$got"
    else
      echo "no published checksum for $f — refusing to use it" >&2
      return 1
    fi
  done
  chmod +x "$SERVER" "$CLI" "$MON"
  ln -sf "$LIB" "$FDB_HOME/lib/libfdb_c.so"
  log "assets complete under $FDB_HOME"
}

require_assets() {
  [ -x "$SERVER" ] && [ -x "$CLI" ] && [ -e "$FDB_HOME/lib/libfdb_c.so" ] || {
    echo "assets missing; run: bash .build-bin/fdb_env.sh fetch" >&2
    return 3
  }
}

cmd_up() {
  require_assets
  mkdir -p "$FDB_RUN_DIR/data" "$FDB_RUN_DIR/logs"
  if [ ! -f "$CLUSTER" ]; then
    printf 'grovefs:devpass1@127.0.0.1:%s\n' "$FDB_PORT" > "$CLUSTER"
    log "wrote cluster file $CLUSTER"
  fi
  if pgrep -f "fdbserver.x86_64.*$FDB_PORT" >/dev/null 2>&1; then
    log "already running"
  else
    # Detached start: setsid + nohup + redirect, otherwise the ssh/tool session
    # taking it down with it is a live hazard (see the "经 ssh 启动后台守护进程"
    # lesson — same failure mode applies to any non-interactive parent).
    setsid nohup "$SERVER" \
      --cluster-file "$CLUSTER" \
      -d "$FDB_RUN_DIR/data" -L "$FDB_RUN_DIR/logs" \
      -l "127.0.0.1:$FDB_PORT" -p "127.0.0.1:$FDB_PORT" \
      -m 2000000 > "$FDB_RUN_DIR/server.out" 2>&1 &
    local i
    for i in $(seq 1 30); do
      sleep 1
      grep -q "FDBD joined cluster" "$FDB_RUN_DIR/server.out" 2>/dev/null && break
    done
    log "fdbserver pid=$(pgrep -f "fdbserver.x86_64.*$FDB_PORT" | head -1)"
  fi
  # A brand-new cluster reports "unavailable" until it is configured once.
  if ! "$CLI" -C "$CLUSTER" --exec "status minimal" 2>&1 | grep -q "available"; then
    log "configuring new database (single/$FDB_ENGINE)"
    "$CLI" -C "$CLUSTER" --exec "configure new single $FDB_ENGINE" || true
  fi
  # Readiness is proven by `fdbcli`, not by the pid — that is the acceptance
  # command in the plan, so it is also the gate here.
  for _ in $(seq 1 20); do
    if "$CLI" -C "$CLUSTER" --exec "status minimal" 2>&1 | grep -q "The database is available"; then
      log "The database is available."
      return 0
    fi
    sleep 1
  done
  echo "database did not become available; see $FDB_RUN_DIR/server.out" >&2
  tail -20 "$FDB_RUN_DIR/server.out" >&2 || true
  return 1
}

cmd_status() {
  require_assets
  "$CLI" -C "$CLUSTER" --exec "status minimal"
}

cmd_down() {
  pkill -f "fdbserver.x86_64.*$FDB_PORT" 2>/dev/null || true
  sleep 1
  pgrep -f "fdbserver.x86_64.*$FDB_PORT" >/dev/null 2>&1 \
    && echo "still running" >&2 && return 1
  log "stopped"
}

# vars: the environment a build/test run wants. Two of these are NOT the same
# kind of thing:
#   FDB_CLIENT_LIB_PATH  BUILD time — foundationdb-sys' link search path, and
#                        since `build.rs` exists it is also baked into the ELF as
#                        RUNPATH (together with $ORIGIN/lib and $ORIGIN/../lib).
#   LD_LIBRARY_PATH      no longer required at runtime for a binary built from
#                        this tree; kept so a `$GROVEFS_FDB_BIN` from before that
#                        change (or a hand-placed .so) still resolves.
cmd_vars() {
  cat <<EOF
export GROVEFS_FDB_CLUSTER_FILE=$CLUSTER
export FDB_CLIENT_LIB_PATH=$FDB_HOME/lib
export LD_LIBRARY_PATH=$FDB_HOME/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}
export GROVEFS_META_STORE=fdb://\$CLUSTER
EOF
}

case "${1:-up}" in
  fetch)  cmd_fetch ;;
  up)     cmd_up ;;
  status) cmd_status ;;
  down)   cmd_down ;;
  vars)   cmd_vars ;;
  *)      echo "usage: $0 {fetch|up|status|down|vars}" >&2; exit 2 ;;
esac
