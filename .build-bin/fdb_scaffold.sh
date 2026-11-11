#!/usr/bin/env bash
# Shared plumbing for the suites that can re-run against a second metadata
# backend (`LOCKS_BACKEND=fdb`, `LAYOUT_BACKEND=fdb`, ...). The rule these
# helpers exist for is the one P4 established and P5 repeats: an assertion is
# never edited to fit a backend — only HOW the deployment is stood up changes
# (a local `--home` mount becomes a mount of one remote view; a manifest-seeded
# SQLite db becomes a provisioned subspace seeded through the mount).
#
# Sourcing this file changes nothing by itself. A suite calls:
#     BACKEND="${FOO_BACKEND:-sqlite}"
#     . .build-bin/fdb_scaffold.sh
#     fdb_gate                          # skip cleanly / fail under FDB_TEST=1
#     fdb_binary                        # $Y := a binary that links libfdb_c
#     fdb_up <workdir> <view> <meta-listen> <data-listen>
#                                       # subspace + meta-server + data-server
#     fdb_drop_views <meta-dir> <data-dir> <view>...
#                                       # hand the subspaces back at exit
# and then uses $MSARG / $REMOTE / $BIN where it used the bare `grovefs view`
# and a local mount, plus kp()/wk()/mnt_gate()/seed_through_mount().
#
# What each variable means to the suite:
#   $MSARG   `--meta-store fdb://<cluster>` on the SERVER and on the OFFLINE
#            governance commands, and NOTHING on a SQLite run — so every
#            existing command line keeps its exact shape.
#   $REMOTE  `--meta-server H:P --data-servers H:P` for a mount. Empty on SQLite,
#            where a mount reads its own `--home` tree and needs neither.
#   $EARLY   1 when fdb_up already started a meta-server + data-server for the
#            whole run: sections that would otherwise start their own pair skip
#            it, because under `fdb://` the trees are the cluster's and one
#            server pair serving one scratch root is enough (and two roots would
#            split the DEFINITIONS, which stay files — see docs/fdb-keyspace.md).
set -u

: "${BACKEND:=sqlite}"
FDB_HOME="${FDB_HOME:-/data/grovefs/.fdbbin}"
FDB_RUN_DIR="${FDB_RUN_DIR:-/data/grovefs/.fdbtest}"
CLUSTER="${GROVEFS_FDB_CLUSTER_FILE:-$FDB_RUN_DIR/test.cluster}"
FDBURI="fdb://$CLUSTER"
MSARG=""; REMOTE=""; EARLY=0; FDB_META=""; FDB_DATA=""; FDB_MP=""; FDB_DP=""
BIN="${BIN:-$Y}"

# fdb_gate: the cluster is external state, so its absence is a SKIP, not a FAIL —
# the board must stay GREEN on a host that never installed FDB. `FDB_TEST=1` is
# the CI switch that turns the same absence into a failure, because there "no
# cluster" means the harness is broken, not that the backend is untested.
fdb_available(){
    [ -x "$FDB_HOME/fdbcli.x86_64" ] && [ -f "$CLUSTER" ] || return 1
    "$FDB_HOME/fdbcli.x86_64" -C "$CLUSTER" --exec "status minimal" 2>&1 \
        | grep -q "The database is available"
}
fdb_gate(){
    [ "$BACKEND" = fdb ] || return 0
    fdb_available && return 0
    if [ "${FDB_TEST:-0}" = 1 ]; then
        echo "SUMMARY: pass=0 fail=1 SKIP=0 (FDB_TEST=1 but no available cluster at $CLUSTER)"
        exit 1
    fi
    echo "SUMMARY: pass=0 fail=0 SKIP=1 (no FoundationDB cluster; bash .build-bin/fdb_env.sh fetch && ... up)"
    exit 0
}

# fdb_binary: the default release build does not link libfdb_c, and handing an
# `fdb://` URI to THAT binary is the case that must refuse rather than quietly
# serve SQLite — so an fdb run gets its own feature-built binary, never the
# suite's default one.
fdb_binary(){
    [ "$BACKEND" = fdb ] || return 0
    BIN="${GROVEFS_FDB_BIN:-./target/fdb/release/grovefs}"
    if [ ! -x "$BIN" ] || [ -n "${FDB_REBUILD:-}" ]; then
        FDB_CLIENT_LIB_PATH="${FDB_CLIENT_LIB_PATH:-$FDB_HOME/lib}" \
        LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}" \
            cargo build --release --features fdb --target-dir target/fdb > .fdbtest-build.log 2>&1 \
            || { echo "FAIL: cargo build --release --features fdb (see .fdbtest-build.log)"; exit 1; }
    fi
    Y="$BIN"
    export LD_LIBRARY_PATH="$FDB_HOME/lib:${LD_LIBRARY_PATH:-}"
    MSARG="--meta-store $FDBURI"
}

