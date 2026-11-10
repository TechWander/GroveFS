#!/usr/bin/env bash
# End-to-end self-sufficiency proof for the FDB delivery package
# (dist/grovefs-fdb-experimental-linux-x86_64): EVERY process is started with
# `env -i` — no inherited environment, no LD_LIBRARY_PATH, no GROVEFS_* — and
# only from files inside the package directory.
set -u
D=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
W=/tmp/pkgverify
URI=fdb:///data/grovefs/.fdbtest/test.cluster
MP=57782; DP=57781
VIEW=pkgv

rm -rf "$W"; mkdir -p "$W"/{home,data,h1,mnt}

echo "=== 1) data-server (package binary, empty env) ==="
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" data-server --listen 127.0.0.1:$DP \
    --data-dir "$W/data" >"$W/data.log" 2>&1 &
DPID=$!
echo "=== 2) meta-server on the FDB authority (empty env) ==="
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" meta-server --listen 127.0.0.1:$MP \
    --home "$W/home" --meta-store "$URI" >"$W/meta.log" 2>&1 &
MPID=$!
for _ in $(seq 1 20); do
    grep -q 'metadata backend foundationdb' "$W/meta.log" 2>/dev/null && break
    sleep 1
done
echo "--- meta-server banner ---"; head -8 "$W/meta.log"
if ! grep -q 'metadata backend foundationdb' "$W/meta.log" 2>/dev/null; then
    echo "FATAL: meta-server did not report the foundationdb backend — silent downgrade"
    kill $MPID $DPID 2>/dev/null
    exit 1
fi

echo "=== 3) view create + list over fdb:// (empty env) ==="
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" view --meta-store "$URI" --meta-dir "$W/home" \
    create --view "$VIEW" >"$W/create.log" 2>&1; echo "create rc=$?"; head -2 "$W/create.log"
env -i PATH=/usr/bin:/bin HOME="$W" "$D/grovefs" view --meta-store "$URI" --meta-dir "$W/home" \
    list >"$W/list.log" 2>&1; echo "list rc=$?"; grep -i "$VIEW" "$W/list.log" | head -2

echo "=== 4) mount + real file I/O (empty env) ==="
env -i PATH=/usr/bin:/bin HOME="$W/h1" "$D/grovefs" --home "$W/h1" mount --view "$VIEW" \
    --mountpoint "$W/mnt" --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP \
    --foreground >"$W/mnt.log" 2>&1 &
MNTID=$!
# Poll the FSTYPE of the mountpoint itself: `findmnt -T <path>` succeeds for ANY
# existing path (it reports the backing fs), so "did findmnt return 0" is not a
# readiness gate — that mistake kills the servers under a mount still starting up
# and then reports "Connection refused" as if the package were broken.
FSTYPE=
for _ in $(seq 1 30); do
    FSTYPE=$(findmnt -n -o FSTYPE -T "$W/mnt" 2>/dev/null || true)
    case "$FSTYPE" in fuse*) break ;; esac
    sleep 1
done
# FAIL-FAST: writing before the mount is alive would land the bytes in the bare
# directory and "prove" a self-sufficiency claim that never tested the mount path.
if ! printf '%s' "$FSTYPE" | grep -q '^fuse'; then
    echo "FATAL: mount not alive (fstype='${FSTYPE:-none}') — aborting before any write"
    tail -8 "$W/mnt.log"
    kill $MNTID $MPID $DPID 2>/dev/null
    exit 1
fi
echo "mount alive: fstype=$FSTYPE"
echo "self-sufficient-pkg-write" > "$W/mnt/a.txt" 2>&1 && echo "write rc=0"
echo "read back: $(cat "$W/mnt/a.txt" 2>&1)"
mkdir "$W/mnt/dir1" 2>&1 && echo "mkdir rc=0"; ls "$W/mnt"

echo "=== 5) the subspace really holds the tree (raw read with the package fdbcli) ==="
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "getrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01 4" \
    2>&1 | grep 'is ' | head -4

echo "=== 6) teardown ==="
fusermount -u "$W/mnt" 2>&1 || umount "$W/mnt" 2>&1
sleep 1
kill $MNTID 2>/dev/null; kill $MPID $DPID 2>/dev/null
wait 2>/dev/null

echo "=== 7) leave the cluster as found ==="
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "writemode on; clearrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01" \
    2>&1 | tail -1
echo "keys left under the probe view: $(env -i PATH=/usr/bin:/bin \
    FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    "$D/fdb/fdbcli" --exec "getrange \x01grovefs\x00\x02$VIEW\x00 \x01grovefs\x00\x02$VIEW\x01 4" \
    2>&1 | grep -c 'is ')"
rm -rf "$W"
echo PKG_VERIFY_DONE
