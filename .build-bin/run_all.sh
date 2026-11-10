#!/usr/bin/env bash
# Unified test entry for grovefs.
#
#   bash .build-bin/run_all.sh                 # full board: build + unit + all suites + bench
#   bash .build-bin/run_all.sh unit e2e        # only the named suites (build auto-prepended)
#   SKIP_BENCH=1 bash .build-bin/run_all.sh    # full board but skip the throughput bench
#
# Suites (canonical order):
#   build   cargo build --release              (gates everything; short-circuits on fail)
#   unit    cargo test --release (meta/proto/remote integration)
#   posix   symlink/hardlink/statfs/setattr/chmod/mknod completeness
#   edge    boundary + concurrency correctness (writeback path)
#   e2e     multi-user view isolation + read/write e2e
#   remote  remote (no-NFS) TCP data path e2e
#   errors  errno correctness (ENOTDIR/EISDIR/EEXIST/ENOENT/EINVAL) + read-only EROFS
#   resilience  remote data-server restart -> transparent client reconnect
#   concurrent  same-user multi-process metadata consistency (WAL busy_timeout + tx GC)
#   metasrv     central meta-server: one authoritative view shared across clients
#   ha          meta-server primary/standby: replication + failover + event relay
#   striping    deterministic cross-server data striping (multi --data-servers)
#   locks       POSIX advisory lock e2e: flock/fcntl/lease/HA-replication
#   freshness   non-strict cross-client push-invalidation visibility e2e
#   disp_fresh  display-coordinate freshness e2e: relocated entry under synth parent
#               converges via enriched push (not TTL); exercises display_targets
#   layout      presentation overlay e2e (`mount --layout`): one stored view, a
#               different lens per client — hide/only/map/link, write-through to
#               the real entries, lens-side deletes, the synthesized-dir guard.
#               `LAYOUT_BACKEND=fdb` re-runs every assertion against an
#               FDB-backed meta-server instead of the local SQLite tree
#   import      zero-copy legacy onboarding e2e (hardlink + incremental)
#   fork        fork views e2e: an independent metadata tree seeded from a physical
#               base — structure writable, content EROFS, deletes only the fork's
#               names, shared bytes via the cross-view ledger, local + meta-server
#               mounts, `view list` (fork + bare base) and `apikey list --view`
#   migrate     metadata migration e2e: `view export`/`view import` moves ONE
#               view's tree as host-neutral JSONL to a different store path
#               (blocks/ copied separately) — ids/inodes/hard links/mode/owner
#               survive, the ledger is rebuilt, import is atomic (a failed
#               --replace keeps the old tree), `--as-fork` restores fork
#               semantics, and several base views stay independent
#   govcheck    governance counterexample study (paper Sec. 7.3): overlay/symlink
#               vs GroveFS identity-preservation assertions I1-I4 (overlay self-skips if unmountable)
#   govtime     governance convergence timing (paper Sec. 7.4): hot-reload / push-inval / --retire latencies
#   grovesci    astronomy "non-landing data" end-to-end case (paper Sec. 7): zero-copy
#               import + read-only derived data-release + revocable public alias
#   mcp         `grovefs mcp`: AI-agent entrance onto view governance over stdio
#               JSON-RPC (inline-rule lenses, alias rotation, CLI-identical
#               contract, protocol error paths)
#   apikey      `grovefs apikey` + `grovefs mcp --require-api-key`: API-key auth
#               (hashed at rest) + per-call audit log — gate/admit/revoke/env
#               pre-auth and the opt-in default (a plain `grovefs mcp` stays open)
#   mountkey    revocable per-caller MOUNT credentials: `--require-api-key` on the
#               meta/data planes + `mount --api-key` — scoped minting (--for
#               mount|mcp|any), connect auditing, refusal reasons on the wire, a
#               revoke killing a LIVE mount (clean unmount, no stale endpoint),
#               gate composition (token AND key) and the untouched opt-in default
#   config      auto-loaded KEY=VALUE config file + env fallbacks: the flag > env >
#               file > default chain (GROVEFS_HOME / META / DATA / TOKEN /
#               DB_PATH), parser edge cases, and a real FUSE mount driven
#               entirely from the file (no coordinates on the command line)
#   pg          PostgreSQL metadata backend e2e (self-skips if PG unreachable)
#   pg_parallel PostgreSQL SERIALIZABLE/SSI parallel-write e2e (self-skips if
#               PG unreachable); drives three concurrent FUSE clients against
#               one PG-backed meta-server and asserts parallelism + contention
#               correctness (paper Sec. 5 I3 + Sec. 9 scalability boundary)
#   evict       idle-engine eviction e2e (`meta-server --engine-idle-secs`): N
#               mounted views hold N engines, the sweeper retires an engine once
#               its last session closes, and a remount serves the same tree.
#               PG-backed (it measures the connection wall); self-skips without PG
#   control     control-plane unification e2e: under `--meta-store postgres://…`
#               view definitions AND api keys live in `grovefs_control`, nothing
#               lands in the meta-server's local dir, and M_OPEN resolves both out
#               of PostgreSQL. Self-skips without PG
#
#   gc          offline orphan-object audit / reclaim (`grovefs gc`): ledger-held
#               vs orphan, grace window, live-mount and wrong-root guards, --json
#               (SQLite stores only; a PG deployment is refused)
#   viewrm      whole base view removal e2e (`view rm --base`): the guard set
#               (live local mount, governed name, base still named by an
#               alias/derived/fork definition, missing tree, PG deployment), tree
#               +WAL removal, ledger rebuild, and shared bytes surviving for the
#               fork that still names them
#   bwlimit     per-mount bandwidth shaping e2e: `--bw-read`/`--bw-write` pin a
#               24 MiB transfer to its floor with no byte lost, the unlimited
#               mount of the same view stays materially faster,
#               GROVEFS_BW_READ/GROVEFS_BW_WRITE env fallback
#   bench   throughput micro-benchmark        (informational; never gates the result)
#
# Experimental — selected by name only, never part of the gating board:
#   fdb         FoundationDB metadata backend e2e (`--meta-store fdb://<cluster
#               file>`): subspace provisioning, the tree body, the lock surface,
#               derived/alias governance and an explicit fork verdict, the two-way
#               `view export`/`view import` migration chain (ids/inodes kept), and
#               what `--engine-idle-secs` means on this backend. Registers SKIP
#               when no cluster is reachable; FDB_TEST=1 turns that absence into
#               a FAIL (CI). OUT OF CANON because the G3 throughput gate measured
#               fail on this host: best cell 1601 ops/s (RF=3 `ssd`, four repeats;
#               most cells 133-998) against a 4.6k ops/s bar, while the PG arm run in the same session reached
#               4.1k-4.9k ops/s (and 1642 ops/s even with the disk taken out of the
#               commit path, so the shortfall is not only media). The functional
#               verdict stands, the capacity verdict does not. Run it with:
#                 bash .build-bin/run_all.sh fdb
#
# Each suite is time-boxed; every mount-based suite cleans up its own FUSE mount and
# this script also force-unmounts any leftover mount before starting and on exit.
set -u
cd "$(dirname "$0")/.."
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
SKIP_BENCH=${SKIP_BENCH:-0}

