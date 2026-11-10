#!/usr/bin/env python3
"""Bench defect, caught by a board run on a loaded machine (2026-11-09,
logs/20261109-213901/locks.log: `pass=15 fail=2`, section 3 only; the same suite
passes 17/0 three times in a row when nothing else competes for the CPU).

Section 3 asked a holder process to take fcntl [0,100) and gave it 40*0.05s = 2s
to report the grant, while the holder itself stayed alive for a FIXED 3s. Two
assertions then ran against that lock. On a loaded machine the readiness wait
expires first and the holder's 3s is already burning, so:

    bad "holder never acquired"          — the grant landed after the budget
    bad "overlap expected refused"       — by then the holder had left, so the
                                           overlapping request was legitimately
                                           granted against an unlocked range

Both failures said "the lock surface is wrong" while they measured the harness.
The fix removes the timing coupling instead of widening a constant:

  * the holder keeps the lock until the SCRIPT lifts a marker file (bounded by a
    deadline, so a wedged mount still cannot hang the suite), so the two requests
    are guaranteed to run while it is held — and a live holder surviving past one
    lease window is exactly what section 4b asserts, so outlasting the 3s lease is
    a supported state, not a new risk;
  * the readiness wait goes to ~10s and PRINTS how long the grant took, so a slow
    machine shows up in the log as a note instead of a red assertion;
  * a failure now carries the holder's own output, which is what was missing when
    the board went red and the reason had to be reconstructed from the script.

Same judgement, same assertion count, no longer time-raced.
"""
F = '/data/grovefs/grovefs/.build-bin/test_locks.sh'
TXT = open(F, encoding='utf-8').read()
assert 'HOLD=.locks/hold.marker' not in TXT, 'already applied'

OLD_PY = """    if mode == 'holder':
        # writer lock [0, 100)
        fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 0, 100))
        print('holder:acquired', flush=True)
        time.sleep(3)"""
NEW_PY = """    if mode == 'holder':
        # writer lock [0, 100)
        fcntl.fcntl(fd, fcntl.F_SETLK, mklock(fcntl.F_WRLCK, 0, 100))
        print('holder:acquired', flush=True)
        # Hold until the script lifts the marker, so the disjoint/overlapping
        # requests below provably run against a LOCKED range. This used to be
        # `time.sleep(3)`, which the readiness wait raced: on a loaded machine the
        # wait expired first and the two assertions then measured an already
        # released lock. The deadline only exists so a wedged mount cannot hang
        # the suite; living past one 3s lease window is what section 4b asserts.
        deadline = time.time() + 120
        while os.path.exists(sys.argv[3]) and time.time() < deadline:
            time.sleep(0.05)"""

OLD_SH = """  python3 .locks/fcntl_test.py holder "$F" > .locks/h.out 2>&1 &
  HP=$!
  # Wait for the holder to print 'acquired'.
  for _ in $(seq 1 40); do grep -q 'holder:acquired' .locks/h.out 2>/dev/null && break; sleep 0.05; done
  grep -q 'holder:acquired' .locks/h.out && ok "holder fcntl W acquired [0,100)" || bad "holder never acquired\""""
NEW_SH = """  HOLD=.locks/hold.marker
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
  fi"""

OLD_TAIL = """  wait $HP 2>/dev/null
fi

# ---------------------------------------------------------------------------
echo "===== 4) SIGKILL a single-process holder: FUSE release reaps its lock immediately =====\""""
NEW_TAIL = """  rm -f $HOLD                   # done asking: let the holder go
  wait $HP 2>/dev/null
fi

# ---------------------------------------------------------------------------
echo "===== 4) SIGKILL a single-process holder: FUSE release reaps its lock immediately =====\""""

for old, new in ((OLD_PY, NEW_PY), (OLD_SH, NEW_SH), (OLD_TAIL, NEW_TAIL)):
    assert TXT.count(old) == 1, 'anchor hit %d: %r' % (TXT.count(old), old[:48])
    TXT = TXT.replace(old, new)

open(F, 'w', encoding='utf-8').write(TXT)
print('test_locks.sh: section 3 holder is marker-released, readiness budget 10s with elapsed note')
