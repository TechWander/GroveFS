#!/usr/bin/env bash
# Regression probe for the "one permanent fd per object" defect.
#
# Runs a data-server under a deliberately small RLIMIT_NOFILE (512), writes
# $N distinct objects through a FUSE mount, then reads every byte back. The
# old cache kept one fd open per object forever, so it hit EMFILE a few hundred
# objects in; the bounded cache must serve all of them with a fd count that
# never approaches the limit.
#
#   bash .bneck/probe_fd_emfile.sh [path/to/grovefs-binary]
set -u
cd /data/grovefs/grovefs
export LC_ALL=C LANG=C
Y=${1:-./target/release/grovefs}
N=${N:-2000}
LIMIT=${LIMIT:-512}
ROOTS=${ROOTS:-1}   # --data-dir count: the fd budget must be SPLIT across them
B=$PWD/.bneck/fdtest
D=127.0.0.1:16920
M=127.0.0.1:16921

rm -rf "$B"
mkdir -p "$B/central" "$B/home" "$B/mnt"
ROOTLIST=""
for r in $(seq 1 $ROOTS); do mkdir -p "$B/data$r"; ROOTLIST="${ROOTLIST:+$ROOTLIST,}$B/data$r"; done

wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# Only the data-server gets the small fd budget — that is the component under test.
( ulimit -n $LIMIT; exec "$Y" data-server --listen $D --data-dir "$ROOTLIST" ) > "$B/data.log" 2>&1 &
DP=$!
"$Y" meta-server --meta-dir "$B/central" --listen $M > "$B/meta.log" 2>&1 & MP=$!
wait_port $D || { echo "FAIL data-server down"; cat "$B/data.log"; kill $MP 2>/dev/null; exit 1; }
wait_port $M || { echo "FAIL meta-server down"; cat "$B/meta.log"; kill $DP 2>/dev/null; exit 1; }
"$Y" view --meta-dir "$B/central" create --view v1 > "$B/create.log" 2>&1 || { cat "$B/create.log"; kill $DP $MP; exit 1; }

"$Y" --home "$B/home" mount --view v1 --mountpoint "$B/mnt" \
    --meta-server $M --data-servers $D --threads 8 --foreground > "$B/mnt.log" 2>&1 &
FP=$!
cleanup(){ fusermount -u "$B/mnt" 2>/dev/null; kill $FP $DP $MP 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq 1 100); do mountpoint -q "$B/mnt" && break; sleep 0.1; done

pattern(){ printf 'obj-%s-payload' "$1"; }

echo "== write $N objects to a data-server with RLIMIT_NOFILE=$LIMIT =="
fail=0
for i in $(seq 1 $N); do
  if ! pattern "$i" > "$B/mnt/f$i" 2>"$B/err"; then
    fail=$i
    break
  fi
done
fds=$(ls /proc/$DP/fd 2>/dev/null | wc -l)
if [ "$fail" != 0 ]; then
  echo "  FAIL: write #$fail returned an error: $(cat "$B/err")"
  echo "  data-server fds at failure: $fds (limit $LIMIT)"
else
  echo "  OK: all $N objects written; data-server fds=$fds (limit $LIMIT)"
fi

echo "== read every object back =="
bad=0
for i in $(seq 1 $N); do
  [ "$fail" != 0 ] && [ "$i" -ge "$fail" ] && { bad=$((bad+1)); continue; }
  got=$(cat "$B/mnt/f$i" 2>/dev/null)
  [ "$got" = "$(pattern "$i")" ] || { bad=$((bad+1)); [ $bad -le 3 ] && echo "  mismatch at f$i: '$got'"; }
done
echo "  verified $((N-bad))/$N objects byte-exact"
echo "== data-server fds after read-back: $(ls /proc/$DP/fd 2>/dev/null | wc -l) =="

if [ "$fail" = 0 ] && [ "$bad" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
grep -i "too many open files\|EMFILE" "$B/data.log" "$B/mnt.log" 2>/dev/null | head -3
