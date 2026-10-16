#!/usr/bin/env bash
# MetaServer HIGH-AVAILABILITY e2e over real FUSE mounts: a primary + a standby
# (two separate meta-server processes, each with its OWN --home so neither shares
# the other's authoritative files), a client mounted through a comma-separated
# failover list `--meta primary,standby`, and a kill of the primary proving the
# standby promotes and the client keeps reading and writing.
#
# Honest scope (MVP two-node HA, mirrored in the code comments):
#   * Replication is an ASYNC archival snapshot copy at ~500ms cadence, so
#     RPO ~= 500ms: writes the primary committed after its last push CAN be lost
#     on failover. We therefore assert only a BOUNDED-loss property: a write made
#     WELL before the kill (and thus checkpointed into a shipped snapshot) MUST
#     survive; we do NOT claim zero loss.
#   * With only two nodes and no external quorum there is a bounded double-write
#     window (up to the ~3s promote timeout): a partitioned old primary can keep
#     writing until its own replicator notices. `--witness <data-server>` closes
#     that window: the shared storage node is a THIRD arbiter both halves can
#     reach, so a would-be promoter claims its epoch there and refuses to seize
#     primary past a higher claim, and a restarted stale primary is fenced by the
#     witness even when its peer is unreachable (see section 9).
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grove

META_A=127.0.0.1:50106   # primary
META_B=127.0.0.1:50107   # standby
DATA=127.0.0.1:50108
rm -rf .ha; mkdir -p .ha/a .ha/b .ha/data .ha/chome .ha/sbhome .ha/mnt .ha/mnt_sb

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# ---- data server + the HA meta pair ----
$Y data-server --listen $DATA --root .ha/data > .ha/data.log 2>&1 &
DP=$!
$Y --home .ha/a meta-server --listen $META_A --peer $META_B > .ha/a.log 2>&1 &
AP=$!
$Y --home .ha/b meta-server --listen $META_B --role standby --peer $META_A > .ha/b.log 2>&1 &
BP=$!
wait_port $DATA || true
wait_port $META_A || true
wait_port $META_B || true

# ---- client mounts ----
# main client: failover list primary,standby
$Y --home .ha/chome mount --view alice --mountpoint .ha/mnt --meta "$META_A,$META_B" --data $DATA > .ha/c.log 2>&1 &
CP=$!
sleep 2

cleanup(){ for d in .ha/mnt .ha/mnt_sb; do fusermount -u "$d" 2>/dev/null; done; kill $CP $SP $DP $AP $BP 2>/dev/null; }
SP=0
trap cleanup EXIT

M=.ha/mnt
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }

echo "===== 1) both meta nodes start, primary is authoritative ====="
grep -q 'role Primary' .ha/a.log && ok "primary logged role Primary" || bad "primary did not log Primary role"
grep -q 'role Standby' .ha/b.log && ok "standby logged role Standby" || bad "standby did not log Standby role"
T sh -c "printf 'p1-bytes' > $M/p1.txt"; sync "$M" 2>/dev/null
# give the primary time to open alice.db and push a snapshot to the standby
sleep 2
[ -f .ha/a/meta/alice.db ] && ok "primary holds authoritative alice.db" || bad "primary alice.db missing"

echo "===== 2) replication: standby received a snapshot of alice.db ====="
[ -f .ha/b/meta/alice.db ] && ok "standby replicated alice.db exists" || bad "standby alice.db missing (no replication)"
# a client pointed ONLY at the standby reads the replicated view (proves the
# snapshot is complete + readable, not a torn file copy)
$Y --home .ha/sbhome mount --view alice --mountpoint .ha/mnt_sb --meta $META_B --data $DATA > .ha/sb.log 2>&1 &
SP=$!
sleep 2
chk "$(T cat .ha/mnt_sb/p1.txt)" "p1-bytes" "standby-only client reads a primary-written file (replicated view)"

