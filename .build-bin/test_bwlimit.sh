#!/usr/bin/env bash
# Per-mount bandwidth shaping e2e (client-side token bucket in the FUSE read /
# write path). Covers what the feature promises:
#   1) `--bw-read N` caps sequential-read throughput: 24 MiB at 8 MiB/s with a
#      1 s burst leaves ~16 MiB of debt (~2 s), so a capped read must NOT be
#      instant — the deficit is slept, not dropped, and no bytes go missing;
#   2) an unlimited mount of the SAME view is materially faster on the same
#      data — the knob, not the disk, is the difference;
#   3) `--bw-write N` caps streaming writes (conv=fsync forces the dirty pages
#      through the throttled FUSE write path before dd returns), and the
#      throttled bytes are intact (cmp against the source);
#   4) env fallback `GROVEFS_BW_READ=8M` caps exactly like the flag;
#   5) `--bw-read 0` means unlimited (mount accepted, read not slowed);
#   6) an invalid rate is REFUSED at parse time with a clear message —
#      silently ignoring a cap the operator believes is on is worse;
#   7) a read-only mount accepts `--bw-write` but still refuses writes with
#      EROFS: the budget is never spent because the write never happens.
#
# Timing assertions use LOWER bounds only. A missing throttle clocks ~0.2 s
# and fails the 1.4 s floor; a slow CI disk only adds time, never subtracts.
# Assertion-based (pass/fail counters + real exit code).
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
B=.bwtest
rm -rf $B
mkdir -p $B/home $B/mnt_cap $B/mnt_free $B/mnt_env $B/mnt_zero $B/mnt_ro
export GROVEFS_HOME=$PWD/$B/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
nowms(){ date +%s%3N; }
# Time `dd` between two calls; sets MS. $1=src $2=dst $3..=extra dd args.
timed_dd(){ local t0 t1; t0=$(nowms); "$@" 2>/dev/null; local rc=$?; t1=$(nowms); MS=$((t1-t0)); return $rc; }

cleanup(){
  for m in mnt_cap mnt_free mnt_env mnt_zero mnt_ro; do
    $Y umount -m $B/$m > /dev/null 2>&1 || fusermount -u $B/$m 2>/dev/null
  done
  pkill -f "$B/" 2>/dev/null
}
trap cleanup EXIT

echo "===== 1) seed a view and open capped / unlimited / env / zero / RO mounts ====="
cat > $B/bw.manifest <<'EOF'
mkdir seed
touch seed/keep.bin
EOF
$Y init --view bw --manifest $B/bw.manifest > $B/init.log 2>&1
[ $? -eq 0 ] && ok "init --view bw" || bad "init failed: $(cat $B/init.log)"

$Y mount --view bw -m $B/mnt_cap  --bw-read 8M --bw-write 8M > $B/cap.log 2>&1
$Y mount --view bw -m $B/mnt_free > $B/free.log 2>&1
GROVEFS_BW_READ=8M $Y mount --view bw -m $B/mnt_env > $B/env.log 2>&1
$Y mount --view bw -m $B/mnt_zero --bw-read 0 > $B/zero.log 2>&1
$Y mount --view bw -m $B/mnt_ro --ro --bw-write 8M > $B/ro.log 2>&1
live=0
for m in mnt_cap mnt_free mnt_env mnt_zero mnt_ro; do mountpoint -q $B/$m && live=$((live+1)); done
[ "$live" -eq 5 ] && ok "five mounts live (capped / unlimited / env / zero / read-only)" \
  || bad "only $live/5 mounts live (see $B/*.log)"

echo "===== 2) --bw-read 8M caps a 24 MiB sequential read ====="
head -c 25165824 /dev/urandom > $B/src.bin
timed_dd dd if=$B/src.bin of=$B/mnt_free/big.bin bs=1M conv=fsync status=none \
  && ok "seeded 24 MiB through the unlimited mount" || bad "seed write failed"

