#!/usr/bin/env bash
# Bisect which local-root state makes the FDB import refusal path segfault.
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
CL=fdb:///data/grovefs/.fdbtest/test.cluster
Y=./target/fdb/release/grovefs
printf '{"format":"other","version":1}\n' > /tmp/seg/bad.jsonl

run(){ # run <label> <root> <tag> <in-file> [extra args...]
    local label=$1 root=$2 tag=$3 inf=$4; shift 4
    local c=0 n=8 rc
    for i in $(seq 1 $n); do
        $Y view --meta-store "$CL" --meta-dir "$root" import --view "$tag$i" --in "$inf" "$@" >/dev/null 2>&1
        rc=$?; [ $rc -eq 139 ] && c=$((c+1))
    done
    echo "$label: segv=$c/$n"
}

rm -rf /tmp/seg/A /tmp/seg/B /tmp/seg/C; mkdir -p /tmp/seg/A/meta/views /tmp/seg/B/meta/views /tmp/seg/C/meta/views
cp /tmp/seg/home2/meta/api_keys.db /tmp/seg/B/meta/ 2>/dev/null
cp /tmp/seg/home2/meta/views/*.layout /tmp/seg/C/meta/views/ 2>/dev/null
run "empty-root"      /tmp/seg/A sa /tmp/seg/bad.jsonl
run "root+api_keys"   /tmp/seg/B sb /tmp/seg/bad.jsonl
run "root+layouts"    /tmp/seg/C sc /tmp/seg/bad.jsonl
