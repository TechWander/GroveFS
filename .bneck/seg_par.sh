#!/usr/bin/env bash
# Load makes the race: fire N concurrent short-lived fdb:// refusals, with and
# without the SIGSEGV forensics preload, and report both rates.
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=fdb:///data/grovefs/.fdbtest/test.cluster
P=$PWD/.bneck/seghnd.so
N=${N:-40}
tag=${TAG:-par}

fire(){ # fire <root> <outdir> <preload-or-empty>
    local root=$1 out=$2 pre=$3 i
    rm -rf $out; mkdir -p $out
    for i in $(seq 1 $N); do
        if [ -n "$pre" ]; then
            ( LD_PRELOAD=$pre $Y view --meta-store "$CL" --meta-dir "$root" import \
                --view "${tag}p$i" --in /tmp/seg/bad.jsonl >$out/$i.log 2>&1; echo $? >$out/$i.rc ) &
        else
            ( $Y view --meta-store "$CL" --meta-dir "$root" import \
                --view "$tag$i" --in /tmp/seg/bad.jsonl >$out/$i.log 2>&1; echo $? >$out/$i.rc ) &
        fi
    done
    wait
    local c; c=$(cat $out/*.rc | grep -c 139)
    echo "$4: segv=$c/$N"
}

fire /tmp/seg/home2 /tmp/seg/pA ""     "plain"
fire /tmp/seg/home2 /tmp/seg/pB "$P"   "preloaded"
grep -A20 "SIGSEGV" /tmp/seg/pB/*.log 2>/dev/null | head -30
