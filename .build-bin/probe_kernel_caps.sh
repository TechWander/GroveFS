set -u
# Kernel-capability matrix probe: run identically on any Linux host with /dev/fuse.
# Usage: BASE=<scratch dir> KV=<version tag> bash probe_kernel_caps.sh
cd "${BASE:-/tmp}/.." 2>/dev/null || true
B="${BASE:?set BASE to a scratch dir}"
G="${GROVEFS_BIN:?path to the release binary}"
rm -rf "$B" && mkdir -p "$B/home" "$B/mnt"
export GROVEFS_HOME="$B/home"
echo "=== kernel: $(uname -r) ($(grep PRETTY /etc/os-release | cut -d'"' -f2)) ==="
echo "=== fuse param files: $(ls /sys/module/fuse/parameters/ 2>/dev/null | tr '\n' ' ')"

"$G" init --view alice >"$B/init.log" 2>&1 || { echo "init FAILED"; cat "$B/init.log"; exit 1; }
"$G" mount --view alice --mountpoint "$B/mnt" --foreground --stats > "$B/mnt.log" 2>&1 &
MPID=$!
sleep 2
grep -qs " $B/mnt " /proc/mounts || { echo "MOUNT NOT LIVE"; cat "$B/mnt.log"; exit 1; }
echo "=== negotiated caps echo (from mount log) ==="
grep "FUSE caps granted" "$B/mnt.log" || echo "(no caps line)"

echo "=== O_TRUNC round-trip A/B (setattr delta over .grove-stats O_DIRECT) ==="
python3 - "$B/mnt" <<'EOF'
import os, sys, time, re
M = sys.argv[1]
def ops():
    fd = os.open(f"{M}/.grove-stats", os.O_RDONLY | os.O_DIRECT)
    raw = os.read(fd, 65536); os.close(fd)
    t = {}
    for line in raw.decode("utf-8", "replace").splitlines():
        m = re.search(r"^\s+(\S+)\s+ops=(\d+)", line)
        if m: t[m.group(1)] = int(m.group(2))
    return t
with open(f"{M}/t.bin", "wb") as f:
    f.write(b"x" * (1 << 20)); os.fsync(f.fileno())
time.sleep(3)
a = ops()
t0 = time.perf_counter()
for _ in range(1000):
    fd = os.open(f"{M}/t.bin", os.O_WRONLY | os.O_TRUNC); os.close(fd)
t1 = time.perf_counter()
time.sleep(1)
b = ops()
print(f"O_TRUNC open+close: {(t1-t0)*1e6/1000:.1f} us/op")
for k in sorted(set(a) | set(b)):
    d = b.get(k, 0) - a.get(k, 0)
    if d: print(f"  {k:24s} +{d}")
t0 = time.perf_counter()
for _ in range(1000):
    fd = os.open(f"{M}/t.bin", os.O_WRONLY); os.close(fd)
t1 = time.perf_counter()
print(f"plain WRONLY open+close: {(t1-t0)*1e6/1000:.1f} us/op")
EOF

echo "=== large-block read/write floor (64MiB, page-cache-warm) ==="
dd if=/dev/urandom of="$B/src.bin" bs=1M count=64 status=none
python3 - "$B" <<'EOF'
import sys, subprocess, time, os
B = sys.argv[1]
def t(cmd):
    s = time.perf_counter(); subprocess.run(cmd, shell=True, check=True); return time.perf_counter() - s
dur = t(f"cp {B}/src.bin {B}/mnt/f64.bin && sync"); print(f"write 64M: {dur:.3f} s ({64/dur:.0f} MB/s)")
dur = t(f"dd if={B}/mnt/f64.bin of=/dev/null bs=1M status=none"); print(f"read 64M(warm): {dur:.3f} s ({64/dur:.0f} MB/s)")
dur = t(f"dd if={B}/src.bin of={B}/dst.ext4.bin bs=1M status=none"); print(f"read ext4 64M: {dur:.3f} s ({64/dur:.0f} MB/s)")
os.remove(f"{B}/dst.ext4.bin")
EOF

echo "=== threads>1 (clone_fd) live on this kernel ==="
grep -c "n_threads\|clone" "$B/mnt.log" 2>/dev/null || true
"$G" mount --help 2>&1 | grep -A1 "threads" | head -3

kill $MPID 2>/dev/null; sleep 1
fusermount -u "$B/mnt" 2>/dev/null; wait 2>/dev/null
echo "=== done ==="
