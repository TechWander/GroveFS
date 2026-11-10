#!/usr/bin/env bash
# Close the one open cell in the package's mount matrix: does the mount step skip
# the setuid helper when the process has CAP_SYS_ADMIN? No root on this machine,
# but `unshare -r -m` gives a uid-0 mapping + full caps inside a private mount
# namespace, which is exactly the credential fuser's direct mount(2) branch needs.
# FUSERMOUNT_PATH=/nonexistent is set for the WHOLE run, so success can only come
# from mount(2) — and the namespace is torn down with the mount inside it.
set -u
D=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
W=/tmp/pkgroot
VIEW=pkgroot
MP=57822; DP=57821
LOG="$W/result.log"
rm -rf "$W"; mkdir -p "$W"/{home,data,h1,mnt}

unshare -r -m --propagation private bash -eu -c '
D='"$D"'; W='"$W"'; VIEW='"$VIEW"'; MP='"$MP"'; DP='"$DP"'
export FUSERMOUNT_PATH=/nonexistent            # helper unreachable for EVERY process here
echo "inside ns: id=$(id -u) $(id -un)  caps=$(grep CapEff /proc/self/status | awk "{print \$2}")"
if [ -e /bin/fusermount ]; then echo "note: /bin/fusermount EXISTS on disk, but FUSERMOUNT_PATH points the lookup away from it"; fi

"$D/grovefs" data-server --listen 127.0.0.1:$DP --data-dir "$W/data" >"$W/data.log" 2>&1 &
DPID=$!
"$D/grovefs" meta-server --listen 127.0.0.1:$MP --home "$W/home" \
    --meta-store fdb:///data/grovefs/.fdbtest/test.cluster >"$W/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 20); do grep -q "metadata backend foundationdb" "$W/meta.log" && break; sleep 1; done
grep -q "metadata backend foundationdb" "$W/meta.log" || { echo "FATAL: no fdb banner"; exit 1; }
"$D/grovefs" view --meta-store fdb:///data/grovefs/.fdbtest/test.cluster --meta-dir "$W/home" \
    create --view "$VIEW" >"$W/create.log" 2>&1
echo "view create rc=$?"
"$D/grovefs" --home "$W/h1" mount --view "$VIEW" --mountpoint "$W/mnt" \
    --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground >"$W/mnt.log" 2>&1 &
MID=$!
FS=
for _ in $(seq 1 30); do
    FS=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FS" in fuse*) break ;; esac
    kill -0 $MID 2>/dev/null || { FS=dead; break; }
    sleep 1
done
echo "MOUNT fstype=${FS:-none}   (helper was unreachable the whole time)"
if [ "${FS#fuse}" = "$FS" ]; then
    echo "--- mount log ---"; tail -4 "$W/mnt.log"
else
    echo "direct-write" > "$W/mnt/a.txt" && echo "write ok, read back: $(cat "$W/mnt/a.txt")"
    mkdir "$W/mnt/d1" && echo "mkdir ok"
    grep "$W/mnt" /proc/mounts | sed "s/^/  /"
    umount "$W/mnt" && echo "umount(2) direct ok, entries left: $(ls -A "$W/mnt" | wc -l)"
fi
kill $MID $MPID $DPID 2>/dev/null
wait 2>/dev/null
' > "$LOG" 2>&1
echo "namespace rc=$?"; cat "$LOG"

echo "=== leave the cluster as found ==="
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "writemode on; clearrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01" >/dev/null 2>&1
echo "keys left: $(env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "getrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01 4" 2>&1 | grep -c ' is ')"
rm -rf "$W"
echo PKG_ROOTNS_DONE
