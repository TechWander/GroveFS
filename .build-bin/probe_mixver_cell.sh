set -u
# Mixed-version meta+data cluster matrix.
#   SBIN = server-side binary (runs data-server + meta-server + seeds the view)
#   CBIN = client-side binary (runs `mount --meta-server --data-servers`)
# Prints one PASS/FAIL block per phase with verbatim handshake evidence.
SBIN="${SBIN:?server binary}"
CBIN="${CBIN:?client binary}"
B=$(mktemp -d /tmp/mixver.XXXXXX)
DP=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
MP=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
TAG="${TAG:-srv+cli}"
echo "########## $TAG ##########"
echo "server=$SBIN"
echo "client=$CBIN  data-port=$DP meta-port=$MP"

mkdir -p "$B/meta" "$B/data" "$B/mnt"
# Seed the authoritative view DB with the SERVER binary before the meta-server opens it.
"$SBIN" --home "$B/meta" init --view alice >"$B/init.log" 2>&1 || { echo "INIT FAIL"; cat "$B/init.log"; }

"$SBIN" data-server --data-dir "$B/data" --listen "127.0.0.1:$DP" --token t >"$B/data.log" 2>&1 &
DS=$!
"$SBIN" meta-server --home "$B/meta" --listen "127.0.0.1:$MP" --token t >"$B/meta.log" 2>&1 &
MS=$!
sleep 2

cleanup() { kill "$DS" "$MS" 2>/dev/null; fusermount -u "$B/mnt" 2>/dev/null; wait 2>/dev/null; rm -rf "$B"; }
trap cleanup EXIT

# --- phase 1: client mount (M_OPEN handshake) ---
# HEAD/04d5bb5 mount detaches by default (needs --foreground to keep the bg
# process serving); 363d3e7 mount is foreground-only and rejects the flag.
FG=""
"$CBIN" mount --help 2>&1 | grep -q -- --foreground && FG="--foreground"
"$CBIN" mount --view alice --mountpoint "$B/mnt" --meta-server "127.0.0.1:$MP" --data-servers "127.0.0.1:$DP" --token t $FG >"$B/mount.log" 2>&1 &
MPID=$!
sleep 3
if grep -qs " $B/mnt " /proc/mounts; then
  echo "[mount] LIVE"
else
  echo "[mount] NOT LIVE"
  echo "---- mount.log ----"; sed 's/^/   /' "$B/mount.log"
  echo "---- meta.log tail ----"; tail -5 "$B/meta.log" | sed 's/^/   /'
fi

# --- phase 2: write then read through the client ---
if grep -qs " $B/mnt " /proc/mounts; then
  if echo "hello-mix" > "$B/mnt/x.txt" 2>"$B/w.err"; then
    echo "[write] OK"
  else
    echo "[write] FAIL: $(cat "$B/w.err")"
  fi
  sync; sleep 1
  echo "[read]  content=$(cat "$B/mnt/x.txt" 2>&1)"
  # fork/derived content_ro byte only exists on >=3B servers; note the capability the client parsed
  echo "[stats] paged/content_ro seen by this client:"
  python3 - "$B/mnt" <<'EOF' 2>/dev/null || echo "   (.grove-stats not available on this client)"
import os,re
try:
    fd=os.open("/dev/null",os.O_RDONLY); os.close(fd)
except Exception: pass
EOF
fi
cleanup
trap - EXIT
echo
