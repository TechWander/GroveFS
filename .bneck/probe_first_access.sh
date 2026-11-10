#!/usr/bin/env bash
# How long is the FIRST access to a mount of a freshly imported subspace?
# Measures it, because an assertion that waits 10s and still sees nothing has to
# be told apart from a mount that takes 10s to answer its first getattr.
set -u
cd /data/grovefs/grovefs
export LD_LIBRARY_PATH=/data/grovefs/.fdbbin/lib
Y=./target/fdb/release/grovefs
CL=/data/grovefs/.fdbtest/test.cluster
MS=127.0.0.1:15420
DS=127.0.0.1:15421
W=/tmp/probe; V="pt$(date +%s)"
rm -rf $W; mkdir -p $W/home/meta/views $W/data $W/h1 $W/mnt
printf '{"format":"grovefs-view-metadata","version":1,"nodes":1}\n{"id":1,"ino":1,"parent_id":null,"name":"/","is_dir":1,"data_ref":null,"link_target":null,"mode":493,"uid":0,"gid":0,"mtime":1,"atime":1,"ctime":1}\n' > $W/root.jsonl

free(){ ! (echo >/dev/tcp/${1%%:*}/${1##*:}) 2>/dev/null; }
for p in $MS $DS; do free $p || { echo "port $p busy"; exit 1; }; done

$Y view --meta-store "fdb://$CL" --meta-dir $W/home import --view "$V" --in $W/root.jsonl > $W/imp.log 2>&1 \
    || { echo "import failed"; cat $W/imp.log; exit 1; }
echo "imported $V"
$Y data-server --listen $DS --data-dir $W/data > $W/data.log 2>&1 & DP=$!
$Y meta-server --listen $MS --home $W/home --meta-store "fdb://$CL" > $W/meta.log 2>&1 & MP=$!
for _ in $(seq 1 100); do (echo >/dev/tcp/${MS%%:*}/${MS##*:}) 2>/dev/null && break; sleep 0.05; done
trap 'fusermount -u $W/mnt 2>/dev/null; kill $MP $DP 2>/dev/null' EXIT

t0=$(date +%s%N)
$Y --home $W/h1 mount --view "$V" --mountpoint $W/mnt --meta-server $MS --data-servers $DS \
    --foreground > $W/mnt.log 2>&1 &
# Wait for the kernel to have the mount ATACHED: until then the path is the plain
# empty directory mkdir made, and `[ -d ]` there proves nothing about the daemon.
ATTACH=""
for i in $(seq 1 400); do
    if findmnt -rn --mountpoint "$W/mnt" >/dev/null 2>&1; then
        ATTACH=$(( ( $(date +%s%N) - t0 ) / 1000000 )); break
    fi
    sleep 0.05
done
echo "findmnt saw the mount at: ${ATTACH:-never}ms"
UP=""
for i in $(seq 1 400); do
    if [ -d $W/mnt ]; then UP=$(( ( $(date +%s%N) - t0 ) / 1000000 )); break; fi
    sleep 0.05
done
echo "first [ -d ] answered by the daemon: ${UP:-never}ms (after mount start)"
t1=$(date +%s%N)
ls $W/mnt > $W/ls.log 2>&1; rc=$?
echo "ls rc=$rc took $(( ( $(date +%s%N) - t1 ) / 1000000 ))ms: $(tr '\n' ' ' < $W/ls.log)"
timeout 5 stat -c '%i %a %n' $W/mnt 2>&1 | head -2
grep -c . $W/mnt.log >/dev/null && echo "--- mount log:" && head -5 $W/mnt.log
echo "--- meta log tail:"; tail -3 $W/meta.log