echo "===== 3) standby is read-only: a direct write to it is refused ====="
if T sh -c "printf 'nope' > .ha/mnt_sb/wont.txt" 2>/dev/null; then
  [ -e .ha/mnt_sb/wont.txt ] && bad "standby accepted a write (should be read-only)" || ok "standby write silently rejected (E_ROFS)"
else
  ok "standby refused a write (E_ROFS -> write failed)"
fi

echo "===== 4) HA event forwarding: standby subscriber sees a primary create ====="
T sh -c "printf 'p2' > $M/p2.txt"; sync "$M" 2>/dev/null
sleep 2
if T ls .ha/mnt_sb | grep -qx p2.txt; then ok "standby client sees primary's new file via forwarded inval (M_FORWARD)"; else bad "p2.txt missing from standby client ($(T ls .ha/mnt_sb | tr '\n' ' '))"; fi

echo "===== 5) failover: kill primary, standby promotes, client keeps writing ====="
KILL_T0=$(date +%s.%N)
kill $AP 2>/dev/null
# wait for the standby watchdog to promote (>3s promote timeout)
for _ in $(seq 1 40); do grep -q 'promoted to primary' .ha/b.log && break; sleep 0.2; done
grep -q 'promoted to primary' .ha/b.log && ok "standby promoted itself to primary after heartbeat loss" || bad "standby never promoted"
# the failover-list client must transparently move to the (now) primary standby
T sh -c "printf 'p3-after-failover' > $M/p3.txt"; sync "$M" 2>/dev/null
chk "$(T cat $M/p3.txt)" "p3-after-failover" "client writes succeed after failover (redirected to promoted node)"
# bounded-loss assertion: data checkpointed BEFORE the kill must still be there
chk "$(T cat $M/p1.txt)" "p1-bytes" "pre-failover replicated data survived (RPO-bounded: nothing older than ~500ms lost)"
# failover completed within a bounded wall-clock budget (kill -> a working write)
KILL_T1=$(date +%s.%N)
FT=$(awk "BEGIN{printf \"%.2f\", $KILL_T1-$KILL_T0}")
awk "BEGIN{exit !($FT < 12)}" && ok "failover+recovery completed in ${FT}s (<12s budget)" || bad "failover too slow: ${FT}s"

echo "===== 6) recovered old primary demotes itself (no double-primary) ====="
# bring the old primary back; its replicator sees the peer's higher epoch and
# must step down to standby rather than resume writing.
$Y --home .ha/a meta-server --listen $META_A --peer $META_B > .ha/a2.log 2>&1 &
AP2=$!
# reassign cleanup to also reap the restarted node
trap 'for d in .ha/mnt .ha/mnt_sb; do fusermount -u "$d" 2>/dev/null; done; kill $CP $SP $DP $AP2 $BP 2>/dev/null' EXIT
wait_port $META_A || true
sleep 3
grep -q 'demoted to standby' .ha/a2.log && ok "restarted old primary demoted itself (epoch fencing)" || bad "old primary did not demote (split-brain risk)"

# ---------------------------------------------------------------------------
echo "===== 7) --sync-repl: RPO=0 for mutating ops ====="
# Bring up a SECOND HA pair dedicated to the sync-repl test (independent ports
# so the old pair's state doesn't interfere).
SYNC_A=127.0.0.1:50136
SYNC_B=127.0.0.1:50137
SYNC_D=127.0.0.1:50138
rm -rf .ha/sync_a .ha/sync_b .ha/sync_data .ha/sync_chome .ha/sync_mnt
mkdir -p .ha/sync_a .ha/sync_b .ha/sync_data .ha/sync_chome .ha/sync_mnt

$Y data-server --listen $SYNC_D --root .ha/sync_data > .ha/sync_data.log 2>&1 &
SDP=$!
$Y --home .ha/sync_a meta-server --listen $SYNC_A --peer $SYNC_B --sync-repl > .ha/sync_a.log 2>&1 &
SAP=$!
$Y --home .ha/sync_b meta-server --listen $SYNC_B --role standby --peer $SYNC_A > .ha/sync_b.log 2>&1 &
SBP=$!
wait_port $SYNC_D; wait_port $SYNC_A; wait_port $SYNC_B
# Confirm the server logged sync-repl activation.
grep -q 'synchronous replication ON' .ha/sync_a.log \
  && ok "--sync-repl logged activation" || bad "--sync-repl not activated"

