#!/usr/bin/env bash
# Operator teardown for PostgreSQL-mode base views, which GroveFS deliberately
# will not delete for you.
#
# `grovefs view rm --base` refuses a `--meta-store postgres://…` deployment on
# purpose — its own help says the tree "is a PG schema — drop the schema and let
# PG/data servers own the bytes".  Nothing in the CLI does that drop, so any
# benchmark that creates one view per cell (bench_g3.sh, the `pg` suites) leaves
# residue with no supported removal path.  This script IS that path, done in
# full:
#
#   * a view's tree is its schema `grovefs_v_<hex of the view name>` (verified by
#     decoding the schemas already present in this database, not guessed);
#   * the cross-view ledger `grovefs_refs.holders` is GLOBAL and carries no view
#     column, so dropping a schema without first removing that view's holder rows
#     strands one ledger row per object the view booked — the state SQLite-mode
#     `view rm --base` avoids by rebuilding the ledger from surviving trees.
#     Zero-byte files do book an object here: one 3.7k-op bench cell leaves 3,700
#     rows, so this is the normal case rather than a corner of it.
#   * `grovefs_control.view_defs` is NOT touched because it only governs
#     derived/fork/alias definitions; a plain base view has no row there (checked:
#     it was empty while 29 base schemas existed).
#
# What this does NOT do: delete the content bytes, which live on the data-servers
# (`--data-dir`), not in PG.  For the benchmark use that is already handled — the
# scratch data root is removed with the run directory — but do not point this at a
# production view whose objects still matter.
#
# usage:
#   bash .bneck/pg_drop_views.sh --list
#   bash .bneck/pg_drop_views.sh --yes                                # bench residue only
#   bash .bneck/pg_drop_views.sh --yes pg_1794_r1n1 pg_1794_r2n4     # named views
#   PGURI=postgres://u:p@h:5433/db bash .bneck/pg_drop_views.sh --list
#
# Guards: nothing is dropped without `--yes`; names not given explicitly must
# match `--pattern` (default '^pgp?_?[0-9]': pg<ts>, pgp<ts>, pg_<ts>_.. -- exactly what
# the benches and the pg suites mint); the two global schemas are refused always.
# global schemas are refused unconditionally.
set -uo pipefail
PGURI=${PGURI:-postgres://grove:grovepw@127.0.0.1:5433/grovefs}
PATTERN='^pgp?_?[0-9]'
YES=0; LIST=0; NAMES=()
while [ $# -gt 0 ]; do
  case "$1" in
    --list) LIST=1 ;;
    --yes) YES=1 ;;
    --pattern) PATTERN=$2; shift ;;
    --help|-h) sed -n '1,40p' "$0"; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) NAMES+=("$1") ;;
  esac
  shift
done

q(){ psql -P pager=off -qtA "$PGURI" -c "$1" 2>&1; }
hexof(){ printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }

schemas=$(q "select nspname from pg_namespace where nspname like 'grovefs_v_%' order by 1")
if [ "${LIST}" = 1 ] || [ "${YES}" = 0 ]; then
  echo "PGURI=$PGURI"
  echo "base-view schemas: $(printf '%s\n' "$schemas" | grep -c . )"
  printf '%s\n' "$schemas" | python3 -c '
import sys
for l in sys.stdin:
    l=l.strip()
    if not l: continue
    print("  %-46s -> view %r" % (l, bytes.fromhex(l.split("_")[2]).decode()))'
  [ "${YES}" = 1 ] || echo "(dry run: pass --yes to drop; nothing was removed)"
  exit 0
fi

# Resolve the target list: explicit names, else every schema whose view matches.
targets=()
if [ ${#NAMES[@]} -gt 0 ]; then
  for n in "${NAMES[@]}"; do targets+=("grovefs_v_$(hexof "$n")"); done
else
  while read -r s; do
    [ -n "$s" ] || continue
    v=$(printf '%s' "$s" | python3 -c 'import sys;print(bytes.fromhex(sys.stdin.read().strip().split("_")[2]).decode())')
    printf '%s' "$v" | grep -qE "$PATTERN" && targets+=("$s")
  done <<EOF
$schemas
EOF
fi
[ ${#targets[@]} -gt 0 ] || { echo "no view matched (pattern '$PATTERN')"; exit 0; }

before=$(q "select count(*) from grovefs_refs.holders")
bad=0
echo "dropping ${#targets[@]} view schemas (ledger rows before: $before)"
for s in "${targets[@]}"; do
  case "$s" in grovefs_v_*) ;; *) echo "REFUSING $s (not a base-view schema)"; bad=$((bad+1)); continue ;; esac
  if [ -z "$(q "select 1 from pg_namespace where nspname='$s'")" ]; then
    echo "  absent, nothing to drop: $s"; continue
  fi
  view=$(printf '%s' "$s" | python3 -c 'import sys;print(bytes.fromhex(sys.stdin.read().strip().split("_")[2]).decode())')
  led=$(q "select count(*) from grovefs_refs.holders h where h.data_ref in (select nullif(data_ref,'') from \"$s\".node)")
  err=$(q "begin;
    delete from grovefs_refs.holders h
      where h.data_ref in (select nullif(data_ref,'') from \"$s\".node);
    drop schema if exists \"$s\" cascade; commit;" | grep -i '^ERROR' || true)
  if [ -n "$err" ]; then echo "  FAILED $view :: $err"; bad=$((bad+1));
  else echo "  dropped $view  (ledger rows removed: $led)"; fi
done
after=$(q "select count(*) from grovefs_refs.holders")
echo "ledger rows after: $after  (reclaimed $((before-after)))"
echo "remaining base-view schemas: $(q "select count(*) from pg_namespace where nspname like 'grovefs_v_%'")"
echo "db size now: $(q "select pg_size_pretty(pg_database_size(current_database()))")"
[ "$bad" = 0 ] || { echo "PG_GC_RED ($bad view(s) not dropped)"; exit 1; }
echo "PG_GC_GREEN"
echo "note: content bytes live on the data-server (--data-dir), not in PG; this"
echo "      script does not touch them. Reclaim them where they are stored."
