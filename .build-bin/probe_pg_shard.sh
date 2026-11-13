#!/usr/bin/env bash
# View sharding prototype: ONE meta-server, N PostgreSQL databases, each view's
# tree on exactly one of them, chosen by a stable hash of the view name.
#
# Checks (all fail-fast):
#   1. `view create --meta-store "uri1,uri2"` puts each view's schema on EXACTLY
#      one instance (count in db1 + count in db2 == 1 per view), and a realistic
#      set of names actually spreads over both;
#   2. ONE meta-server serves views of BOTH instances (mount works for a view
#      living on either side, and the create writes land in the routed schema);
#   3. the cross-view content ledger is per instance: deleting every name of
#      shard A empties db1's `grovefs_refs.holders` and leaves db2's untouched.
#
# Boundaries (documented in USAGE): `grovefs_control` (keys/defs/audit) stays on
# the FIRST instance; the content ledger lives INSIDE each database, so a shard
# must also own its content directory (one data-server per shard — sharing blobs
# across instances would let one instance free bytes the other still names);
# `refs rebuild` therefore runs once per instance.
#
# Usage: bash .build-bin/probe_pg_shard.sh
set -u
cd "$(dirname "$0")/.." || exit 1
export LC_ALL=C
export PGPASSWORD=grovepw

ROOT=.probe_pg_shard
Y=./target/release/grovefs
PG1="postgres://grove:grovepw@127.0.0.1:5433/grovefs"
PG2="postgres://grove:grovepw@127.0.0.1:5433/grovefs_shard2"
SPEC="$PG1,$PG2"
DATA1=127.0.0.1:15456
DATA2=127.0.0.1:15457
META=127.0.0.1:15458
PSQL="psql -h 127.0.0.1 -p 5433 -U grove -tAq"
# The probe needs a SECOND database; role `grove` has no CREATEDB, so the
# create/drop goes through the local superuser (passwordless sudo here).
ADMIN="sudo -n -u postgres psql -p 5433 -tAq"
N=${N:-400}

cleanup() {
    fusermount -u "$ROOT/mnt1" 2>/dev/null
    fusermount -u "$ROOT/mnt2" 2>/dev/null
    [ -n "${MPID:-}" ] && kill "$MPID" 2>/dev/null
    [ -n "${D1PID:-}" ] && kill "$D1PID" 2>/dev/null
    [ -n "${D2PID:-}" ] && kill "$D2PID" 2>/dev/null
    sleep 0.5
    for v in s1 s2 s3 s4 s5 s6 s7 s8; do
        $Y view --meta-store "$SPEC" rm --base --name "$v" >/dev/null 2>&1
    done
    $ADMIN -c "DROP DATABASE IF EXISTS grovefs_shard2" >/dev/null 2>&1
    rm -rf "$ROOT"
}
trap cleanup EXIT