# fdb_up: provision one scratch view (a SUBSPACE — `view create` is the only
# thing that makes a tree here, exactly as on the other backends) and start the
# server pair every mount of the run goes through. The suite seeds the tree
# through its own base mount afterwards, because `init --manifest` writes a
# SQLite db file and a subspace has no file to write.
fdb_up(){ # fdb_up <workdir> <view> <meta-listen> <data-listen>
    [ "$BACKEND" = fdb ] || return 0
    local wd="$1" view="$2" meta="$3" data="$4"
    FDB_META="$meta"; FDB_DATA="$data"
    REMOTE="--meta-server $meta --data-servers $data"
    EARLY=1
    fdb_ports_free "$meta" "$data"
    mkdir -p "$wd/data" "$wd/h1"
    $Y view $MSARG --meta-dir "$wd" create --view "$view" > "$wd/create.log" 2>&1 \
        || { echo "FAIL: view create --meta-store fdb:// (see $wd/create.log)"; tail -5 "$wd/create.log"; exit 1; }
    $Y data-server --listen "$data" --data-dir "$wd/data" > "$wd/data.log" 2>&1 & FDB_DP=$!
    $Y meta-server --listen "$meta" --home "$wd" $MSARG > "$wd/meta.log" 2>&1 & FDB_MP=$!
    # Reap what we started on EVERY exit path, including a suite that `exit 1`s
    # mid-way (an early abort used to leave the pair listening for the NEXT
    # suite on the same ports — see port_free). Chained rather than installed
    # outright, so a suite whose `trap cleanup EXIT` predates this call still
    # runs its own unmounts; a suite that sets its trap later must keep naming
    # $FDB_MP/$FDB_DP in its cleanup, which the layout/govcheck/govtime suites do.
    local prev; prev="$(trap -p EXIT | sed -E "s/^trap -- '(.*)' EXIT$/\1/")"
    if [ -n "$prev" ]; then trap "$prev; fdb_reap" EXIT; else trap fdb_reap EXIT; fi
    port_wait "$meta" || { echo "FAIL: fdb meta-server did not bind"; tail -5 "$wd/meta.log"; exit 1; }
    port_wait "$data" || { echo "FAIL: fdb data-server did not bind"; tail -5 "$wd/data.log"; exit 1; }
    grep -q "metadata backend foundationdb" "$wd/meta.log" \
        || { echo "FAIL: the meta-server did not banner the foundationdb backend"; head -5 "$wd/meta.log"; exit 1; }
}
fdb_reap(){ kp $FDB_MP $FDB_DP; wk $FDB_MP $FDB_DP; }

