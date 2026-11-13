#!/usr/bin/env bash
# PostgreSQL metadata backend end-to-end over real FUSE mounts against a
# central meta-server started with `--meta-store postgres://…`. Proves:
#   - the meta-server boots and mounts a client against PG (the view's
#     `grovefs_v_<hex(view)>` schema is provisioned explicitly by
#     `view create --meta-store`, never by a mount);
#   - CRUD (mkdir/touch/cat/ls), rename, hardlink and symlink all round-trip
#     through the PG-backed MetaProvider;
#   - two clients on the same view converge on one authoritative PG tree
#     (writes from A are visible to B without any local state on either side);
#   - GroveFS-native HA (`--role standby`/`--peer`) is refused at serve() time
#     under PG, with a clear error naming the incompatibility.
# Requires: a reachable PostgreSQL on 127.0.0.1 (5432 or 5433; set PGPORT to pin
# one) with a `grove/grovepw` role and a `grovefs` database. The suite SKIPS
# cleanly (exit 0, no FAILs) if PG is unreachable, so the whole board stays GREEN
# on hosts without PG -- which is precisely why the port is PROBED rather than
# assumed: pinned to 5432 the suite skipped on a host whose PostgreSQL listens on
# 5433, and the board printed `PASS` over `SKIP=1 pass=0`, a green that proved
# nothing.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

PGHOST=${PGHOST:-127.0.0.1}
if [ -z "${PGPORT:-}" ]; then
    for P in 5432 5433; do
        if (exec 3<>/dev/tcp/${PGHOST}/$P) 2>/dev/null; then
            exec 3<&- 2>/dev/null; exec 3<&- 2>/dev/null || true
            PGPORT=$P; break
        fi
    done
    PGPORT=${PGPORT:-5432}
fi
PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"

# ---- PG reachability probe (skip cleanly if absent) ----
if ! (exec 3<>/dev/tcp/${PGHOST}/${PGPORT}) 2>/dev/null; then
    echo "SUMMARY: pass=0 fail=0 SKIP=1 (no PostgreSQL on ${PGHOST}:${PGPORT})"
    echo "note: PG suite self-skips when the server is unreachable; install one"
    echo "      (e.g. docker run --name postgres -p 5432:5432 -e POSTGRES_PASSWORD=... postgres:latest)"
    echo "      to exercise the backend end-to-end."
    exit 0
fi
exec 3<&- 2>/dev/null; exec 3<&- 2>/dev/null || true

META=127.0.0.1:15096
DATA=127.0.0.1:15097
VIEW="pg$(date +%s)"   # unique per run: fresh grovefs_v_<hex> schema, no bleed
rm -rf .pgtest; mkdir -p .pgtest/central/data .pgtest/home .pgtest/m1 .pgtest/m2 .pgtest/mnt_a .pgtest/mnt_b

wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }

# Run a psql statement through the docker `postgres` container (host psql
# unavailable in CI). Falls back to a no-op when docker is missing so the
# server-side row-count assertions degrade gracefully (warn, not fail) — the
# data-plane assertions below still exercise PG through the meta-server.
psql_in_docker(){
    if sudo -n docker ps --format '{{.Names}}' 2>/dev/null | grep -qx postgres; then
        sudo -n docker exec -e PGPASSWORD=grovepw postgres \
            psql -U grovefs -d grovefs -tA -c "$1" 2>/dev/null
    else
        echo ""
    fi
}

echo "===== 1) refuse --role standby + --meta-store postgres://… at serve() time ====="
$Y meta-server --listen 127.0.0.1:15055 --home .pgtest/refuse \
    --role standby --peer 127.0.0.1:1 \
    --meta-store "$PGURI" > .pgtest/refuse.log 2>&1
RRC=$?
[ "$RRC" != 0 ] && ok "PG + HA refused at startup (rc=$RRC)" \
    || bad "PG + HA should have failed but returned 0"
grep -qi "cannot be combined\|incompatible" .pgtest/refuse.log \
    && ok "startup error names the incompatibility" \
    || bad "startup error unclear: $(cat .pgtest/refuse.log)"