LOGDIR=.build-bin/logs/$(date +%Y%m%d-%H%M%S)
mkdir -p "$LOGDIR"

# `fdb` is NOT in CANON: the G3 performance gate (docs/fdb-keyspace.md §15) measured
# fail on 2026-11-09 — best cell 1601 ops/s (RF=3 `ssd`, four repeats, most cells
# 133-998) against a 4.6k ops/s bar, same host and same op sequence as the PG arm,
# which reached 4.1k-4.9k in the same session; and 1642 ops/s even with the disk
# taken out of the commit path, so the gap is not only media. Matrix: §14 v1.8.
# Per the plan's R3 disposition the backend stays a default-off experiment: reachable
# by name, never part of the gating board, and promised nowhere in the user docs.
CANON="build unit posix edge e2e remote errors resilience concurrent metasrv ha striping locks freshness disp_fresh layout fork viewrm gc bwlimit migrate import govcheck govtime grovesci mcp apikey mountkey config pg pg_parallel evict control bench"
# EXPERIMENTAL suites gate nothing but can still be selected explicitly.
EXPERIMENTAL="fdb"
if [ "$#" -gt 0 ]; then SEL="$*"; else SEL="$CANON"; fi
[ "$SKIP_BENCH" = "1" ] && SEL=${SEL// bench/}

# validate names
for s in $SEL; do
  case " $CANON $EXPERIMENTAL " in *" $s "*) ;; *) echo "unknown suite '$s'; valid: $CANON $EXPERIMENTAL"; exit 2 ;; esac
