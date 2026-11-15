#!/usr/bin/env bash
# REMOTE `view fork` / `view graft` e2e — the two tree-copy verbs, for the
# server-side backend that keeps trees outside the operator's machine.
#
# The claim under test: fork and graft work over the governance port exactly as
# they do on the local SQLite file store, for the backend where a "tree" is not
# a file: a `grovefs_v_<hex>` SCHEMA under PostgreSQL. A fork is a copy of one
# whole tree into another in ONE transaction (`INSERT ... SELECT` between
# schemas), a graft places a curated subset with fresh
# ids/inodes — and in both cases the operator's host holds no metadata at all.
#
# What is asserted (PG; the arm self-skips when its
# infrastructure is absent — the SQLite arm is `test_fork.sh`):
#   - `view fork` (remote) copies the whole tree, ZERO bytes are copied on the
#     data server, and every shared object gets a second ledger holder;
#   - the fork is an INDEPENDENT tree: structure writable, content refused
#     EROFS; an unlink inside the fork never frees an object the base still
#     names (the base still reads its bytes afterwards);
#   - `view rm` on a fork drops its tree: the name becomes a fresh `view create`
#     target again (a lingering tree would refuse with "already exists");
#   - `view fork --empty` + `view graft` places a curated subset with renames
#     and a `--dest` prefix, creating missing directories; re-running the plan
#     refuses, `--skip-existing` merges, a `..` destination and a missing source
#     path are refused, and an alias cannot receive;
#   - the cross-view ledger is exact: after every fork/graft is removed, deleting
#     the base's LAST name for each object frees it, and the data root ends up
#     with zero objects (no leaked holders, none freed early either).
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

D=.forktest2
rm -rf $D
mkdir -p $D

# ---- PostgreSQL reachability (self-skip) ----------------------------------
PGHOST=${PGHOST:-127.0.0.1}
if [ -z "${PGPORT:-}" ]; then
    for P in 5432 5433; do
        if (exec 3<>/dev/tcp/${PGHOST}/$P) 2>/dev/null; then
            PGPORT=$P; break
        fi
    done
    PGPORT=${PGPORT:-5432}
fi
PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"
if (exec 3<>/dev/tcp/${PGHOST}/${PGPORT}) 2>/dev/null; then PG_OK=1; else PG_OK=0; fi

pass=0; fail=0; skip=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
skp(){ echo "  SKIP: $1"; skip=$((skip+1)); }
good(){ local what=$1; shift; local out; if out=$("$@" 2>&1); then ok "$what"; else bad "$what — rc=$? :: $(echo "$out" | head -3 | tr '\n' ' ')"; fi; }
deny(){ local what=$1 want=$2; shift 2; local out; out=$("$@" 2>&1) && { bad "$what — was admitted (expected a refusal)"; return; }; if echo "$out" | grep -qF -- "$want"; then ok "$what"; else bad "$what — refused but did not say '$want' :: $(echo "$out" | head -2 | tr '\n' ' ')"; fi; }
keyof(){ awk '/^  key: /{print $2}'; }
# `/proc/mounts` records the ABSOLUTE mountpoint, so compare both spellings: the
# suite runs with $D relative, and a relative-only test silently fails every
# "mounted?" assertion (measured — it turned 8 checks red for no real reason).
mounted(){ local m=$1; awk -v m="$m" -v ma="$PWD/$m" '$2==m||$2==ma{found=1} END{exit !found}' /proc/mounts; }
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
objs(){ find "$1/blocks" -type f 2>/dev/null | wc -l; }

