#!/usr/bin/env bash
# CONFIG-FILE & ENV-FALLBACK e2e: the "long commands shrink" surface.
#   (1) a KEY=VALUE file is auto-loaded into the env ($GROVEFS_CONFIG >
#       ~/.config/grovefs/grovefs.conf) without
#       clobbering live env — the chain is flag > env > file > default;
#   (2) the repeat-heavy knobs resolve from it: GROVEFS_HOME (init/mount),
#       GROVEFS_META / GROVEFS_DATA / GROVEFS_TOKEN (mount + servers),
#       GROVEFS_DB_PATH (meta-server);
#   (3) parser edge cases: comments, `export` lines, quoted values, non-
#       GROVEFS_* keys (warn + ignore), malformed lines (warn), an unreadable
#       explicit $GROVEFS_CONFIG (warn + proceed with env/defaults);
#   (4) a real FUSE mount with NO --meta/--data/--token on the command line —
#       only --view and --mountpoint — served entirely from the file.
# Assertion-based (pass/fail counters + a real exit code) like the other
# suites; emits a SUMMARY line run_all reads.
set -u
cd /data/xy/grovefs
export LC_ALL=C LANG=C
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs

META=127.0.0.1:15082
DATA=127.0.0.1:15083
TOK=cfgsecret
rm -rf .cfgtest
mkdir -p .cfgtest/homeA .cfgtest/homeB .cfgtest/homeC .cfgtest/default_home \
    .cfgtest/central .cfgtest/srv_db .cfgtest/dbD \
    .cfgtest/mnt1 .cfgtest/mnt2 .cfgtest/mnt3 .cfgtest/mnt4

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 8 "$@"; }
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
# a mountpoint dir EXISTS before FUSE attaches (manifest-created dirs make a bare
# `[ -d ]` race), so readiness must check the kernel mount table instead
ismnt(){ grep -q " $(realpath "$1" 2>/dev/null) " /proc/self/mounts 2>/dev/null; }
SIDS=""
srv(){ SIDS="$SIDS $1"; }
cleanup(){ for d in .cfgtest/mnt1 .cfgtest/mnt2 .cfgtest/mnt3 .cfgtest/mnt4 .cfgtest/mnt_seed; do fusermount -u "$d" 2>/dev/null; done; kill $SIDS 2>/dev/null; }
trap cleanup EXIT

cat > .cfgtest/team.manifest <<'EOF'
mkdir docs
touch docs/plan.md
EOF
# seed a view in the central store the token-gated servers will serve
$Y --home .cfgtest/central init --view team --manifest .cfgtest/team.manifest > .cfgtest/seed.log 2>&1
# give the seeded file content through a short-lived LOCAL mount (the store
# itself keeps bytes only under blocks/, no plain paths to write into)
mkdir -p .cfgtest/mnt_seed
$Y --home .cfgtest/central mount --view team --mountpoint .cfgtest/mnt_seed --foreground > .cfgtest/seed_mnt.log 2>&1 &
SEEDP=$!
for _ in $(seq 1 60); do ismnt .cfgtest/mnt_seed && break; sleep 0.25; done
ismnt .cfgtest/mnt_seed || bad "seed mount never attached"
echo "planning text" > .cfgtest/mnt_seed/docs/plan.md 2>/dev/null || true
# read back THROUGH the mount before detaching, and prove the bytes reached the
# store after unmount — otherwise the remote mount below reads an empty seed
chk "$(cat .cfgtest/mnt_seed/docs/plan.md 2>/dev/null)" "planning text" "seed write visible through the mount"
fusermount -u .cfgtest/mnt_seed 2>/dev/null; kill $SEEDP 2>/dev/null; wait $SEEDP 2>/dev/null
grep -rqs "planning text" .cfgtest/central/blocks/ && ok "seed bytes landed in the central store" || bad "central blocks/ has no seed content"

# ---- config files (one per scenario; $GROVEFS_CONFIG points at the active one) ----
cat > .cfgtest/c_home.conf <<EOF
GROVEFS_HOME=$PWD/.cfgtest/homeA
EOF
cat > .cfgtest/c_env.conf <<EOF
# a comment line
export GROVEFS_HOME=$PWD/.cfgtest/ignored_by_env
BOGUS_KEY=should_be_ignored
BAD LINE WITHOUT EQUALS
EOF
cat > .cfgtest/c_mount.conf <<EOF
GROVEFS_META=$META
GROVEFS_DATA=$DATA
GROVEFS_TOKEN=$TOK
EOF
cat > .cfgtest/c_envtok.conf <<EOF
GROVEFS_META=$META
GROVEFS_DATA=$DATA
GROVEFS_TOKEN=WRONGTOKEN_from_file
EOF
cat > .cfgtest/c_quoted.conf <<EOF
GROVEFS_HOME="$PWD/.cfgtest/homeC"
EOF
cat > .cfgtest/c_badtok.conf <<EOF
GROVEFS_META=$META
GROVEFS_DATA=$DATA
GROVEFS_TOKEN=WRONGTOKEN
EOF
cat > .cfgtest/c_srv.conf <<EOF
GROVEFS_TOKEN=$TOK
GROVEFS_DB_PATH=$PWD/.cfgtest/srv_db
EOF

