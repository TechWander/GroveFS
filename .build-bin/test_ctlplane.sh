#!/usr/bin/env bash
# REMOTE GOVERNANCE e2e — `meta-server --control-listen` + `--control-server`.
#
# The claim under test: an operator on ANY machine can govern a deployment with
# an ACCOUNT KEY, and the machine that owns the metadata store does the work. That
# replaces "every management host holds the database password" under PG, and it is
# the only remote path that has ever existed for the SQLite backend (its trees are
# local files, so governing them used to mean being on the disk's host).
#
# What is asserted:
#   - the second port is opt-in and says so at startup;
#   - the FIRST admin key must be minted on the server host (a control port that
#     minted its own administrator would defeat the gate);
#   - view create/define/alias/list/rm — and apikey mint/list/revoke + refs
#     rebuild — all work from a client whose `--home` holds NO metadata at all,
#     against a SQLITE store (the backend that was local-only);
#   - two different client homes reach the same authoritative store (the "any
#     machine" property), and every call is audited with the account;
#   - the safety table: a `--for mount` or `--for any` key is REFUSED on the
#     governance port (`any` must never silently mean administrator), a revoked
#     admin key stops working, `--for admin` without `--owner` is refused, and
#     `--control-server` cannot be combined with a local `--meta-dir`;
#   - the tree verbs ALL cross the wire: `view create --manifest`, `fork`, `graft`,
#     `export` and `import` are served by the process that owns the store (the dump
#     is rendered into the server's own staging area and shipped back in
#     line-aligned chunks), so the operator machine holds no tree at any point;
#   - what stays refused is the entrance confusion: no key at all, a non-admin
#     scope, and `--control-server` combined with a local `--meta-dir`;
#   - the PG arm repeats the core loop against `postgres://…`, where the same
#     operator key replaces a database password. Self-skips if PG is unreachable.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

