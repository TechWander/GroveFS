#!/usr/bin/env bash
# Cross-host verification of the FoundationDB metadata backend on the REAL
# four-zone cluster (.bneck/fdb3vm_up.sh builds it), covering the three facts
# the deployment claims that only more than one machine can settle:
#
#   A. the writer lease is keyed by the canonical `--meta-dir` STRING, so two
#      machines naming the SAME path are ONE deployment and the second must be
#      refused BEFORE it binds;
#   B. two machines naming DIFFERENT paths are two deployments: each holds its
#      own row and serves its own views at the same time (the lease fences a
#      deployment, never the cluster);
#   C. the expiry is compared against each host's own wall clock, so an
#      observer whose clock runs ahead takes the lease over EARLY by the skew —
#      the documented limit, measured instead of assumed.
#
# Plus one fact about the fault domain: the cluster survives losing a machine
# while a holder is serving (all four guests are coordinators, so the loss is
# simultaneously a coordinator and a storage-zone loss — there is no
# "non-coordinator" machine in this topology to destroy instead).
#
# usage: bash .bneck/fdb3vm_lease.sh [ship|all|A|B|C|D]
#   ship  (re)install the binary + helper on the guests and exit
#   C     the fault-domain section (virsh destroy / start gfs-fdb4)
set -u
ROOT=/data/grovefs/.fdb3vm
CLUSTER=$ROOT/cluster3vm
URI="fdb://$CLUSTER"                      # as seen from THIS host
RURI="fdb:///etc/foundationdb/fdb.cluster" # the same cluster, as seen from a guest
SRC=/data/grovefs/grovefs/target/fdb/release/grovefs
HELPER=/data/grovefs/grovefs/.bneck/gfs_srv.sh
LIB=/data/grovefs/.fdbbin/libfdb_c.x86_64.so
FCLI=/data/grovefs/.fdbbin/fdbcli.x86_64
PROBE=/data/grovefs/grovefs/.bneck/fdb_probe.py
LOCAL=$SRC
DEST=/opt/gfsprod
G1=gfs1; G2=gfs2; FDB4=gfs-fdb4
IP1=192.168.122.124; IP2=192.168.122.193
P1=50110; P2=50111                  # meta-server ports
D1=50120; D2=50121                  # data-server ports
DEP_A=/srv/grovefs/cluster          # deployment A -- the SAME string on both hosts in A)
DEP_B=/srv/grovefs/cluster-b        # deployment B -- a different string
DAT_A=/srv/grovefs/data-a
DAT_B=/srv/grovefs/data-b
OUT=$ROOT/lease
mkdir -p "$OUT"
lease_all(){ $LOCAL view --meta-store "$URI" lease 2>/dev/null; }
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
mit(){ echo "    $1"; }

SSHO="-o ConnectTimeout=8 -o BatchMode=yes -o ServerAliveInterval=10 -o ServerAliveCountMax=3"
# rsh: run a command on a guest; stdout only (the guests' bash prints a
# setlocale warning on every non-interactive start, which is noise, not a
# result). rok is the predicate form: its exit status is the remote one.
rsh(){ local h=$1; shift; timeout 120 ssh $SSHO -n "$h" "$@" 2>/dev/null; }
rok(){ local h=$1; shift; timeout 120 ssh $SSHO -n "$h" "$@" >/dev/null 2>&1; }
srv(){ rsh "$1" "sudo -n $DEST/srv.sh $2" ; }

mlog(){ echo "/tmp/gfs_meta_$(basename "$1")_$2.log"; }

# ship the FDB-linked binary (RUNPATH $ORIGIN/../lib finds libfdb_c) and the
# start/stop helper that keeps ssh channels from hanging (see gfs_srv.sh).
ship(){
    [ -x "$SRC" ] || { echo "missing $SRC (build with --features fdb)"; exit 1; }
    for h in $G1 $G2; do
        rok "$h" "sudo -n mkdir -p $DEST/lib"
        scp -q "$SRC" "$h":/tmp/grovefs.fdb 2>/dev/null
        scp -q "$LIB" "$h":/tmp/libfdb_c.so 2>/dev/null
        scp -q "$HELPER" "$h":/tmp/srv.sh 2>/dev/null
        rsh "$h" "sudo -n install -m 0755 /tmp/grovefs.fdb $DEST/grovefs && \
                  sudo -n install -m 0644 /tmp/libfdb_c.so $DEST/lib/libfdb_c.so && \
                  sudo -n install -m 0755 /tmp/srv.sh $DEST/srv.sh && \
                  rm -f /tmp/grovefs.fdb /tmp/libfdb_c.so /tmp/srv.sh && \
                  $DEST/grovefs --version 2>&1 | tail -1"
    done
    echo "  shipped $DEST/{grovefs,lib/libfdb_c.so,srv.sh} to $G1 $G2"
}
if [ "${1:-all}" = ship ]; then ship; exit 0; fi

