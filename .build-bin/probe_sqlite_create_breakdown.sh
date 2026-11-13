#!/usr/bin/env bash
# What is inside one SQLite create op?
#
# Local view (no network), `--stats` on, three arms measured on the *same*
# fresh view so the numbers are comparable:
#
#   file   : os.open(O_CREAT|O_WRONLY)      -> backend object + node row + ledger
#   mkdir  : os.mkdir()                     -> node row only (no data_ref, no ledger)
#   touch  : os.utime(existing file)        -> one UPDATE, no INSERT (setattr path)
#
# `meta.create` from the stats report is the metadata transaction; the gap to
# the per-op wall time is FUSE dispatch + kernel LOOKUP + backend object create.
# file-minus-mkdir isolates the first-name `COUNT` + `ledger_add` + data_ref.
#
# Usage: N=2000 bash .build-bin/probe_sqlite_create_breakdown.sh
set -u
cd "$(dirname "$0")/.." || exit 1

N=${N:-2000}
ROOT=.probe_sqlite_breakdown
BIN=./target/release/grovefs
export LC_ALL=C

cleanup() {
    fusermount -u "$ROOT/mnt" 2>/dev/null
    [ -n "${MPID:-}" ] && kill "$MPID" 2>/dev/null
    rm -rf "$ROOT"
}
trap cleanup EXIT

rm -rf "$ROOT"; mkdir -p "$ROOT/home" "$ROOT/mnt"
"$BIN" view --home "$ROOT/home" create --view v1 >/dev/null || { echo VIEW_FAILED; exit 1; }

# arm <name> <python-body>
arm() {
    local name=$1 body=$2
    local log="$ROOT/mnt-$name.log"
    "$BIN" mount --view v1 -m "$ROOT/mnt" --home "$ROOT/home" \
        --foreground --stats --threads 4 </dev/null >"$log" 2>&1 &
    MPID=$!
    sleep 1.5
    kill -0 "$MPID" 2>/dev/null || { echo "MOUNT_FAILED arm=$name"; tail -3 "$log"; exit 1; }
    ls "$ROOT/mnt" >/dev/null 2>&1 || { echo "MOUNT_FAILED arm=$name"; tail -3 "$log"; exit 1; }
    python3 - "$ROOT/mnt" "$N" "$body" <<'PY'
import os, sys, time
mnt, n, body = sys.argv[1], int(sys.argv[2]), sys.argv[3]
os.makedirs(os.path.join(mnt, "d"), exist_ok=True)
os.chdir(os.path.join(mnt, "d"))
if body == "touch":
    with open("t", "w"):
        pass
t0 = time.perf_counter()
for i in range(n):
    if body == "file":
        fd = os.open(f"f{i}", os.O_CREAT | os.O_WRONLY, 0o644)
        os.close(fd)
    elif body == "mkdir":
        os.mkdir(f"d{i}")
    else:
        os.utime("t", (1700000000 + i, 1700000000 + i))
dt = time.perf_counter() - t0
print(f"{body}: N={n} WALL_MS={dt*1000:.1f} PER_OP_MS={dt/n*1000:.3f} OPS_S={n/dt:.1f}")
PY
    fusermount -u "$ROOT/mnt" 2>/dev/null
    sleep 1.2
    echo "-- stats[$name] (µs) --"
    grep -E '^  (meta\.create|meta\.setattr|sqlite\.[a-z_]+|op_total|backend_total|write\.backend_total)' "$log" | sed 's/^  //'
}

arm file file
arm mkdir mkdir
arm touch touch
