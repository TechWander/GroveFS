#!/usr/bin/env bash
# Control-plane-unified e2e: prove the two historical "local-file exceptions"
# (api_keys.db and derived-view .layout definitions) now FOLLOW the metadata
# backend. Under `--meta-store postgres://…` BOTH live in the shared
# `grovefs_control` schema and NOTHING lands in the meta-server's local dir;
# a meta-server started on PG resolves view definitions and mount keys straight
# out of PostgreSQL at M_OPEN. Self-skips cleanly (exit 0) when no PG is
# reachable, so the board stays green without one.
#
# Covers:
#   1) view define / alias / list / rm round-trip through grovefs_control.view_defs
#      with NO local <home>/meta/views/*.layout created;
#   2) apikey create/list through grovefs_control.api_keys with NO local
#      api_keys.db created;
#   3) a meta-server --meta-store PG SERVES a derived lens defined in PG (a remote
#      mount of the derived name shows the shaped, read-only tree);
#   4) that lens hot-reloads: replacing the definition in PG changes what a
#      fresh mount of the same name sees;
#   5) mount admission gated by a PG-stored key: a --require-api-key meta-server
#      on PG admits a mount presenting the key and refuses one that does not.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=${Y:-./target/release/grovefs}
[ -x "$Y" ] || Y=./target/debug/grovefs

# ---- locate a reachable PostgreSQL (env override, else probe 5432 then 5433) ----
PGHOST=127.0.0.1
PGURI="${GROVEFS_TEST_PG_URI:-}"
if [ -z "$PGURI" ]; then
  for P in 5432 5433; do
    if (exec 3<>/dev/tcp/${PGHOST}/$P) 2>/dev/null; then
      exec 3<&- 2>/dev/null; exec 3<&- 2>/dev/null || true
      PGURI="postgres://grove:grovepw@${PGHOST}:${P}/grovefs"; PGPORT=$P; break
    fi
  done
else
  PGPORT=$(printf '%s' "$PGURI" | sed -n 's/.*:\([0-9]*\)\/.*/\1/p')
fi
if [ -z "$PGURI" ]; then
  echo "SUMMARY: pass=0 fail=0 SKIP=1 (no PostgreSQL on 127.0.0.1:5432/5433)"
  echo "note: set GROVEFS_TEST_PG_URI=postgres://… to exercise the control plane end-to-end."
  exit 0
fi
PGUSER=grove; PGDB=grovefs
case "$PGURI" in *://*:*) : ;; esac
psqlq(){ PGPASSWORD=grovepw psql -h "$PGHOST" -p "$PGPORT" -U grove -d grovefs -tA -c "$1" 2>/dev/null; }

# unique per run so concurrent/shared-DB runs never bleed defs or keys
TS=$(date +%s)
BASE="ct${TS}b"; LENS="ct${TS}l"; ALIAS="ct${TS}a"; KLBL="ctlabel${TS}"; AREAL="ct${TS}t"

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# fresh local home dirs (their ABSENCE of control files is asserted, not their presence)
rm -rf .cttest; mkdir -p .cttest/home .cttest/data .cttest/mnt_base .cttest/mnt_lens .cttest/mnt_lens2
BASEHOME=.cttest/home
RULES1=.cttest/r1.txt; RULES2=.cttest/r2.txt
printf 'hide /secret\n' > "$RULES1"
printf 'hide /secret\nhide /notes\n' > "$RULES2"

cleanup(){
  for d in .cttest/mnt_base .cttest/mnt_lens .cttest/mnt_lens2; do fusermount -u "$d" 2>/dev/null; done
  # kill ONLY real pids — a literal `0` would signal the whole process group
  # (including this script) and turn a clean pass into a spurious exit-143.
  for p in "${DP:-}" "${MP:-}" "${MP2:-}" "${RAP:-}"; do
    case "$p" in ''|0) ;; *[!0-9]*) ;; *) kill "$p" 2>/dev/null ;; esac
  done
  # leave PG as we found it: drop this run's defs, keys, and base schema
  psqlq "DELETE FROM grovefs_control.view_defs WHERE name IN ('$LENS','$ALIAS')" >/dev/null
  psqlq "DELETE FROM grovefs_control.api_keys WHERE label='$KLBL'" >/dev/null
  psqlq "DROP SCHEMA IF EXISTS grovefs_v_$(printf '%s' "$BASE" | od -An -tx1 | tr -d ' \n') CASCADE" >/dev/null
  psqlq "DROP SCHEMA IF EXISTS grovefs_v_$(printf '%s' "$AREAL" | od -An -tx1 | tr -d ' \n') CASCADE" >/dev/null
}
trap cleanup EXIT

