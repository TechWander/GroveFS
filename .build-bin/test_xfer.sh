#!/usr/bin/env bash
# REMOTE `view export` / `view import` e2e — the portable-dump verbs, for EVERY
# backend, over the governance port.
#
# The claim under test: a metadata dump can LEAVE and ENTER a deployment from an
# operator machine that holds no store credential and no tree, for all three
# backends (SQLite files, a `grovefs_v_<hex>` schema under PostgreSQL, a SUBSPACE
# under FoundationDB). The dump travels in line-aligned CHUNKS through the
# server's staging area, so neither side ever holds it whole and a chunk never
# splits a row — proven by exporting the same tree with a 60-byte window and with
# the default 4 MiB window and comparing the bytes.
#
# What is asserted, once per backend (each arm self-skips when its infrastructure
# is absent; the local-only SQLite arm is the first one, which needs nothing):
#   - remote `export` writes the dump on the OPERATOR's disk (nothing under the
#     operator's `meta/`), in the portable format, metadata only — no content
#     byte appears in the file;
#   - chunk size does not change the bytes; an existing `--out` needs `--force`;
#     an alias and a never-provisioned name are refused by name;
#   - remote `import` installs the tree on the SERVER (the operator home stays
#     empty), content is shared — the data server's object count does not move;
#   - `--as-fork` records the fork definition server-side: structure writable,
#     content EROFS; `--replace` is required to overwrite and re-imports cleanly;
#     importing onto a name that already has a definition is refused;
#   - the round trip is EXACT: re-exporting the imported view yields the same
#     node rows (ids, inodes, times, data_ref) as the original dump;
#   - the ledger is exact: after the imported copies are removed, deleting the
#     base's names frees every object (no holder leaked through import);
#   - the LOCAL CLI form (`--meta-dir/--meta-store` on the store's host) works on
#     all three backends too, and produces the same dump body as the remote one.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

D=.xfertest
rm -rf $D
mkdir -p $D

# ---- backend reachability (self-skip) -------------------------------------
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

FDB_HOME="${FDB_HOME:-/data/grovefs/.fdbbin}"
FDB_RUN_DIR="${FDB_RUN_DIR:-/data/grovefs/.fdbtest}"
CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-$FDB_RUN_DIR/test.cluster}"
FDBURI="fdb://$CLUSTER"
FDBBIN="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
FDB_OK=0
if [ -x "$FDBBIN" ] && [ -x "$FDB_HOME/fdbcli.x86_64" ] && [ -f "$CLUSTER" ]; then
    if "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" --exec "status minimal" 2>&1 \
        | grep -q "The database is available"; then
        FDB_OK=1
        export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"
    fi
fi

pass=0; fail=0; skip=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
skp(){ echo "  SKIP: $1"; skip=$((skip+1)); }
good(){ local what=$1; shift; local out; if out=$("$@" 2>&1); then ok "$what"; else bad "$what — rc=$? :: $(echo "$out" | head -3 | tr '\n' ' ')"; fi; }
deny(){ local what=$1 want=$2; shift 2; local out; out=$("$@" 2>&1) && { bad "$what — was admitted (expected a refusal)"; return; }; if echo "$out" | grep -qF -- "$want"; then ok "$what"; else bad "$what — refused but did not say '$want' :: $(echo "$out" | head -2 | tr '\n' ' ')"; fi; }
keyof(){ awk '/^  key: /{print $2}'; }
mounted(){ local m=$1; awk -v m="$m" -v ma="$PWD/$m" '$2==m||$2==ma{found=1} END{exit !found}' /proc/mounts; }
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
objs(){ find "$1/blocks" -type f 2>/dev/null | wc -l; }
body(){ tail -n +2 "$1" | sort; }

