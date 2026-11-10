#!/usr/bin/env bash
# Same shape that reproduced the crash, with the SIGSEGV forensics preloaded.
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=fdb:///data/grovefs/.fdbtest/test.cluster
P=$PWD/.bneck/seghnd.so
hits=0
for i in $(seq 1 30); do
    LD_PRELOAD=$P $Y view --meta-store "$CL" --meta-dir /tmp/seg/A import --view "sa$i" --in /tmp/seg/bad.jsonl >/dev/null 2>/tmp/seg/err.log
    if [ $? -eq 139 ]; then hits=$((hits+1)); cp /tmp/seg/err.log /tmp/seg/trace_$hits.log; fi
done
echo "segv=$hits/30"
[ $hits -gt 0 ] && cat /tmp/seg/trace_1.log
