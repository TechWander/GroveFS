#!/usr/bin/env bash
# Who fsyncs? Count the meta-server's fsync/fdatasync on the e2e create path.
#
# Creates N files through a `meta-server` mount while strace (with -y, so each
# fd is annotated with its path) counts the server's fsync/fdatasync calls.
# 1:1 with ops means a per-op durable commit; ~1/s means only WAL checkpoints.
# Usage: N=300 bash .build-bin/probe_fsync.sh
set -u
cd "$(dirname "$0")/.." || exit 1

N=${N:-300}
ROOT=.probe_fsync
BIN=./target/release/grovefs
PORT=$((16200 + RANDOM % 500))

cleanup() {
    fusermount -u "$ROOT/mnt" 2>/dev/null
    [ -n "${MPID:-}" ] && kill "$MPID" 2>/dev/null
    rm -rf "$ROOT"
}
trap cleanup EXIT

rm -rf "$ROOT"; mkdir -p "$ROOT/home" "$ROOT/mnt"
"$BIN" view --meta-dir "$ROOT/home" create --view v1 >/dev/null || { echo VIEW_FAILED; exit 1; }

setsid -f strace -f -tt -y -e trace=fsync,fdatasync -o "$ROOT/fsync.trace" \
    "$BIN" meta-server --meta-dir "$ROOT/home" --listen 127.0.0.1:$PORT --token t \
    </dev/null >"$ROOT/meta.log" 2>&1
sleep 1.5
MPID=$(pgrep -f "meta-server --meta-dir $ROOT/home" | head -1)
[ -n "$MPID" ] || { echo SERVER_NOT_STARTED; tail -5 "$ROOT/meta.log"; exit 1; }
echo "port=$PORT meta-server pid=$MPID"

setsid -f "$BIN" mount --view v1 -m "$ROOT/mnt" --meta-server 127.0.0.1:$PORT --token t \
    --foreground --stats --threads 4 </dev/null >"$ROOT/mnt.log" 2>&1
sleep 1.5
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

sleep 0.3
echo "fsync/fdatasync lines: $(grep -cE 'fsync|fdatasync' "$ROOT/fsync.trace" 2>/dev/null || echo 0)"
echo "--- distinct targets ---"
grep -oE '[0-9]+<[^>]*>' "$ROOT/fsync.trace" 2>/dev/null | sed 's/^[0-9]*<//; s/>$//' | sort | uniq -c | sort -rn | head
echo "--- last 3 ---"
grep -E 'fsync|fdatasync' "$ROOT/fsync.trace" 2>/dev/null | tail -3
