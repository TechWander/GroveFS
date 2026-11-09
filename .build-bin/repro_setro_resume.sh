# §5-7 forensics: after `apikey set-ro --rw`, does a LIVE mount resume writes,
# and when? Distinguish "stale re-check window (probe timing)" from "sticky".
set -u
cd /data/grovefs/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=$PWD/.build-bin/pkg-config
Y=./target/release/grovefs
META=127.0.0.1:15310
DATA=127.0.0.1:15311
BASE=$PWD/.s7test/central
rm -rf .s7test; mkdir -p "$BASE/data" "$BASE/meta" .s7test/mnt
$Y view --meta-dir "$BASE" create --view alice >/dev/null 2>&1
$Y data-server --listen $DATA --data-dir "$BASE/data" --require-api-key --meta-dir "$BASE" > .s7test/data.log 2>&1 &
PD=$!
$Y meta-server --meta-dir "$BASE" --listen $META --require-api-key > .s7test/meta.log 2>&1 &
PM=$!
wait_port(){ for _ in $(seq 1 100); do (echo >/dev/tcp/${1%%:*}/${1##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
wait_port $META; wait_port $DATA
C=$($Y apikey --meta-dir "$BASE" create --label s7 --for mount 2>&1)
KEY=$(echo "$C" | sed -n 's/^  key:   //p')
ID=$(echo "$C" | sed -n 's/^  id:    //p')
env -u GROVEFS_API_KEY timeout 600 $Y --home .s7test/home mount --view alice --mountpoint .s7test/mnt \
  --meta-server $META --data-servers $DATA --api-key "$KEY" --foreground > .s7test/mnt.log 2>&1 &
PNT=$!
sleep 2.5
grep -qs " $PWD/.s7test/mnt " /proc/mounts || { echo "MOUNT NOT LIVE — aborting"; cat .s7test/mnt.log; kill $PD $PM 2>/dev/null; exit 1; }
probe() { # prints errno of a write+fsync ladder, or ok
  python3 - "$1" <<'EOF'
import os,sys
fd=os.open(sys.argv[1],os.O_WRONLY|os.O_APPEND)
try:
    os.write(fd,b"x\n")
    try:
        os.fsync(fd); print("write+fsync ok")
    except OSError as e:
        print(f"fsync errno={e.errno} ({os.strerror(e.errno)})")
except OSError as e:
    print(f"write errno={e.errno} ({os.strerror(e.errno)})")
os.close(fd)
EOF
}
F=.s7test/mnt/f.txt
python3 -c "open('$F','wb').write(b'first\n')" && sync "$F"
echo "=== baseline rw write:"; probe "$F"
$Y apikey --meta-dir "$BASE" set-ro --id "$ID"
sleep 1.5
echo "=== after set-ro (ro in effect):"; probe "$F"
$Y apikey --meta-dir "$BASE" set-ro --id "$ID" --rw
echo "=== immediately after resume-rw (same tick may still be stale):"; probe "$F"
sleep 0.5; echo "=== +0.5s:"; probe "$F"
sleep 1.0; echo "=== +1.5s:"; probe "$F"
sleep 1.0; echo "=== +2.5s:"; probe "$F"
echo "=== content now:"; od -c "$F" | head -3
fusermount -u .s7test/mnt 2>/dev/null; kill $PNT $PD $PM 2>/dev/null; wait 2>/dev/null
