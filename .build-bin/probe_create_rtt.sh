#!/usr/bin/env bash
# How many metadata round trips does one create cost?
#
# Mounts a view through a `meta-server` (so every metadata op is a TCP RTT) and
# runs N sequential O_CREAT|O_WRONLY opens. The mount's `--stats` report at
# unmount carries `remeta.rpc ops=<n>`, so ops/N is the RTTs per create.
# A create is expected to be LOOKUP + CREATE (+ GETATTR before the reply
# carried the row), i.e. 3, falling to 2 when the server inlines the node.
#
# Usage: N=300 bash .build-bin/probe_create_rtt.sh
set -u
cd "$(dirname "$0")/.." || exit 1

N=${N:-300}
ROOT=.probe_create_rtt
BIN=./target/release/grovefs
PORT=$((16700 + RANDOM % 500))

cleanup() {
    fusermount -u "$ROOT/mnt" 2>/dev/null
    [ -n "${MPID:-}" ] && kill "$MPID" 2>/dev/null
    rm -rf "$ROOT"
}
trap cleanup EXIT

rm -rf "$ROOT"; mkdir -p "$ROOT/home" "$ROOT/mnt"
"$BIN" view --meta-dir "$ROOT/home" create --view v1 >/dev/null || { echo VIEW_FAILED; exit 1; }

"$BIN" meta-server --meta-dir "$ROOT/home" --listen 127.0.0.1:$PORT --token t \
    </dev/null >"$ROOT/meta.log" 2>&1 &
MPID=$!
sleep 1.5
kill -0 "$MPID" 2>/dev/null || { echo SERVER_NOT_STARTED; tail -5 "$ROOT/meta.log"; exit 1; }
echo "port=$PORT meta-server pid=$MPID"

"$BIN" mount --view v1 -m "$ROOT/mnt" --meta-server 127.0.0.1:$PORT --token t \
    --foreground --stats --threads 4 </dev/null >"$ROOT/mnt.log" 2>&1 &
sleep 1.5
# Fail fast: a dead mount silently turns every "create" into a write in the bare
# directory, which would report a fake multi-thousand ops/s.
ls "$ROOT/mnt" >/dev/null 2>&1 || { echo MOUNT_FAILED; tail -5 "$ROOT/mnt.log"; exit 1; }

python3 - "$ROOT/mnt" "$N" <<'PY'
import os, sys, time
mnt, n = sys.argv[1], int(sys.argv[2])
os.chdir(mnt)
t0 = time.perf_counter()
for i in range(n):
    fd = os.open(f"f{i}", os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
dt = time.perf_counter() - t0
print(f"CREATES={n} WALL_MS={dt*1000:.1f} PER_OP_MS={dt/n*1000:.3f} OPS_S={n/dt:.1f}")
PY

# Unmount cleanly so the mount prints its final stats report to mnt.log.
fusermount -u "$ROOT/mnt" 2>/dev/null
sleep 1.5
line=$(grep -E 'remeta\.rpc' "$ROOT/mnt.log" | head -1)
echo "stats: ${line:-<no remeta.rpc line>}"
ops=$(printf '%s' "$line" | sed -n 's/.*ops=\([0-9][0-9]*\).*/\1/p')
if [ -n "${ops:-}" ]; then
    python3 -c "print(f'RPC_PER_CREATE={$ops/$N:.3f}')"
else
    echo RPC_PER_CREATE=unknown
fi