echo "===== 1) view define + alias land in PG, nothing in the local home ====="
# An alias must name an EXISTING physical view (§5-10). Provision a dedicated
# physical schema `AREAL` as the alias target, deliberately NOT `$BASE` —
# governing `$BASE` would make it unmountable by its real name in section 3.
# `view create` over PG makes a grovefs_v_* schema, not a local db file, so the
# "nothing lands in the local home" assertions still hold.
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" create --view "$AREAL" > .cttest/create.log 2>&1 \
  || { bad "view create (alias target) --meta-store failed: $(cat .cttest/create.log)"; exit 1; }
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" define --name "$LENS" --base "$BASE" --rules "$RULES1" > .cttest/define.log 2>&1 \
  && ok "view define over PG" || bad "view define failed: $(cat .cttest/define.log)"
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" alias --name "$ALIAS" --real "$AREAL" > .cttest/alias.log 2>&1 \
  && ok "view alias over PG" || bad "view alias failed: $(cat .cttest/alias.log)"
# §5-10: an alias to a name with no physical schema is refused at definition.
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" alias --name "ct${TS}x" --real "ct${TS}dangle" > .cttest/alias_dangle.log 2>&1 \
  && bad "alias accepted a dangling physical target" || ok "alias refuses a target with no physical view (§5-10)"
grep -qi "no physical view tree" .cttest/alias_dangle.log \
  && ok "dangling refusal names the missing physical view" || bad "dangle refusal unclear: $(cat .cttest/alias_dangle.log)"
NDEFS=$(psqlq "SELECT count(*) FROM grovefs_control.view_defs WHERE name IN ('$LENS','$ALIAS')")
chk "$NDEFS" "2" "both definitions present in grovefs_control.view_defs"
if [ -e "$BASEHOME/meta/views/$LENS.layout" ]; then bad "LEAK: a local .layout was created under PG"; else ok "no local .layout file under PG"; fi

echo "===== 2) apikey create lands in PG, no local api_keys.db ====="
AKOUT=$($Y apikey --meta-dir "$BASEHOME" --meta-store "$PGURI" create --label "$KLBL" --for mount 2>&1) \
  && ok "apikey create over PG" || bad "apikey create failed: $AKOUT"
NKEYS=$(psqlq "SELECT count(*) FROM grovefs_control.api_keys WHERE label='$KLBL'")
chk "$NKEYS" "1" "the key row lives in grovefs_control.api_keys"
if [ -e "$BASEHOME/meta/api_keys.db" ]; then bad "LEAK: a local api_keys.db was created under PG"; else ok "no local api_keys.db under PG"; fi

echo "===== 3) a meta-server on PG serves a derived lens DEFINED IN PG ====="
DATA=127.0.0.1:15124; META=127.0.0.1:15125
$Y data-server --listen $DATA --data-dir .cttest/data > .cttest/data.log 2>&1 & DP=$!
# Views are explicit: provision the PG schema up front, before any mount.
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" create --view "$BASE" > .cttest/createbase.log 2>&1 \
  || { bad "view create --meta-store failed: $(cat .cttest/createbase.log)"; exit 1; }
$Y meta-server --listen $META --home "$BASEHOME" --meta-store "$PGURI" > .cttest/meta.log 2>&1 & MP=$!
wait_port $DATA || { echo "  (data-server failed)"; cat .cttest/data.log; }
wait_port $META || { echo "  (meta-server failed)"; cat .cttest/meta.log; }

# seed the (already created) base tree through a remote mount
$Y --home .cttest/m0 mount --view "$BASE" --mountpoint .cttest/mnt_base --meta-server $META --data-servers $DATA --foreground > .cttest/base.log 2>&1 & RAP=$!
sleep 1.5
B=.cttest/mnt_base
if T sh -c "mkdir -p $B/secret $B/notes; printf 'pub' > $B/readme" && sync "$B" 2>/dev/null; then ok "seeded base tree (secret, notes, readme)"; else bad "seeding base failed: $(cat .cttest/base.log)"; fi
fusermount -u "$B" 2>/dev/null; wait $RAP 2>/dev/null; RAP=0
sleep 1.5