probe(){ python3 "$PROBE" "$CLUSTER" "$FCLI" /data/grovefs/.fdbbin/lib; }
fdb_avail(){ probe | grep -q 'available=True'; }
fdb_wait(){ local n=${1:-90}; for _ in $(seq 1 "$n"); do fdb_avail && return 0; sleep 1; done; return 1; }

echo "===== 0) prerequisites: cluster available, binary shipped, ports free ====="
fdb_avail && ok "the four-zone cluster reports available" || { bad "cluster unavailable"; exit 1; }
ship >/dev/null
rok $G1 "test -x $DEST/grovefs && test -f /etc/foundationdb/fdb.cluster" && ok "$G1 has the binary and a cluster file"
rok $G2 "test -x $DEST/grovefs && test -f /etc/foundationdb/fdb.cluster" && ok "$G2 has the binary and a cluster file"
busy=0
for p in $P1 $P2 $D1 $D2; do
    rok $G1 "! ss -ltn 2>/dev/null | grep -q ':$p '" || { bad "$G1 port $p already in use"; busy=1; }
    rok $G2 "! ss -ltn 2>/dev/null | grep -q ':$p '" || { bad "$G2 port $p already in use"; busy=1; }
done
[ $busy -eq 0 ] && ok "ports $P1/$P2/$D1/$D2 free on both guests"
mit "cluster now: $(probe)"

cleanup(){
    srv $G1 "stop meta $DEP_A $P1" >/dev/null 2>&1
    srv $G2 "stop meta $DEP_A $P2" >/dev/null 2>&1
    srv $G2 "stop meta $DEP_B $P2" >/dev/null 2>&1
    srv $G1 "stop data $DAT_A $D1" >/dev/null 2>&1
    srv $G2 "stop data $DAT_B $D2" >/dev/null 2>&1
    for m in /tmp/gfs_ha1 /tmp/gfs_hb1; do fusermount -u $m 2>/dev/null; done
}
trap cleanup EXIT

# ---------------------------------------------------------------- A ---------
echo "===== A) one canonical --meta-dir on two hosts = ONE deployment ====="
rsh $G1 "sudo -n mkdir -p $DEP_A $DAT_A"
rsh $G2 "sudo -n mkdir -p $DEP_A"      # deliberately NOT -b: same string, other machine

start_meta(){ srv "$1" "start meta $2 $3 $4 $RURI" ; }
# wait_log <host> <role> <dir> <port> <regex> <tries>
wait_log(){
    rsh "$1" "for _ in \$(seq 1 $6); do grep -qiE '$5' $(mlog "$3" "$4") 2>/dev/null && exit 0; sleep 0.2; done; exit 1"
}

start_meta $G1 $DEP_A $P1 12
wait_log $G1 meta $DEP_A $P1 'writer lease HELD' 100 \
    && ok "A holds the lease on $G1" || bad "A did not take the lease"
rsh $G1 "head -2 $(mlog $DEP_A $P1)" | sed 's/^/    /'
rsh $G1 "sudo -n $DEST/srv.sh alive meta $DEP_A $P1" && ok "A's server is running (pidfile)" || bad "A's server died"

t0=$(date +%s%N)
start_meta $G2 $DEP_A $P2 6            # same path, other machine, waits 6s then refuses
wait_log $G2 meta $DEP_A $P2 'already has a live meta-server writer' 100
t1=$(date +%s%N)
rsh $G2 "tail -2 $(mlog $DEP_A $P2)" | sed 's/^/    /'
rsh $G2 "grep -qi 'already has a live meta-server writer' $(mlog $DEP_A $P2)" \
    && ok "the second server on the same --meta-dir is REFUSED" || bad "second server not refused"
rsh $G2 "sudo -n $DEST/srv.sh alive meta $DEP_A $P2" >/dev/null \
    && bad "the refused server is still running" || ok "and it exited (no server process left)"