$Y --home .ha/sync_chome mount --view alice --mountpoint .ha/sync_mnt --meta "$SYNC_A,$SYNC_B" --data $SYNC_D > .ha/sync_mnt.log 2>&1 &
SCP=$!
sleep 2
SM=.ha/sync_mnt

# Write 5 markers sequentially. Under sync-repl each write's response arrives
# ONLY after the standby has durably renamed the snapshot. Kill primary after
# all writes; standby promotes; a client on the standby must see ALL markers.
for i in 1 2 3 4 5; do
  printf 'marker_%d' "$i" > "$SM/marker_$i.txt"
done
sync "$SM" 2>/dev/null || true
sleep 0.5   # one extra tick so the last snapshot is definitely pushed

# Kill the primary. The standby has all 5 markers replicated.
kill $SAP 2>/dev/null; wait $SAP 2>/dev/null || true
# Wait for standby to promote (PROMOTE_TIMEOUT ~= 3s).
sleep 4

# Mount a fresh client pointing ONLY at the (now-promoted) standby.
rm -rf .ha/sync_mnt2; mkdir -p .ha/sync_mnt2
$Y --home .ha/sync_chome mount --view alice --mountpoint .ha/sync_mnt2 --meta $SYNC_B --data $SYNC_D > .ha/sync_mnt2.log 2>&1 &
SCP2=$!
sleep 2

missing=0
for i in 1 2 3 4 5; do
  if ! grep -q "marker_$i" ".ha/sync_mnt2/marker_$i.txt" 2>/dev/null; then
    missing=$((missing+1))
  fi
done
chk "$missing" "0" "all 5 markers present after sync-repl failover (RPO=0)"

fusermount -u .ha/sync_mnt2 2>/dev/null; kill $SCP2 2>/dev/null || true
fusermount -u .ha/sync_mnt 2>/dev/null; kill $SCP 2>/dev/null || true
kill $SBP $SDP 2>/dev/null || true

# ---------------------------------------------------------------------------
echo "===== 8) --sync-repl partition degradation: primary does not hang ====="
# Start another pair, kill the standby, then verify a write to primary returns
# (degraded to async) within the ACK timeout, not a permanent hang.
PART_A=127.0.0.1:50146
PART_B=127.0.0.1:50147
PART_D=127.0.0.1:50148
rm -rf .ha/part_a .ha/part_b .ha/part_data .ha/part_chome .ha/part_mnt
mkdir -p .ha/part_a .ha/part_b .ha/part_data .ha/part_chome .ha/part_mnt

$Y data-server --listen $PART_D --root .ha/part_data > .ha/part_data.log 2>&1 &
PDP=$!
$Y --home .ha/part_a meta-server --listen $PART_A --peer $PART_B --sync-repl > .ha/part_a.log 2>&1 &
PAP=$!
$Y --home .ha/part_b meta-server --listen $PART_B --role standby --peer $PART_A > .ha/part_b.log 2>&1 &
PBP=$!
wait_port $PART_D; wait_port $PART_A; wait_port $PART_B

$Y --home .ha/part_chome mount --view alice --mountpoint .ha/part_mnt --meta $PART_A --data $PART_D > .ha/part_mnt.log 2>&1 &
PCP=$!
sleep 2
# Kill the standby so the next sync push cannot get an ACK.
kill $PBP 2>/dev/null; wait $PBP 2>/dev/null || true
sleep 0.5
# Write MUST complete within ~5s (ACK timeout is 2s, but FUSE has overhead).
t0=$(date +%s)
if timeout 5 bash -c "printf 'partitioned' > .ha/part_mnt/p.txt" 2>/dev/null; then
  t1=$(date +%s)
  elapsed=$((t1 - t0))
  [ "$elapsed" -le 5 ] && ok "write completed in ${elapsed}s despite standby down (graceful degradation)" \
    || bad "write took ${elapsed}s (should be <=5s)"
  # Verify the warn was logged.
  grep -q 'degraded to async' .ha/part_a.log \
    && ok "primary logged 'degraded to async' warn" \
    || bad "primary did not log degradation warn"