echo "===== 1) GROVEFS_HOME: file < env < flag ====="
out=$(GROVEFS_CONFIG=$PWD/.cfgtest/c_home.conf T $Y init --view h_file 2>&1); rc=$?
chk "$rc" 0 "file-only init exits 0"
[ -f .cfgtest/homeA/meta/h_file.db ] && ok "config GROVEFS_HOME honored (init landed in homeA)" || bad "homeA/meta/h_file.db missing"

out=$(GROVEFS_HOME=$PWD/.cfgtest/homeB GROVEFS_CONFIG=$PWD/.cfgtest/c_home.conf T $Y init --view h_env 2>&1); rc=$?
chk "$rc" 0 "env-vs-file init exits 0"
[ -f .cfgtest/homeB/meta/h_env.db ] && ok "live env beats the config file" || bad "homeB/meta/h_env.db missing"
[ -f .cfgtest/homeA/meta/h_env.db ] && bad "file clobbered the env (homeA got h_env.db)" || ok "file only filled a default (homeA untouched)"

T $Y --home .cfgtest/homeC init --view h_flag > /dev/null 2>&1; rc=$?
chk "$rc" 0 "flag init exits 0"
[ -f .cfgtest/homeC/meta/h_flag.db ] && ok "explicit --home beats both (flag wins)" || bad "homeC/meta/h_flag.db missing"

echo "===== 2) parser: quotes / export / warnings ====="
out=$(GROVEFS_CONFIG=$PWD/.cfgtest/c_quoted.conf T $Y init --view q 2>&1); rc=$?
chk "$rc" 0 "quoted-value init exits 0"
[ -f .cfgtest/homeC/meta/q.db ] && ok "quoted GROVEFS_HOME value unquoted correctly" || bad "homeC/meta/q.db missing"

out=$(GROVEFS_HOME=$PWD/.cfgtest/homeB GROVEFS_CONFIG=$PWD/.cfgtest/c_env.conf T $Y init --view p 2>&1); rc=$?
chk "$rc" 0 "malformed-line init exits 0 (warn, not fatal)"
echo "$out" | grep -q "not a GROVEFS_\* knob" && ok "non-GROVEFS_* key warns and is ignored" || bad "no warning for BOGUS_KEY"
echo "$out" | grep -q "not a KEY=VALUE line" && ok "malformed line warns with location" || bad "no warning for the BAD LINE"
[ -f .cfgtest/ignored_by_env/meta/p.db ] && bad "export line was not honored (should have been ignored via env)" || true
[ -f .cfgtest/homeB/meta/p.db ] && ok "export-prefixed line parsed (env still won the value)" || bad "homeB/meta/p.db missing"

echo "===== 3) unreadable explicit \$GROVEFS_CONFIG ====="
out=$(HOME=$PWD/.cfgtest/default_home GROVEFS_CONFIG=$PWD/.cfgtest/nope_missing.conf T $Y init --view m 2>&1); rc=$?
chk "$rc" 0 "missing explicit config is not fatal"
echo "$out" | grep -q "not readable" && ok "unreadable \$GROVEFS_CONFIG warns" || bad "no 'not readable' warning"
[ -f .cfgtest/default_home/.grovefs/meta/m.db ] && ok "falls through to \$HOME/.grovefs default" || bad "default_home/.grovefs/meta/m.db missing"

echo "===== 4) help advertises the mechanism ====="
$Y --help 2>&1 | grep -q "grovefs.conf" && ok "grovefs --help documents the config file" || bad "help missing config-file paragraph"

echo "===== 5) token-gated servers: file serves mount defaults ====="
# ---- token-gated servers: the data-server serves the SAME store the local
# seed mount wrote into. `--root` is the store ROOT (the backend appends its own
# blocks/ under it), so pointing at central/ makes the server see central/blocks. ----
$Y data-server --listen $DATA --root .cfgtest/central --token $TOK > .cfgtest/data.log 2>&1 & srv $!
$Y meta-server --db-path .cfgtest/central --listen $META --token $TOK > .cfgtest/meta.log 2>&1 & srv $!
wait_port $DATA || bad "data-server not up"; wait_port $META || bad "meta-server not up"

# mount with NO --meta/--data/--token on the command line — all three from the file
GROVEFS_CONFIG=$PWD/.cfgtest/c_mount.conf \
  $Y --home .cfgtest/homeA mount --view team --mountpoint .cfgtest/mnt1 --foreground > .cfgtest/m1.log 2>&1 & srv $!