rok $G2 "ss -ltn 2>/dev/null | grep -q ':$P2 '" \
    && bad "the refused server bound port $P2" || ok "and it never bound its port (refusal is before bind)"
mit "refusal latency: $(( (t1 - t0) / 1000000 ))ms (wait budget was 6s)"
rsh $G1 "sudo -n $DEST/srv.sh alive meta $DEP_A $P1" >/dev/null \
    && ok "the holder on $G1 is unaffected" || bad "the holder on $G1 died"

# ---------------------------------------------------------------- B ---------
echo "===== B) different --meta-dir on two hosts = TWO independent deployments ====="
rsh $G2 "sudo -n mkdir -p $DEP_B $DAT_B"
start_meta $G2 $DEP_B $P2 12
wait_log $G2 meta $DEP_B $P2 'writer lease HELD' 100 \
    && ok "B holds its own row on $G2 while A still holds its own" || bad "B did not take its row"
list=$(lease_all)
echo "$list" | sed 's/^/    /'
chk "$(echo "$list" | awk '$1=="HELD"' | wc -l)" "2" "view lease lists two HELD rows (one per deployment)"
chk "$(echo "$list" | awk -v d="$DEP_A" '$2==d' | wc -l)" "1" "deployment A appears once"
chk "$(echo "$list" | awk -v d="$DEP_B" '$2==d' | wc -l)" "1" "deployment B appears once"

srv $G1 "start data $DAT_A $D1" && ok "data-server A started on $G1:$D1"
srv $G2 "start data $DAT_B $D2" && ok "data-server B started on $G2:$D2"
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI create --view ha1" >/dev/null 2>&1 \
    && ok "created base view ha1 in A's deployment" || bad "could not create ha1"
rsh $G2 "sudo -n $DEST/grovefs view --meta-dir $DEP_B --meta-store $RURI create --view hb1" >/dev/null 2>&1 \
    && ok "created base view hb1 in B's deployment" || bad "could not create hb1"
trees(){ rsh $G1 "sudo -n $DEST/grovefs view --meta-store $RURI scan" | awk 'NR>1{print $1}'; }
chk "$(trees | grep -c '^ha1$')" "1" "the CLUSTER holds A's tree ha1"
chk "$(trees | grep -c '^hb1$')" "1" "the CLUSTER holds B's tree hb1"

mkdir -p /tmp/gfs_ha1 /tmp/gfs_hb1
$LOCAL mount --meta-server $IP1:$P1 --data-servers $IP1:$D1 --view ha1 \
    --mountpoint /tmp/gfs_ha1 --foreground >/tmp/mnt_ha1.log 2>&1 &
$LOCAL mount --meta-server $IP2:$P2 --data-servers $IP2:$D2 --view hb1 \
    --mountpoint /tmp/gfs_hb1 --foreground >/tmp/mnt_hb1.log 2>&1 &
sleep 3
echo one > /tmp/gfs_ha1/a.txt 2>/dev/null && ok "a write through deployment A lands while B is writing" || bad "write through A failed"
echo two > /tmp/gfs_hb1/b.txt 2>/dev/null && ok "a write through deployment B lands while A is writing" || bad "write through B failed"
chk "$(cat /tmp/gfs_ha1/a.txt 2>/dev/null)" "one" "A's mount reads its own byte back"
chk "$(cat /tmp/gfs_hb1/b.txt 2>/dev/null)" "two" "B's mount reads its own byte back"
chk "$(cat /tmp/gfs_ha1/b.txt 2>/dev/null)" "" "B's file is not visible through A (two trees, not one)"
for m in /tmp/gfs_ha1 /tmp/gfs_hb1; do fusermount -u $m 2>/dev/null; done
rsh $G2 "sudo -n $DEST/srv.sh alive meta $DEP_B $P2" >/dev/null \
    && ok "B still running (A's lease did not fence it)" || bad "B died"
# Under fdb:// a BASE view has no definition at all — `view list` says so and
# points at `scan` (the trees are subspaces). Governance files only exist for
# derived/alias/fork, so THAT is what proves the control planes stay separate.
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI list" \
    | grep -q 'a base view has none' \
    && ok "with only base trees, view list names no definition — it directs to 'view scan'" \
    || bad "view list did not explain the fdb:// base-view asymmetry"
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI alias --name pubA --real ha1" >/dev/null 2>&1 \
    && ok "A published an alias pubA -> ha1" || bad "A could not create its alias"
