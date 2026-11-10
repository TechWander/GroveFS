#!/usr/bin/env bash
# Semantic fixture for pg_drop_views.sh --rebuild-ledger.
#
# The live database has 0 surviving trees, so running the rebuild there only
# exercises the wipe -- it proves nothing about the recount. This spins a
# THROWAWAY PostgreSQL cluster under .g3bench/ (the host role cannot CREATEB, and
# a scratch cluster keeps the fixture off the real store entirely) with TWO fake
# view trees and a ledger seeded wrong on purpose, so the properties that matter
# get tested instead of asserted:
#
#   * DISTINCT within a tree   -- tree A names R1 twice and must count as ONE
#                                 holder, mirroring `SELECT DISTINCT data_ref` in
#                                 rebuild_ledger (src/meta/engine.rs:1227);
#   * one row per sharing view -- R2 is named by both trees, so it must come out
#                                 holders=2;
#   * a row no tree names      -- ORPHAN must be gone after the recount, the leak
#                                 `grovefs refs rebuild` exists to clear and that
#                                 PG has no CLI path for;
#   * NULL and '' data_refs    -- skipped, as SQLite skips NULLs.
#
# usage: bash .bneck/pg_rebuild_fixture.sh      # builds and tears down its own cluster
set -uo pipefail
ROOT=/data/grovefs/grovefs
D=$ROOT/.g3bench/rbcluster
PGBIN=/usr/lib/postgresql/12/bin
PORT=${RBPORT:-5439}
SELF=$ROOT/.bneck/pg_drop_views.sh
uri(){ echo "postgres://grove@127.0.0.1:$PORT/rbtest"; }
QB(){ psql -P pager=off -qtA "$(uri)" -c "$1" 2>&1; }
have_err(){ printf '%s\n' "$1" | grep -qi '^ERROR\|FATAL'; }
fail(){ echo "FIXTURE_RED: $1"; exit 1; }
chk(){ have_err "$1" && fail "$2"; }

[ -d "$D" ] && { echo "removing a stale scratch cluster"; "$PGBIN/pg_ctl" -D "$D" stop -m immediate >/dev/null 2>&1 || true; rm -rf "$D"; }
mkdir -p "$D"
echo "== initdb (scratch cluster, port $PORT) =="
"$PGBIN/initdb" -D "$D" -U grove -A trust --auth=trust >/dev/null 2>&1 || fail initdb
"$PGBIN/pg_ctl" -D "$D" -o "-p $PORT -k $D -c listen_addresses=127.0.0.1" -l "$D/pg.log" start >/dev/null 2>&1 || fail "pg_ctl start"
for i in $(seq 20); do "$PGBIN/pg_isready" -h 127.0.0.1 -p "$PORT" >/dev/null 2>&1 && break; sleep 1; done
psql -P pager=off -qtA "postgres://grove@127.0.0.1:$PORT/postgres" -c "create database rbtest" >/dev/null 2>&1 || fail "create database"
echo "  server: $(QB 'select version()' | cut -d' ' -f1-2)"

hexof(){ printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
SA=grovefs_v_$(hexof pg_rb_a)
SB=grovefs_v_$(hexof pg_rb_b)
echo "  trees: $SA $SB"

chk "$(QB "create schema grovefs_refs;
  create table grovefs_refs.holders (data_ref text primary key, holders bigint not null);
  create schema $SA; create schema $SB;
  create table $SA.node (id bigint, data_ref text);
  create table $SB.node (id bigint, data_ref text);")" "ddl"

seed(){
  chk "$(QB "delete from $SA.node; delete from $SB.node; delete from grovefs_refs.holders;
    insert into $SA.node values (1,'R1'),(2,'R1'),(3,'R2'),(4,null),(5,'');
    insert into $SB.node values (10,'R2'),(11,'R3');
    insert into grovefs_refs.holders values ('R1',3),('R2',9),('R3',2),('ORPHAN',7);")" seed
}
ledger(){ QB "select coalesce(string_agg(data_ref||'='||holders, ' ' order by data_ref),'(empty)') from grovefs_refs.holders"; }

seed
echo "== ledger before: deliberately wrong (every count inflated + an ORPHAN) =="
echo "  $(ledger)"

echo "== 1. dry run: previews the diff, touches nothing =="
PGURI=$(uri) bash "$SELF" --rebuild-ledger
[ "$(ledger)" = "ORPHAN=7 R1=3 R2=9 R3=2" ] || fail "dry run changed the ledger"

echo "== 2. recount keeping both trees (the SQL a 0-tree store never reaches) =="
PGURI=$(uri) bash "$SELF" --yes --rebuild-ledger --no-drop
echo "  ledger after: $(ledger)"
[ "$(ledger)" = "R1=1 R2=2 R3=1" ] || fail "want 'R1=1 R2=2 R3=1' (DISTINCT in a tree / shared count / ORPHAN gone / no '' or NULL)"
[ "$(QB "select count(*) from pg_namespace where nspname like 'grovefs_v_%'")" = "2" ] || fail "--no-drop dropped schemas"
echo "  REBUILD_SEMANTICS_GREEN"

echo "== 3. drop the trees, then recount (the shape the live database is in) =="
PGURI=$(uri) bash "$SELF" --yes --rebuild-ledger pg_rb_a pg_rb_b
echo "  ledger after: $(ledger)"
[ "$(ledger)" = "(empty)" ] || fail "a store with no trees must end with 0 holder rows"
[ "$(QB "select count(*) from pg_namespace where nspname like 'grovefs_v_%'")" = "0" ] || fail "schemas survived"
echo "  REBUILD_EMPTY_GREEN"

echo "== 4. the snapshot each rewrite leaves is the rollback (state before the rewrite) =="
for t in $(QB "select tablename from pg_tables where schemaname='grovefs_refs'
                 and tablename like 'holders_bak_%' order by 1" | grep -E '^holders_bak_'); do
  snap=$(QB "select coalesce(string_agg(data_ref||'='||holders,' ' order by data_ref),'-') from grovefs_refs.$t")
  echo "  grovefs_refs.$t  rows=$(QB "select count(*) from grovefs_refs.$t")  ($snap)"
done

echo "== 5. idempotence: a second rebuild is a no-op =="
PGURI=$(uri) bash "$SELF" --yes --rebuild-ledger --no-drop | grep -E 'rebuilt|rows after'
[ "$(ledger)" = "(empty)" ] || fail "rebuild not idempotent"

echo "== teardown =="
"$PGBIN/pg_ctl" -D "$D" stop -m fast >/dev/null 2>&1 || fail "pg_ctl stop"
[ -f "$D/PG_VERSION" ] || fail "refusing to remove $D: not a cluster we made"
rm -rf "$D"
echo "PG_REBUILD_FIXTURE_GREEN"
