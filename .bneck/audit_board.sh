#!/usr/bin/env bash
# Final audit board for the current tree: the full gating board. Everything the
# board runs is gating — there is no separately selectable experimental arm.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"

echo "===== BOARD: PGPORT=5433 run_all.sh ====="
PGPORT=5433 bash .build-bin/run_all.sh
echo "BOARD_EXIT=$?"
