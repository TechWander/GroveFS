#!/usr/bin/env bash
# Which path does the package's mount actually take: mount(2) or the distro's
# setuid fusermount helper? strace answers it; run as the unprivileged user.
set -u
D=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
W=/tmp/pkgstrace
URI=fdb:///data/grovefs/.fdbtest/test.cluster
MP=57802; DP=57801; VIEW=pkgstrace
rm -rf "$W"; mkdir -p "$W"/{home,data,h1,mnt}
echo "whoami=$(id -un) uid=$(id -u)  cap-bounds: $(capsh --print 2>/dev/null | grep -o 'Current:.*' | head -c 120)"

env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" data-server --listen 127.0.0.1:$DP \
    --data-dir "$W/data" >"$W/data.log" 2>&1 &
DPID=$!
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" meta-server --listen 127.0.0.1:$MP \
    --home "$W/home" --meta-store "$URI" >"$W/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 20); do grep -q 'metadata backend foundationdb' "$W/meta.log" && break; sleep 1; done
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" view --meta-store "$URI" --meta-dir "$W/home" \
    create --view "$VIEW" >"$W/create.log" 2>&1; echo "view create rc=$?"

echo "=== A) strace the mount process: mount(2) vs execve of fusermount ==="
env -i PATH=/usr/bin:/bin HOME="$W/h1" strace -f -e trace=mount,execve -o "$W/strace.log" \
    "$D/grovefs" --home "$W/h1" mount --view "$VIEW" --mountpoint "$W/mnt" \
    --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground >"$W/mnt.log" 2>&1 &
SID=$!
FS=
for _ in $(seq 1 60); do
    FS=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FS" in fuse*) break ;; esac
    sleep 1
done
echo "fstype=${FS:-none}"
sleep 1
echo "--- every mount(2) syscall and its result ---"
grep -aE '[^ ]+ mount\(' "$W/strace.log" | head -6
echo "--- fusermount-related execves ---"
grep -a 'execve(".*fusermount' "$W/strace.log" | head -6
echo "--- last 6 log lines ---"; tail -6 "$W/strace.log"
echo "--- mount stdout/stderr ---"; tail -6 "$W/mnt.log"
echo "--- /proc/mounts line ---"; grep -a "$W/mnt" /proc/mounts

kill $SID 2>/dev/null; sleep 1
fusermount -u "$W/mnt" 2>/dev/null; umount "$W/mnt" 2>/dev/null
kill $MPID $DPID 2>/dev/null; wait 2>/dev/null
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "writemode on; clearrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01" >/dev/null 2>&1
rm -rf "$W"
echo PKG_STRACE_DONE