# fdb_drop_views: hand the cluster back every subspace this run made.
#
# A subspace OUTLIVES the run that created it — that is why every suite here uses a
# per-run name — but "outlives the run" is a property of the storage, not a licence
# to leak one tree per execution. Measured on 2026-11-11: running the four
# parameterized arms once each (layout/locks/govcheck/apikey) left four live
# subspaces (`lt…`, `lk…`, `gc…`, `ak…`, keys 76/19/23/27) plus the shared `refs`
# ledger, i.e. the board was planting exactly the residue `view scan` exists to
# find. Only unmounting is not cleanup: it leaks keys, keeps objects booked, and
# makes the next scan report trees nobody can attribute.
#
# `--data-dir` is the suite's scratch root, so the objects this view alone named are
# freed there; pointing it at a now-deleted root can only report "not found", never
# pretend bytes were reclaimed. A missing name is not an error (a suite that aborted
# before provisioning still runs its trap), and the helper never fails the suite.
#
# A lens/alias DEFINITION over the base refuses the `--base` removal, and the
# definition is a file in the same scratch meta-dir the run owns — so the first
# cleanup of a suite that defines one (layout's `trimphotos`, govcheck's lenses)
# would be refused and, because this helper never fails a suite, would leave the
# whole tree behind while the board stayed green. Measured on 2026-11-11: layout's
# first cleanup printed "had no tree to drop (error: view 'trimphotos' is governed
# …)" and left 76 keys in the cluster. The loop below drops such a definition by
# name and retries, bounded so a refusal this helper cannot interpret still ends.
fdb_drop_views(){ # fdb_drop_views <meta-dir> <data-dir> <view>...
    [ "${BACKEND:-}" = fdb ] || return 0
    local md="${1:-}" dd="${2:-}" v out rc def attempt
    [ -n "$md" ] || return 0
    shift 2 || true
    for v in "$@"; do
        [ -n "$v" ] || continue
        mkdir -p "$md"
        attempt=0
        while :; do
            if [ -n "$dd" ] && [ -d "$dd" ]; then
                out=$($Y view $MSARG --meta-dir "$md" rm --name "$v" --base --data-dir "$dd" 2>&1); rc=$?
            else
                out=$($Y view $MSARG --meta-dir "$md" rm --name "$v" --base 2>&1); rc=$?
            fi
            [ $rc -eq 0 ] && break
            def=$(printf '%s' "$out" | sed -n "s/^error: view '\([^']*\)' is governed by a definition.*/\1/p")
            [ -n "$def" ] && [ $attempt -lt 8 ] || break
            attempt=$((attempt + 1))
            $Y view $MSARG --meta-dir "$md" rm --name "$def" >/dev/null 2>&1 || break
            echo "  cluster: removed definition '$def' over '$v' before dropping the tree"
        done
        if [ $rc -eq 0 ]; then
            echo "  cluster: dropped subspace '$v'"
            printf '%s\n' "$out" | grep -E "key\(s\) gone|object\(s\)" | sed 's/^/    /'
        else
            echo "  cluster: '$v' had no tree to drop ($(printf '%s' "$out" | head -1))"
        fi
    done
    return 0
}

# --- small shared helpers ---------------------------------------------------
# A mount that never came up must abort, not let the following writes land in a
# bare directory and print green numbers about nothing.
mnt_gate(){ local mp="$PWD/$1"; for _ in $(seq 1 60); do
    findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }
port_wait(){ local a=$1; for _ in $(seq 1 100); do
    (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
# port_free: the OPPOSITE question, asked BEFORE a server is started. `port_wait`
# only proves SOMETHING answers the address, so a listener left behind by another
# (or an earlier, crashed) run makes this suite talk to a stranger: it passed its
# "servers up" gate, mounted an empty tree and printed five red assertions taken
# from the wrong deployment. A busy port is therefore a hard failure that names
# the holder, never a silent adoption.
port_free(){ local a=$1; ! (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null; }
fdb_ports_free(){ # fdb_ports_free <meta-listen> <data-listen>
    local a
    for a in "$@"; do
        port_free "$a" && continue
        echo "FAIL: port $a is already listening; fdb_up would have mounted a stranger's deployment"
        ss -ltnp 2>/dev/null | grep -F "$a" || true
        exit 1
    done
}
# kp/wk: never hand `kill` an empty or zero placeholder. PID 0 means the CALLER'S
# WHOLE PROCESS GROUP, so a parameterized suite whose server pids stay unset on
# one backend would signal the harness that launched it — and print a green
# SUMMARY on its way out with exit 143 (this ate a real debugging hour).
kp(){ local p; for p in "$@"; do case "$p" in ''|0|*[!0-9]*) continue ;; esac; kill "$p" 2>/dev/null || true; done; }
wk(){ local p; for p in "$@"; do case "$p" in ''|0|*[!0-9]*) continue ;; esac; wait "$p" 2>/dev/null || true; done; }
# seed_through_mount: apply a `grovefs` manifest (the `mkdir X` / `touch Y` lines
# a SQLite `init --manifest` consumes) through a live mount, so both backends end
# up with the SAME names without the suite maintaining two descriptions of them.
seed_through_mount(){ # seed_through_mount <mountpoint> <manifest>
    local m="$1" f="$2" op path
    while read -r op path; do
        [ -n "${op:-}" ] || continue
        case "$op" in
            mkdir) timeout 10 mkdir -p "$m/$path" || return 1 ;;
            touch) timeout 10 sh -c "printf '' > '$m/$path'" || return 1 ;;
            *)     return 2 ;;
        esac
    done < "$f"
}