done
should(){ case " $SEL " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ensure a fresh binary whenever a code suite is selected
NEEDS_BUILD=0
for s in $SEL; do case "$s" in unit|posix|edge|e2e|remote|errors|resilience|concurrent|metasrv|ha|striping|locks|freshness|disp_fresh|layout|fork|viewrm|gc|bwlimit|migrate|import|govcheck|govtime|grovesci|mcp|apikey|mountkey|config|pg|pg_parallel|fdb|bench) NEEDS_BUILD=1 ;; esac; done
if [ "$NEEDS_BUILD" = 1 ] && ! should build; then SEL="build $SEL"; fi

MOUNTS=".test/mnt_alice .test/mnt_bob .test/mnt_r .ptest/mnt .etest/mnt .xtest/mnt .xtest/mnt_ro .rtest/mnt_a .rtest/mnt_b .ctest/m1 .ctest/m2 .msrv/mnt_a .msrv/mnt_b .msrv/mnt_bob .msrv/mnt_s1 .msrv/mnt_s2 .ltest/mnt_base .ltest/mnt_a .ltest/mnt_b .ltest/mnt_c .ltest/mnt_d .ltest/mnt_srv .ltest/mnt_srvbase .ltest/mnt_ld .ftest/mnt_base .ftest/mnt_lab .ftest/mnt_srv_lab .ftest/mnt_srv_base .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab .migtest/mnt_alpha .migtest/mnt_beta .migtest/mnt_all .govcheck/mnt_base .govcheck/mnt_lens .govcheck/mnt_map .govtime/mnt_base .govtime/mnt_dv .govtime/mnt_al .grovesci/mnt_dr .grovesci/mnt_al .grovesci/mnt_refused .mcptest/mnt .mcptest/mnt_al .itest/mnt .itest/mnt_r .itest/mnt_v .bench/mnt .bt-t1/mnt .bt-t4/mnt .br/mnt .ha/mnt .ha/mnt_sb .striping/mnt .striping/mnt_legacy .sigtest/mnt .locks/m1 .locks/m2 .locks/ha/mnt .fresh/mnt_a .fresh/mnt_b .dfresh/mntA .dfresh/mntB .mktest/mnt_ok .mktest/mnt_any .mktest/mnt_env .mktest/mnt_denied .mktest/mnt_tok .mktest/mnt_dat .mktest/mnt_plain .mktest/mnt_ign .mktest/mnt_fo .cfgtest/mnt1 .cfgtest/mnt2 .cfgtest/mnt3 .cfgtest/mnt4 .cfgtest/mnt_seed .pgtest/mnt_a .pgtest/mnt_b .pgpar/mnt_a .pgpar/mnt_b .pgpar/mnt_c .vrtest/mnt .vrtest/mnt2 .vrtest/mntA .gctest/mnt .gctest/mnt2 .gctest/mnt3 .bwtest/mnt_cap .bwtest/mnt_free .bwtest/mnt_env .bwtest/mnt_ro"
umount_all(){ for d in $MOUNTS; do fusermount -u "$d" 2>/dev/null || true; done; }
umount_all
trap umount_all EXIT

overall=0
declare -A RESULT
record(){ # $1=suite $2=PASS|FAIL|TIMEOUT|INFO|SKIP
  RESULT[$1]="$2"
  if [ "$2" != PASS ] && [ "$2" != INFO ]; then overall=1; fi
}

run_build(){
  echo "### build — cargo build --release"
  if timeout 900 cargo build --release >"$LOGDIR/build.log" 2>&1; then
    echo "  OK"; record build PASS; return 0
  else
    echo "  FAILED (see $LOGDIR/build.log)"; tail -25 "$LOGDIR/build.log"
    record build FAIL; return 1
  fi
}

run_unit(){
  echo "### unit — cargo test --release"
  if timeout 900 cargo test --release >"$LOGDIR/unit.log" 2>&1; then
    grep -E 'test result:' "$LOGDIR/unit.log" | sed 's/^/  /'
    record unit PASS
  else
    echo "  FAILED (see $LOGDIR/unit.log)"
    grep -E 'test result:|error(\[|:)|panicked|FAILED' "$LOGDIR/unit.log" | tail -25 | sed 's/^/  /'
    record unit FAIL
  fi
}

