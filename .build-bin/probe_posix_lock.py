#!/usr/bin/env python3
# Real-cluster probe for the §5-3 POSIX close-release fix.
# Run on gfs-client against the systemd-managed mount /home/hanjun/mnt.
#   holder: take a whole-file POSIX write lock (fcntl F_SETLK), then release it
#           with close() ONLY (no explicit F_UNLCK) and exit.
#   asker : a fresh process (different fl_owner) tries a non-blocking F_SETLK.
# Before the fix the kernel gave `release` no lock_owner for POSIX locks, so the
# ghost row was renewed forever and this returned EACCES (errno 13). After the
# fix the mount drops the (ino,owner) row on last close, so it must grant.
import fcntl, os, sys, struct, time

MNT = "/home/hanjun/mnt"
F = os.path.join(MNT, "lockprobe.txt")
WHOLE = struct.pack("hhqqi", fcntl.F_WRLCK, 0, 0, 0, 0)

role = sys.argv[1] if len(sys.argv) > 1 else "cycle"

if role == "holder":
    fd = os.open(F, os.O_RDWR | os.O_CREAT, 0o644)
    fcntl.fcntl(fd, fcntl.F_SETLK, WHOLE)
    print("holder:acquired", flush=True)
    time.sleep(float(sys.argv[2]) if len(sys.argv) > 2 else 2.0)
    os.close(fd)  # release via close only
    print("holder:closed", flush=True)
elif role == "ask":
    fd = os.open(F, os.O_RDWR)
    try:
        fcntl.fcntl(fd, fcntl.F_SETLK, WHOLE)
        print("ask:GRANTED")
    except OSError as ex:
        print("ask:REFUSED errno=%d" % ex.errno)
    finally:
        os.close(fd)
elif role == "block":
    fd = os.open(F, os.O_RDWR)
    t0 = time.time()
    fcntl.fcntl(fd, fcntl.F_SETLKW, WHOLE)  # blocking
    print("block:WAKE %.2fs" % (time.time() - t0))
    os.close(fd)
elif role == "cycle":
    # full end-to-end: holder acquires + closes in a subprocess, then asker.
    import subprocess
    subprocess.run([sys.executable, __file__, "holder", "2"], check=True)
    r = subprocess.run([sys.executable, __file__, "ask"], capture_output=True, text=True)
    print("ask-after-close ->", r.stdout.strip())
    # blocking-wake: holder keeps lock 3s then closes; a waiter should wake ~3s.
    h = subprocess.Popen([sys.executable, __file__, "holder", "3"])
    time.sleep(0.5)
    w = subprocess.run(
        [sys.executable, __file__, "block"], capture_output=True, text=True, timeout=15
    )
    h.wait()
    print("blocked-waiter ->", w.stdout.strip())
    os.unlink(F)
    print("RESULT:", "PASS" if "GRANTED" in r.stdout and "WAKE" in w.stdout else "FAIL")
