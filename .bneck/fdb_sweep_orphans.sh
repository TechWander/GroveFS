#!/usr/bin/env bash
# Sweep ORPHAN trees out of a FoundationDB metadata cluster — the cleanup half of
# the "only build, never tear down" gap that left 169 subspaces in the local
# cluster before `view rm --base` existed for fdb:// (docs/fdb-keyspace.md §12.5).
#
# What an orphan is here: a subspace under the `grovefs` root with no LIVE owner.
# Under fdb:// the control plane (definitions: derived/alias/fork) is LOCAL files
# while the trees are IN the cluster (C6), so a cluster can hold trees whose
# meta-dir no longer exists — every suite run and every benchmark cell that
# provisioned a view and then deleted its scratch home leaves exactly one. They
# are not harmless: each one keeps its content keys booked in the SHARED ledger
# `grovefs/refs`, so the bytes behind them can never be reclaimed, and FDB-mode
# has no `refs rebuild` (it is SQLite-only) to repair a ledger that drifted.
#
# The removal path is the product's own `view rm --base`, which unwinds the
# ledger FIRST and deletes the subspace SECOND, in that order, and deletes each
# view's `'r` rows in the same transaction as the decrement — so an interrupted
# sweep is re-runnable and never double-decrements. Using `fdbcli clearrange`
# instead (the only option before this command existed) leaves one ledger row per
# object, permanently over-counted.
#
#   bash .bneck/fdb_sweep_orphans.sh                 # dry run: what would go
#   bash .bneck/fdb_sweep_orphans.sh --yes           # do it
#   bash .bneck/fdb_sweep_orphans.sh --yes --keep keepme --keep other
#
# Safety: `refs` (the ledger) is NEVER a candidate. `--meta-dir` defaults to a
# fresh empty directory, which is what makes "no definition governs this tree"
# true for debris whose control plane is gone; pass the REAL meta-dir when it
# still exists, and the dependency guard will refuse to remove a base that a
# surviving definition resolves through. Bytes are only attempted when you pass
# `--data-dir`: the metadata URI cannot carry a data root, so with the default
# (a temp dir) every object reports as missing and the blobs stay where the
# data-servers keep them.
set -u
cd "$(dirname "$0")/.."

CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-/data/grovefs/.fdbtest/test.cluster}"
Y="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
APPLY=0
KEEP=" refs "
METADIR=""; DATADIR=""
die(){ echo "fdb_sweep_orphans: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --yes) APPLY=1 ;;
        --keep) shift; [ $# -gt 0 ] || die "--keep needs a name"; KEEP="$KEEP$1 " ;;
        --meta-dir) shift; [ $# -gt 0 ] || die "--meta-dir needs a path"; METADIR="$1" ;;
        --data-dir) shift; [ $# -gt 0 ] || die "--data-dir needs a path"; DATADIR="$1" ;;
        --cluster) shift; [ $# -gt 0 ] || die "--cluster needs a file"; CLUSTER="$1" ;;
        -h|--help) sed -n 2,30p "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument '$1' (try --help)" ;;
    esac
    shift
done

[ -f "$CLUSTER" ] || die "no cluster file at $CLUSTER (see .build-bin/fdb_env.sh)"
[ -x "$Y" ] || die "no feature binary at $Y — build one: FDB_CLIENT_LIB_PATH=\$HOME/../.fdbbin/lib cargo build --release --features fdb --target-dir target/fdb"
URI="fdb://$CLUSTER"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/fdb_sweep.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
METADIR="${METADIR:-$TMP/ctrl}"; DATADIR="${DATADIR:-$TMP/data}"
mkdir -p "$METADIR" "$DATADIR"

# `$4` is the KIND column: "view" for a live tree, "corpse" for one that kept its
# keys but lost its root row (unmountable, and the case most in need of removal),
# "ledger" for the shared `refs` subspace. The summary and the trailing note line
# do not have any of those, so they fall out of the same awk that produces the
# candidate list.
scan(){ "$Y" view --meta-store "$URI" --meta-dir "$METADIR" scan 2>&1; }
names(){ awk 'NR>1 && ($4=="view" || $4=="corpse"){print $1}' | sort; }
ledger_keys(){ awk '$4=="ledger"{print $2}'; }

OUT="$(scan)" || die "view scan failed: $(echo "$OUT" | head -3)"
BEFORE="$(echo "$OUT" | names)"
LK_BEFORE="$(echo "$OUT" | ledger_keys)"
[ -n "$LK_BEFORE" ] || LK_BEFORE=0
TOTAL=$(echo "$BEFORE" | grep -c . || true)

CAND=""
SKIPPED=0
while read -r v; do
    [ -n "$v" ] || continue
    case "$KEEP" in *" $v "*) SKIPPED=$((SKIPPED+1)); continue ;; esac
    CAND="$CAND$v
"
done <<< "$BEFORE"
NCAND=$(printf '%s' "$CAND" | grep -c . || true)

echo "cluster    : $CLUSTER"
echo "trees      : $TOTAL  (candidates $NCAND, kept $SKIPPED)"
echo "refs ledger: $LK_BEFORE key(s)  — each candidate holds rows here until it is removed"
if [ "$NCAND" = 0 ]; then echo "nothing to sweep."; exit 0; fi

if [ "$APPLY" = 0 ]; then
    printf '%s' "$CAND" | sed 's/^/  would remove: /'
    echo "(dry run — re-run with --yes)"
    exit 0
fi

echo "-- removing $NCAND tree(s) with the product's own path"
GONE=0; FAILED=0; FREED=0
while read -r v; do
    [ -n "$v" ] || continue
    if R="$("$Y" view --meta-store "$URI" --meta-dir "$METADIR" rm --name "$v" --base \
            --data-dir "$DATADIR" 2>&1)"; then
        GONE=$((GONE+1))
        # Only a NON-ZERO release means bytes were freed; the report always prints
        # the phrase, so match the number in front of it.
        n=$(echo "$R" | sed -n 's/.*, \([0-9][0-9]*\) object(s) no longer held by any view.*/\1/p' | head -1)
        [ -n "$n" ] && FREED=$((FREED+n))
    else
        FAILED=$((FAILED+1))
        echo "  FAILED $v: $(echo "$R" | head -2)"
    fi
done <<< "$CAND"

OUT2="$(scan)" || die "post-sweep scan failed: $(echo "$OUT2" | head -3)"
AFTER="$(echo "$OUT2" | names)"
LK_AFTER="$(echo "$OUT2" | ledger_keys)"
[ -n "$LK_AFTER" ] || LK_AFTER=0
LEFT=""
while read -r v; do
    [ -n "$v" ] || continue
    case "$KEEP" in *" $v "*) continue ;; esac
    LEFT="$LEFT$v
"
done <<< "$AFTER"

echo "removed    : $GONE  (failed $FAILED)"
echo "refs ledger: $LK_BEFORE -> $LK_AFTER key(s); $FREED object(s) unbooked"
echo "left       : $(printf '%s' "$LEFT" | grep -c . || true) unreserved tree(s)"
RC=0
[ "$FAILED" = 0 ] || RC=1
[ -z "$(printf '%s' "$LEFT" | grep .)" ] || { echo "residual: $(printf '%s' "$LEFT" | tr '\n' ' ')"; RC=1; }
[ "$LK_AFTER" = 0 ] || [ -n "$(printf '%s' "$AFTER" | grep .)" ] || {
    echo "ledger not empty but no tree is left — rows with no owner: $LK_AFTER"; RC=1; }
exit $RC
