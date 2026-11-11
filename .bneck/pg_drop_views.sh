#!/usr/bin/env bash
# Operator teardown for PostgreSQL-mode base views.
#
# STATUS: the CLI now does this natively -- `grovefs view rm --base
# --meta-store postgres://…` unbooks the tree's holds and drops its
# `grovefs_v_<hex>` schema in ONE transaction, and `grovefs refs rebuild
# --meta-store postgres://…` recomputes `grovefs_refs.holders` from the
# surviving trees. `.bneck/pg_view_teardown.sh` is the end-to-end suite for
# both. This script remains as the OUT-OF-BAND inspector, for the cases the CLI
# deliberately will not touch:
#
#   * bulk cleanup after a benchmark run (`--pattern '^pg[0-9]+$'`);
#   * a ledger recount with a printed PREVIEW and a snapshot table
#     (`--rebuild-ledger` writes `grovefs_refs.holders_bak_<ts>` first);
#   * answering "which schemas exist, what do they decode to, and how many
#     ledger rows does each book" without opening a view.
#
# What the CLI and this script agree on:
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
# --rebuild-ledger closes the other half of the same gap: `grovefs refs rebuild`
# is SQLite-only (its help says so), and the PG ledger has no rebuild path in the
# CLI at all (USAGE §15.9).  This recomputes it the way `rebuild_ledger()` does
# (src/meta/engine.rs:1205-1238): in ONE transaction, delete every holder row,
# then for each surviving tree insert its `SELECT DISTINCT data_ref FROM node
# WHERE data_ref IS NOT NULL` and add one per DISTINCT view.  Two properties come
# from that source and are the reason the rewrite is safe:
#
#   * it keeps NO row that no tree names — a row for an out-of-band-deleted view
#     is a leak by definition, and `holders` is never read except at unlink time
#     (`ledger_add_tx`/dec at pg_store.rs:1466/1484/1493), where an over-count
#     only means a blob is kept too long;  `gc` refuses postgres:// stores
#     outright (main.rs:2709), so nothing consumes these rows offline;
#   * a key with NO row is private to one view, so dropping orphan rows does not
#     make anything look shared that is not.
#
# The rewrite is a full wipe, so it must never run while a tree it cannot see is
# alive; it recounts exactly the `grovefs_v_*` schemas present and is a no-op on
# rows those trees still book.  It takes a row-count snapshot first.
#
# usage:
#   bash .bneck/pg_drop_views.sh --list
#   bash .bneck/pg_drop_views.sh --yes                                # bench residue only
#   bash .bneck/pg_drop_views.sh --yes pg_1794_r1n1 pg_1794_r2n4     # named views
#   bash .bneck/pg_drop_views.sh --yes --rebuild-ledger               # drop bench views, then recount
#   bash .bneck/pg_drop_views.sh --yes --rebuild-ledger --no-drop     # recount only, drop nothing
#   PGURI=postgres://u:p@h:5433/db bash .bneck/pg_drop_views.sh --list
#
# Guards: nothing is dropped or rewritten without `--yes`; names not given
# explicitly must match `--pattern` (default '^pgp?_?[0-9]': pg<ts>, pgp<ts>,
# pg_<ts>_r?n? -- exactly what the benches and the pg suites mint); anything not
# named `grovefs_v_*` is refused, and the two global schemas
# (`grovefs_control`, `grovefs_refs`) are refused unconditionally.
# `--rebuild-ledger` implies the full-ledger rewrite described above, so it also
# requires `--yes`; `--no-drop` only makes sense next to it.
set -uo pipefail
PGURI=${PGURI:-postgres://grove:grovepw@127.0.0.1:5433/grovefs}
PATTERN='^pgp?_?[0-9]'
YES=0; LIST=0; REBUILD=0; NODROP=0; NAMES=()
while [ $# -gt 0 ]; do
  case "$1" in
    --list) LIST=1 ;;
    --yes) YES=1 ;;
    --rebuild-ledger) REBUILD=1 ;;
    --no-drop) NODROP=1 ;;
    --pattern) PATTERN=$2; shift ;;
    --help|-h) awk '/^set -uo/{exit} {print}' "$0"; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) NAMES+=("$1") ;;
  esac
  shift
