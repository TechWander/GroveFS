#!/usr/bin/env bash
# Which short-lived fdb:// CLI invocations crash? Count segfaults per command
# shape over N runs, so "the import refusal path" is either confirmed as the
# specific trigger or shown to be one sample of a generic short-lived-process race.
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=fdb:///data/grovefs/.fdbtest/test.cluster
N=${N:-10}
printf '{"format":"grovefs-view-metadata","version":1,"nodes":1}\n{"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1,"data_ref":null,"link_target":null,"mode":493,"uid":0,"gid":0,"mtime":1,"atime":1,"ctime":1}\n' > /tmp/seg/good.jsonl
printf '{"format":"other","version":1}\n' > /tmp/seg/bad.jsonl

count(){ # count <label> <shell command template using $1 as iteration tag>
    local label=$1 c=0 rc i
    for i in $(seq 1 $N); do
        eval "${2//@/$i}" >/dev/null 2>&1; rc=$?
        [ $rc -eq 139 ] && c=$((c+1))
        [ $rc -gt 1 ] && [ $rc -ne 139 ] && echo "  (odd rc=$rc)"
    done
    echo "$label: segv=$c/$N"
}

rm -rf /tmp/seg/R; mkdir -p /tmp/seg/R/meta/views
count "list (no fdb writes)"        "$Y view --meta-store $CL --meta-dir /tmp/seg/R list"
count "create (writes a subspace)"  "$Y view --meta-store $CL --meta-dir /tmp/seg/R create --view cc@"
count "export ghost (refusal)"      "$Y view --meta-store $CL --meta-dir /tmp/seg/R export --view nope@ --out /tmp/seg/R/e@.jsonl"
count "import bad format (refusal)" "$Y view --meta-store $CL --meta-dir /tmp/seg/R import --view bf@ --in /tmp/seg/bad.jsonl"
count "import good (success)"       "$Y view --meta-store $CL --meta-dir /tmp/seg/R import --view ok@ --in /tmp/seg/good.jsonl"
count "export real (success)"       "$Y view --meta-store $CL --meta-dir /tmp/seg/R export --view ok@ --out /tmp/seg/R/x@.jsonl --force"
count "version (no fdb at all)"     "$Y --version"
