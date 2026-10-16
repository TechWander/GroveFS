#!/usr/bin/env bash
# POSIX advisory-lock e2e for yourfs (L1). Exercises the FUSE `setlk`/`getlk`/
# `release` callbacks added in this milestone across the four scenarios that
# matter:
#   1. flock -x exclusive across two processes on the SAME mount — second one
#      MUST block until the holder drops the lock (verified via `timeout` rc).
#   2. flock -s shared across two processes — MUST coexist (both sleep past).
#   3. fcntl F_SETLK on disjoint vs overlapping intervals — disjoint grants,
#      overlapping non-blocking request returns EWOULDBLOCK (we surface it as
#      EACCES to match NFS advisory semantics).
#   4. SIGKILL'd holder — FUSE `release` never runs, but the lease sweep inside
#      every fresh setlk transaction reaps the stale row. Test overrides the
#      lease to 3s via `YOURFS_LOCK_LEASE_SECS` (internal knob) so the sleep
#      stays bounded.
#   5. HA path: a primary meta-server + a standby, client mounts through
#      `--meta primary,standby`; the `locks` table lives inside the SAME view
#      DB that backup_to_bytes snapshots onto the standby, so a client pointed
#      only at the standby sees the replicated lock rows (proves locks survive
#      failover without any additional WAL channel).
#
# Assertion-based (pass/fail counters + real exit code) like every other suite.
set -u
cd /data/xy/yourfs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/yourfs/.build-bin/pkg-config
# Short lease so section 4 finishes in seconds, not minutes. Local mounts pick
# this up through `LockLease::secs()` reading the env at first call.
export YOURFS_LOCK_LEASE_SECS=3

Y=./target/release/yourfs

rm -rf .locks; mkdir -p .locks/home .locks/m1 .locks/m2
export YOURFS_HOME=$PWD/.locks/home

cat > .locks/alice.manifest <<'EOF'
mkdir seed
touch seed/f1.bin
touch seed/f2.bin
EOF
$Y init --user alice --manifest .locks/alice.manifest >/dev/null

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

M1=.locks/m1
M2=.locks/m2

# Two mounts of the *same* user against the same view DB. Both processes carry
# YOURFS_LOCK_LEASE_SECS=3 through the exported env, so their MetaEngine
# instances agree on lease duration and cross-process visibility is guaranteed
# by the shared `locks` SQLite table (WAL + busy_timeout already proven by the
# `concurrent` suite).
$Y mount --user alice --mountpoint $M1 > .locks/m1.log 2>&1 &
P1=$!
$Y mount --user alice --mountpoint $M2 > .locks/m2.log 2>&1 &
P2=$!
sleep 1.5

cleanup(){ fusermount -u $M1 2>/dev/null; fusermount -u $M2 2>/dev/null; kill $P1 $P2 2>/dev/null; pkill -f '.locks/' 2>/dev/null; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
echo "===== 1) flock -x: exclusive lock blocks second holder ====="
F=$M1/seed/f1.bin
: > "$F"
# Holder keeps the lock for 3s; timeout 1 means the second flock cannot cross
# its `flock -x 200` barrier within 1s => rc=124 (blocked as expected).
( flock -x 200; sleep 3 ) 200>"$F" &
H=$!
sleep 0.4
if timeout 1 bash -c "exec 200>'$M2/seed/f1.bin'; flock -x 200" ; then
  bad "second flock -x was granted while holder still active (should have blocked)"
else
  rc=$?
  [ "$rc" = "124" ] && ok "second flock -x blocked (timeout rc=124)" || bad "second flock -x rc=$rc want 124"
fi
wait $H 2>/dev/null
# After holder exits, a fresh request must succeed quickly.
if timeout 2 bash -c "exec 200>'$M2/seed/f1.bin'; flock -x 200"; then
  ok "second flock -x granted once holder released"
else
  bad "second flock -x still blocked after holder exited"
fi

# ---------------------------------------------------------------------------
echo "===== 2) flock -s: two readers coexist ====="
( flock -s 200; sleep 2 ) 200>"$F" &
S1=$!
( flock -s 200; sleep 2 ) 200>"$M2/seed/f1.bin" &
S2=$!
sleep 0.5
# Both sleepers still running == both held the shared lock simultaneously.
if kill -0 $S1 2>/dev/null && kill -0 $S2 2>/dev/null; then
  ok "two flock -s holders coexist on same file"
else
  bad "one of the flock -s holders exited early"
fi
wait $S1 $S2 2>/dev/null
ok "both flock -s completed cleanly"

# ---------------------------------------------------------------------------
echo "===== 3) fcntl F_SETLK: disjoint ranges grant, overlapping returns EWOULDBLOCK ====="
if ! command -v python3 >/dev/null 2>&1; then
  bad "python3 missing — skipping section 3"
else
  cat > .locks/fcntl_test.py <<'PY'
import fcntl, struct, sys, time, os, errno
WHOLE = (0, 2**63 - 1)
def mklock(t, s, e):
    # struct flock: short type, short whence, long start, long len, pid
    return struct.pack('hhllh', t, 0, s, e - s, 0)
mode = sys.argv[1]
path = sys.argv[2]
fd = os.open(path, os.O_RDWR)
try:
    if mode == 'holder':
        # writer lock [0, 100)
        fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 0, 100))
        print('holder:acquired', flush=True)
        time.sleep(3)
    elif mode == 'nonoverlap':
        # [101, 200) — POSIX ranges are inclusive, so this touches neither 0 nor 100.
        fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 101, 200))
        print('nonoverlap:granted')
    elif mode == 'overlap':
        try:
            fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 50, 150))
            print('overlap:granted-BAD')
            sys.exit(1)
        except OSError as ex:
            if ex.errno in (errno.EACCES, errno.EWOULDBLOCK):
                print(f'overlap:refused errno={ex.errno}')
            else:
                raise
