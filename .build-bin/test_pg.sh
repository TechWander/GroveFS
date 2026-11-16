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

cleanup(){ for d in .pgtest/mnt_a .pgtest/mnt_b .pgtest/mnt_c; do fusermount -u "$d" 2>/dev/null; done; for p in "${RAP:-0}" "${RBP:-0}" "${DP:-0}" "${MP:-0}" "${RAP2:-0}" "${MP2:-0}"; do [ "$p" != 0 ] && kill "$p" 2>/dev/null; done; true; }
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

echo "===== 8) multi-instance registry (file: spec): placement, routing, forget-on-rm ====="
# A second DATABASE on the same cluster stands in for a second INSTANCE: this
# section verifies the routing/placement MECHANISM — registry expansion, the
# startup banner, least-loaded placement at create, a --pg-instance pin, the
# schema landing on the home the placement picked, a mount served through the
# SAME meta-server (the client never sees a database URI), and rm forgetting
# the placement row. Real capacity growth means SEPARATE clusters (PG caps
# connections per cluster process, not per database) — that is deployment
# topology, not something a CI box can assert.
if ! command -v psql >/dev/null; then
    echo "  INFO: host psql unavailable — multi-instance registry section skipped"
else
    TEAMDB="grovefs_team_$(date +%s | tail -c 6)"
    if ! psql "postgres://grove:grovepw@${PGHOST}:${PGPORT}/postgres" -qtA \
         -c "CREATE DATABASE $TEAMDB" > /dev/null 2>&1; then
        echo "  INFO: cannot create the second database — registry section skipped"
    else
        TEAMURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/${TEAMDB}"
        REG="$PWD/.pgtest/registry.conf"
        printf '# first entry = control plane\nmain = %s\nteam = %s\n' "$PGURI" "$TEAMURI" > "$REG"
        FREG="file:$REG"
        PGQ2(){ psql "postgres://grove:grovepw@${PGHOST}:${PGPORT}/$1" -qtA -c "$2" 2>/dev/null; }
        VIEW_A="regA$(date +%s)"   # auto-placed: least-loaded, ties -> registry order
        VIEW_B="regB$(date +%s)"   # pinned to 'team' with --pg-instance
        META2=127.0.0.1:15098
        mkdir -p .pgtest/mnt_c .pgtest/mnt_a2
        # Section 7 killed the data-server; restart it so this section's data-
        # plane assertions are REAL (a dead server would still pass the write/
        # read pair out of its own page cache, proving nothing).
        if [ "${DP:-0}" = 0 ]; then
            $Y data-server --listen $DATA --data-dir .pgtest/central/data > .pgtest/data2.log 2>&1 &
            DP=$!
            wait_port $DATA || { echo "  FAIL: data-server restart failed"; cat .pgtest/data2.log; fail=$((fail+1)); }
        fi
        $Y meta-server --listen $META2 --home .pgtest/home --meta-store "$FREG" > .pgtest/meta2.log 2>&1 &
        MP2=$!
        wait_port $META2 || { echo "  FAIL: registry meta-server failed"; cat .pgtest/meta2.log; fail=$((fail+1)); }
        grep -q "2 PG instance(s) \[main, team\]" .pgtest/meta2.log \
            && ok "startup banner names both registry instances" \
            || bad "startup banner wrong: $(grep 'PG instance' .pgtest/meta2.log)"
        grep -q "control plane on 'main'" .pgtest/meta2.log \
            && ok "control plane pinned to the first registry entry" \
            || bad "control plane banner missing"

        $Y view --meta-store "$FREG" create --view "regBad$(date +%s)" --pg-instance nosuch > .pgtest/createBad.log 2>&1 \
            && bad "an unknown --pg-instance was accepted" \
            || ok "an unknown --pg-instance is refused"

        $Y view --meta-store "$FREG" create --view "$VIEW_A" > .pgtest/createA.log 2>&1 \
            && ok "registry create (auto placement) succeeded" \
            || bad "registry create failed: $(cat .pgtest/createA.log)"
        HOME_A=$(sed -n "s/.*on instance '\([^']*\)'.*/\1/p" .pgtest/createA.log | head -1)
        DB_A=grovefs; [ "$HOME_A" = team ] && DB_A=$TEAMDB
        [ -n "$HOME_A" ] && ok "create reported its home: '$HOME_A'" \
            || bad "create did not report a home: $(cat .pgtest/createA.log)"
        if [ -n "$HOME_A" ]; then
            SCHA=$(printf '%s' "$VIEW_A" | od -An -tx1 | tr -d ' \n')
            chk "$(PGQ2 "$DB_A" "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHA}'")" "1" \
                "auto-placed schema really sits on '$HOME_A'"
        fi

        $Y view --meta-store "$FREG" create --view "$VIEW_B" --pg-instance team > .pgtest/createB.log 2>&1 \
            && ok "pinned create (--pg-instance team) succeeded" \
            || bad "pinned create failed: $(cat .pgtest/createB.log)"
        grep -q "on instance 'team'" .pgtest/createB.log \
            && ok "pinned create reports 'team'" \
            || bad "pinned create home wrong: $(cat .pgtest/createB.log)"
        SCHB=$(printf '%s' "$VIEW_B" | od -An -tx1 | tr -d ' \n')
        chk "$(PGQ2 "$TEAMDB" "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHB}'")" "1" \
            "pinned schema sits on the team database"

        $Y --home .pgtest/m1 mount --view "$VIEW_B" --mountpoint .pgtest/mnt_c --meta-server $META2 --data-servers $DATA --foreground > .pgtest/mnt_c.log 2>&1 &
        RAP2=$!
        sleep 1.5
        C=.pgtest/mnt_c
        [ -d "$C" ] && ok "C mounted on the team instance via the SAME meta-server" \
            || bad "C not mounted ($(cat .pgtest/mnt_c.log))"
        T sh -c "printf 'gamma' > $C/in_team" && ok "C writes (team-side data plane)" || bad "C write failed"
        sync "$C" 2>/dev/null; sleep 1.5
        chk "$(T cat $C/in_team)" "gamma" "C reads back its own file"

        fusermount -u .pgtest/mnt_c 2>/dev/null; kill $RAP2 2>/dev/null; RAP2=0; sleep 0.5
        $Y view --meta-dir .pgtest/home --meta-store "$FREG" rm --name "$VIEW_B" --base --data-dir .pgtest/central/data > .pgtest/rmB.log 2>&1
        RRC=$?
        chk "$RRC" "0" "view rm on the registry spec succeeds"
        chk "$(PGQ2 "$TEAMDB" "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHB}'")" "0" \
            "team schema dropped"
        chk "$(PGQ2 grovefs "select count(*) from grovefs_control.view_placement where view='$VIEW_B'")" "0" \
            "placement row forgotten on rm"

        echo "  --- move: the cutover mechanism (snapshot -> build -> flip -> retire) ---"
        SCHA=$(printf '%s' "$VIEW_A" | od -An -tx1 | tr -d ' \n')
        $Y view --meta-dir .pgtest/home --meta-store "$FREG" move --view "$VIEW_A" --pg-instance team > .pgtest/moveA.log 2>&1
        RRC=$?
        chk "$RRC" "0" "view move main->team succeeds"
        grep -q "instance 'main' -> 'team'" .pgtest/moveA.log \
            && ok "move reports the cutover" \
            || bad "move output wrong: $(cat .pgtest/moveA.log)"
        chk "$(PGQ2 grovefs "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHA}'")" "0" \
            "source schema dropped on main"
        chk "$(PGQ2 "$TEAMDB" "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHA}'")" "1" \
            "target schema exists on team"

        $Y view --meta-dir .pgtest/home --meta-store "$FREG" move --view "$VIEW_A" --pg-instance team > .pgtest/moveSame.log 2>&1
        [ $? != 0 ] && ok "a move to the current home is refused" \
            || bad "a move to the current home was accepted"
        grep -q "already lives on" .pgtest/moveSame.log \
            && ok "the refusal names the state" \
            || bad "refusal unclear: $(cat .pgtest/moveSame.log)"

        $Y view --meta-dir .pgtest/home --meta-store "$FREG" move --view "$VIEW_A" --pg-instance main > .pgtest/moveBack.log 2>&1
        RRC=$?
        chk "$RRC" "0" "move back team->main succeeds"

        $Y view --meta-dir .pgtest/home --meta-store "$FREG" move --view "$VIEW_A" --pg-instance team --keep-source > .pgtest/moveKeep.log 2>&1
        RRC=$?
        chk "$RRC" "0" "--keep-source move succeeds"
        chk "$(PGQ2 grovefs "select count(*) from information_schema.schemata where schema_name='grovefs_v_${SCHA}'")" "1" \
            "kept source schema still on main"

        $Y view --meta-dir .pgtest/home --meta-store "$FREG" move --view "$VIEW_A" --pg-instance main > .pgtest/moveRollback.log 2>&1
        RRC=$?
        chk "$RRC" "0" "rollback move reuses the matching copy on main"
        grep -q "instance 'team' -> 'main'" .pgtest/moveRollback.log \
            && ok "rollback cutover reported" \
            || bad "rollback output wrong: $(cat .pgtest/moveRollback.log)"

        $Y --home .pgtest/m1 mount --view "$VIEW_A" --mountpoint .pgtest/mnt_a2 --meta-server $META2 --data-servers $DATA --foreground > .pgtest/mnt_a2.log 2>&1 &
        RAP3=$!
        sleep 1.5
        A2=.pgtest/mnt_a2
        [ -d "$A2" ] && ok "moved view mounts (placement resolves to main)" \
            || bad "moved view not mounted ($(cat .pgtest/mnt_a2.log))"
        T sh -c "printf 'delta' > $A2/after_move" && ok "moved view writes" || bad "moved view write failed"
        sync "$A2" 2>/dev/null; sleep 1.5
        chk "$(T cat $A2/after_move)" "delta" "moved view reads back its own write"
        fusermount -u .pgtest/mnt_a2 2>/dev/null; kill $RAP3 2>/dev/null; RAP3=0; sleep 0.5

        $Y view --meta-dir .pgtest/home --meta-store "$FREG" rm --name "$VIEW_A" --base --data-dir .pgtest/central/data > .pgtest/rmA.log 2>&1
        RRC=$?
        chk "$RRC" "0" "registry rm of the auto-placed view succeeds"

        kill $MP2 2>/dev/null; MP2=0
        psql "postgres://grove:grovepw@${PGHOST}:${PGPORT}/postgres" -qtA \
            -c "DROP DATABASE $TEAMDB" > /dev/null 2>&1 \
            && ok "second database dropped" \
            || echo "  INFO: dropdb $TEAMDB failed (leftover; harmless, next run uses a fresh name)"
    fi
fi

# ---- summary ----
echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "ALL PG-BACKEND CHECKS PASSED"; exit 0; else echo "SOME PG CHECKS FAILED"; exit 1; fi
