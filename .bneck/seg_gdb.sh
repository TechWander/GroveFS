#!/usr/bin/env bash
# Chase the intermittent SIGSEGV on the FDB import refusal path under gdb, with
# concurrency to widen the race the serial run keeps winning.
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=fdb:///data/grovefs/.fdbtest/test.cluster
rm -rf /tmp/seg/g; mkdir -p /tmp/seg/g
for round in 1 2 3; do
    for i in $(seq 1 8); do
        gdb -batch -ex run -ex "bt full" -ex "info threads" -ex "thread apply all bt 8" \
            --args $Y view --meta-store "$CL" --meta-dir /tmp/seg/A import \
            --view "g${round}_$i" --in /tmp/seg/bad.jsonl > /tmp/seg/g/r${round}_$i.log 2>&1 &
    done
    wait
    if grep -l "SIGSEGV" /tmp/seg/g/*.log >/dev/null 2>&1; then
        echo "HIT:"; grep -l "SIGSEGV" /tmp/seg/g/*.log
        cp "$(grep -l SIGSEGV /tmp/seg/g/*.log | head -1)" /tmp/seg/hit.log
        break
    fi
    echo "round $round: no segv"
done