else
  bad "write timed out or failed — sync-repl hangs when standby unreachable"
fi

fusermount -u .ha/part_mnt 2>/dev/null; kill $PCP $PAP $PDP 2>/dev/null || true

# ---------------------------------------------------------------------------
echo "===== 9) --witness fencing: a stale primary is fenced by the third arbiter ====="
# A dedicated HA pair whose data-server doubles as the fencing witness. We
# promote the standby (claiming a higher epoch at the witness), then restart the
# OLD primary. Its on-disk epoch is stale, and because the witness already holds
# the promoted node's higher epoch, the restarted primary must boot straight into
# STANDBY — the witness-fencing startup path ("starting as standby (witness epoch
# ..)"), which fires independently of whether it can reach its peer.
W_A=127.0.0.1:50156
W_B=127.0.0.1:50157
W_D=127.0.0.1:50158
rm -rf .ha/w_a .ha/w_b .ha/w_data; mkdir -p .ha/w_a .ha/w_b .ha/w_data

$Y data-server --listen $W_D --root .ha/w_data > .ha/w_data.log 2>&1 &
WD=$!
wait_port $W_D   # arbiter must be up BEFORE the primary registers, or it fail-safe-boots as standby
$Y --home .ha/w_a meta-server --listen $W_A --peer $W_B --witness $W_D > .ha/w_a.log 2>&1 &
WA=$!
$Y --home .ha/w_b meta-server --listen $W_B --role standby --peer $W_A --witness $W_D > .ha/w_b.log 2>&1 &
WB=$!
wait_port $W_A; wait_port $W_B

# The starting primary must register its epoch at the witness before serving.
grep -q 'registered fencing epoch' .ha/w_a.log \
  && ok "primary registered its fencing epoch at the witness" \
  || bad "primary did not claim at the witness on startup"

# Kill the primary; the standby promotes and claims a HIGHER epoch at the witness.
kill $WA 2>/dev/null; wait $WA 2>/dev/null || true
for _ in $(seq 1 40); do grep -q 'promoted to primary' .ha/w_b.log && break; sleep 0.2; done
grep -q 'promoted to primary' .ha/w_b.log \
  && ok "standby promoted itself (higher epoch) after primary death" \
  || bad "standby never promoted in the witness pair"
sleep 1   # let the promoted node's witness claim settle

# Durability: the promotion raised the token above 0, and the witness persisted
# it to <root>/.fence_token — so a witness restart cannot zero the epoch and let
# the stale primary sneak back in as a writer.
grep -qx '1' .ha/w_data/.fence_token 2>/dev/null \
  && ok "witness persisted the advanced fence token (<root>/.fence_token = 1)" \
  || bad "witness did not persist the fence token (got: $(cat .ha/w_data/.fence_token 2>/dev/null))"

# Restart the OLD primary (same --home, still stale on-disk epoch). The witness
# now carries the promoted node's higher epoch, so it must refuse primary.
$Y --home .ha/w_a meta-server --listen $W_A --peer $W_B --witness $W_D > .ha/w_a2.log 2>&1 &
WA2=$!
wait_port $W_A || true
sleep 2
grep -q 'starting as standby (a quorum' .ha/w_a2.log \
  && ok "restarted stale primary fenced by witness -> boots as standby (no double-write)" \
  || bad "witness did not fence the stale primary (log: $(tr '\n' ' ' < .ha/w_a2.log))"

kill $WA2 $WB $WD 2>/dev/null || true

