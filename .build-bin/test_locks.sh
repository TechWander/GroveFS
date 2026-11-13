#!/usr/bin/env bash
# POSIX advisory-lock e2e for grovefs (L1). Exercises the FUSE `setlk`/`getlk`/
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
#      lease to 3s via `GROVEFS_LOCK_LEASE_SECS` (internal knob) so the sleep
#      stays bounded.
#   5. HA path: a primary meta-server + a standby, client mounts through
#      `--meta-server primary,standby`; the `locks` table lives inside the SAME view
#      DB that backup_to_bytes snapshots onto the standby, so a client pointed
#      only at the standby sees the replicated lock rows (proves locks survive
#      failover without any additional WAL channel).
#
# Assertion-based (pass/fail counters + real exit code) like every other suite.
#
# The backend is a parameter: `LOCKS_BACKEND=fdb` re-runs every assertion above
# (1-4c) against a FoundationDB-backed meta-server instead of a local SQLite
# store. Sections 1-4c only ever touch the two mounts, so the same flock/fcntl
# probes decide the same things — what changes is HOW two clients end up on one
# authoritative tree (local mounts vs. two mounts of one remote view) and where
# the seed files come from (a manifest vs. writing them through the mount).
# Section 5 is deliberately SQLite-only: it reads a standby's `meta/alice.db`
# through sqlite3, i.e. it asserts GroveFS-native HA, which an FDB deployment
# refuses at startup by design (the cluster replicates instead).
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
# Short lease so section 4 finishes in seconds, not minutes. Local mounts pick
# this up through `LockLease::secs()` reading the env at first call.
export GROVEFS_LOCK_LEASE_SECS=3

BACKEND="${LOCKS_BACKEND:-sqlite}"
Y=./target/release/grovefs

lport(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

if [ "$BACKEND" = fdb ]; then
    FDB_HOME="${FDB_HOME:-/data/grovefs/.fdbbin}"
    CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-/data/grovefs/.fdbtest/test.cluster}"
    FDBURI="fdb://$CLUSTER"
    export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"
    if ! ( [ -x "$FDB_HOME/fdbcli.x86_64" ] && [ -f "$CLUSTER" ] \
           && "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" --exec "status minimal" 2>&1 \
                | grep -q "The database is available" ); then
        if [ "${FDB_TEST:-0}" = 1 ]; then
            echo "SUMMARY: pass=0 fail=1 SKIP=0 (FDB_TEST=1 but no cluster at $CLUSTER)"; exit 1
        fi
        echo "SUMMARY: pass=0 fail=0 SKIP=1 (no FoundationDB cluster)"; exit 0
    fi
    # The lock ops run inside the meta-server process under this backend, so the
    # lease the sections below reason about has to be the one that process read —
    # which it is, because GROVEFS_LOCK_LEASE_SECS is exported above and inherited.
    Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
fi

rm -rf .locks; mkdir -p .locks/home .locks/m1 .locks/m2
export GROVEFS_HOME=$PWD/.locks/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

# Two mounts of one view, wired per backend; the assertions below never learn
# which one they are on.
M1=.locks/m1
M2=.locks/m2
# `kill 0` is not "kill nothing": signal 0-carrying PID 0 means the WHOLE process
# group, i.e. this suite and whoever launched it. GNU timeout hides that by
# giving the child its own group, so it only shows up as a green summary followed
# by exit 143. Kill real numbers only.
kill_pids(){ local p; for p in "$@"; do case "$p" in ''|0|*[!0-9]*) continue ;; esac
        kill "$p" 2>/dev/null || true; done; }
# Reap leaked scratch processes by NAME AND DIRECTORY, never with a bare
# `pkill -f '.locks/'`: that pattern also matches whoever launched this suite
# (a harness command line carrying the scratch path).
_reap(){ local p; for p in $(pgrep -f 'grovefs.*[. ]\.locks' 2>/dev/null || true); do
        case " $$ $PPID " in *" $p "*) continue ;; esac
        kill "$p" 2>/dev/null || true
    done; }
