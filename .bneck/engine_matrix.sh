#!/usr/bin/env bash
# Engine completeness matrix: run the SAME governance sequence against the three
# metadata backends and print one table.
#
# What this measures is the GOVERNANCE plane (view trees, definitions, the
# ledger, the cluster's inventory/lease rows, the offline tools, and the
# TEARDOWN of a base tree). The DATA plane is covered per engine by the suites:
# `run_all.sh` (SQLite, 35 suites), `test_pg.sh` + `test_pg_parallel.sh` +
# `test_control.sh` (PG over real FUSE mounts, now self-cleaning through
# `view rm --base`), `test_fdb.sh` (275 assertions, re-run on the four-zone
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
# Every arm names its store EXPLICITLY (`--meta-store` per command, and
# `GROVEFS_META_STORE` only for `gc`, which has no such flag). An inherited
# GROVEFS_META_STORE from the operator's shell would silently make the SQLITE arm
# think it serves an fdb:// deployment — every local verb would then refuse with
# "under a FoundationDB deployment" and the matrix would report three columns of
# the same engine. Drop it here; the per-arm assignment below puts it back.
unset GROVEFS_META_STORE
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
# One graft line: take `sub` and its whole subtree from a seeded tree and place
# it in the fork's root under the same basename.
printf 'sub\n' > "$W/graft.map"
# The seed is a hand-written export file, because the matrix runs NO FUSE and
# `grovefs import` (the usual way to fill a tree) is SQLite-only by design: a
# PG/FDB arm would otherwise have no populated tree to graft FROM. `data_ref` is
# null on purpose — this row measures the tree copy, not the blob ledger (that
# path is `forksrv`/`xfer`, which run real mounts).
cat > "$W/seed.dump" <<'EOF'
{"blobs": 0, "definition": null, "exported_at": 0, "format": "grovefs-view-metadata", "kind": "physical", "nodes": 3, "version": 1, "view": "mxseed"}
{"atime": 0, "ctime": 0, "data_ref": null, "gid": 0, "id": 1, "ino": 1, "is_dir": 1, "link_target": null, "mode": 493, "mtime": 0, "name": "/", "parent_id": null, "uid": 0}
{"atime": 0, "ctime": 0, "data_ref": null, "gid": 0, "id": 2, "ino": 2, "is_dir": 1, "link_target": null, "mode": 493, "mtime": 0, "name": "sub", "parent_id": 1, "uid": 0}
{"atime": 0, "ctime": 0, "data_ref": null, "gid": 0, "id": 3, "ino": 3, "is_dir": 0, "link_target": null, "mode": 420, "mtime": 0, "name": "g.txt", "parent_id": 2, "uid": 0}
EOF

CAPS="create define alias fork list export import importfork seedimport dirimport graft refs gc gcremote du dumiss apikey audit scan lease ha rmbase"
declare -A LABEL=(
  [create]="view create   (provision a base tree)"
  [define]="view define   (derived lens)"
  [alias]="view alias    (pure alias)"
  [fork]="view fork     (independent tree)"
  [list]="view list     (definitions)"
  [export]="view export   (metadata dump)"
  [import]="view import   (dump -> tree)"
  [importfork]="view import --as-fork (dump -> fork)"
  [seedimport]="view import (hand-written seed)"
  [dirimport]="grovefs import (directory -> view)"
  [graft]="view graft    (subtree into a fork)"
  [refs]="refs rebuild  (cross-view ledger)"
  [gc]="gc --data-dir (local page audit)"
  [gcremote]="gc --data-servers (remote audit)"
  [du]="du             (byte totals)"
  [dumiss]="du --view missing (no-tree guard)"
  [apikey]="apikey list    (credential registry)"
  [audit]="apikey calls   (call audit log)"
  [scan]="view scan     (cluster inventory)"
  [lease]="view lease    (writer lease rows)"
  [ha]="meta-server --role standby (native HA)"
  [rmbase]="view rm --base (tear the tree down)"
)