echo "===== 2) start data-server and PG-backed meta-server ====="
$Y data-server --listen $DATA --data-dir .pgtest/central/data > .pgtest/data.log 2>&1 &
DP=$!
# Views are explicit: provision the PG schema up front, before any mount.
$Y view --meta-store "$PGURI" --meta-dir .pgtest/home create --view "$VIEW" > .pgtest/create.log 2>&1 \
    || { echo "FAIL: view create --meta-store failed"; cat .pgtest/create.log; exit 1; }
$Y meta-server --listen $META --home .pgtest/home --meta-store "$PGURI" > .pgtest/meta.log 2>&1 &
MP=$!
wait_port $DATA || { echo "data-server failed"; cat .pgtest/data.log; }
wait_port $META || { echo "meta-server failed"; cat .pgtest/meta.log; }

cleanup(){ for d in .pgtest/mnt_a .pgtest/mnt_b; do fusermount -u "$d" 2>/dev/null; done; for p in "${RAP:-0}" "${RBP:-0}" "${DP:-0}" "${MP:-0}"; do [ "$p" != 0 ] && kill "$p" 2>/dev/null; done; true; }
RAP=0; RBP=0
trap cleanup EXIT

echo "===== 3) mount A against the pre-created grovefs_v_<hex> schema ====="
$Y --home .pgtest/m1 mount --view "$VIEW" --mountpoint .pgtest/mnt_a --meta-server $META --data-servers $DATA --foreground > .pgtest/mnt_a.log 2>&1 &
RAP=$!
sleep 1.5
A=.pgtest/mnt_a
[ -d "$A" ] && ok "A mounted" || bad "A not mounted ($(cat .pgtest/mnt_a.log))"

T mkdir "$A/projects"                && ok "A mkdir (PG INSERT)"        || bad "A mkdir failed"
T sh -c "printf 'alpha' > $A/hello"  && ok "A write file"               || bad "A write failed"
sync "$A" 2>/dev/null; sleep 1.5
chk "$(T cat $A/hello)" "alpha" "A reads back its own file"

T mkdir "$A/sub"                     && ok "A mkdir sub"                || bad "A mkdir sub failed"
T mv "$A/hello" "$A/sub/renamed"     && ok "A rename (PG tx)"           || bad "A rename failed"
chk "$(T cat $A/sub/renamed)" "alpha" "content survived rename"
T ln "$A/sub/renamed" "$A/hardlink"  && ok "A hardlink (PG link_node)"  || bad "A hardlink failed"
T ln -s sub/renamed "$A/sym"         && ok "A symlink (PG create_symlink)" || bad "A symlink failed"
chk "$(T readlink $A/sym)" "sub/renamed" "symlink target round-trips"

echo "===== 4) server-side view: PG actually holds the rows (schema-per-view) ====="
SCH=$(printf '%s' "$VIEW" | od -An -tx1 | tr -d ' \n')
CNT=$(psql_in_docker "SELECT count(*) FROM grovefs_v_${SCH}.node")
if [ -n "$CNT" ]; then
    # root + projects + sub + renamed + hardlink + sym = 6 (data_ref shared
    # across the hardlink; each is its own row).
    [ "$CNT" -ge 6 ] && ok "PG grovefs_v_${SCH}.node rows=$CNT (>=6 expected)" \
        || bad "PG row count unexpectedly low: $CNT"
else
    echo "  INFO: docker exec psql unavailable — server-side row count assertion skipped"
fi

echo "===== 5) mount B on same view: cross-client convergence (shared PG tree) ====="
$Y --home .pgtest/m2 mount --view "$VIEW" --mountpoint .pgtest/mnt_b --meta-server $META --data-servers $DATA --foreground > .pgtest/mnt_b.log 2>&1 &
RBP=$!
sleep 2.5
B=.pgtest/mnt_b
if T ls "$B" | grep -qx projects; then ok "B sees A's 'projects' dir"; else bad "B missing A's dir ($(T ls $B | tr '\n' ' '))"; fi
chk "$(T cat $B/sub/renamed)" "alpha" "B reads A's renamed file (shared blob)"
T sh -c "printf 'beta' > $B/second" && ok "B writes" || bad "B write failed"
sync "$B" 2>/dev/null; sleep 2.5
chk "$(T cat $A/second)" "beta" "A sees B's new file (single authoritative tree)"