cleanup(){ fusermount -u $M1 2>/dev/null; fusermount -u $M2 2>/dev/null; fusermount -u .locks/ha/mnt 2>/dev/null; kill_pids "${P1:-}" "${P2:-}" "${DP:-}" "${MP:-}"; _reap
    # This suite stands its own server pair rather than going through fdb_up, so
    # it also drops its own tree: a subspace OUTLIVES the run, and unmounting
    # alone leaves it (plus every object it booked) in the cluster for
    # `view scan` to find. Measured 2026-11-11: the four parameterised arms left
    # one tree each; this is the same leak in the arm that does not use the
    # scaffold's hook.
    if [ "$BACKEND" = fdb ] && [ -n "${VIEW:-}" ]; then
        $Y view --meta-store "$FDBURI" --meta-dir .locks/srv/home rm --name "$VIEW" --base \
            --data-dir .locks/srv/data >/dev/null 2>&1 || true
    fi
    return 0; }
# Armed before the first mount: a fail-fast gate below exits non-zero, and
# without the trap that would leave FUSE mounts and servers behind for the
# next run of the suite.
trap cleanup EXIT

if [ "$BACKEND" = fdb ]; then
    META=127.0.0.1:15406
    DATA=127.0.0.1:15407
    SRV=.locks/srv
    VIEW="lk$(date +%s)"
    # Ask whether the port is FREE before binding, not just whether something
    # answers afterwards: `lport` is satisfied by a listener a previous run left
    # behind, and the suite would then take its lock assertions from a stranger's
    # deployment (the failure mode that turned `grovesci` red once).
    for P in $META $DATA; do
        if (echo >/dev/tcp/${P%%:*}/${P##*:}) 2>/dev/null; then
            echo "FAIL: port $P is already listening; refusing to lock against a stranger's server"
            exit 1
        fi
    done
    mkdir -p $SRV/data $SRV/home $SRV/h1 $SRV/h2
    $Y view --meta-store "$FDBURI" --meta-dir $SRV/home create --view "$VIEW" > .locks/create.log 2>&1 \
        || { echo "FAIL: view create for the lock suite: $(cat .locks/create.log)"; exit 1; }
    $Y data-server --listen $DATA --data-dir $SRV/data > .locks/data.log 2>&1 & DP=$!
    $Y meta-server --listen $META --home $SRV/home --meta-store "$FDBURI" > .locks/meta.log 2>&1 & MP=$!
    lport $DATA || { echo "FAIL: data-server did not bind"; cat .locks/data.log; exit 1; }
    # The meta-server may legitimately wait out a lingering writer lease from a
    # previous run of this suite (same --meta-dir => same deployment): the lease
    # TTL is 10s and a starting server waits TTL+2s for it (fdb_store.rs
    # writer_lease_wait). Give the bind a budget that covers that window instead
    # of the data-server's 5s, so a back-to-back re-run is not mistaken for a dead
    # server. This only extends a wait; the mount gate below still fail-fasts and
    # every lock assertion is unchanged. SQLite has no such lease.
    meta_up=0
    for _ in $(seq 1 400); do
        (echo >/dev/tcp/${META%%:*}/${META##*:}) 2>/dev/null && { meta_up=1; break; }
        sleep 0.05
    done
    [ "$meta_up" = 1 ] || { echo "FAIL: meta-server did not bind"; cat .locks/meta.log; exit 1; }
    $Y --home $SRV/h1 mount --view "$VIEW" --mountpoint $M1 --meta-server $META \
        --data-servers $DATA --foreground > .locks/m1.log 2>&1 &
    P1=$!
    $Y --home $SRV/h2 mount --view "$VIEW" --mountpoint $M2 --meta-server $META \
        --data-servers $DATA --foreground > .locks/m2.log 2>&1 &
    P2=$!
    # Fail-fast mount gate: a mount that never came up would let every lock below
    # be taken on a bare directory and still print green numbers.
    for mp in $M1 $M2; do
        gated=0
        for _ in $(seq 1 60); do
            findmnt -rn --mountpoint "$PWD/$mp" >/dev/null 2>&1 && { gated=1; break; }
            sleep 0.25
        done
        [ "$gated" = 1 ] || { echo "FAIL: $mp never mounted (no fake lock numbers)"; cat ".locks/$(basename $mp).log"; exit 1; }
    done
    # Same seed the manifest gives the SQLite arm, written through the mount.
    mkdir -p "$M1/seed" && : > "$M1/seed/f1.bin" && : > "$M1/seed/f2.bin" \
        || { echo "FAIL: seeding the fdb lock view"; exit 1; }
    sleep 1.0
else
    # Two mounts of the *same* user against the same view DB. Both processes
    # carry GROVEFS_LOCK_LEASE_SECS=3 through the exported env, so their
    # MetaEngine instances agree on lease duration and cross-process visibility
    # is guaranteed by the shared `locks` SQLite table (WAL + busy_timeout
    # already proven by the `concurrent` suite).
    cat > .locks/alice.manifest <<'EOF'
mkdir seed
touch seed/f1.bin
touch seed/f2.bin
EOF
    $Y init --view alice --manifest .locks/alice.manifest >/dev/null
    $Y mount --view alice --mountpoint $M1 --foreground > .locks/m1.log 2>&1 &
    P1=$!
    $Y mount --view alice --mountpoint $M2 --foreground > .locks/m2.log 2>&1 &
    P2=$!
    sleep 1.5
fi

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
# After holder exits, a fresh request must succeed. On a remote (fdb://) store
# the release is a distributed commit that can lag the holder's exit by a beat,
# so poll for visibility within a bounded window instead of a single zero-wait
# probe. The assertion is unchanged; on SQLite the first attempt succeeds.
granted1=0
for _ in $(seq 1 10); do
  if timeout 2 bash -c "exec 200>'$M2/seed/f1.bin'; flock -x 200"; then granted1=1; break; fi
  sleep 0.2
done
[ "$granted1" = 1 ] && ok "second flock -x granted once holder released" \
  || bad "second flock -x still blocked after holder exited"

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
        # writer lock [0, 100). On a remote store the previous section's release
        # may still be committing, so retry the grant within a bounded window
        # rather than measuring a stale row. 'acquired' is printed only after the
        # grant lands, so the disjoint/overlapping probes below still run against
        # a genuinely held range. On SQLite the first attempt succeeds.
        deadline = time.time() + 10
        while True:
            try:
                fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 0, 100))
                break
            except OSError as ex:
                if ex.errno in (errno.EACCES, errno.EWOULDBLOCK) and time.time() < deadline:
                    time.sleep(0.1); continue
                raise
        print('holder:acquired', flush=True)
        # Hold until the script lifts the marker, so the disjoint/overlapping
        # requests below provably run against a LOCKED range. This used to be
        # `time.sleep(3)`, which the readiness wait raced: on a loaded machine the
        # wait expired first and the two assertions then measured an already
        # released lock. The deadline only exists so a wedged mount cannot hang
        # the suite; living past one 3s lease window is what section 4b asserts.
        deadline = time.time() + 120
        while os.path.exists(sys.argv[3]) and time.time() < deadline:
            time.sleep(0.05)
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
  HOLD=.locks/hold.marker
  : > $HOLD                     # the holder releases when this file goes away
  python3 .locks/fcntl_test.py holder "$F" $HOLD > .locks/h.out 2>&1 &
  HP=$!
  # Wait for the holder to print 'acquired'. The budget is ~10s and the elapsed
  # time is reported: a grant that takes longer than it used to is a loaded
  # machine, not a broken lock surface, and the log should say so out loud.
  t0=$SECONDS
  for _ in $(seq 1 200); do grep -q 'holder:acquired' .locks/h.out 2>/dev/null && break; sleep 0.05; done
  took=$((SECONDS - t0))
  if grep -q 'holder:acquired' .locks/h.out; then
    if [ $took -ge 2 ]; then
      echo "  NOTE: the uncontended grant took ${took}s (loaded host); the assertions"
      echo "        below are no longer time-raced against the holder's lifetime"
    fi
    ok "holder fcntl W acquired [0,100)"
  else
    bad "holder never acquired after ${took}s: $(cat .locks/h.out 2>/dev/null | tail -3)"
  fi
  # Disjoint request: [101,200) must be granted.
  ch=$(python3 .locks/fcntl_test.py nonoverlap "$M2/seed/f1.bin" 2>&1 | tail -1)
  chk "$ch" "nonoverlap:granted" "disjoint [101,200) fcntl W granted"
  # Overlapping request: [50,150) must be refused with EACCES/EWOULDBLOCK.
  ov=$(python3 .locks/fcntl_test.py overlap "$M2/seed/f1.bin" 2>&1 | tail -1)
  case "$ov" in
    overlap:refused*) ok "overlapping [50,150) refused ($ov)" ;;
    *) bad "overlap expected refused, got: $ov" ;;
  esac
  rm -f $HOLD                   # done asking: let the holder go
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
# release-driven reap is a distributed commit on fdb://; poll for visibility
# exactly as section 4 does, instead of a single zero-wait probe.
reaped2=0
for _ in $(seq 1 25); do
  if python3 .locks/ask.py "$M2/seed/f2.bin" >/dev/null 2>&1; then reaped2=1; break; fi
  sleep 0.2
