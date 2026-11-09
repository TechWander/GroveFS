set -u
# Upper-bound evidence for splice/zero-copy: how much userspace CPU the daemon
# spends per GiB read, split daemon-side (proc stat delta) between the two paths.
# drop_caches needs sudo; IO runs as the mount owner (FUSE hides the mount from root).
B=/tmp/splicetest2
G=/data/grovefs/grovefs/target/release/grovefs
PID=$(pgrep -f "grovefs mount --view alice --mountpoint $B/mnt" | head -1)
[ -n "$PID" ] || { echo "no mount pid"; exit 1; }
cpu_ticks() { awk '{print $14+$15}' /proc/$PID/stat; }
dropc() { sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }

dd if=/dev/urandom of=$B/src1g.bin bs=1M count=1024 status=none
cp $B/src1g.bin $B/mnt/g1g.bin; sync

echo "--- ext4 baseline: read 1GiB cold (kernel page-cache path only) ---"
dropc
T0=$(cpu_ticks); S=$(date +%s.%N)
dd if=$B/src1g.bin of=/dev/null bs=1M status=none; T1=$(cpu_ticks); E=$(date +%s.%N)
echo "ext4 cold read wall: $(python3 -c "print(f'{$E-$S:.3f}')") s"

echo "--- grovefs buffered read 1GiB cold through mount ---"
dropc
T0=$(cpu_ticks); S=$(date +%s.%N)
dd if=$B/mnt/g1g.bin of=/dev/null bs=1M status=none
T1=$(cpu_ticks); E=$(date +%s.%N)
echo "buffered wall: $(python3 -c "print(f'{$E-$S:.3f}')") s, daemon ticks +$((T1-T0))"

echo "--- grovefs O_DIRECT read 1GiB (bypass kernel cache; per-4MiB daemon copies) ---"
T0=$(cpu_ticks); S=$(date +%s.%N)
dd if=$B/mnt/g1g.bin of=/dev/null bs=1M iflag=direct status=none
T1=$(cpu_ticks); E=$(date +%s.%N)
echo "direct wall: $(python3 -c "print(f'{$E-$S:.3f}')") s, daemon ticks +$((T1-T0))"

HZ=$(getconf CLK_TCK)
echo "clk_tck=$HZ; 1 tick = $((1000000/HZ)) us CPU; src=$((1<<30)) bytes"