hexof() { printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
schema_count() { $PSQL -d "$1" -c "select count(*) from pg_namespace where nspname='$2'"; }
holders() { $PSQL -d "$1" -c "select coalesce(sum(holders),0) from grovefs_refs.holders" 2>/dev/null || echo 0; }

rm -rf "$ROOT"; mkdir -p "$ROOT/mnt1" "$ROOT/mnt2" "$ROOT/data1" "$ROOT/data2"
$ADMIN -c "DROP DATABASE IF EXISTS grovefs_shard2" >/dev/null || exit 1
$ADMIN -c "CREATE DATABASE grovefs_shard2 OWNER grove" >/dev/null || exit 1
$PSQL -d grovefs_shard2 -c "select 1" >/dev/null || { echo "SHARD2_NOT_REACHABLE_AS_GROVE"; exit 1; }

echo "===== 1) view create routes each tree to exactly one instance ====="
for i in 1 2 3 4 5 6 7 8; do
    v="s$i"
    $Y view --meta-store "$SPEC" create --view "$v" >/dev/null || { echo "CREATE_FAILED $v"; exit 1; }
    sch="grovefs_v_$(hexof "$v")"
    a=$(schema_count grovefs "$sch"); b=$(schema_count grovefs_shard2 "$sch")
    [ "$a" = "" ] && a=0; [ "$b" = "" ] && b=0
    echo "  $v -> $sch  db1=$a db2=$b"
    [ $((a + b)) -eq 1 ] || { echo "ROUTING_FAILED $v (expected exactly one instance)"; exit 1; }
    if [ "$b" = "1" ]; then SHARD2_VIEW="$v"; else SHARD1_VIEW="$v"; fi
done
[ -n "${SHARD1_VIEW:-}" ] && [ -n "${SHARD2_VIEW:-}" ] || { echo "NO_SPREAD: every view landed on one instance"; exit 1; }
echo "  spread: $SHARD2_VIEW on db2, $SHARD1_VIEW on db1"

echo "===== 2) one meta-server serves both shards ====="
$Y data-server --listen $DATA1 --data-dir "$ROOT/data1" </dev/null >"$ROOT/data1.log" 2>&1 &
D1PID=$!
$Y data-server --listen $DATA2 --data-dir "$ROOT/data2" </dev/null >"$ROOT/data2.log" 2>&1 &
D2PID=$!
$Y meta-server --meta-store "$SPEC" --listen $META --token t </dev/null >"$ROOT/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 100); do (echo >/dev/tcp/127.0.0.1/${META##*:}) 2>/dev/null && break; sleep 0.1; done
kill -0 $MPID 2>/dev/null || { echo META_NOT_STARTED; tail -5 "$ROOT/meta.log"; exit 1; }
grep -q "sharded" "$ROOT/meta.log" && echo "  meta-server acknowledges the sharded store"

# Each shard owns its content directory: a blob is only ever named by views of
# the instance whose ledger books it (see the header).
run_view() {
    local v=$1 mp=$2 ds=$3
    $Y mount --view "$v" --meta-server 127.0.0.1:${META##*:} --data-servers "$ds" --token t \
        --threads 4 --mountpoint "$mp" --foreground </dev/null >"$ROOT/mnt-$v.log" 2>&1 &
    local pid=$!
    for _ in $(seq 1 100); do findmnt -T "$mp" 2>/dev/null | grep -q grovefs && break; sleep 0.1; done
    findmnt -T "$mp" 2>/dev/null | grep -q grovefs || { echo "MOUNT_FAILED $v"; tail -3 "$ROOT/mnt-$v.log"; exit 1; }
    python3 - "$mp" "$N" "$v" <<'PY'
import os, sys, time
mp, n, v = sys.argv[1], int(sys.argv[2]), sys.argv[3]
blob = b"x" * 4096
t0 = time.perf_counter()
for i in range(n):
    p = os.path.join(mp, f"{v}-{i}")
    with open(p, "wb") as f:
        f.write(blob + v.encode() + str(i).encode())
dt = time.perf_counter() - t0
print(f"  {v}: {n} files in {dt*1000:.0f}ms ({dt/n*1000:.3f} ms/op)")
PY
    fusermount -u "$mp" 2>/dev/null
    wait "$pid" 2>/dev/null
}

run_view "$SHARD2_VIEW" "$ROOT/mnt1" "$DATA2"
run_view "$SHARD1_VIEW" "$ROOT/mnt2" "$DATA1"

echo "===== 3) rows landed in the routed schema ====="
for v in "$SHARD2_VIEW" "$SHARD1_VIEW"; do
    sch="grovefs_v_$(hexof "$v")"
    # The side that does not own the schema answers with an error; read it as 0.
    a=$($PSQL -d grovefs -c "select count(*) from $sch.node where parent_id=1" 2>/dev/null)
    b=$($PSQL -d grovefs_shard2 -c "select count(*) from $sch.node where parent_id=1" 2>/dev/null)
    [ "$a" = "" ] && a=0; [ "$b" = "" ] && b=0
    echo "  $v: rows db1=$a db2=$b (expect one side == $N, other == 0)"
    [ $((a + b)) -eq "$N" ] || { echo "ROW_COUNT_MISMATCH $v"; exit 1; }
done

echo "===== 4) the content ledger is per instance (shard = its own blob domain) ===="
# Every file written above has unique content, so each blob is named by exactly
# one tree: the ledger rows those blobs own must be db1's alone.
sch1="grovefs_v_$(hexof "$SHARD1_VIEW")"
sch2="grovefs_v_$(hexof "$SHARD2_VIEW")"
hold1() { $PSQL -d grovefs -c "select coalesce(sum(holders),0) from grovefs_refs.holders where data_ref in (select data_ref from $sch1.node where data_ref is not null)" 2>/dev/null || echo 0; }
hold2() { $PSQL -d grovefs_shard2 -c "select coalesce(sum(holders),0) from grovefs_refs.holders where data_ref in (select data_ref from $sch2.node where data_ref is not null)" 2>/dev/null || echo 0; }
h1_before=$(hold1); h2_before=$(hold2)
all2_before=$(holders grovefs_shard2)
echo "  shard-A ledger total=$h1_before, shard-B ledger total=$h2_before (db2 grand total=$all2_before)"
[ "${h1_before:-0}" -ge "$N" ] || { echo "SHARD1_LEDGER_NOT_BOOKED ($h1_before < $N)"; exit 1; }
[ "${h2_before:-0}" -ge "$N" ] || { echo "SHARD2_LEDGER_NOT_BOOKED ($h2_before < $N)"; exit 1; }
# Drop every name of the db1 shard through its own mount; only db1's ledger moves.
mp="$ROOT/mnt2"
$Y mount --view "$SHARD1_VIEW" --meta-server 127.0.0.1:${META##*:} --data-servers "$DATA1" --token t \
    --threads 4 --mountpoint "$mp" --foreground </dev/null >"$ROOT/mnt-del.log" 2>&1 &
delpid=$!
for _ in $(seq 1 100); do findmnt -T "$mp" 2>/dev/null | grep -q grovefs && break; sleep 0.1; done
rm -f "$mp"/* || { echo "DELETE_FAILED"; exit 1; }
sync
fusermount -u "$mp" 2>/dev/null
wait "$delpid" 2>/dev/null
sleep 1
h1_after=$(hold1); h2_after=$(hold2); all2_after=$(holders grovefs_shard2)
echo "  after rm: shard-A=$h1_after shard-B=$h2_after (db2 grand total=$all2_after)"
[ "${h1_after:-1}" -eq 0 ] || { echo "SHARD1_LEDGER_NOT_DRAINED ($h1_after)"; exit 1; }
[ "${all2_after:-0}" -eq "${all2_before:-0}" ] || { echo "CROSS_SHARD_LEDGER_LEAK ($all2_before -> $all2_after)"; exit 1; }

echo "===== 5) refs rebuild refuses a multi-instance spec ====="
out=$($Y refs --meta-store "$SPEC" rebuild 2>&1) && { echo "REBUILD_SHOULD_HAVE_REFUSED"; exit 1; }
echo "$out" | grep -q "one instance at a time" || { echo "UNHELPFUL_REBUILD_ERROR: $out"; exit 1; }
echo "  refused with: $(echo "$out" | head -1 | cut -c1-100)…"

echo "SHARD PROBE PASSED"
