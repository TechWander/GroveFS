#!/usr/bin/env bash
# REMOTE `gc` e2e — the orphan audit served by the control plane, for EVERY
# backend.
#
# The claim under test: an operator on ANY machine audits and reclaims a
# deployment's orphan objects with an admin account key. The SERVER reads the
# trees where they live (SQLite files, PG schemas, FDB subspaces) and the bytes
# are listed from `--data-servers`, so the operator needs neither `<meta-dir>`
# nor `<data-dir>` on their host — the two things that made `gc` host-only.
#
# What is asserted, once per backend (SQLite always; PG and FDB self-skip when
# their infrastructure is absent):
#   - a remote scan sees the same tree/object counts the bytes deserve, with the
#     operator home holding NO metadata at all;
#   - a live mount session on the SERVER blocks `--apply` ("in use") and a scan
#     reports it — the silence proof this command makes, stated as its own limit
#     (a mount on another machine is invisible from the server host);
#   - an out-of-band page in the data root is reported as an orphan, one younger
#     than the grace window is reported but KEPT, and `--apply` deletes exactly
#     the old one from the data server;
#   - a data server sharing no object with the trees is refused (wrong-root
#     guard) before anything is deleted, and --force only bypasses that guard;
#   - the local/remote combinations are refused by name (--meta-dir, --data-dir
#     beside --control-server) instead of silently preferring one side.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
Y=./target/release/grovefs

D=.gcremtest
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

# ---- FoundationDB reachability (self-skip) --------------------------------
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
mounted(){ awk -v m="$1" '$2==m{found=1} END{exit !found}' /proc/mounts; }
wait_port(){ local a=$1; for _ in $(seq 1 200); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }

# The data server serves `<data-dir>/blocks/<FNV-1a(key) mod 256>/<key>`, and
# `remove` recomputes that bucket from the key — so a hand-placed orphan has to
# sit in the bucket the key actually hashes to, or the audit could see it and
# never be able to delete it.
bucket_of(){ python3 - "$1" <<'PY'
import sys
h = 0xcbf29ce484222325
for b in sys.argv[1].encode():
    h ^= b
    h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
print("%02x" % (h % 256))
PY
}