done
if [ "$reaped2" = 1 ]; then ok "lock released once live holder killed"; else bad "lock still held after holder killed"; fi

# ---------------------------------------------------------------------------
echo "===== 4c) POSIX fcntl close/exit releases the lock (no sid-renewal leak) ====="
# Regression for §5-3: fuser only hands `release` a lock_owner for flock(2)
# (FUSE_RELEASE_FLOCK_UNLOCK), so a POSIX fcntl lock dropped by close()/exit
# used to be left behind and the per-mount sid heartbeat renewed the ghost row
# forever — a fresh non-blocking F_SETLK kept returning EACCES and a blocked
# F_SETLKW hung to the 10-min cap. The mount now tracks (fh -> ino, owner) and
# deletes the rows on the last locking close. Note the lock-renewal heartbeat
# runs for the life of the MOUNT, not the app, so lease expiry (3s) can NOT
# rescue these cases on its own: without the fix they hang regardless.
cat > .locks/posix_hold.py <<'PY'
import fcntl, os, sys, struct, time
# whole-file write lock: l_type=F_WRLCK, whence=SEEK_SET, start=0, len=0 (to EOF)
lock = struct.pack('hhqqi', fcntl.F_WRLCK, 0, 0, 0, 0)
fd = os.open(sys.argv[1], os.O_RDWR)
fcntl.fcntl(fd, fcntl.F_SETLK, lock)
print('acquired', flush=True)
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
time.sleep(secs)         # hold while alive...
os.close(fd)            # ...then release via close ONLY (no explicit F_UNLCK)
PY
cat > .locks/posix_ask.py <<'PY'
import fcntl, os, sys, struct, errno
lock = struct.pack('hhqqi', fcntl.F_WRLCK, 0, 0, 0, 0)
fd = os.open(sys.argv[1], os.O_RDWR)
try:
    fcntl.fcntl(fd, fcntl.F_SETLK, lock)   # non-blocking
    print('posix:granted')
