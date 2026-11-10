#!/usr/bin/env python3
"""Decide the G3 gate from a bench_g3.sh row file instead of by eye.

Rows look like:
    G3ROW fb <view> N=1 T=8 ops=27134 wall=6.02s agg=4507 ops/s
Two numbers matter, and only one of them is the arm under test:
  * best FDB cell against G3_GATE -- the gate itself.  §15 of
    docs/fdb-keyspace.md sets it at ~4.6k ops/s (SATA-SSD tier, three
    replicas) and >=9k ops/s (NVMe tier), so the caller picks the tier.
  * best PG cell against the same gate -- the CONTROL.  The "~4.6k PG platform
    value" is a property of the host, not of this repo, so a round on a machine
    whose PG arm does not come near that number is not a measurement of the
    gate in either direction; reading such a round as a pass, or as a fail,
    would be the mistake this script exists to prevent.

Exit code is the verdict, so no harness can read "it ran" as "it passed":
    0 G3_GREEN, 1 G3_RED, 2 G3_INCONCLUSIVE.
"""
import re
import sys


def best(rows, arm):
    vals = [float(m.group(1)) for r in rows
            if (m := re.search(r"agg=([0-9.]+)", r)) and r.split()[1] == arm]
    return max(vals) if vals else None


path, gate = sys.argv[1], float(sys.argv[2])
ctrl = float(sys.argv[3]) if len(sys.argv) > 3 else 0.8

rows = [l for l in open(path) if l.startswith("G3ROW")]
fb, pg = best(rows, "fb"), best(rows, "pg")
floor = gate * ctrl
print("  cells=%d  best fdb=%s  best pg(same session)=%s" % (len(rows), fb, pg))
print("  gate=%.0f ops/s  control floor=%.0f ops/s" % (gate, floor))

if fb is None:
    print("G3_INCONCLUSIVE: the FDB arm produced no measurement")
    sys.exit(2)
if pg is None or pg < floor:
    print("G3_INCONCLUSIVE: the PG control did not reproduce the platform value,"
          " so this round bounds the host rather than judging the gate")
    sys.exit(2)

print("  fdb/gate=%.2f  fdb/pg=%.2f" % (fb / gate, fb / pg))
if fb >= gate:
    print("G3_GREEN")
    sys.exit(0)
print("G3_RED")
sys.exit(1)