run_mountsuite(){ # $1=key $2=script $3=timeout
  local key="$1" script="$2" to="$3" log="$LOGDIR/$1.log" rc sum
  echo "### $key — bash $script (<= ${to}s)"
  # Each suite owns its servers, but a suite that died early can leak one; a
  # stale listener would then turn the NEXT suite into a spurious AddrInUse
  # failure (green code, red board). Reap leaked test servers before every run.
  pkill -f '\./target/release/grovefs meta-server' 2>/dev/null || true
  pkill -f '\./target/release/grovefs data-server' 2>/dev/null || true
  # The FDB suites drive a SEPARATE binary (./target/fdb/release/grovefs, because
  # only that one links libfdb_c), so a pattern naming target/release alone let a
  # leaked fdb-backed pair sit on its port and be adopted by the next suite --
  # which is how `grovesci` once printed five red assertions about nothing.
  pkill -f '\./target/fdb/release/grovefs meta-server' 2>/dev/null || true
  pkill -f '\./target/fdb/release/grovefs data-server' 2>/dev/null || true
  sleep 0.2
  if timeout "$to" bash "$script" >"$log" 2>&1; then rc=0; else rc=$?; fi
  sum=$(grep -E 'SUMMARY: pass=' "$log" | tail -1)
  if [ "$rc" -eq 0 ]; then
    echo "  OK :: ${sum:-completed}"; record "$key" PASS
  elif [ "$rc" -eq 124 ]; then
    echo "  TIMEOUT after ${to}s (see $log)"; tail -12 "$log" | sed 's/^/  /'; record "$key" TIMEOUT
  else
    echo "  FAILED rc=$rc :: ${sum:-no-summary} (see $log)"; tail -20 "$log" | sed 's/^/  /'; record "$key" FAIL
  fi
}

# run_fdb — the FoundationDB backend suite, gated on EXTERNAL infrastructure.
# The cluster is not part of the build, so its absence is a SKIP that must not
# gate the board (R5: a host without FDB still goes GREEN). `test_fdb.sh` signals
# that case by exiting 0 with `SKIP=1` in its SUMMARY; a SKIP is therefore
# registered straight into RESULT, bypassing record()'s "anything but PASS gates"
# rule. `FDB_TEST=1` flips the same absence to exit 1, i.e. a real FAIL for CI.
run_fdb(){
  local log="$LOGDIR/fdb.log" rc sum
  echo "### fdb — bash .build-bin/test_fdb.sh (<= ${FDB_TO:-420}s)"
  pkill -f '\./target/fdb/release/grovefs meta-server' 2>/dev/null || true
  pkill -f '\./target/fdb/release/grovefs data-server' 2>/dev/null || true
  if timeout "${FDB_TO:-420}" bash .build-bin/test_fdb.sh >"$log" 2>&1; then rc=0; else rc=$?; fi
  sum=$(grep -E 'SUMMARY: pass=' "$log" | tail -1)
  if [ "$rc" -eq 0 ]; then
    case "$sum" in
      *"SKIP=1"*) echo "  SKIP (no FoundationDB cluster) :: ${sum:-no-summary}"; RESULT[fdb]=SKIP ;;
      *) echo "  OK :: ${sum:-completed}"; record fdb PASS ;;
    esac
  elif [ "$rc" -eq 124 ]; then
    echo "  TIMEOUT after ${FDB_TO:-420}s (see $log)"; tail -12 "$log" | sed 's/^/  /'; record fdb TIMEOUT
  else
    echo "  FAILED rc=$rc :: ${sum:-no-summary} (see $log)"; tail -20 "$log" | sed 's/^/  /'; record fdb FAIL
  fi
}

run_bench(){
  echo "### bench — throughput micro-benchmark (informational)"
  local log="$LOGDIR/bench.log" rc
  timeout 900 bash .build-bin/bench.sh >"$log" 2>&1; rc=$?
  if [ "$rc" -eq 0 ]; then
    grep -E 'MB/s' "$log" | sed 's/^/  /'
    echo "  (full log: $log)"
  else
    echo "  bench rc=$rc (see $log)"; tail -10 "$log" | sed 's/^/  /'
  fi
  record bench INFO   # never gates
}

