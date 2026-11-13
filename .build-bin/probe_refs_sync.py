#!/usr/bin/env python3
"""Does ATTACH + `PRAGMA refs.journal_mode=WAL` drop the ledgers's synchronous?

GroveFS attaches a sibling `refs.db` ledger to the view connection and switches
it to WAL. `PRAGMA synchronous` is per-database in SQLite >= 3.23, so a ledger
attached after `synchronous=NORMAL` *should* inherit NORMAL. The probe measures
whether it actually does: run once plain and once with an explicit
`PRAGMA refs.synchronous=NORMAL`, then compare fsync counts under strace.

  strace -f -y -e trace=fsync,fdatasync -o tr python3 probe_refs_sync.py [plain|fix]
"""
import os
import sqlite3
import sys

mode = sys.argv[1] if len(sys.argv) > 1 else "plain"
for f in ("main.db", "main.db-wal", "main.db-shm", "refs.db", "refs.db-wal", "refs.db-shm"):
    if os.path.exists(f):
        os.unlink(f)

c = sqlite3.connect("main.db", isolation_level=None)
c.execute("PRAGMA journal_mode=WAL")
c.execute("PRAGMA synchronous=NORMAL")
c.execute("ATTACH 'refs.db' AS refs")
c.execute("PRAGMA refs.journal_mode=WAL")
if mode == "fix":
    c.execute("PRAGMA refs.synchronous=NORMAL")

main_sync = c.execute("PRAGMA main.synchronous").fetchone()[0]
refs_sync = c.execute("PRAGMA refs.synchronous").fetchone()[0]
print(f"mode={mode} main.synchronous={main_sync} refs.synchronous={refs_sync}")

c.execute("CREATE TABLE IF NOT EXISTS refs.h (k TEXT PRIMARY KEY, v INT)")
c.execute("CREATE TABLE IF NOT EXISTS t (k TEXT PRIMARY KEY, v INT)")
for i in range(50):
    c.execute("BEGIN IMMEDIATE")
    c.execute("INSERT INTO refs.h VALUES (?,1)", (f"r{i}",))
    c.execute("INSERT INTO t VALUES (?,1)", (f"t{i}",))
    c.execute("COMMIT")
print("commits=50")
