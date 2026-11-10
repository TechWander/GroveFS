#!/usr/bin/env bash
# Is the distro's setuid `fusermount` helper a real runtime dependency of the
# package's mount step? strace CANNOT answer this (ptracing a setuid binary drops
# the privilege, so fusermount fails with EPERM for the wrong reason). The clean
# probe is fuser's own escape hatch: FUSERMOUNT_PATH points the helper lookup at a
# file that does not exist, leaving everything else in the package untouched.
set -u
D=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
W=/tmp/pkgfh
URI=fdb:///data/grovefs/.fdbtest/test.cluster
MP=57812; DP=57811; VIEW=pkgfh
rm -rf "$W"; mkdir -p "$W"/{home,data,h1,mnt}
echo "uid=$(id -u) user=$(id -un)  helper on PATH: $(command -v fusermount fusermount3 | tr '\n' ' ')"

env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" data-server --listen 127.0.0.1:$DP \
    --data-dir "$W/data" >"$W/data.log" 2>&1 &
DPID=$!
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" meta-server --listen 127.0.0.1:$MP \
    --home "$W/home" --meta-store "$URI" >"$W/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 20); do grep -q 'metadata backend foundationdb' "$W/meta.log" && break; sleep 1; done
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" view --meta-store "$URI" --meta-dir "$W/home" \
    create --view "$VIEW" >"$W/create.log" 2>&1; echo "view create rc=$?"

echo "=== A) CONTROL: helper present (default lookup) ==="
env -i PATH=/usr/bin:/bin HOME="$W/h1" "$D/grovefs" --home "$W/h1" mount --view "$VIEW" \
    --mountpoint "$W/mnt" --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP \
    --foreground >"$W/a.log" 2>&1 &
AID=$!
FA=
for _ in $(seq 1 25); do
    FA=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FA" in fuse*) break ;; esac
    sleep 1
done
echo "control fstype=${FA:-none}"
if [ "$FA" = "${FA#fuse}" ]; then echo "control did NOT mount — aborting probe"; kill $AID $MPID $DPID 2>/dev/null; exit 1; fi
echo "hello" > "$W/mnt/probe.txt" && echo "control write ok: $(cat "$W/mnt/probe.txt")"
kill $AID 2>/dev/null; sleep 1; fusermount -u "$W/mnt" 2>/dev/null; umount "$W/mnt" 2>/dev/null; sleep 1
echo "after unmount, /proc/mounts has it? $(grep -c "$W/mnt" /proc/mounts)"

echo "=== B) PROBE: FUSERMOUNT_PATH=/nonexistent (no helper reachable at all) ==="
env -i PATH=/usr/bin:/bin FUSERMOUNT_PATH=/nonexistent HOME="$W/h1" "$D/grovefs" --home "$W/h1" \
    mount --view "$VIEW" --mountpoint "$W/mnt" --meta-server 127.0.0.1:$MP \
    --data-servers 127.0.0.1:$DP --foreground >"$W/b.log" 2>&1 &
BID=$!
FB=
RC=timeout
for _ in $(seq 1 15); do
    FB=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FB" in fuse*) break ;; esac
    kill -0 $BID 2>/dev/null || { RC=exited; break; }
    sleep 1
done
echo "probe fstype=${FB:-none}  process state=$RC"
echo "--- probe log ---"; tail -4 "$W/b.log"
echo "--- did bytes land in the BARE directory (the false-success trap)? ---"
ls -A "$W/mnt" | sed 's/^/  entry: /'
kill $BID 2>/dev/null
kill $MPID $DPID 2>/dev/null; wait 2>/dev/null
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "writemode on; clearrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01" >/dev/null 2>&1
rm -rf "$W"
echo PKG_FUSERMOUNT_PROBE_DONE