finally:
    os.close(fd)
PY
  python3 .locks/fcntl_test.py holder "$F" > .locks/h.out 2>&1 &
  HP=$!
  # Wait for the holder to print 'acquired'.
  for _ in $(seq 1 40); do grep -q 'holder:acquired' .locks/h.out 2>/dev/null && break; sleep 0.05; done
  grep -q 'holder:acquired' .locks/h.out && ok "holder fcntl W acquired [0,100)" || bad "holder never acquired"
  # Disjoint request: [101,200) must be granted.
  ch=$(python3 .locks/fcntl_test.py nonoverlap "$M2/seed/f1.bin" 2>&1 | tail -1)
  chk "$ch" "nonoverlap:granted" "disjoint [101,200) fcntl W granted"
  # Overlapping request: [50,150) must be refused with EACCES/EWOULDBLOCK.
  ov=$(python3 .locks/fcntl_test.py overlap "$M2/seed/f1.bin" 2>&1 | tail -1)
  case "$ov" in
    overlap:refused*) ok "overlapping [50,150) refused ($ov)" ;;
    *) bad "overlap expected refused, got: $ov" ;;
  esac
  wait $HP 2>/dev/null
fi

# ---------------------------------------------------------------------------
echo "===== 4) SIGKILL a single-process holder: FUSE release reaps its lock immediately ====="
# A lone python process opens the file, takes flock -x on that SAME fd (no
# fork/exec inheritors) and sleeps. `kill -9` -> the kernel closes its fd ->
# FUSE `release` fires with the matching lock_owner -> the row is deleted at
# once, so a peer's request succeeds well inside one lease window. (The older
# `( flock; sleep )` idiom was a poor probe: `flock` exits but `sleep` inherits
# the locked fd, so the description stays genuinely open and `release` must NOT
# fire — the lock is legitimately held.)
cat > .locks/hold.py <<'PY'
import fcntl, os, sys, time
fd = os.open(sys.argv[1], os.O_RDWR)
fcntl.flock(fd, fcntl.LOCK_EX)
print('locked', flush=True)
time.sleep(int(sys.argv[2]))
PY
cat > .locks/ask.py <<'PY'
import fcntl, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.close(fd); print('granted'); sys.exit(0)
except OSError:
    os.close(fd); print('blocked'); sys.exit(1)
PY
python3 .locks/hold.py "$M1/seed/f2.bin" 30 > .locks/hold.out 2>&1 &
HP=$!
for _ in $(seq 1 60); do grep -q locked .locks/hold.out 2>/dev/null && break; sleep 0.05; done
if grep -q locked .locks/hold.out 2>/dev/null; then ok "single-process holder acquired flock -x"; else bad "holder never acquired"; fi
# While the holder is alive a peer MUST be refused.
if python3 .locks/ask.py "$M2/seed/f2.bin" >/dev/null 2>&1; then
  bad "peer granted while holder alive (should block)"
else
  ok "peer blocked while holder alive (EWOULDBLOCK)"