PIDS=""; MOUNTS=""
cleanup(){ for m in $MOUNTS; do fusermount -u "$m" 2>/dev/null || true; done; for p in $PIDS; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# One full cycle against the server-side backend. $1 label, $2 server binary,
# $3 meta-store URI, $4 metaport, $5 control port, $6 dataport.
exercise(){
    local label=$1 SBIN=$2 MSTOR=$3 MP=$4 CP=$5 DP=$6
    local S=$(date +%s)
    local B=base$label$S F=full$label$S G=cur$label$S AL=al$label$S
    local H=$D/$label MNT=$D/$label/mnt
    mkdir -p "$H/srv" "$H/data" "$H/op" "$MNT"
    echo "===== flavour $label (meta-store ${MSTOR}) ====="

    $Y data-server --listen 127.0.0.1:$DP --data-dir "$H/data" > "$H/data.log" 2>&1 &
    local DPID=$!; PIDS="$PIDS $DPID"
    $SBIN meta-server --meta-dir "$H/srv" --listen 127.0.0.1:$MP \
        --control-listen 127.0.0.1:$CP --meta-store "$MSTOR" > "$H/meta.log" 2>&1 &
    local MPID=$!; PIDS="$PIDS $MPID"
    if wait_port 127.0.0.1:$DP && wait_port 127.0.0.1:$MP && wait_port 127.0.0.1:$CP; then
        ok "[$label] data-server + meta-server + control port up"
    else
        bad "[$label] servers did not come up :: $(tail -3 "$H/meta.log" | tr '\n' ' ')"
        echo; echo "==================== SUMMARY: pass=$pass fail=$fail SKIP=$skip ===================="
        echo "SERVERS FAILED"; exit 1
    fi

    local OUT KEY
    OUT=$($SBIN apikey --meta-store "$MSTOR" --meta-dir "$H/srv" \
        create --label "$label-admin" --for admin --owner alice 2>&1)
    KEY=$(echo "$OUT" | keyof)
    if [ -n "$KEY" ]; then ok "[$label] admin key minted on the server host"; else
        bad "[$label] mint failed :: $(echo "$OUT" | tail -3 | tr '\n' ' ')"; exit 1
    fi
    local CTL=(--control-server 127.0.0.1:$CP --api-key "$KEY")
    V(){ $Y --home "$H/op" view "${CTL[@]}" "$@"; }

    echo "----- [$label] 1) seed a BASE view with real content ====="
    good "[$label] remote 'view create' provisions the tree" V create --view $B
    $Y --home "$H/op" mount --view $B --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_base.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    mounted "$MNT" && ok "[$label] mounted the base view" || bad "[$label] base mount failed :: $(tail -3 "$H/mnt_base.log" | tr '\n' ' ')"
    mkdir -p "$MNT/docs" 2>/dev/null
    printf 'alpha-body' > "$MNT/docs/a.md" 2>/dev/null
    printf 'beta-body'  > "$MNT/docs/b.md" 2>/dev/null
    printf 'top-body'   > "$MNT/top.txt" 2>/dev/null
    sync "$MNT" 2>/dev/null; sleep 1.2
    [ "$(cat "$MNT/docs/a.md" 2>/dev/null)" = "alpha-body" ] \
        && ok "[$label] content written and read back through the base mount" \
        || bad "[$label] base write/read failed"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }
    mounted "$MNT" && bad "[$label] base mount did not unmount" || ok "[$label] base mount unmounted"
    local OBJS0; OBJS0=$(objs "$H/data")
    [ "$OBJS0" -ge 3 ] && ok "[$label] the base holds $OBJS0 object(s) on the data server" \
        || bad "[$label] expected >=3 objects on the data server, got $OBJS0"

    echo "----- [$label] 2) view fork: whole-tree copy, bytes shared ====="
    local FKOUT
    FKOUT=$(V fork --name $F --base $B 2>&1)
    if echo "$FKOUT" | grep -q "shared object"; then ok "[$label] remote 'view fork' copied the tree ($(echo "$FKOUT" | head -1))"; else bad "[$label] fork failed :: $(echo "$FKOUT" | head -3 | tr '\n' ' ')"; fi
    local OBJS1; OBJS1=$(objs "$H/data")
    [ "$OBJS1" = "$OBJS0" ] && ok "[$label] the fork copied ZERO bytes (data server still holds $OBJS1 object(s))" \
        || bad "[$label] fork changed the object count ($OBJS0 -> $OBJS1)"
    [ ! -e "$H/op/meta/$F.db" ] && ok "[$label] the operator home holds no tree for the fork" || bad "[$label] fork landed on the operator machine"
    deny "[$label] forking onto an existing physical view name is refused" "already names a physical view" \
        V fork --name $B --base $F

    echo "----- [$label] 3) the fork is independent: same names, own metadata ====="
    $Y --home "$H/op" mount --view $F --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_fork.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ "$(cat "$MNT/docs/a.md" 2>/dev/null)" = "alpha-body" ] \
        && ok "[$label] the fork renders the base's files (metadata copied, bytes shared)" \
        || bad "[$label] fork content unreadable :: $(tail -3 "$H/mnt_fork.log" | tr '\n' ' ')"
    mkdir -p "$MNT/newdir" 2>/dev/null && [ -d "$MNT/newdir" ] \
        && ok "[$label] structure is writable in the fork" || bad "[$label] mkdir in the fork failed"
    WOUT=$(sh -c 'printf nope > "$1"' _ "$MNT/docs/a.md" 2>&1)
    if echo "$WOUT" | grep -qE "Read-only file system|只读文件系统"; then ok "[$label] content writes in the fork are refused (EROFS)"; else bad "[$label] fork accepted a content write :: $WOUT"; fi
    rm -f "$MNT/docs/a.md" 2>/dev/null
    [ ! -e "$MNT/docs/a.md" ] && ok "[$label] unlink inside the fork removes only the fork's name" || bad "[$label] unlink inside the fork failed"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }
    local OBJS2; OBJS2=$(objs "$H/data")
    [ "$OBJS2" = "$OBJS0" ] && ok "[$label] the fork's unlink freed NOTHING (the base still holds the object)" \
        || bad "[$label] the fork's unlink changed the object count ($OBJS0 -> $OBJS2)"
    $Y --home "$H/op" mount --view $B --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_base2.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ "$(cat "$MNT/docs/a.md" 2>/dev/null)" = "alpha-body" ] \
        && ok "[$label] the base still reads the bytes the fork unlinked" \
        || bad "[$label] the fork's unlink damaged the base"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }

    echo "----- [$label] 4) view rm of a fork drops its own tree ====="
    deny "[$label] an occupied name blocks a same-name create (so the check below is real)" "already has a derived/alias/fork definition" \
        V create --view $F
    good "[$label] remote 'view rm' removes the fork definition" V rm --name $F
    V list 2>&1 | grep -q "'$F'" && bad "[$label] '$F' still listed after rm" || ok "[$label] '$F' is gone from the view list"
    good "[$label] the name is a fresh create target again (tree really dropped)" V create --view $F
    good "[$label] cleanup of that probe tree" V rm --name $F --base

    echo "----- [$label] 5) --empty fork + graft: curated subset, renames, --dest ====="
    good "[$label] remote 'view fork --empty' makes a root-only tree" V fork --name $G --base $B --empty
    printf 'docs\tstuff\ntop.txt\tmeta/top-renamed.txt\n' > "$H/plan.tsv"
    good "[$label] remote 'view graft' places the plan on the server" \
        V graft --name $G --src $B --map "$H/plan.tsv"
    $Y --home "$H/op" mount --view $G --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_cur.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ -d "$MNT/stuff" ] && [ -f "$MNT/stuff/a.md" ] \
        && ok "[$label] the grafted directory subtree arrived under its new name" \
        || bad "[$label] grafted 'docs -> stuff' missing :: $(ls "$MNT" 2>/dev/null | tr '\n' ' ')"
    [ "$(cat "$MNT/stuff/b.md" 2>/dev/null)" = "beta-body" ] \
        && ok "[$label] grafted bytes are the source's (shared, not copied)" || bad "[$label] grafted file content wrong"
    [ -f "$MNT/meta/top-renamed.txt" ] && [ "$(cat "$MNT/meta/top-renamed.txt")" = "top-body" ] \
        && ok "[$label] the graft created the missing 'meta/' directory for a renamed file" \
        || bad "[$label] renamed file with a new parent directory is missing"
    [ ! -e "$MNT/docs" ] && [ ! -e "$MNT/top.txt" ] \
        && ok "[$label] the fork holds only the grafted subset" || bad "[$label] the empty fork is not empty"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }

    deny "[$label] re-running the plan collides (all-or-nothing)" "already exists" \
        V graft --name $G --src $B --map "$H/plan.tsv"
    local SK
    SK=$(V graft --name $G --src $B --map "$H/plan.tsv" --skip-existing 2>&1)
    echo "$SK" | grep -q "3 skipped" && ok "[$label] --skip-existing merges and reports 3 skipped entries" \
        || bad "[$label] --skip-existing did not report the skipped entries :: $(echo "$SK" | head -2 | tr '\n' ' ')"
    good "[$label] graft --dest places a second curated copy under a new prefix" \
        V graft --name $G --src $B --map "$H/plan.tsv" --dest /sub/dir --skip-existing
    deny "[$label] a '..' in --dest is refused by name" "must not contain '..'" \
        V graft --name $G --src $B --map "$H/plan.tsv" --dest ../escape --skip-existing
    printf 'nope/missing.txt\n' > "$H/bad.tsv"
    deny "[$label] a plan naming a path the source lacks is refused" "does not exist in view" \
        V graft --name $G --src $B --map "$H/bad.tsv"
    good "[$label] remote 'view alias' for the alias-destination check" V alias --name $AL --real $B
    deny "[$label] an alias cannot receive a graft (it owns no tree)" "has no tree of its own" \
        V graft --name $AL --src $B --map "$H/plan.tsv" --skip-existing
    good "[$label] remote 'view rm' removes the alias" V rm --name $AL

    echo "----- [$label] 6) the ledger is exact: last name frees, shared name does not ====="
    good "[$label] remote 'view rm' removes the grafted fork" V rm --name $G
    # Now the base is the ONLY holder of all three objects. Deleting one must free
    # exactly that object, and deleting the rest must leave the data root empty.
    $Y --home "$H/op" mount --view $B --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_base3.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    rm -f "$MNT/top.txt" 2>/dev/null; sync "$MNT" 2>/dev/null; sleep 1.2
    local OBJS3; OBJS3=$(objs "$H/data")
    [ "$OBJS3" -lt "$OBJS0" ] && ok "[$label] the base's last name freed its object ($OBJS0 -> $OBJS3)" \
        || bad "[$label] deleting the last name did not free the object ($OBJS0 -> $OBJS3)"
    rm -rf "$MNT/docs" 2>/dev/null; sync "$MNT" 2>/dev/null; sleep 1.5
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }
    local OBJS4; OBJS4=$(objs "$H/data")
    [ "$OBJS4" = 0 ] && ok "[$label] every object was freed: no holder leaked through fork/graft (0 left)" \
        || bad "[$label] $OBJS4 object(s) survived after every name was removed (ledger leak)"

    echo "----- [$label] 7) cleanup ====="
    good "[$label] remote 'view rm --base' drops the base tree" V rm --name $B --base
    V list 2>&1 | grep -q "'$B'" && bad "[$label] '$B' still listed after --base rm" || ok "[$label] $B is gone"
    good "[$label] the base name is a fresh create target again" V create --view $B
    good "[$label] final cleanup" V rm --name $B --base

    kill $MPID $DPID 2>/dev/null
    sleep 0.3
    echo
}

if [ "$PG_OK" = 1 ]; then
    exercise postgres "$Y" "$PGURI" 15310 15311 15312
else
    skp "PostgreSQL fork/graft arm (no server on ${PGHOST}:${PGPORT})"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail SKIP=$skip ===================="
[ "$fail" -eq 0 ] && echo "ALL REMOTE FORK/GRAFT CHECKS PASSED" || echo "SOME REMOTE FORK/GRAFT CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