PIDS=""; MOUNTS=""
cleanup(){ for m in $MOUNTS; do fusermount -u "$m" 2>/dev/null || true; done; for p in $PIDS; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# One full cycle against one backend. $1 label, $2 server binary (the one that
# links the backend), $3 meta-store URI ("" = the plain SQLite file store),
# $4 metaport, $5 control port, $6 dataport.
exercise(){
    local label=$1 SBIN=$2 MSTOR=$3 MP=$4 CP=$5 DP=$6
    local S=$(date +%s)
    local B=base$label$S I=imp$label$S AF=afork$label$S LB=lbase$label$S AL=al$label$S
    local H=$D/$label MNT=$D/$label/mnt
    mkdir -p "$H/srv" "$H/data" "$H/op" "$MNT"
    echo "===== flavour $label (meta-store ${MSTOR:-sqlite files}) ====="

    $Y data-server --listen 127.0.0.1:$DP --data-dir "$H/data" > "$H/data.log" 2>&1 &
    local DPID=$!; PIDS="$PIDS $DPID"
    if [ -n "$MSTOR" ]; then
        $SBIN meta-server --meta-dir "$H/srv" --listen 127.0.0.1:$MP \
            --control-listen 127.0.0.1:$CP --meta-store "$MSTOR" > "$H/meta.log" 2>&1 &
    else
        $SBIN meta-server --meta-dir "$H/srv" --listen 127.0.0.1:$MP \
            --control-listen 127.0.0.1:$CP > "$H/meta.log" 2>&1 &
    fi
    local MPID=$!; PIDS="$PIDS $MPID"
    if wait_port 127.0.0.1:$DP && wait_port 127.0.0.1:$MP && wait_port 127.0.0.1:$CP; then
        ok "[$label] data-server + meta-server + control port up"
    else
        bad "[$label] servers did not come up :: $(tail -3 "$H/meta.log" | tr '\n' ' ')"
        echo; echo "==================== SUMMARY: pass=$pass fail=$fail SKIP=$skip ===================="
        echo "SERVERS FAILED"; exit 1
    fi

    local OUT KEY
    OUT=$($SBIN apikey --meta-dir "$H/srv" ${MSTOR:+--meta-store "$MSTOR"} \
        create --label "$label-admin" --for admin --owner alice 2>&1)
    KEY=$(echo "$OUT" | keyof)
    if [ -n "$KEY" ]; then ok "[$label] admin key minted on the server host"; else
        bad "[$label] mint failed :: $(echo "$OUT" | tail -3 | tr '\n' ' ')"; exit 1
    fi
    local CTL=(--control-server 127.0.0.1:$CP --api-key "$KEY")
    V(){ $Y --home "$H/op" view "${CTL[@]}" "$@"; }
    # The same verbs run ON the store's host, with no control plane in between.
    # The binary must be the one that LINKS the backend ($SBIN), exactly as the
    # server is: a plain build refuses `fdb://` by name.
    L(){ $SBIN --home "$H/op" view --meta-dir "$H/srv" ${MSTOR:+--meta-store "$MSTOR"} "$@"; }

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
    local OBJS0; OBJS0=$(objs "$H/data")
    [ "$OBJS0" -ge 3 ] && ok "[$label] the base holds $OBJS0 object(s) on the data server" \
        || bad "[$label] expected >=3 objects on the data server, got $OBJS0"

    echo "----- [$label] 2) remote export: dump lands on the OPERATOR's disk ====="
    local D1=$H/op/export1.jsonl
    GROVEFS_XFER_CHUNK=60 V export --view $B --out "$D1" > "$H/exp1.log" 2>&1
    if [ -f "$D1" ]; then ok "[$label] remote 'view export' wrote $(wc -c < "$D1") byte(s) in 60-byte chunks"; else
        bad "[$label] export failed :: $(tail -3 "$H/exp1.log" | tr '\n' ' ')"; fi
    head -1 "$D1" 2>/dev/null | grep -q '"format":"grovefs-view-metadata"' \
        && ok "[$label] the dump carries the portable format header" || bad "[$label] dump header wrong"
    head -1 "$D1" 2>/dev/null | grep -q '"kind":"physical"' \
        && ok "[$label] the header names the tree kind" || bad "[$label] dump kind wrong"
    [ "$(body "$D1" | wc -l)" -ge 5 ] \
        && ok "[$label] the dump carries every node row ($(body "$D1" | wc -l) rows)" \
        || bad "[$label] dump has too few rows"
    grep -q 'alpha-body' "$D1" && bad "[$label] the dump embeds CONTENT bytes" \
        || ok "[$label] the dump is metadata only (no content byte in it)"
    grep -q '"data_ref"' "$D1" && ok "[$label] objects are referenced by data_ref key" \
        || bad "[$label] no data_ref in the dump"
    [ ! -e "$H/op/meta/$B.db" ] && [ ! -d "$H/op/meta/views" ] \
        && ok "[$label] the operator home still holds no tree and no definition" \
        || bad "[$label] something was written under the operator's meta/"
    GROVEFS_XFER_CHUNK=4194304 V export --view $B --out "$H/op/export1b.jsonl" > /dev/null 2>&1
    # The header carries `exported_at`, so a byte-for-byte cmp of two exports is
    # a coin flip (measured: it went red for FDB alone, where the two exports
    # straddled a second boundary). What must not differ is everything ELSE:
    # the header without its timestamp, and every node row.
    head -1 "$D1" | sed -E 's/"exported_at":[^,]*/"exported_at":0/' > "$H/op/h1.txt"
    head -1 "$H/op/export1b.jsonl" | sed -E 's/"exported_at":[^,]*/"exported_at":0/' > "$H/op/h2.txt"
    body "$D1" > "$H/op/r1.txt"; body "$H/op/export1b.jsonl" > "$H/op/r2.txt"
    cmp -s "$H/op/h1.txt" "$H/op/h2.txt" && cmp -s "$H/op/r1.txt" "$H/op/r2.txt" \
        && ok "[$label] the chunk size does not change one row (60 B vs 4 MiB window)" \
        || bad "[$label] the chunked dump differs from the default-chunk one"
    deny "[$label] an existing --out needs --force" "already exists" \
        V export --view $B --out "$D1"
    good "[$label] --force overwrites it" V export --view $B --out "$D1" --force
    # The three backends word this differently (SQLite/PG: "no metadata|base
    # tree '…'"; FDB: "is not a provisioned subspace"), so assert on the ONE
    # thing they share: the refusal names the view the operator asked for.
    deny "[$label] exporting a name the store never provisioned is refused" "'nosuch$S'" \
        V export --view nosuch$S --out "$H/op/none.jsonl"
    [ ! -e "$H/op/none.jsonl" ] && ok "[$label] the refused export left no file behind" \
        || bad "[$label] a refused export wrote a dump"
    good "[$label] remote 'view alias' for the virtual-name check" V alias --name $AL --real $B
    deny "[$label] an alias cannot be exported (it owns no tree)" "has no metadata tree of its own" \
        V export --view $AL --out "$H/op/al.jsonl"
    good "[$label] remote 'view rm' removes the alias" V rm --name $AL

    echo "----- [$label] 3) remote import: the tree lands on the SERVER ====="
    GROVEFS_XFER_CHUNK=60 V import --view $I --in "$D1" > "$H/imp1.log" 2>&1
    if grep -q "imported BASE view" "$H/imp1.log"; then ok "[$label] remote 'view import' installed the tree ($(grep -o '[0-9]* node(s), [0-9]* blob reference(s)' "$H/imp1.log" | head -1))"; else
        bad "[$label] import failed :: $(tail -3 "$H/imp1.log" | tr '\n' ' ')"; fi
    local OBJS1; OBJS1=$(objs "$H/data")
    [ "$OBJS1" = "$OBJS0" ] && ok "[$label] the import copied ZERO bytes (data server still holds $OBJS1 object(s))" \
        || bad "[$label] import changed the object count ($OBJS0 -> $OBJS1)"
    [ ! -e "$H/op/meta/$I.db" ] && ok "[$label] the operator home holds no tree for the imported view" \
        || bad "[$label] the imported tree landed on the operator machine"
    $Y --home "$H/op" mount --view $I --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_imp.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ "$(cat "$MNT/docs/a.md" 2>/dev/null)" = "alpha-body" ] && [ "$(cat "$MNT/top.txt" 2>/dev/null)" = "top-body" ] \
        && ok "[$label] the imported view renders the original tree (bytes shared)" \
        || bad "[$label] imported tree unreadable :: $(tail -3 "$H/mnt_imp.log" | tr '\n' ' ')"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }

    echo "----- [$label] 4) round trip is EXACT; --replace and the guards ====="
    V export --view $I --out "$H/op/export2.jsonl" > /dev/null 2>&1
    if diff -q <(body "$D1") <(body "$H/op/export2.jsonl") > /dev/null 2>&1; then
        ok "[$label] re-exporting the imported view yields the same rows (ids/inodes/times kept)"
    else
        bad "[$label] the round trip changed node rows :: $(diff <(body "$D1") <(body "$H/op/export2.jsonl") | head -2 | tr '\n' ' ')"
    fi
    deny "[$label] importing over an existing tree needs --replace" "pass --replace" \
        V import --view $I --in "$D1"
    good "[$label] --replace re-imports the tree" V import --view $I --in "$D1" --replace
    good "[$label] remote 'view alias' (a definition an import must not overwrite)" V alias --name $AL --real $B
    deny "[$label] importing onto a name with a definition is refused" "already has a definition" \
        V import --view $AL --in "$D1"

    echo "----- [$label] 5) --as-fork: fork semantics recorded BY THE SERVER ====="
    good "[$label] remote import --as-fork records the fork" V import --view $AF --in "$D1" --as-fork $B
    V list 2>&1 | grep -qF "fork '$AF' of base '$B'" && ok "[$label] 'view list' shows the imported tree as a fork of the base" \
        || bad "[$label] the fork definition was not recorded :: $(V list 2>&1 | grep $AF | tr '\n' ' ')"
    $Y --home "$H/op" mount --view $AF --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_af.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ "$(cat "$MNT/docs/b.md" 2>/dev/null)" = "beta-body" ] \
        && ok "[$label] the fork-imported tree reads the base's bytes" || bad "[$label] fork-imported tree unreadable"
    mkdir -p "$MNT/dir2" 2>/dev/null && [ -d "$MNT/dir2" ] \
        && ok "[$label] structure is writable in the imported fork" || bad "[$label] mkdir in the imported fork failed"
    WOUT=$(sh -c 'printf nope > "$1"' _ "$MNT/top.txt" 2>&1)
    if echo "$WOUT" | grep -qE "Read-only file system|只读文件系统"; then ok "[$label] content writes in the imported fork are refused (EROFS)"; else bad "[$label] the imported fork accepted a content write :: $WOUT"; fi
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }

    echo "----- [$label] 6) the LOCAL CLI form works on this backend too ====="
    L export --view $B --out "$H/local.jsonl" > "$H/lexp.log" 2>&1
    if [ -f "$H/local.jsonl" ]; then ok "[$label] local 'view export --meta-store' dumped the tree"; else
        bad "[$label] local export failed :: $(tail -3 "$H/lexp.log" | tr '\n' ' ')"; fi
    diff -q <(body "$D1") <(body "$H/local.jsonl") > /dev/null 2>&1 \
        && ok "[$label] local and remote exports carry the same rows" \
        || bad "[$label] local and remote exports differ"
    L import --view $LB --in "$H/local.jsonl" > "$H/limp.log" 2>&1
    grep -q "imported BASE view" "$H/limp.log" && ok "[$label] local 'view import --meta-store' installed it" \
        || bad "[$label] local import failed :: $(tail -3 "$H/limp.log" | tr '\n' ' ')"
    $Y --home "$H/op" mount --view $LB --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_lb.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    [ "$(cat "$MNT/docs/b.md" 2>/dev/null)" = "beta-body" ] \
        && ok "[$label] the locally imported view reads the same bytes" || bad "[$label] locally imported view unreadable"
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }

    echo "----- [$label] 7) the ledger is exact through export/import ====="
    good "[$label] remote 'view rm' removes the alias probe" V rm --name $AL
    good "[$label] remote 'view rm' drops the imported fork" V rm --name $AF
    good "[$label] remote 'view rm --base' drops the imported tree" V rm --name $I --base
    good "[$label] remote 'view rm --base' drops the locally imported tree" V rm --name $LB --base
    $Y --home "$H/op" mount --view $B --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground > "$H/mnt_final.log" 2>&1 &
    PIDS="$PIDS $!"; MOUNTS="$MOUNTS $MNT"
    sleep 1.6
    rm -rf "$MNT/docs" "$MNT/top.txt" 2>/dev/null; sync "$MNT" 2>/dev/null; sleep 1.5
    $Y umount -m "$MNT" > /dev/null 2>&1
    sleep 0.8
    mounted "$MNT" && { fusermount -u "$MNT" 2>/dev/null; sleep 0.5; }
    local OBJS3; OBJS3=$(objs "$H/data")
    [ "$OBJS3" = 0 ] && ok "[$label] every object was freed: no holder leaked through export/import (0 left)" \
        || bad "[$label] $OBJS3 object(s) survived after every name was removed (ledger leak)"

    echo "----- [$label] 8) cleanup ====="
    good "[$label] remote 'view rm --base' drops the base tree" V rm --name $B --base
    good "[$label] the base name is a fresh create target again" V create --view $B
    good "[$label] final cleanup" V rm --name $B --base
    find "$H/srv/meta/.xfer" -type f 2>/dev/null | grep -q . \
        && bad "[$label] staged chunks were left behind under meta/.xfer" \
        || ok "[$label] no staged chunk survived (server released every handle)"

    kill $MPID $DPID 2>/dev/null
    sleep 0.3
    echo
}

ARMS="${ARMS:-sqlite postgres fdb}"
has(){ case " $ARMS " in (*" $1 "*) return 0;; (*) return 1;; esac; }

if has sqlite; then
    exercise sqlite "$Y" "" 15330 15331 15332
else
    skp "SQLite arm (not in ARMS=$ARMS)"
fi

if has postgres; then
    if [ "$PG_OK" = 1 ]; then
        exercise postgres "$Y" "$PGURI" 15340 15341 15342
    else
        skp "PostgreSQL export/import arm (no server on ${PGHOST}:${PGPORT})"
    fi
fi

if has fdb; then
    if [ "$FDB_OK" = 1 ]; then
        exercise fdb "$FDBBIN" "$FDBURI" 15350 15351 15352
    else
        skp "FoundationDB export/import arm (no cluster at $CLUSTER or no $FDBBIN)"
    fi
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail SKIP=$skip ===================="
[ "$fail" -eq 0 ] && echo "ALL REMOTE EXPORT/IMPORT CHECKS PASSED" || echo "SOME REMOTE EXPORT/IMPORT CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