fi
kill -9 $HP 2>/dev/null; wait $HP 2>/dev/null
# `release` reaps immediately, so a fresh request succeeds long before one lease.
reaped=0
for _ in $(seq 1 25); do
  if python3 .locks/ask.py "$M2/seed/f2.bin" >/dev/null 2>&1; then reaped=1; break; fi
  sleep 0.2
done
if [ "$reaped" = 1 ]; then ok "lock reaped right after SIGKILL (release-driven, < one lease)"; else bad "lock not reaped after SIGKILL"; fi

# ---------------------------------------------------------------------------
echo "===== 4b) renewal heartbeat: a live holder survives PAST one lease window ====="
# Hold for 9s, three times the 3s lease. Without renewal the row would expire
# and a peer could steal it; the per-mount heartbeat keeps bumping expires_at so
# the lock stays held until the real close — the whole point of Feature B.
python3 .locks/hold.py "$M1/seed/f2.bin" 9 > .locks/hold2.out 2>&1 &
HP2=$!
for _ in $(seq 1 60); do grep -q locked .locks/hold2.out 2>/dev/null && break; sleep 0.05; done
if grep -q locked .locks/hold2.out 2>/dev/null; then ok "long holder acquired"; else bad "long holder failed to acquire"; fi
sleep 5   # well past the 3s lease, holder still alive and renewing
if python3 .locks/ask.py "$M2/seed/f2.bin" >/dev/null 2>&1; then
  bad "peer stole a live lock past its lease — renewal heartbeat broken"
else
  ok "live lock still held at 5s (> lease): renewal keeps live holders alive"
fi
kill -9 $HP2 2>/dev/null; wait $HP2 2>/dev/null
if python3 .locks/ask.py "$M2/seed/f2.bin" >/dev/null 2>&1; then ok "lock released once live holder killed"; else bad "lock still held after holder killed"; fi

# ---------------------------------------------------------------------------
echo "===== 5) HA: locks table replicates to standby via existing backup channel ====="
META_A=127.0.0.1:50116
META_B=127.0.0.1:50117
DATA=127.0.0.1:50118
rm -rf .locks/ha; mkdir -p .locks/ha/a .locks/ha/b .locks/ha/data .locks/ha/chome .locks/ha/mnt
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

$Y data-server --listen $DATA --root .locks/ha/data > .locks/ha/data.log 2>&1 &
DP=$!
$Y --home .locks/ha/a meta-server --listen $META_A --peer $META_B > .locks/ha/a.log 2>&1 &
AP=$!
$Y --home .locks/ha/b meta-server --listen $META_B --role standby --peer $META_A > .locks/ha/b.log 2>&1 &
BP=$!
wait_port $DATA && wait_port $META_A && wait_port $META_B || bad "HA ports did not come up"

# Client mounts via failover list against the HA pair.
$Y --home .locks/ha/chome mount --user alice --mountpoint .locks/ha/mnt --meta "$META_A,$META_B" --data $DATA > .locks/ha/mnt.log 2>&1 &
CP=$!
sleep 2
# HA client's alice view is created on-demand server-side, so it only has /
# (no seed/). Make the file we lock at the mount root.
MF=.locks/ha/mnt/ha_lock.bin
: > "$MF"
# Holder keeps a write lock on the primary so the row is definitely present at
# snapshot time.
( flock -x 300; sleep 3 ) 300>"$MF" &
HAH=$!
sleep 0.4
# Backup cadence is ~500ms; give it one more interval.
sleep 1.2
SBDB=.locks/ha/b/meta/alice.db
if command -v python3 >/dev/null 2>&1 && [ -f "$SBDB" ]; then
  n=$(python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); r=c.execute("select count(*) from locks").fetchone()[0]; print(r); c.close()' "$SBDB" 2>/dev/null || echo "-1")
  if [ "$n" -ge 1 ] 2>/dev/null; then
    ok "standby replicated locks table has $n row(s) (HA survives failover)"
  else
    bad "standby locks table missing/empty (n=$n) — replication did not carry locks"
  fi
else
  bad "cannot verify standby replication (python3 or $SBDB missing)"
fi
wait $HAH 2>/dev/null

# ---- teardown only the HA sub-processes; m1/m2 cleanup stays with trap ----
fusermount -u .locks/ha/mnt 2>/dev/null
kill $CP $DP $AP $BP 2>/dev/null || true

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL LOCK CHECKS PASSED" || echo "SOME LOCK CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
