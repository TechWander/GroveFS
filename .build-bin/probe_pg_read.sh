#!/usr/bin/env bash
# A/B probe: PG read path end-to-end (stat-heavy load over a real mount).
#
# Seeds a PG-backed view with N files through one mount, then issues R lookups
# (`os.stat`) back-to-back and reports ops/s. Run it once per binary to compare
# the unnamed-statement path against the prepared-statement cache; the mount is
# the only moving part, the store and seed size are fixed.
#
#   BIN=./target/release/grovefs R=3000 bash .build-bin/probe_pg_read.sh
set -u
cd "$(dirname "$0")/.." || exit 1

BIN=${BIN:-./target/release/grovefs}
N=${N:-300}
R=${R:-3000}
ROOT=.probe_pgread
PGURI=${PGURI:-postgres://grove:grovepw@127.0.0.1:5433/grovefs}
PORT=$((16600 + RANDOM % 300))

cleanup() {
    fusermount -u "$ROOT/mnt" 2>/dev/null
    [ -n "${MPID:-}" ] && kill "$MPID" 2>/dev/null
    rm -rf "$ROOT"
}
trap cleanup EXIT

rm -rf "$ROOT"; mkdir -p "$ROOT/mnt"
VIEW="pread$RANDOM"
$BIN view --meta-store "$PGURI" create --view "$VIEW" >/dev/null || { echo VIEW_FAILED; exit 1; }

setsid -f "$BIN" meta-server --meta-store "$PGURI" --listen 127.0.0.1:$PORT --token t \
    </dev/null >"$ROOT/meta.log" 2>&1
sleep 1.2
MPID=$(pgrep -f "meta-server --meta-store $PGURI --listen 127.0.0.1:$PORT" | head -1)
[ -n "$MPID" ] || { echo SERVER_NOT_STARTED; tail -5 "$ROOT/meta.log"; exit 1; }

setsid -f "$BIN" mount --view "$VIEW" -m "$ROOT/mnt" --meta-server 127.0.0.1:$PORT --token t \
    --foreground --threads 4 --no-cache </dev/null >"$ROOT/mnt.log" 2>&1
sleep 1.5
ls "$ROOT/mnt" >/dev/null 2>&1 || { echo MOUNT_FAILED; tail -5 "$ROOT/mnt.log"; exit 1; }

R="$R" python3 - "$ROOT/mnt" "$N" <<'PY'
import os, sys, time
mnt, n = sys.argv[1], int(sys.argv[2])
r = int(os.environ["R"])
os.chdir(mnt)
for i in range(n):
    fd = os.open(f"f{i}", os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
names = [f"f{i}" for i in range(n)]

# warm the dentry/inode cache so the measurement is the server path, not the
# first-touch cold walk
for nm in names[:50]:
    os.stat(nm)

t0 = time.perf_counter()
for i in range(r):
    os.stat(names[i % n])
dt = time.perf_counter() - t0
print(f"BIN={os.environ.get('BIN_LABEL','?')} STATS={r} OVER={n} WALL_MS={dt*1000:.1f} "
      f"PER_OP_MS={dt/r*1000:.3f} OPS_S={r/dt:.1f}")
PY

$BIN view --meta-store "$PGURI" rm --name "$VIEW" >/dev/null 2>&1 || true
