#!/usr/bin/env bash
# Governance convergence timing (paper Sec. 7.4 measurement).
#
# Turns the three previously-qualitative governance windows into REAL measured
# latencies on one box, over a central meta-server + data-server:
#   1) derived-view hot reload (widen + narrow) convergence — the per-op mtime
#      check bounded by the client's attr/dentry TTL;
#   2) cross-client invalidation immediacy (notify push) — a base write becoming
#      visible through an already-mounted derived viewer;
#   3) pure-alias gapless name rotation (--retire) command latency.
#
# Honesty: these are single-host, indicative numbers (min/median of a few
# cycles), NOT a throughput/scalability benchmark. Gating asserts CONVERGENCE
# within a generous bound; the millisecond figures are printed as INFO.
#
# Assertion-based like the other suites; emits a SUMMARY line for run_all.sh.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs
rm -rf .govtime; mkdir -p .govtime/home .govtime/mnt_base .govtime/mnt_dv .govtime/mnt_al
export GROVEFS_HOME=$PWD/.govtime/home

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }
ns(){ date +%s%N; }
med(){ # $@ = ascending-safe list of ints -> median (int)
  local arr=($(printf '%s\n' "$@" | sort -n)); local n=${#arr[@]}; local m=$(( (n-1)/2 ))
  echo "${arr[$m]}"; }
min(){ printf '%s\n' "$@" | sort -n | head -1; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
# poll-until with deadline; echoes elapsed_ms on success, rc=1 on timeout
poll(){ # $1=deadline_ms $2=cmd-string ; returns 0 when cmd true
  local dl=$1 cmd=$2 t0 t el; t0=$(ns)
  while :; do eval "$cmd" && { el=$(( ($(ns)-t0)/1000000 )); echo "$el"; return 0; }
            el=$(( ($(ns)-t0)/1000000 )); [ "$el" -ge "$dl" ] && { echo "$el"; return 1; }; sleep 0.02; done; }

# ---- a small authoritative tree over the central services -------------------
cat > .govtime/team.manifest <<'EOF'
mkdir docs
touch docs/a.md
mkdir photos
touch photos/y.jpg
EOF
$Y init --view team --manifest .govtime/team.manifest >/dev/null || { echo "init failed"; exit 1; }
cat > .govtime/narrow.rules <<'EOF'
only /docs
EOF
cat > .govtime/broad.rules <<'EOF'
only /docs
only /photos
EOF

META=127.0.0.1:15172; DATA=127.0.0.1:15173
$Y meta-server --meta-dir "$GROVEFS_HOME" --listen $META > .govtime/meta.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --data-dir "$GROVEFS_HOME" > .govtime/data.log 2>&1 &
DSP=$!
cleanup(){ fusermount -u .govtime/mnt_base .govtime/mnt_dv .govtime/mnt_al 2>/dev/null; kill $MSP $DSP $(jobs -p) 2>/dev/null; }
trap cleanup EXIT
wait_port $META && wait_port $DATA && ok "meta/data servers up" || { bad "servers failed to start"; exit 1; }

# writable base mount + a derived viewer mounted BY NAME, non-strict (kernel TTL caching on)
$Y mount --view team -m .govtime/mnt_base --meta-server $META --data-servers $DATA --foreground > .govtime/base.log 2>&1 &
$Y view --meta-dir "$GROVEFS_HOME" define --name dv --base team --rules .govtime/narrow.rules > .govtime/def.log 2>&1
$Y mount --view dv -m .govtime/mnt_dv --meta-server $META --data-servers $DATA --foreground > .govtime/dv.log 2>&1 &
sleep 2
SB=.govtime/mnt_base; SV=.govtime/mnt_dv
chk "$(T ls $SV | tr '\n' ' ')" "docs " "derived viewer starts narrowed to /docs"

# ---- 1) hot-reload convergence: widen, then narrow, back and forth -----------
echo "===== 1) derived-view hot reload convergence ====="
widen=(); narrow=()
for i in 1 2 3; do
  $Y view --meta-dir "$GROVEFS_HOME" define --name dv --base team --rules .govtime/broad.rules >/dev/null 2>&1
  el=$(poll 6000 "T ls $SV | grep -q photos"); rc=$?
  [ $rc -ne 0 ] && bad "hot reload widen #i did not converge in >6000ms" || widen+=("$el")
  $Y view --meta-dir "$GROVEFS_HOME" define --name dv --base team --rules .govtime/narrow.rules >/dev/null 2>&1
  el=$(poll 6000 "! T ls $SV | grep -q photos"); rc=$?
  [ $rc -ne 0 ] && bad "hot reload narrow #i did not converge in >6000ms" || narrow+=("$el")
done
if [ "${#widen[@]}" -eq 3 ] && [ "${#narrow[@]}" -eq 3 ]; then
  ok "live mount converged across 3 widen+narrow cycles (no remount)"
  echo "  INFO widen  convergence ms: min=$(min "${widen[@]}") median=$(med "${widen[@]}")  (all: ${widen[*]})"
  echo "  INFO narrow convergence ms: min=$(min "${narrow[@]}") median=$(med "${narrow[@]}")  (all: ${narrow[*]})"
fi

# ---- 2) cross-client invalidation immediacy (notify push) --------------------
echo "===== 2) cross-client push-invalidation immediacy (I3) ====="
inval=()
for i in 1 2 3; do
  name="pushed$i.md"
  t0=$(ns)
  T sh -c "printf 'p' > $SB/docs/$name" 2>/dev/null
  el=$(poll 5000 "T ls $SV/docs | grep -q $name"); rc=$?
  if [ $rc -ne 0 ]; then bad "viewer never saw base write (push) #i"; else inval+=("$el"); fi
  T rm "$SB/docs/$name" 2>/dev/null
done
[ "${#inval[@]}" -eq 3 ] && ok "derived viewer saw each base write via the push channel" \
                         && echo "  INFO invalidation-visibility ms: min=$(min "${inval[@]}") median=$(med "${inval[@]}")  (all: ${inval[*]})"

# ---- 3) pure-alias gapless name rotation (--retire) latency ------------------
echo "===== 3) alias --retire rotation latency (atomic, no gap) ====="
$Y view --meta-dir "$GROVEFS_HOME" alias --name pub --real team > .govtime/al1.log 2>&1 \
  && ok "alias pub -> team defined" || bad "alias create: $(cat .govtime/al1.log)"
# measure the rotation command wall-clock (single atomic definition swap)
rt0=$(ns); $Y view --meta-dir "$GROVEFS_HOME" alias --name pubnew --real team --retire pub > .govtime/rot.log 2>&1; rrc=$?
rt=$(( ($(ns)-rt0)/1000000 ))
[ $rrc -eq 0 ] && grep -q gapless .govtime/rot.log && ok "gapless rotation pub -> pubnew" || bad "rotation failed: $(cat .govtime/rot.log)"
echo "  INFO --retire command latency: ${rt}ms"
# the retired old name must NOT resolve to the real tree (zero window)
$Y mount --view pub -m .govtime/mnt_al --meta-server $META --data-servers $DATA --foreground > .govtime/old.log 2>&1 &
OLDP=$!; sleep 1.5
[ ! -d .govtime/mnt_al/docs ] && ok "retired alias does not reach the real tree (no leak window)" || bad "old alias still served team"
kill $OLDP 2>/dev/null; wait $OLDP 2>/dev/null; fusermount -u .govtime/mnt_al 2>/dev/null; rm -rf .govtime/mnt_al; mkdir -p .govtime/mnt_al
# the new name serves the full tree immediately
$Y mount --view pubnew -m .govtime/mnt_al --meta-server $META --data-servers $DATA --foreground > .govtime/new.log 2>&1 &
NEWP=$!; sleep 2
chk "$(T ls .govtime/mnt_al | tr '\n' ' ')" "docs photos " "rotated alias pubnew serves the tree"
T rm .govtime/mnt_al/docs/a.md 2>/dev/null; fusermount -u .govtime/mnt_al 2>/dev/null; kill $NEWP 2>/dev/null; wait $NEWP 2>/dev/null

fusermount -u $SV $SB 2>/dev/null
kill $MSP $DSP $(jobs -p) 2>/dev/null; wait 2>/dev/null

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "GOVTIME: all convergence windows closed (see INFO latencies)" || echo "GOVTIME: SOME WINDOWS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
