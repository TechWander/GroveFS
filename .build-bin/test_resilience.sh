#!/usr/bin/env bash
# Remote data-path RECONNECT resilience for grovefs. Unlike test_remote.sh (which
# checks happy-path round trips), this drives a real failure/recovery cycle by
# killing and restarting the actual `data-server` process at a FIXED address,
# while the client mount stays up the whole time:
#
#   A) seed a file (warms a pooled connection)
#   B) server DOWN  -> creating a file fails fast/gracefully (bounded, no hang,
#      client process alive); the local metadata view still lists files.
#   C) server RESTARTED (same addr+root) -> the same mount recovers: a create
#      succeeds via a fresh connect, prior data persisted on the shared root.
#   D) live pool + immediate restart -> next op silently retries over a dead
#      pooled socket and succeeds (transparent reconnect, app sees no error).
#   E) two client mounts against one server write concurrently and stay isolated.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
ADDR=127.0.0.1:50098
HOST=${ADDR%:*}; PORT=${ADDR#*:}

rm -rf .rtest; mkdir -p .rtest/home .rtest/rsrv .rtest/mnt_a .rtest/mnt_b
export GROVEFS_HOME=$PWD/.rtest/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

SP=""; MA=""; MB=""
umount_all(){ fusermount -u .rtest/mnt_a 2>/dev/null; fusermount -u .rtest/mnt_b 2>/dev/null; }
cleanup(){ umount_all; kill ${MA:-} ${MB:-} ${SP:-} 2>/dev/null; }
trap cleanup EXIT

wait_port(){ for _ in $(seq 1 50); do (exec 3<>"/dev/tcp/$HOST/$PORT") 2>/dev/null && { exec 3>&- 3<&-; return 0; }; sleep 0.1; done; return 1; }
start_server(){ # $1=tag ; retries ride out a transient EADDRINUSE right after a restart
  for _try in 1 2 3; do
    $Y data-server --listen $ADDR --root "$PWD/.rtest/rsrv" > ".rtest/srv_$1.log" 2>&1 & SP=$!
    if wait_port; then return 0; fi
    kill $SP 2>/dev/null; wait $SP 2>/dev/null || true; sleep 0.5
  done
  echo "server $1 not listening"; exit 9
}
stop_server(){ kill $SP 2>/dev/null; wait $SP 2>/dev/null || true; }

A=.rtest/mnt_a
B=.rtest/mnt_b

echo "===== A) server up: mount alice + seed ====="
start_server 1
$Y init --view alice >/dev/null
$Y init --view bob   >/dev/null
$Y mount --view alice --mountpoint $A --data $ADDR --foreground > .rtest/a.log 2>&1 & MA=$!
sleep 1.2
timeout 10 sh -c "printf 'persistent payload\n' > $A/seed.txt" && sync
chk "$(timeout 5 cat $A/seed.txt)" "persistent payload" "seed written+read on live server"
seed_md5=$(timeout 5 md5sum $A/seed.txt | awk '{print $1}')

echo "===== B) server DOWN: graceful failure, client alive, metadata still local ====="
stop_server
timeout 8 sh -c "touch $A/during_down.txt"; rc=$?
if [ "$rc" -ne 0 ]; then ok "create while server down fails (rc=$rc), bounded by timeout"; else bad "create wrongly 'succeeded' while server down"; fi
if kill -0 $MA 2>/dev/null; then ok "client mount process survived the outage"; else bad "client mount process died"; fi
if timeout 5 ls $A | grep -qx seed.txt; then ok "local view still lists files during outage"; else bad "listing failed during outage"; fi

echo "===== C) server RESTARTED (same addr+root): recovery + persistence ====="
start_server 2
timeout 10 sh -c "printf 'recovered\n' > $A/recovered.txt"; rc=$?
chk "$rc" "0" "create succeeds after server restart (reconnect)"
chk "$(timeout 5 cat $A/recovered.txt)" "recovered" "post-reconnect write is readable"
chk "$(timeout 5 md5sum $A/seed.txt | awk '{print $1}')" "$seed_md5" "pre-outage data persisted across restart"
if find .rtest/rsrv/blocks -type f >/dev/null 2>&1 && [ "$(find .rtest/rsrv/blocks -type f | wc -l)" -ge 2 ]; then ok "reconnected writes landed on the (new) server disk"; else bad "server disk missing objects"; fi

echo "===== D) live pool + immediate restart: silent transparent retry ====="
timeout 10 sh -c "printf 'more\n' >> $A/recovered.txt" && sync   # ensure a warm pooled socket is in use
stop_server; start_server 3                                       # pooled socket is now dead
timeout 10 sh -c "printf 'transparent\n' > $A/transparent.txt"; rc=$?
chk "$rc" "0" "op over a stale pooled socket recovers with NO error surfaced"
chk "$(timeout 5 cat $A/transparent.txt)" "transparent" "transparently-recovered write is readable"

echo "===== E) two client mounts vs one server: concurrent + isolated ====="
$Y mount --view bob --mountpoint $B --data $ADDR --foreground > .rtest/b.log 2>&1 & MB=$!
sleep 1.2
p1=""; p2=""
timeout 30 sh -c "for n in \$(seq 1 200); do echo a-\$n > $A/mc.txt; done" & p1=$!
timeout 30 sh -c "for n in \$(seq 1 200); do echo b-\$n > $B/mc.txt; done" & p2=$!
wait $p1 $p2   # ONLY the two writers, never the background mount/server daemons
chk "$(timeout 5 cat $A/mc.txt)" "a-200" "alice concurrent writes intact"
chk "$(timeout 5 cat $B/mc.txt)" "b-200" "bob concurrent writes intact"
if [ ! -e "$B/seed.txt" ] && [ ! -e "$B/transparent.txt" ]; then ok "bob cannot see alice's files (view isolation)"; else bad "view leak across clients"; fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL RESILIENCE CHECKS PASSED" || echo "SOME RESILIENCE CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
