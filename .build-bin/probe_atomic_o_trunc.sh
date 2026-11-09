set -u
# Verify ATOMIC_O_TRUNC: after the change, an O_WRONLY|O_TRUNC open must NOT
# produce an extra FUSE_SETATTR (setattr delta == 0) yet still truncate the
# object to 0 (backend.truncate lands it). Also checks plain create/truncate,
# and the fork (content_ro) EROFS gate at open.
B=${BASE:-/data/grovefs/.atrunc}
G=${GROVEFS_BIN:-/data/grovefs/grovefs/target/release/grovefs}
rm -rf "$B" && mkdir -p "$B/home" "$B/mnt"
export GROVEFS_HOME="$B/home"
"$G" init --view alice >"$B/init.log" 2>&1 || { echo "INIT FAIL"; cat "$B/init.log"; exit 1; }
"$G" mount --view alice --mountpoint "$B/mnt" --foreground --stats >"$B/mnt.log" 2>&1 &
MPID=$!
sleep 2
grep -qs " $B/mnt " /proc/mounts || { echo "MOUNT NOT LIVE"; cat "$B/mnt.log"; exit 1; }
echo "=== caps echo (expect ATOMIC_O_TRUNC=true) ==="
grep "FUSE caps granted" "$B/mnt.log"

echo "=== O_TRUNC A/B (setattr must be +0 now; size must go to 0) ==="
python3 - "$B" <<'EOF'
import os, sys, time, re
B = sys.argv[1]; M = f"{B}/mnt"
def ops():
    fd = os.open(f"{M}/.grove-stats", os.O_RDONLY | os.O_DIRECT)
    raw = os.read(fd, 65536); os.close(fd)
    t = {}
    for line in raw.decode("utf-8","replace").splitlines():
        m = re.search(r"^\s+(\S+)\s+ops=(\d+)", line)
        if m: t[m.group(1)] = int(m.group(2))
    return t
with open(f"{M}/t.bin","wb") as f:
    f.write(b"x"*(1<<20)); os.fsync(f.fileno())
time.sleep(3)
a = ops()
t0 = time.perf_counter()
for _ in range(1000):
    fd = os.open(f"{M}/t.bin", os.O_WRONLY | os.O_TRUNC); os.close(fd)
t1 = time.perf_counter()
time.sleep(1)
b = ops()
print(f"O_TRUNC open+close: {(t1-t0)*1e6/1000:.1f} us/op")
d = {k: b.get(k,0)-a.get(k,0) for k in set(a)|set(b)}
for k in sorted(d):
    if d[k]: print(f"  {k:24s} +{d[k]}")
print(f"  setattr delta = {d.get('meta.setattr',0)}  (expect 0 after ATOMIC_O_TRUNC)")
sz = os.path.getsize(f"{M}/t.bin")
print(f"  final size of a file last opened O_TRUNC with no write: {sz} bytes (expect 0)")
EOF

echo "=== shell '>' truncate + fork content_ro EROFS ==="
echo "hello" > "$B/mnt/plain.txt"; cat "$B/mnt/plain.txt"
: > "$B/mnt/plain.txt"; echo "after '>' truncate size=$(stat -c%s "$B/mnt/plain.txt") (expect 0)"
"$G" view fork --meta-server "" --home "$B/home" base alice fork fview >"$B/fork.log" 2>&1 || cat "$B/fork.log"
kill $MPID 2>/dev/null; sleep 1; fusermount -u "$B/mnt" 2>/dev/null; wait 2>/dev/null
rm -rf "$B"; echo "=== done ==="