rsh $G2 "sudo -n $DEST/grovefs view --meta-dir $DEP_B --meta-store $RURI alias --name pubB --real hb1" >/dev/null 2>&1 \
    && ok "B published an alias pubB -> hb1" || bad "B could not create its alias"
chk "$(rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI list" | grep -c "alias 'pubA'")" "1" "A's control plane lists its alias pubA"
chk "$(rsh $G2 "sudo -n $DEST/grovefs view --meta-dir $DEP_B --meta-store $RURI list" | grep -c "alias 'pubB'")" "1" "B's control plane lists its alias pubB"
chk "$(rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI list" | grep -c "alias 'pubB'")" "0" "A cannot see B's definition (separate control planes in separate meta-dirs)"
chk "$(rsh $G2 "sudo -n $DEST/grovefs view --meta-dir $DEP_B --meta-store $RURI list" | grep -c "alias 'pubA'")" "0" "B cannot see A's definition"

# ---------------------------------------------------------------- C ---------
if [ "${1:-all}" = all ] || [ "${1:-}" = C ]; then
echo "===== C) fault domain: losing one guest (coordinator AND storage zone) ====="
mit "before:  $(probe)"
t0=$(date +%s%N)
virsh destroy $FDB4 >/dev/null 2>&1 && ok "destroyed $FDB4 mid-service (holder A is renewing)" || bad "virsh destroy $FDB4 failed"
# Availability is the claim under test: poll and count how often the database
# reports itself unavailable. One lost coordinator (quorum 3 of 4) plus one
# lost storage zone must cost neither. Note that `status json` itself gets
# slower while a coordinator is missing (~5s/call here), which is why this
# loop — not a single probe — is what can say "never unavailable".
unavail=0; samples=0; first_loss=""; repl_gap=0; s=""
for _ in $(seq 1 10); do
    s=$(probe); samples=$((samples+1))
    echo "$s" | grep -q 'available=True' || unavail=$((unavail+1))
    echo "$s" | grep -q 'full_replication=False' && repl_gap=$((repl_gap+1))
    if [ -z "$first_loss" ] && echo "$s" | grep -q 'coordinators=3/4'; then
        first_loss=$(( ($(date +%s%N) - t0) / 1000000 ))
    fi
    sleep 0.6
done
mit "during (last sample): $s"
mit "probe loop: ${samples} samples in $(( ($(date +%s%N) - t0) / 1000000 ))ms — status is slow while a coordinator is missing"
chk "$unavail" "0" "the cluster stayed AVAILABLE across $samples probes after losing one of four machines"
[ -n "$first_loss" ] && ok "the loss was visible in the cluster's own view after ${first_loss}ms (coordinators fell to 3/4)" \
    || bad "the destroyed machine was never reported as a lost coordinator"
echo "$s" | grep -q 'zones=n1,n2,n3 ' && ok "and only three storage zones remain (n4 was a storage server too)" \
    || bad "storage-zone list did not shrink: $s"
mit "full_replication read False in $repl_gap/$samples samples — re-replication onto three zones completed inside the outage"
sleep 12   # four renewal ticks (3s each)
rsh $G1 "sudo -n $DEST/srv.sh alive meta $DEP_A $P1" >/dev/null \
    && ok "the holder survived the loss (it did not give the lease up)" || bad "the holder exited"
row=$(lease_all | awk -v d="$DEP_A" '$2==d')
mit "row during the outage: $row"
echo "$row" | grep -q HELD && ok "the lease row is still HELD (renewal retried through the recovery)" \
    || bad "the lease row was lost during the outage"
# A write through the control plane must still work: losing one machine costs
# neither the lease nor the ability to govern.
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI create --view fault1" >/dev/null 2>&1 \
    && ok "a NEW view could be created during the outage (control plane still writes)" \
    || bad "control-plane write failed during the outage"
t1=$(date +%s%N)
virsh start $FDB4 >/dev/null 2>&1 && mit "restarting $FDB4" || bad "virsh start $FDB4 failed"
for _ in $(seq 1 90); do rok $FDB4 "true" && break; sleep 2; done
rok $FDB4 "true" && ok "the guest is back on the network ($(( ($(date +%s%N) - t1) / 1000000 ))ms)" \
    || bad "the guest never came back"