mrc=0; for _ in $(seq 1 60); do ismnt .cfgtest/mnt1 && break; sleep 0.25; done
ismnt .cfgtest/mnt1 && ok "bare mount works on file-served coordinates (kernel-mounted)" || { bad "mnt1 not mounted"; mrc=1; }
[ "$mrc" = 0 ] && chk "$(cat .cfgtest/mnt1/docs/plan.md 2>/dev/null)" "planning text" "file-served mount reads through"
if [ "$mrc" = 0 ] && echo "written-by-config-mount" > .cfgtest/mnt1/docs/notes.md 2>/dev/null; then
  sleep 0.5   # let the async write-back flush the blob
  grep -rqs "written-by-config-mount" .cfgtest/central/blocks/ \
    && ok "file-served mount writes land in the central store" || bad "write-through from file-served mount failed"
else
  bad "write-through from file-served mount failed"
fi

# flag beats the file: a wrong --meta on the command line must override GROVEFS_META
GROVEFS_CONFIG=$PWD/.cfgtest/c_mount.conf \
  timeout 6 $Y --home .cfgtest/homeA mount --view team --mountpoint .cfgtest/mnt2 --meta 127.0.0.1:59999 --foreground > .cfgtest/m2.log 2>&1
rc=$?
[ "$rc" != 0 ] && grep -q "127.0.0.1:59999" .cfgtest/m2.log \
  && ok "explicit --meta flag overrides GROVEFS_META from the file" || bad "flag-vs-meta override not observable (rc=$rc)"

# env beats the file: WRONGTOKEN in the file, right token exported live
GROVEFS_TOKEN=$TOK GROVEFS_CONFIG=$PWD/.cfgtest/c_envtok.conf \
  $Y --home .cfgtest/homeA mount --view team --mountpoint .cfgtest/mnt3 --foreground > .cfgtest/m3.log 2>&1 & srv $!
m3=0; for _ in $(seq 1 60); do ismnt .cfgtest/mnt3 && { m3=1; break; }; sleep 0.25; done
[ "$m3" = 1 ] && ok "live GROVEFS_TOKEN beats the file's WRONGTOKEN" || bad "mnt3 not mounted (env token should win)"
fusermount -u .cfgtest/mnt3 2>/dev/null

# neither: the file's WRONGTOKEN reaches the server and is refused
GROVEFS_CONFIG=$PWD/.cfgtest/c_badtok.conf \
  timeout 6 $Y --home .cfgtest/homeA mount --view team --mountpoint .cfgtest/mnt4 --foreground > .cfgtest/m4.log 2>&1
rc=$?
[ "$rc" != 0 ] && ok "file's WRONGTOKEN refused at connect (rc=$rc)" || bad "bad-token mount unexpectedly succeeded"
grep -qi "permission\|auth\|token" .cfgtest/m4.log && ok "refusal is attributed to the token" || bad "no token refusal reason in m4.log: $(head -3 .cfgtest/m4.log)"

echo "===== 6) servers read their own defaults from the file ====="
# meta-server with NO --token and NO --db-path flag: both come from c_srv.conf;
# the mount side needs neither --token nor the token env — its token also comes
# from the file (c_mount.conf), and a WRONG one would fail the mount.
mnt_srv=.cfgtest/mnt2
# Strict view gate: create the view before any mount. Seeding it in srv_db
# (the dir the config file names) also makes the successful mount below proof
# that the file's GROVEFS_DB_PATH was consumed by the server.
$Y view --db-path .cfgtest/srv_db create --view srvview > .cfgtest/srv_seed.log 2>&1
GROVEFS_CONFIG=$PWD/.cfgtest/c_srv.conf \
  $Y meta-server --listen 127.0.0.1:15084 > .cfgtest/meta_srv2.log 2>&1 & srv $!
wait_port 127.0.0.1:15084 || bad "file-configured meta-server not up"
GROVEFS_CONFIG=$PWD/.cfgtest/c_mount.conf \
  $Y --home .cfgtest/homeA mount --view srvview --mountpoint $mnt_srv --meta 127.0.0.1:15084 --foreground > .cfgtest/m5.log 2>&1 & srv $!
m5=0; for _ in $(seq 1 60); do ismnt $mnt_srv && { m5=1; break; }; sleep 0.25; done
[ "$m5" = 1 ] && ok "mount authenticates with a file-served token against a file-configured server" || bad "$mnt_srv not mounted (m5)"
[ -f .cfgtest/srv_db/meta/srvview.db ] && ok "GROVEFS_DB_PATH from the file steered meta-server (the mount resolved its tree in srv_db)" || { bad "srv_db/meta/srvview.db missing — file db-path not consumed by meta-server"; cat .cfgtest/meta_srv2.log .cfgtest/m5.log; }

echo
echo "SUMMARY: pass=$pass fail=$fail"
[ "$fail" = 0 ] || exit 1