echo "===== 6) unlink -> PG row deleted, entry gone from both mounts ====="
T rm "$A/hardlink" && ok "A removes hardlink" || bad "A unlink failed"
sleep 1.5
[ ! -e "$B/hardlink" ] && ok "hardlink gone on B (shared delete)" || bad "hardlink still visible on B"

echo "===== 7) teardown: view rm --base drops the schema AND unwinds the ledger ====="
# This suite used to leak by construction: every run left its `grovefs_v_<hex>`
# schema behind (13 schemas / 288 stuck `grovefs_refs.holders` rows when it was
# measured), because a PG tree had no removal path in the CLI at all. It ends by
# removing what it created, and asserts BOTH halves — the schema is gone and the
# view's ledger rows are gone — since doing only the drop is exactly the leak
# `.bneck/pg_drop_views.sh` used to exist to clean up by hand.
for d in .pgtest/mnt_a .pgtest/mnt_b; do fusermount -u "$d" 2>/dev/null; done
kill $RAP $RBP 2>/dev/null; RAP=0; RBP=0
kill $MP $DP 2>/dev/null; MP=0; DP=0
sleep 0.5
export PGPASSWORD=grovepw
PGQ(){ psql "postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs" -qtA -c "$1" 2>/dev/null; }
SCH=$(printf '%s' "$VIEW" | od -An -tx1 | tr -d ' \n')
if ! command -v psql >/dev/null; then
    echo "  INFO: host psql unavailable — teardown row assertions skipped (the rm still ran)"
fi
BEFORE_SCH=""
[ -n "$(command -v psql)" ] && BEFORE_SCH=$(PGQ "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCH}'")
if [ -n "$(command -v psql)" ]; then
    chk "$BEFORE_SCH" "1" "schema grovefs_v_${SCH} exists before teardown"
    LED_REFS=$(PGQ "select data_ref from grovefs_v_${SCH}.node where data_ref is not null order by 1")
    BEFORE_LED=$(printf '%s\n' "$LED_REFS" | grep -c .)
    [ "${BEFORE_LED:-0}" -gt 0 ] && ok "the view booked ${BEFORE_LED} ledger row(s)" \
        || bad "ledger booked nothing for a view that holds files (got '$BEFORE_LED')"
fi
$Y view --meta-dir .pgtest/home --meta-store "$PGURI" rm --name "$VIEW" --base --data-dir .pgtest/central/data > .pgtest/rm.log 2>&1
RRC=$?
[ "$RRC" = 0 ] && ok "view rm --base on postgres://… succeeds" || bad "view rm --base failed: $(cat .pgtest/rm.log)"
if [ -n "$(command -v psql)" ]; then
    chk "$(PGQ "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCH}'")" "0" \
        "schema grovefs_v_${SCH} is gone"
    # The PG cluster is SHARED with the other suites (and with whatever an
    # earlier killed run left behind), so a global `holders = 0` answers the
    # wrong question: it fails on THEIR residue, not on a leak of ours. Ask the
    # precise one instead — for every content key THIS view held, the ledger row
    # must be gone unless a surviving schema still names that key (a legitimate
    # shared hold, which is exactly what `holders` is for).
    RESID=0
    for r in $LED_REFS; do
        [ "$(PGQ "select count(*) from grovefs_refs.holders where data_ref='$r'")" = "0" ] && continue
        STILL=0
        for s in $(PGQ "select schema_name from information_schema.schemata where schema_name like 'grovefs_v_%'"); do
            STILL=$((STILL + $(PGQ "select count(*) from ${s}.node where data_ref='$r'")))
        done
        [ "$STILL" = 0 ] && RESID=$((RESID + 1))
    done
    chk "$RESID" "0" \
        "every ledger row this view booked is unwound (holds surviving schemas still need are kept)"
fi
$Y view --meta-dir .pgtest/home --meta-store "$PGURI" rm --name "$VIEW" --base > .pgtest/rm2.log 2>&1
[ $? != 0 ] && ok "a second removal is refused (a typo never reads as success)" \
    || bad "a second removal of the same view returned 0"

# ---- summary ----
echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "ALL PG-BACKEND CHECKS PASSED"; exit 0; else echo "SOME PG CHECKS FAILED"; exit 1; fi
