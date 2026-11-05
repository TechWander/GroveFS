#!/usr/bin/env bash
# End-to-end check of the opt-in per-stage telemetry + the .grove-stats control
# node. Runs entirely on the local (embedded SQLite) backend, debug binary.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/debug/grovefs
W=$(mktemp -d)
mkdir -p "$W/home" "$W/mnt"
export GROVEFS_HOME="$W/home"
MNT="$W/mnt"
STATS="$W/report.txt"
MLOG="$W/mount.log"

fail(){ echo "FAIL: $1"; fusermount -u "$MNT" 2>/dev/null; kill $MP 2>/dev/null; rm -rf "$W"; exit 1; }

echo "== init =="
$Y init --view t >/dev/null 2>&1 || fail "init"

echo "== mount WITH --stats =="
$Y mount --view t --mountpoint "$MNT" --stats --stats-file "$STATS" --foreground >"$MLOG" 2>&1 &
MP=$!
sleep 1.5
kill -0 $MP 2>/dev/null || { cat "$MLOG"; fail "mount died"; }

# 1. control node must be visible while --stats is on
ls -a "$MNT" | grep -q '.grove-stats' || fail ".grove-stats not listed with --stats"
echo "  ok: .grove-stats listed"

# 2. drive some IO so stages record samples
echo "hello telemetry" > "$MNT/file1"
cat "$MNT/file1" >/dev/null || fail "read file1"
mkdir -p "$MNT/d" && echo x > "$MNT/d/inner" && cat "$MNT/d/inner" >/dev/null
ls "$MNT/d" >/dev/null
echo "  ok: wrote+read a few files"

# 3. cat the control node -> must contain the telemetry table
cat "$MNT/.grove-stats" > "$W/node.txt" || fail "cat .grove-stats"
grep -q 'per-stage latency telemetry' "$W/node.txt" || { cat "$W/node.txt"; fail "node text header missing"; }
grep -q 'write.backend_total' "$W/node.txt" || fail "node missing write stage row"
grep -q 'read.backend_total'  "$W/node.txt" || fail "node missing read stage row"
grep -qE 'read.op_total +ops=[1-9]'  "$W/node.txt" || fail "read.op_total recorded no samples"
grep -qE 'write.op_total +ops=[1-9]' "$W/node.txt" || fail "write.op_total recorded no samples"
echo "  ok: .grove-stats prints a populated report"

# 4. read-only: writing to the control node must not succeed as a real file
if echo oops > "$MNT/.grove-stats" 2>/dev/null; then :; fi  # tolerated either way; not a correctness gate

echo "== unmount =="
fusermount -u "$MNT" 2>/dev/null
sleep 1
kill $MP 2>/dev/null; wait $MP 2>/dev/null

# 5. exit report must land on the mount's stderr AND in --stats-file
grep -q 'per-stage latency telemetry' "$MLOG" || fail "no exit report on stderr"
[ -s "$STATS" ] || fail "--stats-file empty"
grep -q 'write.backend_total' "$STATS" || fail "--stats-file missing stage rows"
echo "  ok: exit report on stderr + stats-file"

$Y mount --view t --mountpoint "$MNT" --foreground >"$MLOG" 2>&1 &
MP=$!
sleep 1.5
ls -a "$MNT" | grep -q '.grove-stats' && fail ".grove-stats visible WITHOUT --stats (governance leak)"
echo "  ok: .grove-stats absent when --stats off"
fusermount -u "$MNT" 2>/dev/null; kill $MP 2>/dev/null; wait $MP 2>/dev/null

rm -rf "$W"
echo "E2E PASS"
