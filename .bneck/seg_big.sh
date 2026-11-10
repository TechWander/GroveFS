#!/usr/bin/env bash
# Crash-rate recheck at higher N, plus proof the refusal still says the same
# thing with rc=1 (the fix must not change the verdict, only the teardown).
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=fdb:///data/grovefs/.fdbtest/test.cluster
N=${N:-100}
rm -rf /tmp/seg/pC; mkdir -p /tmp/seg/pC
for i in $(seq 1 $N); do
    ( $Y view --meta-store "$CL" --meta-dir /tmp/seg/home2 import \
        --view "big$i" --in /tmp/seg/bad.jsonl > /tmp/seg/pC/$i.log 2>&1; echo $? > /tmp/seg/pC/$i.rc ) &
    if [ $((i % 20)) -eq 0 ]; then wait; fi
done
wait
echo "segv=$(cat /tmp/seg/pC/*.rc | grep -c 139)/$N"
echo "rc1 =$(cat /tmp/seg/pC/*.rc | grep -c '^1$')/$N"
sort -u /tmp/seg/pC/1.log | head -3