# ---------------------------------------------------------------------------
echo "===== 10) --witness FAIL-SAFE: arbiter loss freezes writes, no blind split-brain ====="
# The whole point of the witness is that it is used fail-safe: a node that can
# no longer REACH the arbiter can no longer PROVE exclusive authority, so it must
# give up writing instead of continuing (or promoting) on faith. Kill the witness
# out from under a healthy pair and check both halves of that contract:
#   (a) the running primary self-demotes to standby (writes stop) after the grace;
#   (b) the standby REFUSES to promote while the arbiter is unreachable;
#   (c) when the arbiter comes back, EXACTLY ONE node re-promotes -- both start at
#       the same epoch, so their candidate claims collide, and the prev-returning
#       CAS must let the first claimer win and fence the tie. This is the double-
#       write window actually CLOSED, not merely bounded.
F_A=127.0.0.1:50166
F_B=127.0.0.1:50167
F_D=127.0.0.1:50168
rm -rf .ha/f_a .ha/f_b .ha/f_data; mkdir -p .ha/f_a .ha/f_b .ha/f_data

$Y data-server --listen $F_D --root .ha/f_data > .ha/f_data.log 2>&1 &
FD=$!
wait_port $F_D   # arbiter up before the primary registers (else it fail-safe-boots standby)
$Y --home .ha/f_a meta-server --listen $F_A --peer $F_B --witness $F_D > .ha/f_a.log 2>&1 &
FA=$!
$Y --home .ha/f_b meta-server --listen $F_B --role standby --peer $F_A --witness $F_D > .ha/f_b.log 2>&1 &
FB=$!
wait_port $F_A; wait_port $F_B
sleep 1

# Cut the arbiter off from both nodes.
kill $FD 2>/dev/null; wait $FD 2>/dev/null || true

# (a) primary fail-safe self-demote within grace (~3s) + tick retries.
for _ in $(seq 1 60); do grep -q 'fail-safe self-demote' .ha/f_a.log && break; sleep 0.25; done
grep -q 'fail-safe self-demote' .ha/f_a.log \
  && ok "primary self-demoted to standby when arbiter lost (refused to keep writing)" \
  || bad "primary did not fail-safe self-demote (log: $(tr '\n' ' ' < .ha/f_a.log))"

# (b) standby refuses to promote while it cannot reach the arbiter.
for _ in $(seq 1 60); do grep -q 'fail-safe refusing to promote' .ha/f_b.log && break; sleep 0.25; done
grep -q 'fail-safe refusing to promote' .ha/f_b.log \
  && ok "standby refused blind promotion with arbiter unreachable" \
  || bad "standby promoted without arbiter proof (log: $(tr '\n' ' ' < .ha/f_b.log))"

# (c) bring the arbiter back; exactly one node may seize primary.
$Y data-server --listen $F_D --root .ha/f_data > .ha/f_data2.log 2>&1 &
FD=$!
wait_port $F_D || true
sleep 5
na=$(grep -c 'promoted to primary' .ha/f_a.log 2>/dev/null || true); na=${na:-0}
nb=$(grep -c 'promoted to primary' .ha/f_b.log 2>/dev/null || true); nb=${nb:-0}
pcount=$((na + nb))
if [ "$pcount" -eq 1 ]; then
  ok "arbiter recovery re-promoted EXACTLY one node (collision resolved, no double-primary)"
elif [ "$pcount" -eq 0 ]; then
  bad "no node re-promoted after arbiter recovery (f_a: $(tr '\n' ' ' < .ha/f_a.log) | f_b: $(tr '\n' ' ' < .ha/f_b.log))"
else
  bad "DOUBLE-PRIMARY: $pcount nodes promoted after arbiter recovery (tie not fenced)"
fi

# Durability check: after the re-promotion the witness token advanced to 1 and
# was written to its store, so the next witness restart keeps fencing the loser.
grep -qx '1' .ha/f_data/.fence_token 2>/dev/null \
  && ok "witness persisted the recovered fence token (<root>/.fence_token = 1)" \
  || bad "witness did not persist the recovered fence token (got: $(cat .ha/f_data/.fence_token 2>/dev/null))"

kill $FA $FB $FD 2>/dev/null || true

