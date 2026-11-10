#!/usr/bin/env bash
# Does the delivery package need the distro's FUSE userspace helper (fusermount)
# to mount? `env -i PATH=/nonexistent` removes it from PATH (it is NOT a hardcoded
# absolute path in our code, so a missing PATH is the honest experiment).
set -u
D=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
W=/tmp/pkgfuse
URI=fdb:///data/grovefs/.fdbtest/test.cluster
MP=57792; DP=57791; VIEW=pkgfuse
rm -rf "$W"; mkdir -p "$W"/{home,data,h1,mnt}

env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" data-server --listen 127.0.0.1:$DP \
    --data-dir "$W/data" >"$W/data.log" 2>&1 &
DPID=$!
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" meta-server --listen 127.0.0.1:$MP \
    --home "$W/home" --meta-store "$URI" >"$W/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 20); do
    grep -q 'metadata backend foundationdb' "$W/meta.log" && break
    sleep 1
done
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" view --meta-store "$URI" --meta-dir "$W/home" \
    create --view "$VIEW" >"$W/create.log" 2>&1
echo "view create rc=$?"

echo "=== control: mount with PATH=/usr/bin:/bin (fusermount present) ==="
which fusermount fusermount3 2>/dev/null
env -i PATH=/usr/bin:/bin HOME="$W/h1" "$D/grovefs" --home "$W/h1" mount --view "$VIEW" \
    --mountpoint "$W/mnt" --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP \
    --foreground >"$W/mnt_ok.log" 2>&1 &
OKID=$!
FS=
for _ in $(seq 1 20); do
    FS=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FS" in fuse*) break ;; esac
    sleep 1
done
echo "control fstype=${FS:-none}"
kill $OKID 2>/dev/null; sleep 1
fusermount -u "$W/mnt" 2>/dev/null; umount "$W/mnt" 2>/dev/null
sleep 1

echo "=== probe: same mount with an EMPTY PATH (no fusermount reachable) ==="
env -i PATH=/nonexistent HOME="$W/h1" "$D/grovefs" --home "$W/h1" mount --view "$VIEW" \
    --mountpoint "$W/mnt" --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP \
    --foreground >"$W/mnt_nopath.log" 2>&1 &
NPID=$!
FS2=
for _ in $(seq 1 15); do
    FS2=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FS2" in fuse*) break ;; esac
    sleep 1
done
echo "nopath fstype=${FS2:-none}"
echo "--- log ---"; tail -5 "$W/mnt_nopath.log"
kill $NPID 2>/dev/null
[ "$FS2" = fuse ] || [ "$FS2" = fuse.grovefs ] && fusermount -u "$W/mnt" 2>/dev/null
kill $MPID $DPID 2>/dev/null; wait 2>/dev/null
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "writemode on; clearrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01" >/dev/null 2>&1
rm -rf "$W"
echo PKG_FUSE_PROBE_DONE
