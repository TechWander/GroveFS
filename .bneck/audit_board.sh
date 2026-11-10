#!/usr/bin/env bash
# Final audit board for the current tree: the full gating board first (nothing
# else may run while it does), then the experimental arm selected by name so the
# P6 criterion "run_all.sh fdb is separately runnable" is backed by an execution.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
export FDB_CLIENT_LIB_PATH=/data/grovefs/.fdbbin/lib
export GROVEFS_FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster

echo "===== BOARD: PGPORT=5433 run_all.sh ====="
PGPORT=5433 bash .build-bin/run_all.sh
echo "BOARD_EXIT=$?"

echo
echo "===== BOARD: run_all.sh fdb (by name) ====="
PGPORT=5433 bash .build-bin/run_all.sh fdb
echo "FDB_EXIT=$?"
