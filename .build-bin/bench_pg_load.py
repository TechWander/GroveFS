#!/usr/bin/env python3
"""Per-mount metadata load driver for bench_pg_conc.sh.

Modes:
  time-boxed (dirs==0): every thread creates uniquely-named zero-byte files
    in its own distinct parent dirs until <secs> elapsed; ops counted.
    -> aggregate throughput = sum(ops) / window, the steady-state metric.
  count (dirs>0): thread i builds c{i}/d{..}/f{..} exactly (contention
    variant with --contend: all threads/mounts collide on identical names,
    exactly one winner per name).

Metadata-only either way (zero-byte files; the data plane stays out).
Start is gated on a barrier file so N mount-drivers fire simultaneously.
"""
import os, sys, time, threading

mnt = sys.argv[1]
nthreads = int(sys.argv[2])
dirs = int(sys.argv[3])
files = int(sys.argv[4])
secs = float(sys.argv[5])
barrier = sys.argv[6]
contend = "--contend" in sys.argv

ops = 0
errs = []
lock = threading.Lock()


def worker(i):
    global ops
    while not os.path.exists(barrier):
        time.sleep(0.005)
    if dirs == 0:  # time-boxed, distinct parents (tagged per mount so
        # concurrent mounts of the SAME view never collide)
        tag = os.path.basename(mnt)
        base = f"{mnt}/c{i}"
        try:
            os.mkdir(base)
        except FileExistsError:
            pass
        n = 0
        d = 0
        deadline = time.time() + secs
        while time.time() < deadline:
            p = f"{base}/d{tag}_{i}_{d}"
            os.mkdir(p)
            n += 1
            for f in range(20):
                open(f"{p}/f{f}.x", "x").close()
                n += 1
            d += 1
        with lock:
            ops += n
    else:  # fixed count
        base = mnt + "/shared" if contend else f"{mnt}/c{i}"
        try:
            os.mkdir(base)
        except FileExistsError:
            pass
        n = 0
        for d in range(dirs):
            p = f"{base}/d{d}"
            try:
                os.mkdir(p)
                n += 1
            except FileExistsError:
                pass
            except OSError as e:
                errs.append(f"mkdir {p}: {e}")
            for f in range(files):
                try:
                    open(f"{p}/f{f}.dat", "x").close()
                    n += 1
                except FileExistsError:
                    pass
                except OSError as e:
                    errs.append(f"create {p}/f{f}: {e}")
        with lock:
            ops += n


ts = [threading.Thread(target=worker, args=(i,)) for i in range(nthreads)]
[t.start() for t in ts]
start = time.time()
[t.join() for t in ts]
end = time.time()
print(f"RESULT ops={ops} start={start:.3f} end={end:.3f} errs={len(errs)}")
for e in errs[:10]:
    print("ERR", e)
