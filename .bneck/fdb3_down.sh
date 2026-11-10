#!/usr/bin/env bash
# Tear the G3 RF=3 cluster down: only the pids listening on the three reserved
# ports, and only inside $ROOT.  The single-node test cluster on :4500 (used by
# the fdb test suite) is deliberately left alone.
set -u
ROOT=${FDB3_ROOT:-/data/grovefs/.fdb3}
BIN=/data/grovefs/.fdbbin
for p in ${FDB3_PORTS:-4510 4511 4512}; do
  for pid in $(ss -lntp 2>/dev/null | awk -v p=":$p" '$4 ~ p {print $NF}' | sed -n 's/.*pid=\([0-9]*\).*/\1/p'); do
    exe=$(readlink -f /proc/$pid/exe 2>/dev/null)
    case "$exe" in $BIN/fdbserver.x86_64) echo "kill $pid ($exe)"; kill $pid ;; esac
  done
done
sleep 2
for pid in $(pgrep -f "fdbserver.x86_64.*$ROOT" 2>/dev/null); do kill -9 $pid 2>/dev/null; done
rm -rf $ROOT
echo "removed $ROOT; remaining G3 servers: $(pgrep -fc "fdbserver.x86_64.*$(basename $ROOT)" 2>/dev/null || echo 0)"
for p in ${FDB3_PORTS:-4510 4511 4512}; do (echo >/dev/tcp/127.0.0.1/$p) 2>/dev/null && echo "$p still BUSY" || echo "$p free"; done
echo "test cluster on 4500: $( (echo >/dev/tcp/127.0.0.1/4500) 2>/dev/null && echo alive || echo DOWN )"
