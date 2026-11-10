#!/usr/bin/env bash
# Measurement behind the USAGE claim that `--engine-idle-secs` under `fdb://`
# reclaims an IN-PROCESS engine only (there is no per-view connection pool to
# close, and the session gate still holds a mounted view). Runs on scratch ports
# 15286/15287 so it cannot collide with the suites' 15186-15188.
set -u
cd /data/grovefs/grovefs
export LC_ALL=C LANG=C
FDB_HOME=/data/grovefs/.fdbbin
CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-/data/grovefs/.fdbtest/test.cluster}"
[ -f "$CLUSTER" ] || { echo "PROBE: no cluster file at $CLUSTER"; exit 1; }
export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"
Y=./target/fdb/release/grovefs
W=/tmp/gvev; rm -rf $W; mkdir -p $W/data $W/mnt
V="ev$(date +%s)"
MP=127.0.0.1:15286; DP=127.0.0.1:15287
for a in $MP $DP; do
    (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && { echo "PROBE: port $a already listening"; exit 1; }
done
$Y view --meta-store "fdb://$CLUSTER" --meta-dir $W create --view "$V" > $W/create.log 2>&1 \
    || { echo "PROBE: view create failed"; cat $W/create.log; exit 1; }
$Y data-server --listen $DP --data-dir $W/data > $W/data.log 2>&1 & DP_PID=$!
$Y meta-server --listen $MP --home $W --meta-store "fdb://$CLUSTER" --engine-idle-secs 3 > $W/meta.log 2>&1 & MP_PID=$!
cleanup(){ fusermount -u $W/mnt 2>/dev/null; kill $MP_PID $DP_PID 2>/dev/null; wait $MP_PID 2>/dev/null; wait $DP_PID 2>/dev/null; true; }
trap cleanup EXIT
for _ in $(seq 1 100); do (echo >/dev/tcp/127.0.0.1/15286) 2>/dev/null && break; sleep 0.05; done
grep -q "metadata backend foundationdb" $W/meta.log || { echo "PROBE: backend banner missing"; head -5 $W/meta.log; exit 1; }
$Y --home $W mount --view "$V" -m $W/mnt --meta-server $MP --data-servers $DP --foreground > $W/mnt.log 2>&1 & MNT=$!
mounted=0
for _ in $(seq 1 60); do
    findmnt -rn --mountpoint "$W/mnt" >/dev/null 2>&1 && { mounted=1; break; }
    sleep 0.25
done
[ "$mounted" = 1 ] || { echo "PROBE: the view never mounted"; cat $W/mnt.log; exit 1; }
timeout 10 sh -c "printf x > $W/mnt/f" && echo "PROBE: wrote through the mount"
echo "PROBE: eviction lines while MOUNTED (expect none): $(grep -c evicted $W/meta.log)"
fusermount -u $W/mnt 2>/dev/null; kill $MNT 2>/dev/null; wait $MNT 2>/dev/null
echo "PROBE: unmounted; sleeping 12s (> the 3s idle threshold)"
sleep 12
echo "PROBE: eviction lines AFTER UNMOUNT:"
grep evicted $W/meta.log || echo "PROBE: NONE"