echo "=================================================================="
echo " grovefs test board   ($(date '+%Y-%m-%d %H:%M:%S'))"
echo " suites: $SEL"
echo " logs:   $LOGDIR"
echo "=================================================================="

stop=0
# $CANON $EXPERIMENTAL: iterating CANON alone would make `run_all.sh fdb` a no-op
# that still prints SKIP — the experimental arm is selectable precisely by name.
for s in $CANON $EXPERIMENTAL; do
  should "$s" || { RESULT[$s]=SKIP; continue; }
  if [ "$stop" = 1 ]; then RESULT[$s]=SKIP; continue; fi
  case "$s" in
    build)  run_build  || stop=1 ;;
    unit)   run_unit ;;
    posix)  run_mountsuite posix  .build-bin/test_posix.sh  240 ;;
    edge)   run_mountsuite edge   .build-bin/test_edge.sh   300 ;;
    e2e)    run_mountsuite e2e    .build-bin/test_e2e.sh    240 ;;
    remote) run_mountsuite remote .build-bin/test_remote.sh 300 ;;
    errors) run_mountsuite errors .build-bin/test_errors.sh 180 ;;
    resilience) run_mountsuite resilience .build-bin/test_resilience.sh 240 ;;
    concurrent) run_mountsuite concurrent .build-bin/test_concurrent.sh 300 ;;
    metasrv)  run_mountsuite metasrv .build-bin/test_metasrv.sh 240 ;;
    ha)       run_mountsuite ha .build-bin/test_ha.sh 240 ;;
    striping) run_mountsuite striping .build-bin/test_striping.sh 240 ;;
    locks)    run_mountsuite locks   .build-bin/test_locks.sh   240 ;;
    freshness) run_mountsuite freshness .build-bin/test_freshness.sh 240 ;;
    disp_fresh) run_mountsuite disp_fresh .build-bin/test_disp_fresh.sh 240 ;;
    layout)   run_mountsuite layout .build-bin/test_layout.sh 240 ;;
    fork)     run_mountsuite fork   .build-bin/test_fork.sh   300 ;;
    viewrm)   run_mountsuite viewrm  .build-bin/test_viewrm.sh 300 ;;
    gc)       run_mountsuite gc     .build-bin/test_gc.sh     240 ;;
    bwlimit)  run_mountsuite bwlimit .build-bin/test_bwlimit.sh 300 ;;
    migrate)  run_mountsuite migrate .build-bin/test_migrate.sh 300 ;;
    import)   run_mountsuite import   .build-bin/test_import.sh   240 ;;
    govcheck) run_mountsuite govcheck .build-bin/test_govcheck.sh 240 ;;
    govtime)  run_mountsuite govtime  .build-bin/test_govtime.sh  240 ;;
    grovesci) run_mountsuite grovesci .build-bin/test_grovesci.sh 240 ;;
    mcp)      run_mountsuite mcp .build-bin/test_mcp.sh 240 ;;
    apikey)   run_mountsuite apikey .build-bin/test_apikey.sh 180 ;;
    mountkey) run_mountsuite mountkey .build-bin/test_mountkey.sh 420 ;;
    config)   run_mountsuite config .build-bin/test_config.sh 240 ;;
    pg)       run_mountsuite pg .build-bin/test_pg.sh 240 ;;
    pg_parallel) run_mountsuite pg_parallel .build-bin/test_pg_parallel.sh 300 ;;
    evict)    run_mountsuite evict .build-bin/test_engine_evict.sh 300 ;;
    control)  run_mountsuite control .build-bin/test_control.sh 300 ;;
    fdb)      run_fdb ;;
    bench)  run_bench ;;
  esac
  echo
done

if [ "$stop" = 1 ]; then
  echo "!! build failed — downstream suites skipped"; echo
fi

echo "========================= BOARD =================================="
for s in $CANON $EXPERIMENTAL; do
  should "$s" || continue
  printf "  %-7s %s\n" "$s" "${RESULT[$s]:-SKIP}"
done
echo "================================================================"
if [ "$overall" -eq 0 ]; then
  echo "RESULT: GREEN (all gating suites passed)"
else
  echo "RESULT: RED (see failures above; logs in $LOGDIR)"
fi
umount_all
exit $overall