timed_dd dd if=$B/mnt_cap/big.bin of=/dev/null bs=1M status=none
[ $? -eq 0 ] && ok "capped read completed (no dropped/errored bytes)" || bad "capped read failed"
[ "$MS" -ge 1400 ] && ok "capped read took ${MS}ms (>=1400ms floor — bytes are paced)" \
  || bad "capped read took only ${MS}ms — the read cap is not enforced"
CAP_READ=$MS

timed_dd dd if=$B/mnt_free/big.bin of=/dev/null bs=1M status=none
FREE_READ=$MS
[ "$FREE_READ" -lt $((CAP_READ - 300)) ] \
  && ok "unlimited read ${FREE_READ}ms vs capped ${CAP_READ}ms — the cap is the difference" \
  || bad "unlimited read ${FREE_READ}ms not materially faster than capped ${CAP_READ}ms"

echo "===== 3) --bw-write 8M caps a 24 MiB streaming write ====="
timed_dd dd if=$B/src.bin of=$B/mnt_cap/w.bin bs=1M conv=fsync status=none
[ $? -eq 0 ] && ok "capped write completed" || bad "capped write failed"
[ "$MS" -ge 1400 ] && ok "capped write took ${MS}ms (>=1400ms floor)" \
  || bad "capped write took only ${MS}ms — the write cap is not enforced"
CAP_WRITE=$MS
cmp -s $B/src.bin $B/mnt_cap/w.bin && ok "throttled bytes are intact (cmp vs source)" \
  || bad "throttled write corrupted the file"

timed_dd dd if=$B/src.bin of=$B/mnt_free/free.bin bs=1M conv=fsync status=none
FREE_WRITE=$MS
[ "$FREE_WRITE" -lt $((CAP_WRITE - 300)) ] \
  && ok "unlimited write ${FREE_WRITE}ms vs capped ${CAP_WRITE}ms" \
  || bad "unlimited write ${FREE_WRITE}ms not materially faster than capped ${CAP_WRITE}ms"

echo "===== 4) env fallback GROVEFS_BW_READ=8M ====="
timed_dd dd if=$B/mnt_env/big.bin of=/dev/null bs=1M status=none
[ "$MS" -ge 1400 ] && ok "env-capped read took ${MS}ms (>=1400ms floor)" \
  || bad "env-capped read took only ${MS}ms — GROVEFS_BW_READ not applied"

echo "===== 5) --bw-read 0 means unlimited ====="
timed_dd dd if=$B/mnt_zero/big.bin of=/dev/null bs=1M status=none
[ $? -eq 0 ] && [ "$MS" -lt 1200 ] && ok "zero-rate mount read in ${MS}ms (unlimited)" \
  || bad "zero-rate mount read took ${MS}ms — expected an unthrottled read"

echo "===== 6) invalid rates are refused at parse time ====="
if $Y mount --view bw -m $B/mnt_bad --bw-read 12Z > $B/bad.log 2>&1; then
  bad "--bw-read 12Z was accepted"
else
  ok "--bw-read 12Z refused"
fi
grep -q "invalid rate" $B/bad.log && ok "refusal names the invalid rate" \
  || bad "refusal text: $(cat $B/bad.log)"
if $Y mount --view bw -m $B/mnt_bad --bw-write 1.5M > $B/bad2.log 2>&1; then
  bad "--bw-write 1.5M was accepted"
else
  ok "--bw-write 1.5M refused"
fi

echo "===== 7) help surface + read-only mount with --bw-write ====="
$Y mount --help 2>&1 | grep -q -- '--bw-read' && $Y mount --help 2>&1 | grep -q -- '--bw-write' \
  && ok "mount --help lists --bw-read and --bw-write" || bad "help text misses the flags"

if { echo hi > $B/mnt_ro/probe.txt; } 2> $B/ro_write.err; then
  bad "read-only mount accepted a write"
else
  ok "read-only mount still refuses writes (EROFS) despite --bw-write"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL BWLIMIT CHECKS PASSED" || echo "SOME BWLIMIT CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
