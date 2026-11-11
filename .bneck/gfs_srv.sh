#!/bin/sh
# Remote service helper shipped to /opt/gfsprod/srv.sh by .bneck/fdb3vm_lease.sh.
#
# Why a helper instead of one long ssh command line:
#   * `setsid` WITHOUT -f leaves the launcher as the grandparent of a
#     long-lived server, so sudo never returns and the ssh channel hangs
#     until the timeout (measured: 45s, rc=124/1). `setsid -f` forks and the
#     launcher returns in ~280ms; the server keeps running.
#   * stopping by `pkill -f 'meta-server --meta-dir X'` matches the ssh
#     command line that carries the pattern, so the clean-up kills its own
#     session. A pidfile written by the server itself is unambiguous.
#
# usage: srv.sh start <meta|data> <dir> <port> <waitsecs> <fdb-uri|->
#        srv.sh stop  <meta|data> <dir> <port>
#        srv.sh alive <meta|data> <dir> <port>
#        srv.sh log   <meta|data> <dir> <port>
set -u
act=${1:-}
role=${2:-}
dir=${3:-}
port=${4:-}
name=$(basename "$dir")
log=/tmp/gfs_${role}_${name}_${port}.log
pidf=/tmp/gfs_${role}_${name}_${port}.pid
bin=/opt/gfsprod/grovefs

case $act in
start)
    wait=${5:-10}
    uri=${6:-}
    # Truncate the log so a poll never matches the PREVIOUS server's banner.
    : > "$log"
    rm -f "$pidf"
    if [ "$role" = meta ]; then
        GROVEFS_FDB_WRITER_WAIT_SECS=$wait
        export GROVEFS_FDB_WRITER_WAIT_SECS
        setsid -f sh -c "echo \$\$ > $pidf; exec $bin meta-server --meta-dir $dir --listen 0.0.0.0:$port --meta-store $uri >> $log 2>&1" </dev/null >/dev/null 2>&1
    else
        setsid -f sh -c "echo \$\$ > $pidf; exec $bin data-server --listen 0.0.0.0:$port --data-dir $dir >> $log 2>&1" </dev/null >/dev/null 2>&1
    fi
    # Wait until the pidfile exists so the caller can poll the log immediately.
    for _ in $(seq 1 50); do [ -s "$pidf" ] && exit 0; sleep 0.1; done
    echo "start: no pidfile after 5s (log: $(tail -1 "$log" 2>/dev/null))" >&2
    exit 1
    ;;
stop)
    p=$(cat "$pidf" 2>/dev/null || true)
    [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null
    rm -f "$pidf"
    sleep 0.3
    if [ -n "${p:-}" ] && kill -0 "$p" 2>/dev/null; then
        echo "alive:$p"
    else
        echo "stopped"
    fi
    ;;
alive)
    p=$(cat "$pidf" 2>/dev/null || true)
    [ -n "${p:-}" ] && kill -0 "$p" 2>/dev/null && exit 0
    exit 1
    ;;
log)
    [ -f "$log" ] && cat "$log"
    exit 0
    ;;
*)
    echo "usage: srv.sh start|stop|alive|log <meta|data> <dir> <port> [wait] [uri]" >&2
    exit 2
    ;;
esac
