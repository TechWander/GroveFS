#!/usr/bin/env bash
# Engine completeness matrix: run the SAME governance sequence against the three
# metadata backends and print one table.
#
# What this measures is the GOVERNANCE plane (view trees, definitions, the
# ledger, the cluster's inventory/lease rows, the offline tools, and the
# TEARDOWN of a base tree). The DATA plane is covered per engine by the suites:
# `run_all.sh` (SQLite, 33 suites), `test_pg.sh` + `test_pg_parallel.sh` +
# `test_control.sh` (PG over real FUSE mounts, now self-cleaning through
# `view rm --base`), `test_fdb.sh` (272 assertions, re-run on the four-zone
# cluster) and `.bneck/fdb3vm_lease.sh` (cross-host lease/fault/clock).
#
# A cell is OK when the operation worked on that engine, REFUSED when the engine
# is designed not to support it (the refusal is quoted), and ERROR otherwise —
# an ERROR is a gap to fix, a REFUSED is a documented boundary to write down.
#
# The sequence ends by REMOVING what it created on every engine, and then
# verifies the store is actually empty (no `<view>.db`, no `grovefs_v_*` schema,
# no subspace). A teardown that leaves residue is not a teardown: that is
# exactly why `rmbase` is a row in the table rather than a cleanup detail.
#
# usage: bash .bneck/engine_matrix.sh
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/fdb/release/grovefs}
[ -x "$BIN" ] || BIN=$ROOT/target/release/grovefs
# The fdb arm needs libfdb_c at run time. The packaged build carries it in
# $ORIGIN/lib; a source-tree build does not, so point at the vendored copies
# instead of failing the whole fdb column with "error while loading shared
# libraries" (that reads like an engine gap when it is a PATH problem).
if ldd "$BIN" 2>/dev/null | grep -q 'libfdb_c.*not found'; then
    for d in "$ROOT/dist/grovefs-fdb-experimental-linux-x86_64/lib" \
             "$ROOT/target/fdb/release/lib" \
             "$ROOT/.fdbbin/lib" "$ROOT/../.fdbbin/lib"; do
        [ -e "$d/libfdb_c.so" ] && { export LD_LIBRARY_PATH="$d${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"; break; }
    done