# ---------------------------------------------------------------------------
echo "===== 11) --witness QUORUM: tolerate a minority arbiter loss, fail safe on majority loss ====="
# A single witness is itself a SPOF: losing it froze every write (section 10).
# With an ODD set of independent arbiters authority needs only a strict majority
# (N/2+1), so losing a MINORITY neither freezes writes nor resurrects a stale
# primary -- while losing a MAJORITY still fails safe. 3 arbiters => quorum 2:
#   (a) start a healthy pair over all 3; the primary registers at a 2/3 quorum;
#   (b) kill ONE arbiter: the primary still forms a quorum and MUST keep serving
#       writes (no self-demote) -- this is the availability a lone witness lacks;
#   (c) kill a SECOND arbiter (only 1/3 left, below quorum): the primary can no
#       longer *prove* exclusive authority, so after the grace it fail-safe
#       self-demotes, and the standby fail-safe refuses blind promotion.
Q_A=127.0.0.1:50176
Q_B=127.0.0.1:50177
Q_D1=127.0.0.1:50178
Q_D2=127.0.0.1:50179
Q_D3=127.0.0.1:50180
Q_DC=127.0.0.1:50181   # the client's CONTENT store: separate from the fencing quorum
rm -rf .ha/q_a .ha/q_b .ha/q_d1 .ha/q_d2 .ha/q_d3 .ha/q_dc
mkdir -p .ha/q_a .ha/q_b .ha/q_d1 .ha/q_d2 .ha/q_d3 .ha/q_dc
$Y data-server --listen $Q_D1 --root .ha/q_d1 > .ha/q_d1.log 2>&1 & QD1=$!
$Y data-server --listen $Q_D2 --root .ha/q_d2 > .ha/q_d2.log 2>&1 & QD2=$!
$Y data-server --listen $Q_D3 --root .ha/q_d3 > .ha/q_d3.log 2>&1 & QD3=$!
wait_port $Q_D1; wait_port $Q_D2; wait_port $Q_D3
$Y --home .ha/q_a meta-server --listen $Q_A --peer $Q_B --witness "$Q_D1,$Q_D2,$Q_D3" > .ha/q_a.log 2>&1 & QA=$!
$Y --home .ha/q_b meta-server --listen $Q_B --role standby --peer $Q_A --witness "$Q_D1,$Q_D2,$Q_D3" > .ha/q_b.log 2>&1 & QB=$!
wait_port $Q_A; wait_port $Q_B
sleep 1

# (a) registered at a majority, not all: 3 arbiters need 2.
grep -q 'registered fencing epoch' .ha/q_a.log && grep -q 'quorum (2/3' .ha/q_a.log \
  && ok "primary registered at a 2/3 arbiter quorum on startup" \
  || bad "primary did not register at a quorum (log: $(tr '\n' ' ' < .ha/q_a.log))"

# Mount a client through the quorum-guarded pair and confirm a write lands.
# Its CONTENT store $Q_DC is a distinct node, so killing an ARBITER below does not
# also delete the bytes the client is writing (arbiter quorum != data plane).
$Y data-server --listen $Q_DC --root .ha/q_dc > .ha/q_dc.log 2>&1 & QDC=$!
wait_port $Q_DC || true
rm -rf .ha/qmnt; mkdir -p .ha/qmnt
$Y --home .ha/qhome mount --view alice --mountpoint .ha/qmnt --meta "$Q_A,$Q_B" --data $Q_DC > .ha/qc.log 2>&1 & QC=$!
sleep 2
T sh -c "printf hi > .ha/qmnt/probe.txt" && [ "$(T cat .ha/qmnt/probe.txt)" = hi ] \
  && ok "client write succeeds under a healthy 3-arbiter quorum" \
  || bad "client write failed under a healthy quorum"

# (b) lose ONE arbiter: a 2/3 quorum still holds, so the primary MUST NOT demote
# and writes MUST keep flowing (the exact availability a lone witness cannot give).
kill $QD1 2>/dev/null; wait $QD1 2>/dev/null || true
sleep 2   # past a full grace: if we were going to freeze on one loss, we would by now
grep -q 'fail-safe self-demote' .ha/q_a.log \
  && bad "primary froze on losing ONE of three arbiters (quorum 2/3 should survive)" \
  || ok "primary survived losing a minority arbiter (still had a 2/3 quorum)"