# These guests have no systemd unit for fdbserver (the fixture starts it by
# hand), so "the machine returns" includes starting its server again — the
# exact command .bneck/fdb3vm_up.sh uses. Nothing here touches the cluster
# configuration: rejoining is the server's own doing.
# The redirect happens as the *login* user, so it must not point into a
# root-only directory (measured: `>> /data/fdb/fdb/stdout.log` fails with
# permission denied and the server never starts). fdbserver keeps its own
# logs in `-L`.
rsh $FDB4 "sudo -n setsid -f /usr/local/sbin/fdbserver --cluster-file /etc/foundationdb/fdb.cluster \
    -d /data/fdb/fdb/data -L /data/fdb/fdb/logs -l 192.168.122.248:4500 -p 192.168.122.248:4500 \
    -m 2000000000 --locality-zoneid n4 --locality-machineid m-n4 \
    </dev/null >/dev/null 2>&1" >/dev/null 2>&1 \
    && mit "started n4's fdbserver again" || bad "could not restart n4's fdbserver"
# A rejoined guest counts once its zone is back in the cluster, then once its
# coordinator address answers again.
for _ in $(seq 1 120); do
    s=$(probe)
    echo "$s" | grep -q 'zones=n1,n2,n3,n4' && break
    sleep 2
done
mit "after:   $s  ($(( ($(date +%s%N) - t1) / 1000000 ))ms after restart)"
chk "$(echo "$s" | grep -c 'zones=n1,n2,n3,n4')" "1" "the machine rejoined as a storage zone"
for _ in $(seq 1 60); do
    s=$(probe)
    echo "$s" | grep -q 'coordinators=4/4' && break
    sleep 2
done
chk "$(echo "$s" | grep -c 'coordinators=4/4')" "1" "and the coordinator quorum is whole again"
chk "$(echo "$s" | sed 's/.*full_replication=\([^ ]*\).*/\1/')" "True" \
    "self-healing: full replication is back with the machine rejoined (no operator step)"
row=$(lease_all | awk -v d="$DEP_A" '$2==d')
mit "row after rejoin: $row"
echo "$row" | grep -q HELD && ok "the lease is still the same deployment's, uninterrupted" || bad "lease row changed"
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI rm --name fault1 --base --data-dir $DAT_A" >/dev/null 2>&1
fi

# ---------------------------------------------------------------- D ---------
echo "===== D) expiry is judged by the observer's own wall clock (skew = early takeover) ====="
# A useful number out of a lease row: seconds left, negative once expired.
rem_secs(){ awk '{ if ($(NF-1)=="in") print $NF+0; else print -($(NF-1)+0) }' <<EOF
$1
EOF
}
sleep 3
mit "row while A is alive: $(lease_all | awk -v d="$DEP_A" '$2==d')"
skew=8
t_base=$(date -u +%s)
# systemd-timesyncd snaps any manual step back within seconds (measured: a
# +8s step read back as +1s), so the synchroniser is stopped for the duration.
rsh $G2 "sudo -n systemctl stop systemd-timesyncd 2>/dev/null; sudo -n date -u -s @$(( t_base + skew ))" >/dev/null 2>&1
sleep 0.2
after=$(rsh $G2 "date -u +%s")
chk "$(( after - t_base ))" "$skew" "$G2's clock now runs ${skew}s ahead of this host"
row=$(lease_all | awk -v d="$DEP_A" '$2==d')
local_secs=$(rem_secs "$row")
skewed_row=$(rsh $G2 "sudo -n $DEST/grovefs view --meta-store $RURI lease" | awk -v d="$DEP_A" '$2==d')
skewed_secs=$(rem_secs "$skewed_row")
mit "one row, two verdicts: correct clock says '${local_secs}s left', the +${skew}s host says '${skewed_secs}s left'"
[ -n "$skewed_secs" ] && ok "the skewed host can still read the row (skew moves the verdict, not the read)" \
    || bad "the +${skew}s host could not read the row"
[ "$skewed_secs" -le $(( local_secs - skew + 2 )) ] \
    && ok "the skewed host judges the SAME row shorter by about the skew" \
    || bad "the skewed verdict did not shift (local ${local_secs}s, skewed ${skewed_secs}s)"

srv $G1 "stop meta $DEP_A $P1" >/dev/null 2>&1   # SIGKILL: the row outlives the holder
sleep 0.5
row=$(lease_all | awk -v d="$DEP_A" '$2==d')
mit "row 0.5s after killing A: $row"
echo "$row" | grep -qi 'HELD\|expire' && ok "the dead holder's row is still listed (a crash leaves evidence)" \
    || bad "the row vanished with the holder"