# mount the derived lens name: server resolves hide /secret from PG
$Y --home .cttest/m0 mount --view "$LENS" --mountpoint .cttest/mnt_lens --meta-server $META --data-servers $DATA --foreground > .cttest/lens.log 2>&1 & RAP=$!
sleep 2.5
L=.cttest/mnt_lens
if [ ! -e "$L/secret" ] && [ -e "$L/readme" ]; then ok "lens hides /secret, keeps /readme (definition read from PG)"; else bad "lens shape wrong: $(ls -a $L 2>/dev/null | tr '\n' ' ')"; fi
if T sh -c "printf x > $L/secret2" 2>/dev/null; then bad "derived lens was writable"; else ok "derived lens is read-only"; fi
fusermount -u "$L" 2>/dev/null; wait $RAP 2>/dev/null; RAP=0
sleep 1.5

echo "===== 4) hot-reload: replacing the PG definition changes a fresh mount ====="
$Y view --meta-dir "$BASEHOME" --meta-store "$PGURI" define --name "$LENS" --base "$BASE" --rules "$RULES2" > .cttest/define2.log 2>&1 \
  && ok "redefined lens (now also hides /notes) in PG" || bad "redefine failed: $(cat .cttest/define2.log)"
$Y --home .cttest/m0 mount --view "$LENS" --mountpoint .cttest/mnt_lens2 --meta-server $META --data-servers $DATA --foreground > .cttest/lens2.log 2>&1 & RAP=$!
sleep 2.5
L2=.cttest/mnt_lens2
if [ ! -e "$L2/secret" ] && [ ! -e "$L2/notes" ] && [ -e "$L2/readme" ]; then ok "fresh mount reflects the reloaded PG definition (secret+notes hidden)"; else bad "reload shape wrong: $(ls -a $L2 2>/dev/null | tr '\n' ' ')"; fi
fusermount -u "$L2" 2>/dev/null; wait $RAP 2>/dev/null; RAP=0

echo "===== 5) mount admission gated by the PG-stored key ====="
kill $MP 2>/dev/null; wait $MP 2>/dev/null; MP=0
$Y meta-server --listen 127.0.0.1:15126 --home "$BASEHOME" --meta-store "$PGURI" --require-api-key > .cttest/meta_keyed.log 2>&1 & MP2=$!
wait_port 127.0.0.1:15126 || { echo "  (keyed meta-server failed)"; cat .cttest/meta_keyed.log; }
FULLKEY=$(printf '%s' "$AKOUT" | sed -n "s/.*key:[[:space:]]*\([0-9a-f:]*\).*/\1/p" | head -1)
mkdir -p /tmp/never.$TS
# no key -> refused
$Y --home .cttest/m0 mount --view "$BASE" --mountpoint /tmp/never.$TS --meta-server 127.0.0.1:15126 --data-servers $DATA --foreground > .cttest/nokey.log 2>&1 & RAP=$!
sleep 1.5
if grep -qi "api.key\|auth\|denied\|refus" .cttest/nokey.log; then ok "mount without a key is refused by the PG-keyed meta-server"; else bad "expected key refusal: $(tail -3 .cttest/nokey.log)"; fi
kill $RAP 2>/dev/null; RAP=0
fusermount -u /tmp/never.$TS 2>/dev/null
# with the PG-minted key -> admitted
$Y --home .cttest/m0 mount --view "$BASE" --mountpoint .cttest/mnt_lens --meta-server 127.0.0.1:15126 --data-servers $DATA --api-key "$FULLKEY" --foreground > .cttest/withkey.log 2>&1 & RAP=$!
sleep 2.5
if T ls .cttest/mnt_lens >/dev/null 2>&1; then ok "mount WITH the PG-stored key is admitted"; else bad "keyed mount failed: $(tail -3 .cttest/withkey.log)"; fi
fusermount -u .cttest/mnt_lens 2>/dev/null; wait $RAP 2>/dev/null; RAP=0

# ---- summary ----
echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "ALL CONTROL-PLANE/PG CHECKS PASSED"; exit 0; else echo "SOME CHECKS FAILED"; exit 1; fi