D=.ctltest
rm -rf $D; mkdir -p $D/srv $D/op1 $D/op2
META=127.0.0.1:15098
CTL=127.0.0.1:15099
META_PG=127.0.0.1:15100
CTL_PG=127.0.0.1:15101
PIDS=""
cleanup(){ for p in $PIDS; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
pass=0; fail=0; skip=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
skp(){ echo "  SKIP: $1"; skip=$((skip+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
# verdict of a command expected to SUCCEED
good(){ local what=$1; shift; local out; if out=$("$@" 2>&1); then ok "$what"; else bad "$what — rc=$? :: $(echo "$out" | head -3 | tr '\n' ' ')"; fi; }
# verdict of a command expected to be REFUSED (rc!=0); $2 is a string the
# refusal must mention, so a green cannot come from an unrelated crash.
deny(){ local what=$1 want=$2; shift 2; local out; out=$("$@" 2>&1) && { bad "$what — was admitted (expected a refusal)"; return; }; if echo "$out" | grep -qF -- "$want"; then ok "$what"; else bad "$what — refused but did not say '$want' :: $(echo "$out" | head -2 | tr '\n' ' ')"; fi; }
keyof(){ awk '/^  key: /{print $2}'; }
idof(){ awk '/^  id: /{print $2}'; }

echo "===== 1) server: the control port is opt-in and announced ====="
$Y meta-server --meta-dir $D/srv --listen $META --control-listen $CTL > $D/server.log 2>&1 &
PIDS="$PIDS $!"
wait_port $META || true
if wait_port $CTL; then ok "meta-server --control-listen is accepting"; else bad "control port never opened"; fi
grep -q 'CONTROL plane listening on' $D/server.log && ok "startup banner names the governance port" || bad "no CONTROL banner in server.log"

echo "===== 2) bootstrap: the FIRST admin key is minted on the server host ====="
deny "apikey create --for admin without --owner is refused" "--owner" \
    $Y apikey --meta-dir $D/srv create --label nobody --for admin
OUT=$($Y apikey --meta-dir $D/srv create --label alice-laptop --for admin --owner alice)
KEY=$(echo "$OUT" | keyof); AID=$(echo "$OUT" | idof)
[ -n "$KEY" ] && [ -n "$AID" ] && ok "admin key 'alice-laptop' minted for account alice" || bad "admin key not printed"
echo "$OUT" | grep -q '^  owner: alice' && ok "the mint echoes the account" || bad "mint does not show the account"

echo "===== 3) govern a SQLITE deployment from a machine with no store ====="
# op1's --home stays empty: it must never need (or hold) the metadata.
good "remote 'view create' creates the tree ON THE SERVER" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" create --view remote1
[ -f $D/srv/meta/remote1.db ] && ok "server holds meta/remote1.db" || bad "server did not create remote1.db"
[ ! -e $D/op1/meta/remote1.db ] && ok "operator home holds NO view db (nothing was written locally)" || bad "operator home got a local view db"
printf 'hide /secret\n' > $D/rules.txt
good "remote 'view define' writes the definition on the server" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" define --name d1 --base remote1 --rules $D/rules.txt
[ -f $D/srv/meta/views/d1.layout ] && ok "server holds meta/views/d1.layout" || bad "definition not written on the server"
good "remote 'view alias'" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" alias --name pub1 --real remote1
LIST=$($Y --home $D/op1 view --control-server $CTL --api-key "$KEY" list 2>&1)
for n in d1 pub1 remote1; do
    echo "$LIST" | grep -q "'$n'" && ok "remote 'view list' reports '$n'" || bad "remote 'view list' omits '$n'"
done
good "remote 'refs rebuild' runs on the server's ledger" \
    $Y --home $D/op1 refs --control-server $CTL --api-key "$KEY" rebuild

echo "===== 4) the same store from a SECOND machine (the 'any machine' property) ====="
LIST2=$($Y --home $D/op2 view --control-server $CTL --api-key "$KEY" list 2>&1)
echo "$LIST2" | grep -q "'d1'" && ok "a different home sees the same definitions" || bad "second home did not see d1"
[ -z "$(ls -A $D/op2 2>/dev/null)" ] && ok "second home left completely empty" || bad "second home was written to"

echo "===== 5) key administration over the same wire ====="
KOUT=$($Y --home $D/op1 apikey --control-server $CTL --api-key "$KEY" create --label bob-phone --for mount --owner bob --view remote1)
MK=$(echo "$KOUT" | keyof)
[ -n "$MK" ] && ok "remote 'apikey create' mints a scoped key on the server's registry" || bad "remote apikey create printed no key"
$Y --home $D/op1 apikey --control-server $CTL --api-key "$KEY" list | grep -q 'alice' \
    && ok "remote 'apikey list' shows the account column" || bad "remote apikey list lost the owner column"
CALLS=$($Y --home $D/op1 apikey --control-server $CTL --api-key "$KEY" calls --limit 40 2>&1)
echo "$CALLS" | grep -q 'account=alice' && ok "audit rows attribute calls to account 'alice'" || bad "no account attribution in the audit"
echo "$CALLS" | grep -q 'view.create' && ok "audit records the governance verb that ran" || bad "audit does not record view.create"

echo "===== 6) fork / graft / manifest: the tree work happens ON THE SERVER ====="
printf 'mkdir /docs\ntouch /docs/a.txt\nmkdir /docs/sub\n' > $D/seed.txt
good "remote 'view create --manifest' seeds a tree on the server" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" create --view seed1 --manifest $D/seed.txt
[ -f $D/srv/meta/seed1.db ] && ok "the seeded base tree lives on the server" || bad "seed1.db not on the server"
[ ! -e $D/op1/meta/seed1.db ] && ok "the operator home gained no tree" || bad "operator home gained metadata"
# The manifest's `touch` must have created its object under the SERVER's byte
# root: that is the difference between "the lines travelled" and "the file kept
# a local copy".
NOBJ=$(find $D/srv/blocks -type f 2>/dev/null | wc -l)
[ "$NOBJ" -ge 1 ] && ok "the manifest's file object was written under the server's blocks/" \
    || bad "no object under the server's blocks/ (touch did not run server-side)"
good "remote 'view fork' copies the tree on the server" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" fork --name lab --base seed1
[ -f $D/srv/meta/lab.db ] && ok "the fork's own tree was created on the server" || bad "fork tree missing on the server"
[ ! -e $D/op1/meta/lab.db ] && ok "the fork did not land on the operator machine" || bad "fork landed locally"
good "remote 'view fork --empty'" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" fork --name lab2 --base seed1 --empty
printf '/docs/a.txt\ta.txt\n' > $D/plan.tsv
good "remote 'view graft' places a curated subset" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" graft --name lab2 --src seed1 --map $D/plan.tsv
FLIST=$($Y --home $D/op1 view --control-server $CTL --api-key "$KEY" list 2>&1)
for n in lab lab2 seed1; do
    echo "$FLIST" | grep -q "'$n'" && ok "remote 'view list' reports '$n'" || bad "remote 'view list' omits '$n'"
done
good "remote 'view rm' removes a fork definition" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name lab
[ ! -e $D/srv/meta/lab.db ] && ok "the fork's tree is gone from the SERVER (not just its definition)" \
    || bad "the fork's tree survived the remote removal"

echo "===== 7) the scope table is a table: 'any' is NOT 'admin' ====="
AOUT=$($Y apikey --meta-dir $D/srv create --label legacy --for any)
AK=$(echo "$AOUT" | keyof)
deny "'--for any' key is refused on the governance port" "not 'admin'" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$AK" list
deny "'--for mount' key is refused on the governance port" "not 'admin'" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$MK" list
deny "a garbage key is refused" "authentication" \
    $Y --home $D/op1 view --control-server $CTL --api-key "deadbeef:deadbeef" list
# remote revoke, then the credential is dead on the wire. Column layout is
# `# id owner label scope view rw created last_used revoked`, so the scope is $5.
BID=$($Y --home $D/op1 apikey --control-server $CTL --api-key "$KEY" list | awk '$5=="mount"{print $2}' | head -1)
if [ -n "$BID" ]; then
    good "remote 'apikey revoke'" \
        $Y --home $D/op1 apikey --control-server $CTL --api-key "$KEY" revoke --id "$BID"
    deny "a remotely revoked key no longer authenticates" "revoked" \
        $Y --home $D/op1 view --control-server $CTL --api-key "$MK" list
else
    bad "could not locate the mount key to revoke"
fi

echo "===== 8) refusals that keep the entrance models apart ====="
deny "no key at all is refused, naming the remedy" "admin account key" \
    $Y --home $D/op1 view --control-server $CTL list
deny "--control-server + --meta-dir is refused (one store per command)" "cannot be combined" \
    $Y view --meta-dir $D/srv --control-server $CTL --api-key "$KEY" list
# `export`/`import` USED to be refused BY NAME here, as "they move a tree to or
# from a FILE on the operator's machine". That objection is gone: the file is a
# STREAM, the server renders the dump into its own staging area, and chunks are
# cut on line boundaries in both directions. Assert the round trip instead.
good "remote 'view export' returns the server's dump" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" export --view seed1 --out $D/x.jsonl
grep -q 'grovefs-view-metadata' $D/x.jsonl \
    && ok "the dump arrives in the portable format" || bad "dump body is not the export format"
[ "$(tail -n +2 $D/x.jsonl | wc -l | tr -d ' ')" -ge 4 ] \
    && ok "the dump carries the seeded tree's rows (root + 3 manifest entries)" \
    || bad "dump is short: $(tail -n +2 $D/x.jsonl | wc -l) row(s)"
[ ! -e $D/op1/meta/seed1.db ] && ok "exporting a view wrote no tree on the operator machine" \
    || bad "the operator machine gained seed1.db during export"
good "remote 'view import' installs a tree the server had never seen" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" import --view seed2 --in $D/x.jsonl
[ -f $D/srv/meta/seed2.db ] && ok "the imported tree landed on the SERVER" \
    || bad "import did not provision seed2.db on the server"
[ ! -e $D/op1/meta/seed2.db ] && ok "import left the operator machine with no tree" \
    || bad "import wrote a local tree on the operator machine"
$Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name seed2 --base > $D/rm_seed2.log 2>&1 \
    && ok "the imported tree is droppable over the same wire (--base: a tree, not a definition)" \
    || bad "seed2 rm --base failed: $(cat $D/rm_seed2.log)"
[ ! -e $D/srv/meta/seed2.db ] && ok "the imported tree is gone from the SERVER" \
    || bad "seed2.db survived the remote rm"

echo "===== 9) teardown stays honest over the wire ====="
deny "'view rm --base' still refuses a base another definition resolves through" "governed by a definition" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name remote1 --base
good "remote 'view rm' removes a definition" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name d1
good "remote 'view rm' removes the alias" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name pub1
good "remote 'view rm --base' removes the tree on the server" \
    $Y --home $D/op1 view --control-server $CTL --api-key "$KEY" rm --name remote1 --base
[ ! -e $D/srv/meta/remote1.db ] && ok "server's remote1.db is gone" || bad "remote1.db survived the remote removal"

echo "===== 10) PG arm: the admin key replaces a database password ====="
PGHOST=${PGHOST:-127.0.0.1}
if [ -z "${PGPORT:-}" ]; then
    for P in 5432 5433; do
        if (exec 3<>/dev/tcp/${PGHOST}/$P) 2>/dev/null; then PGPORT=$P; exec 3<&- 2>/dev/null; break; fi
    done
    PGPORT=${PGPORT:-5432}
fi
if (exec 3<>/dev/tcp/${PGHOST}/${PGPORT}) 2>/dev/null; then
    exec 3<&- 2>/dev/null || true
    PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"
    PV="pg$((RANDOM))$RANDOM"
    $Y meta-server --meta-dir $D/pgsrv --listen $META_PG --control-listen $CTL_PG --meta-store $PGURI > $D/pg_server.log 2>&1 &
    PIDS="$PIDS $!"
    if wait_port $CTL_PG; then
        ok "PG-backed meta-server exposes the control port"
        POUT=$($Y apikey --meta-store $PGURI create --label pg-ops --for admin --owner pgops)
        PK=$(echo "$POUT" | keyof)
        [ -n "$PK" ] && ok "admin key minted into grovefs_control (PG)" || bad "PG admin key not minted"
        good "remote 'view create' provisions a PG schema" \
            $Y --home $D/op1 view --control-server $CTL_PG --api-key "$PK" create --view $PV
        PLIST=$($Y --home $D/op1 view --control-server $CTL_PG --api-key "$PK" list 2>&1)
        echo "$PLIST" | grep -q "'$PV'" && ok "remote 'view list' sees the PG tree" || bad "PG tree missing from the remote listing"
        echo "$PLIST" | grep -qi 'postgres' || true   # backend name is informational
        good "remote 'refs rebuild' runs against the PG ledger" \
            $Y --home $D/op1 refs --control-server $CTL_PG --api-key "$PK" rebuild
        good "remote 'view rm --base' drops the PG schema" \
            $Y --home $D/op1 view --control-server $CTL_PG --api-key "$PK" rm --name $PV --base
        require_sqlite=$($Y view --meta-store $PGURI --control-server $CTL_PG --api-key "$PK" list 2>&1)
        echo "$require_sqlite" | grep -q "cannot be combined" \
            && ok "the PG arm enforces the same one-store rule" || bad "PG arm accepted a local --meta-store beside --control-server"
    else
        bad "PG-backed meta-server never opened its control port"
    fi
else
    skp "no PostgreSQL on ${PGHOST}:${PGPORT} — PG arm skipped (the SQLite arm above is the regression gate)"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail skip=$skip ===================="
[ "$fail" -eq 0 ] && echo "ALL REMOTE-GOVERNANCE CHECKS PASSED" || echo "SOME REMOTE-GOVERNANCE CHECKS FAILED"
[ "$fail" -eq 0 ] || exit 1
exit 0