done
if [ "$NODROP" = 1 ] && [ "$REBUILD" = 0 ]; then
  echo "--no-drop is only meaningful with --rebuild-ledger (recount, drop nothing)" >&2
  exit 2
fi

q(){ psql -P pager=off -qtA "$PGURI" -c "$1" 2>&1; }
# Value query. stderr must NOT ride along here: the answers feed `$(( ))`,
# `grep -c` and a schema-exists test, so error text arriving as a VALUE turns one
# unreachable store into a dozen misleading failures (`psql: error: ...` inside an
# arithmetic expansion reads as an unbound variable, and a non-empty error string
# makes `select 1 from pg_namespace` look like the schema exists).  The merged-
# stderr form `q` stays for statements whose ERROR text we deliberately grep.
qv(){ psql -P pager=off -qtA "$PGURI" -c "$1" 2>/dev/null; }
hexof(){ printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }

if ! probe=$(psql -P pager=off -qtA "$PGURI" -c "select 1" 2>&1 >/dev/null); then
  echo "PG_GC_RED: cannot reach $PGURI: $probe" >&2
  exit 1
fi

# The ref set of the trees that still exist: one (view, data_ref) pair per object
# a tree still names.  This is the row set `rebuild_ledger` re-inserts, term for
# term -- DISTINCT within a tree (two names, one object), NULL data_refs skipped
# (PG stores them as NULL: `data_ref TEXT` at pg_store.rs:403, Option<String> at
# :89), and the <> '' test is belt-and-braces against an empty key ever landing.
refunion() {
  local u="" s sel
  for s in $(printf '%s\n' "$schemas" | grep .); do
    sel="select distinct '$s' as v, data_ref from \"$s\".node
           where data_ref is not null and data_ref <> ''"
    if [ -z "$u" ]; then u=$sel; else u="$u
union all
$sel"; fi
  done
  printf '%s' "$u"
}

schemas=$(qv "select nspname from pg_namespace where nspname like 'grovefs_v_%' order by 1")
if [ "${LIST}" = 1 ] || [ "${YES}" = 0 ]; then
  echo "PGURI=$PGURI"
  echo "base-view schemas: $(printf '%s\n' "$schemas" | grep -c . )"
  printf '%s\n' "$schemas" | python3 -c '
import sys
for l in sys.stdin:
    l=l.strip()
    if not l: continue
    print("  %-46s -> view %r" % (l, bytes.fromhex(l.split("_")[2]).decode()))'
  [ "${YES}" = 1 ] || echo "(dry run: pass --yes to change anything; nothing was dropped or rewritten)"
  if [ "$REBUILD" = 1 ]; then
    u=$(refunion)
    if [ -z "$u" ]; then
      echo "rebuild preview: 0 surviving trees, so the recount would leave 0 holder rows"
    else
      now=$(qv "select count(*) from grovefs_refs.holders")
      # The outer alias is not style: PG rejects an unaliased subquery in FROM
      # ("subquery in FROM must have an alias"), which is how this line first failed.
      keep=$(qv "select count(*) from (select data_ref from ($u) t group by data_ref) c")
      echo "rebuild preview: $(printf '%s\n' "$schemas" | grep -c .) surviving trees name $keep distinct"
      echo "                 objects; the ledger now holds $now rows, so $(( now - keep )) are"
      echo "                 booked by no tree and would be dropped by the recount"
    fi
  fi
  exit 0
fi

# Resolve the target list: explicit names, else every schema whose view matches.
# --no-drop skips the resolution entirely (recount only).
targets=()
if [ "$NODROP" = 1 ]; then
  :