PIDS=""; MOUNTS=""
cleanup(){ for m in $MOUNTS; do fusermount -u "$m" 2>/dev/null || true; done; for p in $PIDS; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# One full cycle against one backend. $1 label, $2 server binary (the one that
# links the backend), $3 meta-store URI or "-", $4 metaport, $5 control port,
# $6 dataport, $7 second dataport.
exercise(){
    local label=$1 SBIN=$2 MSTOR=$3 MP=$4 CP=$5 DP=$6 DP2=$7
    local H=$D/$label VIEW=${label}v$(date +%s) MNT=$D/$label/mnt
    mkdir -p "$H/srv" "$H/data" "$H/data2" "$H/op" "$MNT"
    echo "===== flavour $label (meta-store ${MSTOR}) ====="

    local mars=()
    [ "$MSTOR" != "-" ] && mars+=(--meta-store "$MSTOR")
    $Y data-server --listen 127.0.0.1:$DP --data-dir "$H/data" > "$H/data.log" 2>&1 &
    local DPID=$!; PIDS="$PIDS $DPID"; MOUNTS="$MOUNTS $MNT"
    $SBIN meta-server --meta-dir "$H/srv" --listen 127.0.0.1:$MP \
        --control-listen 127.0.0.1:$CP "${mars[@]}" > "$H/meta.log" 2>&1 &
    local MPID=$!; PIDS="$PIDS $MPID"
    if wait_port 127.0.0.1:$DP && wait_port 127.0.0.1:$MP && wait_port 127.0.0.1:$CP; then
        ok "[$label] data-server + meta-server + control port up"
    else
        bad "[$label] servers did not come up :: $(tail -2 "$H/meta.log" | tr '\n' ' ')"
        kill $DPID $MPID 2>/dev/null
        return
    fi

    # The first admin key is minted on the server host, against the SAME store
    # the server serves (files, schemas or subspaces).
    local kargs=()
    if [ "$MSTOR" = "-" ]; then kargs=(--meta-dir "$H/srv"); else kargs=(--meta-store "$MSTOR"); fi
    local OUT KEY
    OUT=$($SBIN apikey "${kargs[@]}" create --label "$label-admin" --for admin --owner alice 2>&1)
    KEY=$(echo "$OUT" | keyof)
    if [ -n "$KEY" ]; then
        ok "[$label] admin key minted on the server host"
    else
        bad "[$label] mint failed :: $(echo "$OUT" | tail -3 | tr '\n' ' ')"
        kill $DPID $MPID 2>/dev/null
        return
    fi

    local CTL=(--control-server 127.0.0.1:$CP --api-key "$KEY")
    good "[$label] remote 'view create' provisions the tree on the server" \
        $Y --home "$H/op" view "${CTL[@]}" create --view "$VIEW"
    [ ! -e "$H/op/meta/$VIEW.db" ] \
        && ok "[$label] operator home holds NO view tree" \
        || bad "[$label] operator home got a local tree"

    # A write through a mount IS the object the audit must recognise later.
    $Y --home "$H/op" mount --view "$VIEW" --mountpoint "$MNT" \
        --meta-server 127.0.0.1:$MP --data-servers 127.0.0.1:$DP --foreground \
        > "$H/mnt.log" 2>&1 &
    local FPID=$!; PIDS="$PIDS $FPID"
    sleep 1.6
    if [ -d "$MNT" ]; then ok "[$label] mount against the remote view"; else bad "[$label] mount failed :: $(tail -2 "$H/mnt.log" | tr '\n' ' ')"; fi
    printf 'hello\n' > "$MNT/a.txt" && sync "$MNT" 2>/dev/null; sleep 1.6
    [ -f "$MNT/a.txt" ] && ok "[$label] a file written through the mount" \
        || bad "[$label] write through the mount failed"
    local OBJS
    OBJS=$(find "$H/data/blocks" -type f 2>/dev/null | wc -l)
    [ "$OBJS" -ge 1 ] && ok "[$label] the write landed $OBJS object(s) on the data server" \
        || bad "[$label] no object on the data server after a write"

    # While that session is live, the server must say it is in use and refuse
    # to delete: reclaim is an offline operation, on every backend.
    local SC
    SC=$($Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP --older-than 1h 2>&1)
    echo "$SC" | grep -q "in use:" && ok "[$label] scan reports the live session" \
        || bad "[$label] scan hid the live session :: $(echo "$SC" | head -4 | tr '\n' ' ')"
    deny "[$label] --apply refuses while a session is live" "in use" \
        $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP --older-than 0 --apply

    $Y umount -m "$MNT" > /dev/null 2>&1; sleep 0.8
    if mounted "$MNT"; then bad "[$label] unmount left a live mount"; else ok "[$label] unmounted"; fi

    # A clean store: everything on the data server is named by the tree.
    $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP --older-than 1h --json \
        > "$H/scan1.json" 2>&1
    python3 - "$H/scan1.json" "$label" <<'PY' && ok "[$label] scan: referenced object, zero orphans, no deletion" || bad "[$label] clean scan shape :: $(head -3 "$H/scan1.json" | tr '\n' ' ')"
import json, sys
d = json.load(open(sys.argv[1]))
assert d["objects"] >= 1 and d["matched_objects"] >= 1, d
assert d["orphans"] == [] and d["reclaimable"] == 0, d
assert d["applied"] is False and d["deleted"] == 0, d
assert d["backend"] in ("sqlite", "postgres", "foundationdb"), d["backend"]
assert d["data_roots"], d
PY

    # One stale page and one young one, placed where `remove` will look.
    local BOLD BYOUNG
    BOLD=$(bucket_of orphan-old); BYOUNG=$(bucket_of orphan-young)
    mkdir -p "$H/data/blocks/$BOLD" "$H/data/blocks/$BYOUNG"
    printf 'stale\n' > "$H/data/blocks/$BOLD/orphan-old"
    printf 'fresh\n' > "$H/data/blocks/$BYOUNG/orphan-young"
    touch -d '2 hours ago' "$H/data/blocks/$BOLD/orphan-old"
    $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP --older-than 1h --json \
        > "$H/scan2.json" 2>&1
    python3 - "$H/scan2.json" <<'PY' && ok "[$label] scan: 2 orphans, 1 reclaimable, 1 within grace" || bad "[$label] orphan scan shape :: $(head -3 "$H/scan2.json" | tr '\n' ' ')"
import json, sys
d = json.load(open(sys.argv[1]))
assert len(d["orphans"]) == 2, d["orphans"]
assert d["reclaimable"] == 1 and d["within_grace"] == 1, d
kinds = sorted(c["kind"] for c in d["orphans"])
assert kinds == ["plain", "plain"], kinds
assert d["applied"] is False and d["deleted"] == 0, d
PY

    good "[$label] remote --apply reclaims the old orphan" \
        $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP --older-than 1h --apply
    [ -e "$H/data/blocks/$BOLD/orphan-old" ] \
        && bad "[$label] the old orphan is still on the data server" \
        || ok "[$label] the old orphan was deleted THROUGH the data server"
    [ -e "$H/data/blocks/$BYOUNG/orphan-young" ] \
        && ok "[$label] the within-grace object survived" \
        || bad "[$label] a young object was deleted"
    rm -f "$H/data/blocks/$BYOUNG/orphan-young"

    # Wrong deployment: a second data server whose one object no tree names.
    $Y data-server --listen 127.0.0.1:$DP2 --data-dir "$H/data2" > "$H/data2.log" 2>&1 &
    local D2PID=$!; PIDS="$PIDS $D2PID"
    wait_port 127.0.0.1:$DP2 || bad "[$label] second data-server did not start"
    local B2
    B2=$(bucket_of stray-page); mkdir -p "$H/data2/blocks/$B2"
    printf 'foreign\n' > "$H/data2/blocks/$B2/stray-page"
    touch -d '2 hours ago' "$H/data2/blocks/$B2/stray-page"
    deny "[$label] a data server sharing no key is refused" "share no object" \
        $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP2 --older-than 0 --apply
    [ -e "$H/data2/blocks/$B2/stray-page" ] \
        && ok "[$label] the foreign page survived the wrong-root refusal" \
        || bad "[$label] the wrong-root guard deleted foreign bytes"
    good "[$label] --force bypasses only the guard" \
        $Y --home "$H/op" gc "${CTL[@]}" --data-servers 127.0.0.1:$DP2 --older-than 0 --force
    [ -e "$H/data2/blocks/$B2/stray-page" ] \
        && ok "[$label] --force without --apply deleted nothing" \
        || bad "[$label] --force deleted bytes on a dry run"
    kill $D2PID 2>/dev/null

    # The two sides cannot be mixed: one store per audit, named by its key.
    deny "[$label] --control-server + --meta-dir refused" "--meta-dir" \
        $Y gc "${CTL[@]}" --meta-dir "$H/srv" --data-servers 127.0.0.1:$DP
    deny "[$label] --control-server + --data-dir refused" "--data-dir" \
        $Y gc "${CTL[@]}" --data-dir "$H/data" --data-servers 127.0.0.1:$DP

    # Leave the store as found: a PG schema / FDB subspace outlives the run's
    # temp dir, and a stale tree would make the NEXT run's create fail on a
    # collision instead of testing anything.
    good "[$label] remote 'view rm --base' drops the tree again" \
        $Y --home "$H/op" view "${CTL[@]}" rm --name "$VIEW" --base

    kill $FPID $MPID $DPID 2>/dev/null
    sleep 0.3
    echo
}

exercise sqlite "$Y" "-" 15210 15211 15212 15213

if [ "$PG_OK" = 1 ]; then
    exercise postgres "$Y" "$PGURI" 15220 15221 15222 15223
else
    skp "PostgreSQL arm (no server on ${PGHOST}:${PGPORT})"
fi

if [ "$FDB_OK" = 1 ]; then
    exercise fdb "$FDBBIN" "$FDBURI" 15230 15231 15232 15233
else
    skp "FoundationDB arm (no cluster at $CLUSTER or no $FDBBIN)"
fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail SKIP=$skip ===================="
[ "$fail" -eq 0 ] && echo "ALL REMOTE GC CHECKS PASSED" || echo "SOME REMOTE GC CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