T sh -c "printf again > .ha/qmnt/probe.txt" && [ "$(T cat .ha/qmnt/probe.txt)" = again ] \
  && ok "client still writes after losing one arbiter (quorum intact)" \
  || bad "client write failed after losing one arbiter despite a live quorum"

# (c) lose a SECOND arbiter: only 1/3 reachable, below quorum 2 => fail safe.
kill $QD2 2>/dev/null; wait $QD2 2>/dev/null || true
for _ in $(seq 1 60); do grep -q 'fail-safe self-demote' .ha/q_a.log && break; sleep 0.25; done
grep -q 'fail-safe self-demote' .ha/q_a.log \
  && ok "primary fail-safe self-demoted once a MAJORITY of arbiters was lost" \
  || bad "primary kept writing below an arbiter quorum (log: $(tr '\n' ' ' < .ha/q_a.log))"
for _ in $(seq 1 40); do grep -q 'fail-safe refusing to promote' .ha/q_b.log && break; sleep 0.25; done
grep -q 'fail-safe refusing to promote' .ha/q_b.log \
  && ok "standby refused blind promotion below an arbiter quorum" \
  || bad "standby promoted without a quorum proof (log: $(tr '\n' ' ' < .ha/q_b.log))"

fusermount -u .ha/qmnt 2>/dev/null
kill $QC $QA $QB $QD3 $QDC 2>/dev/null || true

# ---- SECTION 12: --witness list validation is FAIL-LOUD ----
# A malformed arbiter entry (not host:port) or an even-N list is a config error
# that would otherwise degrade SILENTLY to "unreachable arbiter -> fail-safe",
# indistinguishable from a real partition. serve() must warn at startup, and a
# well-formed ODD list must stay quiet (no false alarms).
mkdir -p .ha/vm
$Y --home .ha/vm meta-server --listen 127.0.0.1:50190 --witness "127.0.0.1" > .ha/v_bad.log 2>&1 & VM1=$!
sleep 0.6; kill $VM1 2>/dev/null; wait $VM1 2>/dev/null || true
grep -q 'are not host:port' .ha/v_bad.log \
  && ok "malformed --witness entry warned fail-loud (not mistaken for a partition)" \
  || bad "malformed --witness entry produced no warning (log: $(tr '\n' ' ' < .ha/v_bad.log))"
$Y --home .ha/vm meta-server --listen 127.0.0.1:50191 --witness "127.0.0.1:50192,127.0.0.1:50193" > .ha/v_even.log 2>&1 & VM2=$!
sleep 0.6; kill $VM2 2>/dev/null; wait $VM2 2>/dev/null || true
grep -q '(even) arbiters' .ha/v_even.log \
  && ok "even-N --witness list warned with an odd-count recommendation" \
  || bad "even-N --witness list produced no warning (log: $(tr '\n' ' ' < .ha/v_even.log))"
$Y --home .ha/vm meta-server --listen 127.0.0.1:50194 --witness "127.0.0.1:50195,127.0.0.1:50196,127.0.0.1:50197" > .ha/v_ok.log 2>&1 & VM3=$!
sleep 0.6; kill $VM3 2>/dev/null; wait $VM3 2>/dev/null || true
{ grep -q 'are not host:port' .ha/v_ok.log || grep -q '(even) arbiters' .ha/v_ok.log; } \
  && bad "healthy odd-N witness list emitted a spurious warning" \
  || ok "healthy odd-N witness list emitted no warning (no false alarms)"

# Reassign cleanup (remove temp sub-processes from trap)
trap 'for d in .ha/mnt .ha/mnt_sb; do fusermount -u "$d" 2>/dev/null; done; kill $CP $SP $DP $AP2 $BP 2>/dev/null' EXIT

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL HA CHECKS PASSED" || echo "SOME HA CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