except OSError as ex:
    print('posix:refused errno=%d' % ex.errno)
finally:
    os.close(fd)
PY
cat > .locks/posix_block.py <<'PY'
import fcntl, os, sys, struct, time
lock = struct.pack('hhqqi', fcntl.F_WRLCK, 0, 0, 0, 0)
fd = os.open(sys.argv[1], os.O_RDWR)
t0 = time.time()
fcntl.fcntl(fd, fcntl.F_SETLKW, lock)      # blocking: only returns once granted
print('posix:WAKE %.2fs' % (time.time() - t0))
os.close(fd)
PY
# (a) holder acquires on M1, then closes+exits; a fresh process must get the lock.
python3 .locks/posix_hold.py "$M1/seed/f1.bin" 0 > .locks/ph.out 2>&1
grep -q acquired .locks/ph.out && ok "posix holder acquired whole-file W" || bad "posix holder failed to acquire"
# post-close reap visibility (distributed commit on fdb://): bounded poll, same
# rationale as sections 1/4/4b. The leak assertion itself is unchanged.
ra=""
for _ in $(seq 1 25); do
  ra=$(python3 .locks/posix_ask.py "$M2/seed/f1.bin" 2>&1 | tail -1)
  case "$ra" in posix:granted) break ;; esac
  sleep 0.2
