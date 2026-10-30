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
#   striping    deterministic cross-server data striping (multi --data)
#   locks       POSIX advisory lock e2e: flock/fcntl/lease/HA-replication
#   freshness   non-strict cross-client push-invalidation visibility e2e
#   disp_fresh  display-coordinate freshness e2e: relocated entry under synth parent
#               converges via enriched push (not TTL); exercises display_targets
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
#   bench   throughput micro-benchmark        (informational; never gates the result)
#
# Each suite is time-boxed; every mount-based suite cleans up its own FUSE mount and
# this script also force-unmounts any leftover mount before starting and on exit.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
SKIP_BENCH=${SKIP_BENCH:-0}

LOGDIR=.build-bin/logs/$(date +%Y%m%d-%H%M%S)
mkdir -p "$LOGDIR"

CANON="build unit posix edge e2e remote errors resilience concurrent metasrv ha striping locks freshness disp_fresh layout fork viewrm migrate import govcheck govtime grovesci mcp apikey mountkey config pg pg_parallel control bench"
if [ "$#" -gt 0 ]; then SEL="$*"; else SEL="$CANON"; fi
[ "$SKIP_BENCH" = "1" ] && SEL=${SEL// bench/}

# validate names
for s in $SEL; do
  case " $CANON " in *" $s "*) ;; *) echo "unknown suite '$s'; valid: $CANON"; exit 2 ;; esac
done
should(){ case " $SEL " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ensure a fresh binary whenever a code suite is selected
NEEDS_BUILD=0
for s in $SEL; do case "$s" in unit|posix|edge|e2e|remote|errors|resilience|concurrent|metasrv|ha|striping|locks|freshness|disp_fresh|layout|fork|viewrm|migrate|import|govcheck|govtime|grovesci|mcp|apikey|mountkey|config|pg|pg_parallel|bench) NEEDS_BUILD=1 ;; esac; done
if [ "$NEEDS_BUILD" = 1 ] && ! should build; then SEL="build $SEL"; fi

MOUNTS=".test/mnt_alice .test/mnt_bob .test/mnt_r .ptest/mnt .etest/mnt .xtest/mnt .xtest/mnt_ro .rtest/mnt_a .rtest/mnt_b .ctest/m1 .ctest/m2 .msrv/mnt_a .msrv/mnt_b .msrv/mnt_bob .msrv/mnt_s1 .msrv/mnt_s2 .ltest/mnt_base .ltest/mnt_a .ltest/mnt_b .ltest/mnt_c .ltest/mnt_d .ltest/mnt_srv .ltest/mnt_srvbase .ltest/mnt_ld .ftest/mnt_base .ftest/mnt_lab .ftest/mnt_srv_lab .ftest/mnt_srv_base .migtest/mnt_src .migtest/mnt_dst .migtest/mnt_lab .migtest/mnt_alpha .migtest/mnt_beta .migtest/mnt_all .govcheck/mnt_base .govcheck/mnt_lens .govcheck/mnt_map .govtime/mnt_base .govtime/mnt_dv .govtime/mnt_al .grovesci/mnt_dr .grovesci/mnt_al .grovesci/mnt_refused .mcptest/mnt .mcptest/mnt_al .itest/mnt .itest/mnt_r .itest/mnt_v .bench/mnt .bt-t1/mnt .bt-t4/mnt .br/mnt .ha/mnt .ha/mnt_sb .striping/mnt .striping/mnt_legacy .sigtest/mnt .locks/m1 .locks/m2 .locks/ha/mnt .fresh/mnt_a .fresh/mnt_b .dfresh/mntA .dfresh/mntB .mktest/mnt_ok .mktest/mnt_any .mktest/mnt_env .mktest/mnt_denied .mktest/mnt_tok .mktest/mnt_dat .mktest/mnt_plain .mktest/mnt_ign .mktest/mnt_fo .cfgtest/mnt1 .cfgtest/mnt2 .cfgtest/mnt3 .cfgtest/mnt4 .cfgtest/mnt_seed .pgtest/mnt_a .pgtest/mnt_b .pgpar/mnt_a .pgpar/mnt_b .pgpar/mnt_c .vrtest/mnt .vrtest/mnt2 .vrtest/mntA"
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
for s in $CANON; do
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
    control)  run_mountsuite control .build-bin/test_control.sh 300 ;;
    bench)  run_bench ;;
  esac
  echo
done

if [ "$stop" = 1 ]; then
  echo "!! build failed — downstream suites skipped"; echo
fi

echo "========================= BOARD =================================="
for s in $CANON; do
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