classify(){ # rc log -> OK | REFUSED | ERROR
    local rc=$1 log=$2
    [ "$rc" -eq 0 ] && { echo OK; return; }
    if grep -qiE 'refus|not supported|not implemented|unsupported|SQLite-only|only (works|applies|exists)|incompatible|operates on a local|exists only for|is not an fdb|has no|cannot be combined|rewrites a local|audits LOCAL|reclaim there|no metadata directory|no tree for view' "$log"; then
        echo REFUSED
    else
        echo ERROR
    fi
}
detail(){ grep -viE 'setlocale' "$1" 2>/dev/null | grep -v '^[[:space:]]*$' | head -1 | sed 's/[[:space:]]\+/ /g' | cut -c1-96; }
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

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
    ENG=$1; local md=$2 store=$3 P0=${4:-15360}
    : > "$W/$ENG.res"
    mkdir -p "$W/$ENG/op"
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
        # The SAME dump as a fork: the tree is built and `fork <base>` is recorded,
        # so the result keeps fork semantics (structure writable, content EROFS).
        # A separate row because it is a separate promise — and because under FDB
        # it is the only import that leaves a definition behind.
        run_cap importfork "${V[@]}" import --view mximpf --in "$W/$ENG.dump" --as-fork mxbase
    else
        record import SKIPPED "no dump to import (export was refused)"
        printf '  %-10s %-8s %s\n' import SKIPPED "no dump to import (export was refused)"
    fi
    run_cap dirimport "$BIN" import --meta-dir "$md" --data-dir "$md" --view mxdir --source "$W/src" \
        ${store:+--meta-store "$store"}
    # A populated tree that exists on ALL THREE engines (see seed.dump above):
    # the source for graft, and the tree `du` measures further down.
    run_cap seedimport "${V[@]}" import --view mxseed --in "$W/seed.dump"
    # graft: a CURATED SUBSET of one tree placed into another, which is the
    # operation namespace-poor backends cannot fake with a fork. `mxfork` is the
    # fork created above (an empty tree), `mxseed` the seeded one; the plan
    # brings `sub` and its whole subtree across.
    run_cap graft   "${V[@]}" graft --name mxfork --src mxseed --map "$W/graft.map"
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
    # The REMOTE form, which is the only one a PG/FDB deployment has: the server
    # that owns the store reads its own trees and lists bytes from a
    # data-server, so the operator host needs neither <meta-dir> nor <data-dir>.
    # Measured on the same (empty) tree as the rows above — which is also the
    # case the wrong-root guard must NOT refuse, since nothing is shared and
    # nothing is claimed.
    local DP=$P0 MP=$((P0 + 1)) CP=$((P0 + 2))
    "$BIN" data-server --listen 127.0.0.1:$DP --data-dir "$md" > "$W/$ENG.gcdata.log" 2>&1 &
    local dpid=$!
    "$BIN" meta-server --meta-dir "$md" --listen 127.0.0.1:$MP --control-listen 127.0.0.1:$CP \
        ${store:+--meta-store "$store"} > "$W/$ENG.gcmeta.log" 2>&1 &
    local mpid=$!
    wait_port 127.0.0.1:$DP && wait_port 127.0.0.1:$CP || true
    local kargs=(--meta-dir "$md"); [ -n "$store" ] && kargs=(--meta-store "$store")
    local key
    key=$("$BIN" apikey "${kargs[@]}" create --label mx-admin --for admin --owner alice 2>/dev/null \
        | awk '/^  key: /{print $2}')
    "$BIN" gc --home "$W/$ENG/op" --control-server 127.0.0.1:$CP --api-key "$key" \
        --data-servers 127.0.0.1:$DP --older-than 1h > "$W/$ENG.gcremote.log" 2>&1
    local rrc=$?
    local rv; rv=$(classify "$rrc" "$W/$ENG.gcremote.log")
    record gcremote "$rv" "$(detail "$W/$ENG.gcremote.log")"
    printf '  %-10s %-8s %s\n' gcremote "$rv" "$(detail "$W/$ENG.gcremote.log")"
    # du: the tree comes from the backend, the BYTES are asked of a data-server
    # (a striped store refuses a server list of the wrong length, so the same
    # address that served `gc` above is the one that measures the tree here).
    local dargs=(--meta-dir "$md" --view mxseed --data-servers 127.0.0.1:$DP --total)
    [ -n "$store" ] && dargs+=(--meta-store "$store")
    run_cap du      "$BIN" du "${dargs[@]}"
    # And the same command on a name that has no tree: all three engines must say
    # so in the same words, instead of "0 B" (FDB) or "pg query: db error" (PG).
    local margs=(--meta-dir "$md" --view mxnone --data-servers 127.0.0.1:$DP --total)
    [ -n "$store" ] && margs+=(--meta-store "$store")
    run_cap dumiss  "$BIN" du "${margs[@]}"
    # Credentials and their audit trail are a CONTROL-plane store of their own:
    # a local `api_keys.db` on SQLite, `grovefs_control` on PG, the cluster's
    # `ctl` subspace on FDB — same two commands, three landing places.
    run_cap apikey  "$BIN" apikey "${kargs[@]}" list
    run_cap audit   "$BIN" apikey "${kargs[@]}" calls --limit 5
    kill "$dpid" "$mpid" 2>/dev/null
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
    for v in mxpub mxlens mxfork mximpf mxseed; do
        "${V[@]}" rm --name "$v" >/dev/null 2>&1
    done
    "${V[@]}" rm --name mximpf --base >/dev/null 2>&1
    "${V[@]}" rm --name mxseed --base >/dev/null 2>&1
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
    for v in mxpub mxlens mximp mxdir mxfork mxbase mximpf mxseed; do
        "${V[@]}" rm --name "$v" >/dev/null 2>&1
    done
    # A second pass for the TREES: the first pass only removed definitions, and a
    # definition over `<base>` refuses the base's teardown — one pass in list
    # order leaves any base whose fork was listed after it.
    for v in mxpub mxlens mximp mxdir mxfork mxbase mximpf mxseed; do
        "${V[@]}" rm --name "$v" --base >/dev/null 2>&1
    done
    if [ -n "$store" ]; then
        ( GROVEFS_META_STORE="$store" "$BIN" refs --meta-dir "$md" --meta-store "$store" rebuild ) >/dev/null 2>&1
    fi
}

