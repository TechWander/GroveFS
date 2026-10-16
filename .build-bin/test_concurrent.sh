#!/usr/bin/env bash
# Cross-process METADATA CONSISTENCY test: the *same* user mounted by two
# processes against ONE shared view database (SQLite WAL). This is the "many
# clients, central store" scenario collapsed onto one machine so it is
# deterministic. It validates the hardening in meta.rs:
#   - PRAGMA busy_timeout: concurrent writers WAIT for the write lock instead
#     of erroring out with SQLITE_BUSY (which would surface to FUSE as EIO).
#   - IMMEDIATE transactions on unlink/rename: the read-modify-write (delete ->
#     refcount for GC) is atomic, so hard links unlinked from two mounts at once
#     are GC'd exactly once and no half-applied state is observable.
#   - Final PRAGMA integrity_check == ok.
# Assertion-based (pass/fail counters + real exit code) like the other suites.
set -u
cd /data/xy/yourfs
# Force English coreutils messages so the EEXIST/EIO text assertions below are
# locale-independent (this host's default locale is Chinese).
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/yourfs/.build-bin/pkg-config
Y=./target/release/grove

rm -rf .ctest; mkdir -p .ctest/home .ctest/m1 .ctest/m2
export GROVEFS_HOME=$PWD/.ctest/home
DB=.ctest/home/meta/shared.db
BLK=.ctest/home/blocks

# a minimal view; both mounts open the *same* shared.db
cat > .ctest/shared.manifest <<'EOF'
mkdir seed
EOF
$Y init --user shared --manifest .ctest/shared.manifest >/dev/null

$Y mount --user shared --mountpoint .ctest/m1 > .ctest/m1.log 2>&1 &
P1=$!
$Y mount --user shared --mountpoint .ctest/m2 > .ctest/m2.log 2>&1 &
P2=$!
sleep 1.2

cleanup(){ fusermount -u .ctest/m1 2>/dev/null; fusermount -u .ctest/m2 2>/dev/null; kill $P1 $P2 2>/dev/null; }
trap cleanup EXIT

M1=.ctest/m1
M2=.ctest/m2
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 10 "$@"; }

if ! command -v python3 >/dev/null 2>&1; then
  echo "SKIP: python3 not available for integrity_check"; exit 0
fi

# ---------------------------------------------------------------------------
echo "===== 1) concurrent distinct mkdirs from two mounts (busy_timeout) ====="
N=80
dist(){ # $1=mount $2=prefix  -> prints number of failed mkdirs
  local m=$1 p=$2 f=0 i
  for i in $(seq 1 $N); do mkdir "$m/${p}_$i" 2>/dev/null || f=$((f+1)); done
  echo $f
}
dist "$M1" aa > .ctest/f1 &
D1=$!
dist "$M2" bb > .ctest/f2 &
D2=$!
wait $D1; wait $D2
f1=$(cat .ctest/f1); f2=$(cat .ctest/f2)
chk "$((f1+f2))" "0" "no write lost to SQLITE_BUSY/EIO ($f1+$f2 failures)"
c1=$(T ls "$M1" | grep -cE '^(aa|bb)_[0-9]+$')
c2=$(T ls "$M2" | grep -cE '^(aa|bb)_[0-9]+$')
chk "$c1" "$((2*N))" "mount1 sees all $((2*N)) dirs (both writers)"
chk "$c2" "$((2*N))" "mount2 sees all $((2*N)) dirs (shared view, live readdir)"

# ---------------------------------------------------------------------------
echo "===== 2) same-name mkdir race: exactly one wins, loser gets EEXIST not EIO ====="
K=40
race(){ # $1=mount $2=outfile
  local m=$1 o=$2 i
  : > "$o"
  for i in $(seq 1 $K); do
    if err=$(mkdir "$m/r_$i" 2>&1); then echo "OK" >> "$o"; else echo "ERR:$err" >> "$o"; fi
  done
}
race "$M1" .ctest/ra &
R1=$!
race "$M2" .ctest/rb &
R2=$!
wait $R1; wait $R2
cat .ctest/ra .ctest/rb > .ctest/rall
succ=$(grep -c '^OK' .ctest/rall)
eexist=$(grep -c 'File exists' .ctest/rall)
eio=$(grep -icE 'input/output|i/o error|resource busy|resource temporarily' .ctest/rall)
present=$(T ls "$M1" | grep -cE '^r_[0-9]+$')
chk "$present" "$K" "$K racing names each exist exactly once"
chk "$succ" "$K" "$K mkdirs succeeded (one winner per name)"
chk "$eexist" "$K" "$K mkdirs got EEXIST (not a spurious error)"
chk "$eio" "0" "no EIO/EBUSY from write contention"

# ---------------------------------------------------------------------------
echo "===== 3) concurrent cross-mount hardlink unlink -> object GC exactly once ====="
printf 'shared-bytes' > "$M1/h.txt"
ln "$M1/h.txt" "$M1/h2.txt"            # nlink == 2, one data object
hbefore=$(T ls "$BLK" | wc -l)
( rm "$M1/h.txt" & rm "$M2/h2.txt" & wait )
sleep 0.3
hafter=$(T ls "$BLK" | wc -l)
gone=0
[ -e "$M1/h.txt" ] && gone=1
[ -e "$M2/h2.txt" ] && gone=1
chk "$gone" "0" "both hardlink names removed"
chk "$((hafter))" "$((hbefore-1))" "exactly one block GC'd from concurrent unlinks ($hbefore -> $hafter)"

# ---------------------------------------------------------------------------
echo "===== 4) concurrent rename churn then DB integrity ====="
T mkdir "$M1/box1" "$M2/box2"
( for i in $(seq 1 30); do mv "$M1/box1" "$M2/box1" 2>/dev/null; mv "$M2/box1" "$M1/box1" 2>/dev/null; done ) &
X1=$!
( for i in $(seq 1 30); do mv "$M2/box2" "$M1/box2" 2>/dev/null; mv "$M1/box2" "$M2/box2" 2>/dev/null; done ) &
X2=$!
wait $X1; wait $X2
# after churn both boxes must still exist exactly once (rename is atomic)
chk "$(T ls "$M1" | grep -cx box1)" "1" "box1 present exactly once after rename churn"
chk "$(T ls "$M1" | grep -cx box2)" "1" "box2 present exactly once after rename churn"

integ=$(python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute("PRAGMA integrity_check").fetchone()[0]); c.close()' "$DB" 2>&1)
chk "$integ" "ok" "PRAGMA integrity_check == ok after all contention"

# node count sanity: shared.db readable by a 3rd process, root + our entries present
rows=$(python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute("select count(*) from node").fetchone()[0]); c.close()' "$DB" 2>&1)
if [ "$rows" -gt 0 ] 2>/dev/null; then ok "external reader sees $rows nodes (WAL cross-process read)"; else bad "external reader failed: $rows"; fi

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL CONCURRENCY CHECKS PASSED" || echo "SOME CONCURRENCY CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