elif [ ${#NAMES[@]} -gt 0 ]; then
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
if [ ${#targets[@]} -eq 0 ] && [ "$REBUILD" = 0 ]; then
  echo "no view matched (pattern '$PATTERN')"; exit 0
fi

before=$(qv "select count(*) from grovefs_refs.holders")
bad=0
echo "dropping ${#targets[@]} view schemas (ledger rows before: $before)"
for s in "${targets[@]}"; do
  case "$s" in grovefs_v_*) ;; *) echo "REFUSING $s (not a base-view schema)"; bad=$((bad+1)); continue ;; esac
  if [ -z "$(qv "select 1 from pg_namespace where nspname='$s'")" ]; then
    echo "  absent, nothing to drop: $s"; continue
  fi
  view=$(printf '%s' "$s" | python3 -c 'import sys;print(bytes.fromhex(sys.stdin.read().strip().split("_")[2]).decode())')
  led=$(qv "select count(*) from grovefs_refs.holders h where h.data_ref in (select nullif(data_ref,'') from \"$s\".node)")
  err=$(q "begin;
    delete from grovefs_refs.holders h
      where h.data_ref in (select nullif(data_ref,'') from \"$s\".node);
    drop schema if exists \"$s\" cascade; commit;" | grep -i '^ERROR' || true)
  if [ -n "$err" ]; then echo "  FAILED $view :: $err"; bad=$((bad+1));
  else echo "  dropped $view  (ledger rows removed: $led)"; fi
done

# The rebuild step, mirroring rebuild_ledger(): one transaction, wipe the ledger,
# re-insert one row per DISTINCT data_ref with holders = number of surviving
# trees naming it.  The tree list is re-read AFTER the drops above, so a view
# dropped in this run cannot prop its own rows back up.  The whole table is
# snapshotted first: the rewrite is a wipe, and a snapshot is the only rollback.
if [ "$REBUILD" = 1 ]; then
  schemas=$(qv "select nspname from pg_namespace where nspname like 'grovefs_v_%' order by 1")
  ntree=$(printf '%s\n' "$schemas" | grep -c .)
  u=$(refunion)
  # Underscores only: an unquoted PG identifier may not contain '-', so the dash
  # form of the timestamp made the whole rewrite die on a syntax error.
  bak="holders_bak_$(date +%Y%m%d_%H%M%S)"
  case "$bak" in *[!A-Za-z0-9_]*) echo "PG_GC_RED: bad snapshot name $bak" >&2; exit 1 ;; esac
  live=$(qv "select count(*) from pg_stat_activity where usename = current_user
              and pid <> pg_backend_pid() and state <> 'idle'")
  sql="begin;
    create table grovefs_refs.$bak as select * from grovefs_refs.holders;
    delete from grovefs_refs.holders;"
  if [ -n "$u" ]; then
    sql="$sql
    insert into grovefs_refs.holders (data_ref, holders)
      select data_ref, count(*) from ($u) t group by data_ref;"
  fi
  sql="$sql
    commit;"
  err=$(q "$sql" | grep -i '^ERROR' || true)
  if [ -n "$err" ]; then
    echo "  FAILED rebuild-ledger :: $err"; bad=$((bad+1))
  else
    echo "  ledger rebuilt from $ntree surviving tree(s)"
    echo "    snapshot: grovefs_refs.$bak  (rollback: insert into grovefs_refs.holders"
    echo "    select * from grovefs_refs.$bak; then drop that table)"
    [ "$live" = 0 ] || {
      echo "    note: $live other active connection(s) on this store during the rewrite. A"
      echo "          concurrent writer can only be over-counted by it (its rows land after"
      echo "          the wipe, or are re-added from the tree it names), and an over-count"
      echo "          keeps bytes rather than freeing one still named -- the safe direction."
    }
  fi
fi

after=$(qv "select count(*) from grovefs_refs.holders")
echo "ledger rows after: $after  (reclaimed $((before-after)))"
echo "remaining base-view schemas: $(qv "select count(*) from pg_namespace where nspname like 'grovefs_v_%'")"
echo "db size now: $(qv "select pg_size_pretty(pg_database_size(current_database()))")"
[ "$bad" = 0 ] || { echo "PG_GC_RED ($bad step(s) failed)"; exit 1; }
echo "PG_GC_GREEN"
echo "note: content bytes live on the data-server (--data-dir), not in PG; this"
echo "      script does not touch them. Reclaim them where they are stored."
