#!/usr/bin/env bash
# PostgreSQL base-view TEARDOWN and LEDGER REPAIR, end to end over a real FUSE
# mount. Until this change the PG engine was the one backend a view could not be
# removed from: `view rm --base` refused it outright, so every `test_pg.sh` run
# left a `grovefs_v_<hex>` schema behind (13 of them, with 288 stuck
# `grovefs_refs.holders` rows, when this was first measured) and `refs rebuild`
# had no PG path at all — the shell script `.bneck/pg_drop_views.sh` existed
# precisely to do by hand what this suite now checks the CLI does.
#
# What is asserted, in order:
#   A. a PG view created, mounted, written to (real FUSE, real data-server);
#   B. its tree is a schema, its holds are ledger rows, its bytes are blobs;
#   C. `view rm --base` drops the schema AND unwinds the ledger ATOMICALLY,
#      then frees the blobs it reached zero on;
#   D. a second removal is refused (a typo must never read as success);
#   E. a schema dropped OUT OF BAND strands ledger rows, and `refs rebuild`
#      deletes them (the over-count direction);
#   F. a LOST booking is restored by `refs rebuild` (the unsafe direction);
#   G. the guard that a governed name needs the plain `view rm` first still
#      holds on PG, and the tree cannot be removed from under its definition;
#   H. nothing is left behind: 0 schemas, 0 ledger rows.
#
# Requires: PostgreSQL on 127.0.0.1 with a `grove:grovepw` role and a `grovefs`
# database, `psql` on the host, FUSE (`fusermount`), and a binary with the
# `native` feature. Self-skips (exit 0, no FAILs) when PG is unreachable, like
# `test_pg.sh`, so a host without PG does not paint the board red.
#
# usage: bash .bneck/pg_view_teardown.sh
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
Y=${Y:-./target/fdb/release/grovefs}
[ -x "$Y" ] || Y=./target/release/grovefs

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5433}
PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"
export PGPASSWORD=grovepw
q(){ psql -h "$PGHOST" -p "$PGPORT" -U grove -d grovefs -Atc "$1" 2>/dev/null; }
schemas(){ q "select count(*) from information_schema.schemata where schema_name like 'grovefs_v_%'"; }
ledger(){ q "select count(*) from grovefs_refs.holders"; }

if ! (exec 3<>/dev/tcp/${PGHOST}/${PGPORT}) 2>/dev/null; then
    echo "SUMMARY: pass=0 fail=0 SKIP=1 (no PostgreSQL on ${PGHOST}:${PGPORT})"
    exit 0
fi
exec 3<&- 2>/dev/null; exec 3<&- 2>/dev/null || true
command -v psql >/dev/null || { echo "SKIP=1 (no psql on the host: the ledger cannot be inspected)"; exit 0; }

