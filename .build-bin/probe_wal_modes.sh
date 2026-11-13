#!/usr/bin/env bash
# What does one SQLite commit cost, per durability setting?
#
# A create is ~3 statements plus ONE commit; on a real volume that commit is
# where the time goes. This measures exactly that: N autocommit single-row
# INSERTs (one commit each, like one create) on the same schema shape, varying
# only journal_mode / synchronous, on two filesystems:
#
#   $STORE   the volume deployments actually use   (here: xfs on /data)
#   $TMPFS   RAM, i.e. the same code with the fsync taken out
#
# It uses the system libsqlite3 (the engine links rusqlite; version stated in
# the output) so the numbers are a floor for the commit path, not a GroveFS
# end-to-end figure — the e2e figure comes from probe_sqlite_create_breakdown.sh.
#
# Usage: N=500 bash .build-bin/probe_wal_modes.sh
set -u
cd "$(dirname "$0")/.." || exit 1
export LC_ALL=C

N=${N:-500}
STORE=${STORE:-/data/grovefs/grovefs/.probe_wal}
TMPFS=${TMPFS:-/dev/shm/.probe_wal}

cleanup() { rm -rf "$STORE" "$TMPFS"; }
trap cleanup EXIT
rm -rf "$STORE" "$TMPFS"; mkdir -p "$STORE" "$TMPFS"

python3 - "$N" "$STORE" "$TMPFS" <<'PY'
import os, sqlite3, statistics, sys, time

n, *dirs = sys.argv[1:]
n = int(n)
print(f"libsqlite3={sqlite3.sqlite_version} N={n} commits/config")

SCHEMA = """
create table node(
  id integer primary key autoincrement,
  parent_id integer not null,
  name text not null,
  mode integer not null,
  size integer not null default 0,
  data_ref text
);
create index node_parent_name on node(parent_id, name);
create table ledger(data_ref text primary key, holders integer not null);
"""

def run(tag, path, mode, sync, tmpfs):
    db = os.path.join(path, f"{mode}-{sync}.db")
    c = sqlite3.connect(db, isolation_level=None)
    c.executescript(SCHEMA)
    c.execute(f"pragma journal_mode={mode}")
    got = c.execute("pragma journal_mode").fetchone()[0]
    c.execute(f"pragma synchronous={sync}")
    got_sync = c.execute("pragma synchronous").fetchone()[0]
    per = []
    t_all = time.perf_counter()
    for i in range(n):
        t0 = time.perf_counter()
        c.execute("begin immediate")
        c.execute("insert into node(parent_id,name,mode) values(1,?,420)", (f"f{i}",))
        c.execute("insert into ledger(data_ref,holders) values(?,1)", (f"r{i}",))
        c.execute("commit")
        per.append((time.perf_counter() - t0) * 1000)
    wall = time.perf_counter() - t_all
    c.close()
    os.unlink(db)
    for suf in ("-wal", "-shm"):
        try: os.unlink(db + suf)
        except FileNotFoundError: pass
    fs = "tmpfs" if tmpfs else "fs"
    print(f"ROW {tag:8s} {fs:5s} journal={got:5s} sync={got_sync} "
          f"p50_ms={statistics.median(per):8.3f} mean_ms={sum(per)/n:8.3f} "
          f"ops_s={n/wall:8.1f}")

for tmpfs, path in ((False, dirs[0]), (True, dirs[1])):
    run("wal-norm", path, "wal", "normal", tmpfs)
    run("wal-off", path, "wal", "off", tmpfs)
    run("wal-full", path, "wal", "full", tmpfs)
    run("del-norm", path, "delete", "normal", tmpfs)
PY