reset_engine "$W/sqlite" ""
reset_engine "$W/pg"     "$PGSTORE"
reset_engine "$W/fdb"    "$FDBSTORE"

# The PG cluster is SHARED with the other suites (and with whatever an earlier
# killed run left behind), so cleanliness has to be measured as a DELTA —
# "nothing NEW was left" — instead of a global zero that fails on THEIR residue.
PGQ(){ PGPASSWORD=grovepw psql -h 127.0.0.1 -p 5433 -U grove -d grovefs -Atc "$1" 2>/dev/null; }
pgs0=$(PGQ "select count(*) from information_schema.schemata where schema_name like 'grovefs_v_%'")
pgled0=$(PGQ "select count(*) from grovefs_refs.holders")

engine sqlite "$W/sqlite" ""        15360
engine pg     "$W/pg"     "$PGSTORE" 15370
engine fdb    "$W/fdb"    "$FDBSTORE" 15380

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
echo "===== post-teardown verification (nothing NEW left anywhere) ====="
# `refs.db` is the ledger and `api_keys.db` is the credential registry: neither is
# a TREE, so both legitimately survive (the ledger is rebuilt empty, which is the
# state the teardown of the last tree leaves it in; the key was minted by the
# gcremote row to log in at all).
left=$(find "$W/sqlite/meta" -name '*.db' ! -name 'refs.db' ! -name 'api_keys.db' 2>/dev/null | wc -l | tr -d ' ')
printf '%-24s %s\n' "sqlite meta/*.db" "$left"
pgs=$(PGQ "select count(*) from information_schema.schemata where schema_name like 'grovefs_v_%'")
pgled=$(PGQ "select count(*) from grovefs_refs.holders")
printf '%-24s %s\n' "pg grovefs_v_* schemas" "$pgs (baseline ${pgs0:-?})"
printf '%-24s %s\n' "pg ledger rows" "$pgled (baseline ${pgled0:-?})"
fdbs=$("$BIN" view --meta-store "$FDBSTORE" scan 2>&1 | grep -m1 'subspace(s) under')
fdbtr=$(printf '%s' "$fdbs" | sed -n 's/.*: \([0-9]\+\) tree(s).*/\1/p')
printf '%-24s %s\n' "fdb tree subspaces" "${fdbtr:-?} (control/lease subspaces are not trees)"
[ "${left:-1}" = 0 ] && [ "${pgs:-999}" -le "${pgs0:-0}" ] && [ "${pgled:-999}" -le "${pgled0:-0}" ] \
    && [ "${fdbtr:-1}" = 0 ] \
    && echo "TEARDOWN CLEAN: every engine removed what it created" \
    || echo "TEARDOWN RESIDUE: see the four counts above"