t0=$(date +%s%N)
start_meta $G2 $DEP_A $P2 20
wait_log $G2 meta $DEP_A $P2 'writer lease HELD' 150
t1=$(date +%s%N)
took=$(( (t1 - t0) / 1000000 ))
mit "takeover on the +${skew}s host: ${took}ms after the kill (a correct clock waits ~$((${local_secs}*1000))ms)"
rsh $G2 "grep -q 'writer lease HELD' $(mlog $DEP_A $P2)" \
    && ok "the skewed host took the lease over" || bad "takeover did not happen"
[ "$took" -le $(( (local_secs - skew + 3) * 1000 )) ] \
    && ok "and it took over EARLY by roughly the skew — the documented limit, measured" \
    || bad "takeover did not follow the skew (${took}ms)"
# Restore to the CURRENT host time, not to t_base: the takeover spent a few
# seconds, and setting the old value leaves the guest that much behind.
rsh $G2 "sudo -n date -u -s @$(date -u +%s) >/dev/null 2>&1; sudo -n systemctl start systemd-timesyncd 2>/dev/null; true" >/dev/null 2>&1
off=$(( $(rsh $G2 "date -u +%s") - $(date -u +%s) ))
[ "${off#-}" -le 1 ] && ok "clock restored within 1s and the synchroniser restarted (offset ${off}s)" \
    || bad "clock not restored (offset ${off}s)"
srv $G2 "stop meta $DEP_A $P2" >/dev/null 2>&1
ok "stopped the takeover server (row will expire on its own)"

# ---------------------------------------------------------------- E ---------
echo "===== E) cleanup: views, data, lease rows ====="
# Definitions first (an alias is a file, not a tree; removing the base name
# does not delete the alias that publishes it), then the trees.
rsh $G1 "sudo -n $DEST/grovefs view --meta-dir $DEP_A --meta-store $RURI rm --name pubA" >/dev/null 2>&1 \
    && ok "removed A's alias definition pubA" || mit "pubA was already gone"
rsh $G2 "sudo -n $DEST/grovefs view --meta-dir $DEP_B --meta-store $RURI rm --name pubB" >/dev/null 2>&1 \
    && ok "removed B's alias definition pubB" || mit "pubB was already gone"
for v in "ha1 $DEP_A $DAT_A" "hb1 $DEP_B $DAT_B" "fault1 $DEP_A $DAT_A"; do
    set -- $v
    h=$G1; [ "$2" = "$DEP_B" ] && h=$G2
    rsh $h "sudo -n $DEST/grovefs view --meta-dir $2 --meta-store $RURI rm --name $1 --base --data-dir $3" >/dev/null 2>&1 \
        && ok "removed view $1" || mit "view $1 was already gone"
done
srv $G1 "stop data $DAT_A $D1" >/dev/null
srv $G2 "stop data $DAT_B $D2" >/dev/null
for d in "$DEP_A" "$DEP_B"; do
    $LOCAL view --meta-store "$URI" lease --release --deployment "$d" --force >/dev/null 2>&1
done
sleep 1
final=$(lease_all)
echo "$final" | sed 's/^/    /'
chk "$(echo "$final" | grep -c HELD)" "0" "no live writer rows remain"
chk "$(rsh $G1 "sudo -n ls $DEP_A/meta/views 2>/dev/null | wc -l")" "0" \
    "A's meta-dir holds no view definition any more"
rsh $G1 "sudo -n rm -rf $DEP_A $DAT_A; sudo -n rm -f /tmp/gfs_*_cluster_50110.* /tmp/gfs_*_data-a_50120.*; echo cleaned:$?" | sed 's/^/    /'
rsh $G2 "sudo -n rm -rf $DEP_A $DEP_B $DAT_B; sudo -n rm -f /tmp/gfs_*_cluster_50111.* /tmp/gfs_*_cluster-b_50111.* /tmp/gfs_*_data-b_50121.*; echo cleaned:$?" | sed 's/^/    /'
$LOCAL view --meta-store "$URI" scan 2>/dev/null | tail -3 | sed 's/^/    /'

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL CROSS-HOST LEASE CHECKS PASSED" || echo "SOME CROSS-HOST LEASE CHECKS FAILED"
[ "$fail" -eq 0 ] || exit 1