fi
PGSTORE=${PGSTORE:-postgres://grove:grovepw@127.0.0.1:5433/grovefs}
FDBSTORE=${FDBSTORE:-fdb:///data/grovefs/.fdb3vm/cluster3vm}
W=/tmp/engmx
rm -rf "$W"; mkdir -p "$W/sqlite" "$W/pg" "$W/fdb" "$W/src"
printf 'hide /nothing-here\n' > "$W/rules.layout"
mkdir -p "$W/src/sub" && echo hello > "$W/src/f.txt" && echo world > "$W/src/sub/g.txt"

CAPS="create define alias fork list export import dirimport refs gc scan lease ha rmbase"
declare -A LABEL=(
  [create]="view create   (provision a base tree)"
  [define]="view define   (derived lens)"
  [alias]="view alias    (pure alias)"
  [fork]="view fork     (independent tree)"
  [list]="view list     (definitions)"
  [export]="view export   (metadata dump)"
  [import]="view import   (dump -> tree)"
  [dirimport]="grovefs import (directory -> view)"
  [refs]="refs rebuild  (cross-view ledger)"
  [gc]="gc            (offline orphan reclaim)"
  [scan]="view scan     (cluster inventory)"
  [lease]="view lease    (writer lease rows)"
  [ha]="meta-server --role standby (native HA)"
  [rmbase]="view rm --base (tear the tree down)"
)

classify(){ # rc log -> OK | REFUSED | ERROR
    local rc=$1 log=$2
    [ "$rc" -eq 0 ] && { echo OK; return; }
    if grep -qiE 'refus|not supported|not implemented|unsupported|SQLite-only|only (works|applies|exists)|incompatible|operates on a local|exists only for|is not an fdb|has no|cannot be combined|rewrites a local|audits LOCAL|reclaim there|no metadata directory' "$log"; then
        echo REFUSED
    else
        echo ERROR
    fi
}
detail(){ grep -viE 'setlocale' "$1" 2>/dev/null | grep -v '^[[:space:]]*$' | head -1 | sed 's/[[:space:]]\+/ /g' | cut -c1-96; }

record(){ printf '%s|%s|%s\n' "$1" "$2" "$3" >> "$W/$ENG.res"; }

# run_cap <cap> <cmd...>  — one measured command for the engine under test.
run_cap(){
    local cap=$1; shift
    "$@" > "$W/$ENG.$cap.log" 2>&1
    local rc=$?
    local v; v=$(classify "$rc" "$W/$ENG.$cap.log")
    record "$cap" "$v" "$(detail "$W/$ENG.$cap.log")"
    printf '  %-10s %-8s %s\n' "$cap" "$v" "$(detail "$W/$ENG.$cap.log")"
}

engine(){
    ENG=$1; local md=$2 store=$3
    : > "$W/$ENG.res"
    echo "===== engine: $ENG  (meta-dir $md, store ${store:-sqlite}) ====="
    local V=("$BIN" view --meta-dir "$md")
    [ -n "$store" ] && V=("$BIN" view --meta-dir "$md" --meta-store "$store")

    run_cap create  "${V[@]}" create --view mxbase
    run_cap define  "${V[@]}" define --name mxlens --base mxbase --rules "$W/rules.layout"
    run_cap alias   "${V[@]}" alias --name mxpub --real mxbase
    run_cap fork    "${V[@]}" fork --name mxfork --base mxbase
    run_cap list    "${V[@]}" list
    run_cap export  "${V[@]}" export --view mxbase --out "$W/$ENG.dump" --force
    if [ -s "$W/$ENG.dump" ]; then
        run_cap import "${V[@]}" import --view mximp --in "$W/$ENG.dump"
    else
        record import SKIPPED "no dump to import (export was refused)"
        printf '  %-10s %-8s %s\n' import SKIPPED "no dump to import (export was refused)"
    fi
    run_cap dirimport "$BIN" import --meta-dir "$md" --data-dir "$md" --view mxdir --source "$W/src" \
        ${store:+--meta-store "$store"}
    run_cap refs    "$BIN" refs --meta-dir "$md" ${store:+--meta-store "$store"} rebuild
    # `gc` reads the BACKEND from the environment (it has no --meta-store flag):
    # without GROVEFS_META_STORE the guard cannot fire and a store URI would fall
    # through to "no metadata directory", which is a different (and misleading)
    # answer than the designed refusal.
    GROVEFS_META_STORE="$store" "$BIN" gc --meta-dir "$md" --data-dir "$md" > "$W/$ENG.gc.log" 2>&1
    local grc=$?
    local gv; gv=$(classify "$grc" "$W/$ENG.gc.log")
    record gc "$gv" "$(detail "$W/$ENG.gc.log")"
    printf '  %-10s %-8s %s\n' gc "$gv" "$(detail "$W/$ENG.gc.log")"
    run_cap scan    "$BIN" view --meta-store "${store:-sqlite}" scan
    run_cap lease   "$BIN" view --meta-store "${store:-sqlite}" lease
    # Native HA: SQLite ships snapshots; the other engines must refuse. The
    # classification is by MESSAGE, not by exit status — a standby asked for a
    # peer that is not there may exit for its own reasons, and that is not the
    # incompatibility under test.
    timeout 4 "$BIN" meta-server --meta-dir "$md" --listen 127.0.0.1:50999 \
        --role standby --peer 127.0.0.1:50998 ${store:+--meta-store "$store"} \
        > "$W/$ENG.ha.log" 2>&1
    local rc=$?
    if grep -qiE 'sqlite-only|cannot be combined' "$W/$ENG.ha.log"; then
        record ha REFUSED "$(detail "$W/$ENG.ha.log")"
        printf '  %-10s %-8s %s\n' ha REFUSED "$(detail "$W/$ENG.ha.log")"
    elif [ $rc -eq 124 ]; then
        record ha OK "accepted and stayed up (killed after 4s)"
        printf '  %-10s %-8s %s\n' ha OK "accepted and stayed up (killed after 4s)"
    else
        record ha OK "accepted (exited rc=$rc: $(detail "$W/$ENG.ha.log"))"
        printf '  %-10s %-8s %s\n' ha OK "accepted (rc=$rc)"
    fi
    # Teardown is a CAPABILITY, not cleanup: definitions first, then the base
    # trees. On SQLite that unlinks meta/<view>.db, on PG it unbooks + drops the
    # schema in one transaction, on FDB it clears the subspace and unwinds the
    # cluster ledger — three different mechanisms behind one command, which is
    # exactly what the row is for. The measured tree is mxbase; the two the
    # import capabilities created (mxdir, mximp) are removed first so the row
    # measures ONE teardown and the verification below has nothing to forgive.
    for v in mxpub mxlens mxfork; do
        "${V[@]}" rm --name "$v" >/dev/null 2>&1
    done
    "${V[@]}" rm --name mximp --base >/dev/null 2>&1
    "${V[@]}" rm --name mxdir --base >/dev/null 2>&1
    run_cap rmbase  "${V[@]}" rm --name mxbase --base --data-dir "$md"
}

# Fresh store every run, and clear anything a PREVIOUS run left behind: a
# `view create` refused because "mxbase already exists" is state pollution, not
# a gap, and it once read as an ERROR on PG (which then had no way to remove it).
reset_engine(){ # <meta-dir> <store>
    local md=$1 store=$2
    rm -rf "$md"; mkdir -p "$md"
    local V=("$BIN" view --meta-dir "$md")
    [ -n "$store" ] && V=("$BIN" view --meta-dir "$md" --meta-store "$store")
    for v in mxpub mxlens mximp mxdir mxfork mxbase; do
        "${V[@]}" rm --name "$v" >/dev/null 2>&1
        "${V[@]}" rm --name "$v" --base >/dev/null 2>&1
    done
    if [ -n "$store" ]; then
        ( GROVEFS_META_STORE="$store" "$BIN" refs --meta-dir "$md" --meta-store "$store" rebuild ) >/dev/null 2>&1
    fi
}

reset_engine "$W/sqlite" ""
reset_engine "$W/pg"     "$PGSTORE"
reset_engine "$W/fdb"    "$FDBSTORE"

engine sqlite "$W/sqlite" ""
engine pg     "$W/pg"     "$PGSTORE"
engine fdb    "$W/fdb"    "$FDBSTORE"

echo
echo "===== MATRIX (OK = works, REFUSED = by design, ERROR = gap) ====="
printf '%-44s %-9s %-9s %-9s\n' CAPABILITY sqlite pg fdb
for c in $CAPS; do
    row=$(printf '%-44s' "${LABEL[$c]}")
    for e in sqlite pg fdb; do
        v=$(awk -F'|' -v c="$c" '$1==c{print $2}' "$W/$e.res")
        row+=$(printf ' %-9s' "${v:-?}")
    done
    printf '%s\n' "$row"
done
echo
echo "===== details (first line of each non-OK cell) ====="
for e in sqlite pg fdb; do
    echo "--- $e"
    awk -F'|' '$2!="OK"{printf "  %-10s %-8s %s\n", $1, $2, $3}' "$W/$e.res"
done

# The teardown row is only honest if the store really is empty afterwards.
echo
echo "===== post-teardown verification (nothing left anywhere) ====="
# `refs.db` is the ledger, not a tree: it legitimately survives (it is rebuilt
# empty, which is the state the teardown of the last tree leaves it in).
left=$(find "$W/sqlite/meta" -name '*.db' ! -name 'refs.db' 2>/dev/null | wc -l | tr -d ' ')
printf '%-24s %s\n' "sqlite meta/*.db" "$left"
pgs=$(PGPASSWORD=grovepw psql -h 127.0.0.1 -p 5433 -U grove -d grovefs -Atc \
    "select count(*) from information_schema.schemata where schema_name like 'grovefs_v_%'" 2>/dev/null)
pgled=$(PGPASSWORD=grovepw psql -h 127.0.0.1 -p 5433 -U grove -d grovefs -Atc \
    "select count(*) from grovefs_refs.holders" 2>/dev/null)
printf '%-24s %s\n' "pg grovefs_v_* schemas" "${pgs:-?}"
printf '%-24s %s\n' "pg ledger rows" "${pgled:-?}"
fdbs=$("$BIN" view --meta-store "$FDBSTORE" scan 2>&1 | head -1)
printf '%-24s %s\n' "fdb subspaces" "$fdbs"
[ "${left:-1}" = 0 ] && [ "${pgs:-1}" = 0 ] && [ "${pgled:-1}" = 0 ] \
    && echo "$fdbs" | grep -qi "no subspaces" \
    && echo "TEARDOWN CLEAN: every engine removed what it created" \
    || echo "TEARDOWN RESIDUE: see the four counts above"