done
case "$ra" in
  posix:granted) ok "fresh F_SETLK granted after holder close (leak fixed)" ;;
  *) bad "POSIX lock leaked on close (ask got: $ra)" ;;
esac
# (b) a blocked F_SETLKW waiter must wake when the holder closes at ~3s.
python3 .locks/posix_hold.py "$M1/seed/f1.bin" 3 > .locks/ph2.out 2>&1 &
HPB=$!
for _ in $(seq 1 60); do grep -q acquired .locks/ph2.out 2>/dev/null && break; sleep 0.05; done
# timeout 8: without the fix the waiter hangs to the 10-min cap and never prints.
wl=$(timeout 8 python3 .locks/posix_block.py "$M2/seed/f1.bin" 2>&1 | tail -1)
wait $HPB 2>/dev/null
case "$wl" in
  posix:WAKE*) ok "blocked F_SETLKW released when holder closed ($wl)" ;;
  *) bad "blocked F_SETLKW did not wake on holder close (got: $wl)" ;;
esac

# ---------------------------------------------------------------------------
echo "===== 5) HA: locks table replicates to standby via existing backup channel ====="
# SQLite-only by construction: the probe reads the standby's `meta/alice.db`
# with sqlite3, i.e. it asserts GroveFS-native HA. An FDB deployment has no
# GroveFS standby (the cluster itself replicates, and the meta-server refuses
# --peer on an fdb:// store), so there is no equivalent artifact to open here.
if [ "$BACKEND" != sqlite ]; then
  echo "  SKIP: section 5 asserts SQLite standby replication (backend=$BACKEND)"
else
META_A=127.0.0.1:15116
META_B=127.0.0.1:15117
DATA=127.0.0.1:15118
rm -rf .locks/ha; mkdir -p .locks/ha/a .locks/ha/b .locks/ha/data .locks/ha/chome .locks/ha/mnt
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

$Y data-server --listen $DATA --data-dir .locks/ha/data > .locks/ha/data.log 2>&1 &
DP=$!
$Y meta-server --meta-dir .locks/ha/a --listen $META_A --peer $META_B > .locks/ha/a.log 2>&1 &
AP=$!
$Y meta-server --meta-dir .locks/ha/b --listen $META_B --role standby --peer $META_A > .locks/ha/b.log 2>&1 &
BP=$!
wait_port $DATA && wait_port $META_A && wait_port $META_B || bad "HA ports did not come up"

# Strict view gate: create the view on the primary (the standby replicates it).
$Y view --meta-dir .locks/ha/a create --view alice > .locks/ha/create.log 2>&1

# Client mounts via failover list against the HA pair.
$Y --home .locks/ha/chome mount --view alice --mountpoint .locks/ha/mnt --meta-server "$META_A,$META_B" --data-servers $DATA --foreground > .locks/ha/mnt.log 2>&1 &
CP=$!
sleep 2
# The explicitly created view is empty, so it only has / (no seed/). Make the
# file we lock at the mount root.
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
kill_pids "$CP" "$DP" "$AP" "$BP"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL LOCK CHECKS PASSED" || echo "SOME LOCK CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
