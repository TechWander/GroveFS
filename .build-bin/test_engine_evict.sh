#!/usr/bin/env bash
# Idle-engine eviction e2e (meta-server --engine-idle-secs). Proves the
# resident-connection wall around view count is gone: under a PG-backed
# meta-server each OPEN view holds 8 writers + 4 readers; after its last
# session closes and the idle window passes, the sweeper must retire the
# engine and RELEASE those connections, and the next mount must re-open
# the same tree transparently (content intact, writes durable).
#   A) N mounted views hold ~= N*12 grove backends in pg_stat_activity;
#   B) after unmount + idle window the count collapses toward zero and
#      the server logs the eviction;
#   C) remount reads pre-eviction data and writes post-eviction data;
#   D) the PG schema rows survived the engine's whole life-cycle.
# Requires PostgreSQL on $PGHOST:$PGPORT (default 127.0.0.1:5433) with the
# grove/grovepw role on the grovefs database; self-skips if unreachable.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5433}
PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"
q(){ PGPASSWORD=grovepw psql -h "$PGHOST" -p "$PGPORT" -U grove -d grovefs -tAX -c "$1" 2>/dev/null; }

if ! (exec 3<>/dev/tcp/${PGHOST}/${PGPORT}) 2>/dev/null; then
    echo "SUMMARY: pass=0 fail=0 SKIP=1 (no PostgreSQL on ${PGHOST}:${PGPORT})"
    exit 0
fi
exec 3<&- 2>/dev/null; exec 3<&- 2>/dev/null || true

META=127.0.0.1:15250
DATA=127.0.0.1:15251
IDLE=3                       # engine idle window (s); sweep fires at 1.5s
N=6                          # views cycled through the eviction life-cycle
S=.evicttest
TAG="ev$(date +%s)"
rm -rf $S; mkdir -p $S/central/data $S/home
for i in $(seq 0 $((N-1))); do mkdir -p $S/mnt$i $S/h$i; done

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
# Mandatory fail-fast gate (lesson from the bench harness): a mount that
# never came up must abort, not silently write into the bare directory.
mnt_gate(){ local mp="$PWD/$S/mnt$1"; for _ in $(seq 1 60); do
    findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }

conns(){ q "SELECT count(*) FROM pg_stat_activity WHERE usename='grove' AND pid<>pg_backend_pid()"; }

cleanup(){
    for i in $(seq 0 $((N-1))); do fusermount -u $S/mnt$i 2>/dev/null; done
    kill $MP $DP "${MNT_PIDS[@]}" 2>/dev/null
    wait 2>/dev/null
    for i in $(seq 0 $((N-1))); do
        vh=$(printf '%s' "${TAG}v$i" | od -An -tx1 | tr -d ' \n')
        q "DROP SCHEMA IF EXISTS grovefs_v_${vh} CASCADE" >/dev/null
    done
}
trap cleanup EXIT
MNT_PIDS=()

echo "===== 0) provision $N PG views, start meta-server (--engine-idle-secs $IDLE) ====="
for i in $(seq 0 $((N-1))); do
    $Y view --meta-store "$PGURI" --meta-dir $S/home create --view "${TAG}v$i" > $S/create$i.log 2>&1 \
        || { bad "view create ${TAG}v$i"; cat $S/create$i.log; exit 1; }
done
$Y data-server --listen $DATA --data-dir $S/central/data > $S/data.log 2>&1 & DP=$!
$Y meta-server --listen $META --home $S/home --meta-store "$PGURI" \
    --engine-idle-secs $IDLE > $S/meta.log 2>&1 & MP=$!
wait_port $DATA || { bad "data-server did not bind"; exit 1; }
wait_port $META || { bad "meta-server did not bind"; cat $S/meta.log; exit 1; }
ok "servers up"

echo "===== 1) mount all $N views concurrently: connections scale with OPEN views ====="
for i in $(seq 0 $((N-1))); do
    $Y --home $S/h$i mount --view "${TAG}v$i" --mountpoint $S/mnt$i \
        --meta-server $META --data-servers $DATA --foreground > $S/mnt$i.log 2>&1 &
    MNT_PIDS+=($!)
done
for i in $(seq 0 $((N-1))); do
    mnt_gate $i || { bad "mount $i never came up (fail-fast, no fake numbers)"; exit 1; }
    sh -c "printf 'pre-%s' '$i' > $S/mnt$i/pre.txt" || bad "write into mount $i failed"
done
sleep 1
C1=$(conns)
# $N views * (8 writers + 4 readers) expected resident; tolerate in-flight
# freshness churn but require clearly past "a couple of pools" territory.
[ "$C1" -ge $(( N * 8 )) ] && ok "$C1 grove backends while all $N views mounted (>= N*8 writers)" \
                           || bad "only $C1 backends with $N mounted views (expected >= $((N*8)))"

echo "===== 2) unmount everything, wait out the idle window: connections collapse ====="
for i in $(seq 0 $((N-1))); do fusermount -u $S/mnt$i 2>/dev/null; done
for p in "${MNT_PIDS[@]}"; do wait "$p" 2>/dev/null; done
sleep $(( IDLE * 2 + 3 ))    # window + at least one sweep tick of slack
C2=$(conns)
[ "$C2" -le 12 ] && ok "backends dropped to $C2 after eviction (was $C1; <= one view's pool)" \
                 || bad "backends still $C2 after eviction — sweeper not releasing pools"
grep -q "idle engine" $S/meta.log \
    && ok "meta-server logged the eviction sweep" \
    || { bad "no eviction log line in meta.log"; tail -3 $S/meta.log; }

echo "===== 3) remount after eviction: transparent reopen, data intact, writes durable ====="
$Y --home $S/h0 mount --view "${TAG}v0" --mountpoint $S/mnt0 \
    --meta-server $META --data-servers $DATA --foreground > $S/remnt.log 2>&1 &
MNT_PIDS=($!)
mnt_gate 0 || { bad "remount after eviction failed to come up"; cat $S/remnt.log; exit 1; }
got=$(cat $S/mnt0/pre.txt 2>/dev/null)
[ "$got" = "pre-0" ] && ok "pre-eviction content readable through the re-opened engine" \
                    || bad "post-eviction read got '$got' want 'pre-0'"
sh -c "printf 'post' > $S/mnt0/post.txt" && ok "post-eviction write accepted" || bad "post-eviction write failed"
sleep 2
vh=$(printf '%s' "${TAG}v0" | od -An -tx1 | tr -d ' \n')
rows=$(q "SELECT count(*) FROM grovefs_v_${vh}.node WHERE name IN ('pre.txt','post.txt')")
[ "$rows" = "2" ] && ok "both rows present in the PG schema (single source of truth survived)" \
                 || bad "PG row count for pre/post = '$rows' want 2"
fusermount -u $S/mnt0 2>/dev/null; wait "${MNT_PIDS[0]}" 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "ALL ENGINE-EVICTION CHECKS PASSED"; exit 0; else echo "SOME ENGINE-EVICTION CHECKS FAILED"; exit 1; fi
