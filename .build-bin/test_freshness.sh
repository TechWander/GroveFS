#!/usr/bin/env bash
# Non-strict cross-client FRESHNESS e2e (L3). This suite proves that the
# existing push-based invalidation model (EventBus + inval_inode/inval_entry)
# already gives immediate visibility between clients sharing a `meta-server`,
# WITHOUT `--strict`. The `--strict` flag (FOPEN_DIRECT_IO + zero attr TTL) is
# a conservative fallback for the writeback-cache race window; it is NOT
# required for the normal "A writes → B reads fresh data" scenario.
#
# Scenarios:
#   1) Sequential append visibility:
#      A writes 100 lines to shared.log; B reads within 500ms of A's last
#      sync and must see all 100 lines.
#   2) Truncate visibility:
#      A truncates shared.log to 0 bytes; B must immediately see an empty file
#      (proves EV_INODE attr change propagates size updates).
#   3) Reverse-read-after-append:
#      A opens a file for reading; B appends to the same file; A re-reads and
#      must get B's new bytes (proves inval_inode drops A's stale page cache).
#
# Assertion-based (pass/fail counters + real exit code).
set -u
cd /data/xy/yourfs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/yourfs/.build-bin/pkg-config
Y=./target/release/yourfs

META=127.0.0.1:50156
DATA=127.0.0.1:50157
rm -rf .fresh; mkdir -p .fresh/central .fresh/m1 .fresh/m2

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- central services ----
$Y data-server --listen $DATA --root .fresh/central/data > .fresh/data.log 2>&1 &
DP=$!
$Y --home .fresh/central meta-server --listen $META > .fresh/meta.log 2>&1 &
MP=$!
wait_port $DATA && wait_port $META || { echo "servers failed"; exit 1; }

# ---- two NON-STRICT client mounts of the same user ----
$Y --home .fresh/m1 mount --user alice --mountpoint .fresh/mnt_a --meta $META --data $DATA > .fresh/a.log 2>&1 &
AP=$!
$Y --home .fresh/m2 mount --user alice --mountpoint .fresh/mnt_b --meta $META --data $DATA > .fresh/b.log 2>&1 &
BP=$!
sleep 2

cleanup(){ fusermount -u .fresh/mnt_a 2>/dev/null; fusermount -u .fresh/mnt_b 2>/dev/null; kill $AP $BP $DP $MP 2>/dev/null; }
trap cleanup EXIT

A=.fresh/mnt_a
B=.fresh/mnt_b
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

# ---------------------------------------------------------------------------
echo "===== 1) Sequential append: 100 lines visible to peer within 500ms ====="
# Seed the file so both mounts have it cached.
printf '' > "$A/shared.log"
sync "$A" 2>/dev/null; sync "$B" 2>/dev/null
sleep 0.3

for i in $(seq 1 100); do
  printf 'line_%d\n' "$i" >> "$A/shared.log"
  if [ $((i % 10)) -eq 0 ]; then sync "$A" 2>/dev/null; fi
done
sync "$A" 2>/dev/null
# Give invalidation events up to 500ms to propagate.
sleep 0.5

lines_b=$(T cat "$B/shared.log" | wc -l)
chk "$lines_b" "100" "B sees all 100 lines appended by A (non-strict, push invalidation)"

# ---------------------------------------------------------------------------
echo "===== 2) Truncate to zero: peer immediately sees size=0 ====="
: > "$A/shared.log"
sync "$A" 2>/dev/null
sleep 0.3

size_b=$(T stat --format='%s' "$B/shared.log" 2>/dev/null || echo "-1")
chk "$size_b" "0" "B sees truncated file (size=0) after A's :> (no --strict)"

# ---------------------------------------------------------------------------
echo "===== 3) Reverse-read-after-append: A sees B's new bytes ====="
printf 'original' > "$A/rbase.txt"
sync "$A" 2>/dev/null; sleep 0.3

# A reads once (caches the page).
a_first=$(T cat "$A/rbase.txt")
chk "$a_first" "original" "A reads its own write correctly"

# B appends new data.
printf '_from_B' >> "$B/rbase.txt"
sync "$B" 2>/dev/null
sleep 0.5

# A re-reads; inval_inode should have dropped A's cached page.
a_second=$(T cat "$A/rbase.txt")
chk "$a_second" "original_from_B" "A sees B's appended bytes after push invalidation"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL FRESHNESS CHECKS PASSED" || echo "SOME FRESHNESS CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