META=127.0.0.1:15296
DATA=127.0.0.1:15297
W=.bneck/.pgtd
rm -rf "$W"; mkdir -p "$W/home" "$W/droot" "$W/m1" "$W/mnt"
printf 'hide /nothing-here\n' > "$W/rules.layout"
V1="pgtd$(date +%s)"
V2="${V1}b"
V3="${V1}c"
hexof(){ printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
S1="grovefs_v_$(hexof "$V1")"
S2="grovefs_v_$(hexof "$V2")"
S3="grovefs_v_$(hexof "$V3")"
blobs(){ find "$W/droot/blocks" -type f 2>/dev/null | wc -l | tr -d ' '; }

pass=0; fail=0; skip=0
ok(){ echo "  OK  : $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3 ($1)"; else bad "$3 (got '$1' want '$2')"; fi; }
gt0(){ [ "${1:-0}" -gt 0 ] 2>/dev/null && ok "$2 ($1)" || bad "$2 (got '$1', want > 0)"; }
T(){ timeout 8 "$@"; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

DP=0; MP=0; RP=0
cleanup(){
    mountpoint -q "$W/mnt" && fusermount -uz "$W/mnt" 2>/dev/null
    [ "$RP" != 0 ] && kill "$RP" 2>/dev/null
    [ "$MP" != 0 ] && kill "$MP" 2>/dev/null
    [ "$DP" != 0 ] && kill "$DP" 2>/dev/null
    true
}
trap cleanup EXIT

# serve <view> — start data-server + meta-server fresh (both are cheap; a stale
# server on a dropped schema would only muddy the next assertion).
serve(){
    "$Y" data-server --listen $DATA --data-dir "$W/droot" > "$W/data.log" 2>&1 & DP=$!
    "$Y" meta-server --listen $META --home "$W/home" --meta-store "$PGURI" > "$W/meta.log" 2>&1 & MP=$!
    wait_port $DATA || { bad "data-server failed ($(tail -1 "$W/data.log"))"; return 1; }
    wait_port $META || { bad "meta-server failed ($(tail -1 "$W/meta.log"))"; return 1; }
    return 0
}
stop_servers(){ [ "$MP" != 0 ] && kill "$MP" 2>/dev/null; MP=0; [ "$DP" != 0 ] && kill "$DP" 2>/dev/null; DP=0; }

# mount_write <view> <schema> <nfiles> — mount, write content, sync, unmount.
#
mount_write(){
    local v=$1 sch=$2 n=$3 i
    # Readiness is `mountpoint`, not `ls`: an EMPTY DIRECTORY also answers `ls`
    # with success, so an `ls` probe races the mount and lets every write land in
    # the bare directory instead of the tree (that produced a schema with only
    # its root row and a later "mountpoint is not empty").
    "$Y" --home "$W/m1" mount --view "$v" --mountpoint "$W/mnt" \
        --meta-server $META --data-servers $DATA --foreground > "$W/mnt.log" 2>&1 & RP=$!
    for _ in $(seq 1 150); do mountpoint -q "$W/mnt" && break; sleep 0.1; done
    if ! mountpoint -q "$W/mnt"; then
        bad "mount of '$v' never appeared ($(tail -1 "$W/mnt.log"))"; RP=0; return 1
    fi
    for i in $(seq 1 "$n"); do
        T sh -c "printf 'payload-%s-%s' '$v' '$i' > '$W/mnt/f$i'" || bad "write f$i on '$v' failed"
    done
    T ln "$W/mnt/f1" "$W/mnt/h1" 2>/dev/null && ok "'$v' hardlink written (one object, two names)"
    sync "$W/mnt" 2>/dev/null; sleep 1.2
    for _ in $(seq 1 30); do mountpoint -q "$W/mnt" || break; fusermount -u "$W/mnt" 2>/dev/null; sleep 0.1; done
    if mountpoint -q "$W/mnt"; then
        bad "unmount of '$v' failed — forcing it (`fusermount -uz`)"
        fusermount -uz "$W/mnt" 2>/dev/null
    fi
    kill "$RP" 2>/dev/null; RP=0
    sleep 0.7
    # The write must be OBSERVABLE IN POSTGRES, not merely on the mount: that is
    # the difference between "the FUSE layer said yes" and "the tree has rows".
    local rows
    rows=$(q "select count(*) from \"$sch\".node")
    [ "${rows:-0}" -ge $((n + 2)) ] && ok "PG tree $sch holds $rows row(s) ($n files + hardlink + root)" \
        || bad "PG tree $sch has only '${rows:-0}' row(s) after writing $n file(s)"
    return 0
}

echo "===== 0) baseline: no base schemas, no ledger rows ====="
chk "$(schemas)" "0" "no grovefs_v_* schema before the run"
chk "$(ledger)" "0" "no grovefs_refs.holders rows before the run"

echo "===== A) create + mount + write (a real PG tree with real bytes) ====="
"$Y" view --meta-dir "$W/home" --meta-store "$PGURI" create --view "$V1" > "$W/create.log" 2>&1 \
    && ok "view create '$V1' on postgres://…" || bad "view create failed ($(cat "$W/create.log"))"
chk "$(q "select count(*) from information_schema.schemata where schema_name='$S1'")" "1" "the tree IS schema $S1"
serve || exit 1
mount_write "$V1" "$S1" 4

echo "===== B) the tree, the holds and the bytes are three separate facts ====="
rows=$(q "select count(*) from \"$S1\".node")
gt0 "$rows" "schema $S1 holds node rows"
blobs_now=$(blobs)
gt0 "$blobs_now" "data root holds objects under blocks/"
led=$(ledger)
gt0 "$led" "ledger books one row per content key"
refs=$(q "select count(distinct data_ref) from \"$S1\".node where data_ref is not null")
chk "$led" "$refs" "ledger rows == distinct refs the tree names"

echo "===== C) view rm --base: schema drop + ledger unwind + byte reclaim ====="
stop_servers
out=$("$Y" view --meta-dir "$W/home" --meta-store "$PGURI" rm --name "$V1" --base --data-dir "$W/droot" 2>&1); rc=$?
chk "$rc" "0" 'view rm --base on a PG tree succeeds'
echo "$out" | sed 's/^/    | /'
echo "$out" | grep -q "schema dropped" && ok "report names the schema drop" || bad "report does not name the schema drop"
chk "$(q "select count(*) from information_schema.schemata where schema_name='$S1'")" "0" "schema $S1 is gone"
chk "$(ledger)" "0" "every ledger row the tree booked is gone (unwound in the same transaction)"
chk "$(blobs)" "0" "all objects freed from the data root"
echo "$out" | grep -q "0 not found under that data root" && ok "byte accounting is exact (nothing 'not found')" \
    || bad "byte accounting unclear: expected 0 missing"

echo "===== D) a second removal is refused, not reported as success ====="
out=$("$Y" view --meta-dir "$W/home" --meta-store "$PGURI" rm --name "$V1" --base --data-dir "$W/droot" 2>&1); rc=$?
[ "$rc" != 0 ] && ok "second removal exits non-zero (rc=$rc)" || bad "second removal returned 0 (a typo would read as success)"
echo "$out" | grep -qi "no schema" && ok "error names the missing schema" || bad "error unclear: $(echo "$out" | head -1)"

echo "===== E) schema dropped OUT OF BAND strands ledger rows; refs rebuild clears them ====="
"$Y" view --meta-dir "$W/home" --meta-store "$PGURI" create --view "$V2" > "$W/create2.log" 2>&1 \
    || bad "view create '$V2' failed ($(cat "$W/create2.log"))"
serve || exit 1
mount_write "$V2" "$S2" 3
stop_servers
stranded=$(ledger)
gt0 "$stranded" "tree $V2 booked $stranded ledger row(s)"
# Capture what the schema names BEFORE destroying it: an out-of-band drop also
# destroys the ONLY record of which objects the view held, so no later step can
# report them for reclaim. That is asserted in section H instead of glossed over.
v2refs=$(q "select count(distinct data_ref) from \"$S2\".node where data_ref is not null")
q "drop schema \"$S2\" cascade"                       # the out-of-band act
chk "$(q "select count(*) from information_schema.schemata where schema_name='$S2'")" "0" "schema $S2 dropped behind the CLI's back"
chk "$(ledger)" "$stranded" "its holds are still booked (the leak this repair exists for)"
out=$("$Y" refs --meta-dir "$W/home" --meta-store "$PGURI" rebuild 2>&1); rc=$?
chk "$rc" "0" "refs rebuild on postgres://… succeeds"
echo "$out" | sed 's/^/    | /'
deleted=$(echo "$out" | sed -n 's/.*\([0-9]\+\) row(s) deleted.*/\1/p')
chk "$deleted" "$stranded" "exactly the stranded rows are deleted"
chk "$(ledger)" "0" "ledger is empty again"

echo "===== F) a LOST booking is restored (the unsafe direction) ====="
"$Y" view --meta-dir "$W/home" --meta-store "$PGURI" create --view "$V3" > "$W/create3.log" 2>&1 \
    || bad "view create '$V3' failed ($(cat "$W/create3.log"))"
serve || exit 1
mount_write "$V3" "$S3" 2
stop_servers
booked=$(ledger)
gt0 "$booked" "tree $V3 booked $booked ledger row(s)"
q "delete from grovefs_refs.holders where data_ref in (select data_ref from \"$S3\".node where data_ref is not null)"
chk "$(ledger)" "0" "bookings wiped by hand (a restore that lost the ledger looks exactly like this)"
out=$("$Y" refs --meta-dir "$W/home" --meta-store "$PGURI" rebuild 2>&1); rc=$?
chk "$rc" "0" "refs rebuild restores them"
fixed=$(echo "$out" | sed -n 's/.*\([0-9]\+\) row(s) corrected.*/\1/p')
chk "$fixed" "$booked" "every lost booking is rewritten from the tree"
chk "$(ledger)" "$booked" "ledger matches what the tree actually names"

echo "===== G) a governed name still needs the plain view rm first ====="
"$Y" view --meta-dir "$W/home" --meta-store "$PGURI" define --name "${V3}lens" --base "$V3" \
    --rules "$W/rules.layout" > "$W/define.log" 2>&1 \
    && ok "lens defined over base '$V3'" || bad "define failed ($(cat "$W/define.log"))"
out=$("$Y" view --meta-dir "$W/home" --meta-store "$PGURI" rm --name "$V3" --base --data-dir "$W/droot" 2>&1); rc=$?
[ "$rc" != 0 ] && ok "removing a base with a live definition is refused (rc=$rc)" || bad "base removed from under its lens"
echo "$out" | grep -qi "governed by a view definition\|over base" && ok "refusal names the dependent definition" \
    || bad "refusal unclear: $(echo "$out" | head -1)"
chk "$(q "select count(*) from information_schema.schemata where schema_name='$S3'")" "1" "the tree is untouched by the refusal"
"$Y" view --meta-dir "$W/home" --meta-store "$PGURI" rm --name "${V3}lens" > "$W/rmlens.log" 2>&1 \
    && ok "definition removed with the plain view rm" || bad "removing the definition failed ($(cat "$W/rmlens.log"))"
out=$("$Y" view --meta-dir "$W/home" --meta-store "$PGURI" rm --name "$V3" --base --data-dir "$W/droot" 2>&1); rc=$?
chk "$rc" "0" "the base tree now removes cleanly"
chk "$(q "select count(*) from information_schema.schemata where schema_name='$S3'")" "0" "schema $S3 is gone"

echo "===== H) what is left behind, and why it is exactly that ====="
chk "$(schemas)" "0" "0 base-view schemas in the database"
chk "$(ledger)" "0" "0 ledger rows in the database"
# V1 and V3 were removed THROUGH the CLI, which knows the refs they held and
# frees their bytes. V2's schema was dropped behind its back, so nothing in the
# database can say which objects it named: the ledger is repaired, the BYTES
# are orphaned. Reporting that honestly is the point — a teardown path that
# cannot enumerate the refs must not pretend the store is clean.
chk "$(blobs)" "${v2refs:-0}" "the only objects left are V2's ${v2refs:-?} — stranded by the out-of-band drop (unreclaimable: its ref list died with the schema)"
chk "$(blobs)" "$stranded" "that is one blob per stranded ledger row"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
if [ "$fail" = 0 ]; then echo "PG VIEW TEARDOWN + LEDGER REPAIR: ALL CHECKS PASSED"; exit 0; fi
echo "PG VIEW TEARDOWN + LEDGER REPAIR: FAILURES ABOVE"; exit 1
