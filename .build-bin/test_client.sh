#!/usr/bin/env bash
# Platform-client e2e: the `grovefs-client` workspace members (the macOS/Linux
# `grovefs-mac` shell, which speaks the SAME framed protocol as `grovefs mount`
# but through `remote-only` + `fuse-bridge`) mounted against the servers built
# from THIS tree. It answers a question no other suite does — the rest of the
# board only ever exercises the kernel's own client CLI, so a protocol or
# handshake regression that only bites an external client would sail through
# green.
#   A) SQLite control plane: client mounts a view the kernel created, writes
#      through it, bytes land in the DATA-server (not on the client host),
#      getattr sizes are right, and a kernel mount of the same view reads back
#      the client's bytes (one tree, two clients);
#   B) an unknown view name is REFUSED by the client with the explicit-view
#      message — M_OPEN never auto-creates a tree behind a client's back;
#   C) token gate: a client without `--token` cannot mount a token-gated
#      cluster, and with it the same mount works;
#   D) PostgreSQL control plane (`meta-server --meta-store postgres://…`): the
#      identical client mount/read round-trips, proving the client is agnostic
#      to which of the two metadata tiers resolves its view.
# Requires a sibling `grovefs-client` checkout (override with $GROVEFS_CLIENT)
# holding a mountable binary ($CLIENT_BIN, default `dist/grovefs-mac`, the
# shipped prebuilt — a genuinely CROSS-VERSION pairing against this kernel).
# Self-skips with exit 0 when the checkout, the binary or FUSE is absent, and
# the PG arm skips on hosts without a reachable PostgreSQL, so the board stays
# GREEN either way. Mount liveness is GATED before any write assertion: a dead
# mount left the following `echo > file` to write into the bare directory and
# report green numbers about nothing.
set -u
cd "$(dirname "$0")/.."
export LC_ALL=C LANG=C
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
. .build-bin/suite_helpers.sh
Y=./target/release/grovefs
D=.clitest; rm -rf $D; mkdir -p $D/central $D/data $D/home $D/mnt_c $D/mnt_k $D/mnt_g $D/mnt_t $D/mnt_pg

CROOT="${GROVEFS_CLIENT:-$PWD/../grovefs-client}"
CBIN="${CLIENT_BIN:-$CROOT/dist/grovefs-mac}"
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 15 "$@"; }
newport(){ python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }
srvalive(){ kill -0 "$1" 2>/dev/null && kill -0 "$2" 2>/dev/null; }
cln(){ fusermount -u "$1" 2>/dev/null || true; }

if [ ! -x "$CBIN" ]; then
  echo "SUMMARY: pass=0 fail=0 SKIP=1 (no mountable client binary at $CBIN)"
  echo "note: clone the sibling grovefs-client repo, or build one here with"
  echo "      'cargo build --release -p grovefs-mac' and pass CLIENT_BIN=..."
  exit 0
fi
if ! command -v fusermount >/dev/null 2>&1; then
  echo "SUMMARY: pass=0 fail=0 SKIP=1 (no fusermount helper on this host)"
  exit 0
fi
echo "client binary: $CBIN"
echo "               sha256 $(sha256sum "$CBIN" | cut -c1-16)…"

# ------------------------------------------------------------------ A) SQLite
echo "===== A) SQLite control plane x external client ====="
MA=127.0.0.1:$(newport); DA=127.0.0.1:$(newport)
$Y data-server --listen $DA --data-dir $PWD/$D/data > $D/data.log 2>&1 & DP=$!
$Y meta-server --listen $MA --meta-dir $PWD/$D/central > $D/meta.log 2>&1 & MP=$!
sleep 1.2
if ! srvalive $DP $MP; then
  bad "servers did not come up"; tail -3 $D/data.log $D/meta.log | sed 's/^/    /'
  echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
  exit 1
fi
ok "kernel servers alive (meta $MA, data $DA)"
$Y view --meta-dir $PWD/$D/central create --view alice > $D/create.log 2>&1 \
  && ok "control plane created base view alice" \
  || { bad "view create failed"; tail -3 $D/create.log | sed 's/^/    /'; }

$CBIN --meta-addr $MA --data-addr $DA --view alice --mount $PWD/$D/mnt_c \
  > $D/cli_c.log 2>&1 & CP=$!
if mnt_gate $D/mnt_c; then ok "client mounted alice (handshake accepted)"; mounted_c=1; else
  bad "client never mounted"; tail -3 $D/cli_c.log | sed 's/^/    /'; mounted_c=0
fi
if [ "$mounted_c" = 1 ]; then
  M=$D/mnt_c
  T mkdir $M/docs && T bash -c "echo hello-client > '$M/docs/a.txt'" \
    && chk "$(T cat $M/docs/a.txt)" "hello-client" "client write/read round-trips over the remote tiers"
  chk "$(find $D/data -type f | wc -l)" "1" "content bytes live on the data-server, not the client host"
  chk "$(stat -c %s $M/docs/a.txt)" "13" "getattr reports the written size"
  $Y mount --view alice --mountpoint $PWD/$D/mnt_k --meta-server $MA \
    --data-servers $DA --home $PWD/$D/khome --foreground > $D/mnt_k.log 2>&1 & KP=$!
  mnt_gate $D/mnt_k || bad "kernel mount of the same view failed"
  chk "$(T cat $D/mnt_k/docs/a.txt 2>/dev/null)" "hello-client" \
    "kernel mount reads back the client's bytes (one tree, two clients)"
else
  echo "  -- R/W assertions skipped: no live mount to write through"
fi

echo "===== B) unknown view is refused, never auto-created ====="
$CBIN --meta-addr $MA --data-addr $DA --view ghost --mount $PWD/$D/mnt_g \
  > $D/cli_g.log 2>&1 & GP=$!
for _ in $(seq 1 20); do
  grep -qi 'does not exist\|no tree' $D/cli_g.log && break; sleep 0.25
done
if findmnt -rn --mountpoint "$PWD/$D/mnt_g" >/dev/null 2>&1; then
  bad "a mount of the unknown view appeared"
else
  grep -qi 'does not exist' $D/cli_g.log \
    && ok "unknown view refused with the explicit-view message" \
    || bad "unknown view not mounted but refusal text unexpected: $(tail -1 $D/cli_g.log)"
fi
cln $D/mnt_c; kp $CP $KP; wk $CP $KP

echo "===== C) token gate honoured by the client ====="
# reuse the SAME view/store, gated by a token, on fresh ports
MA2=127.0.0.1:$(newport); DA2=127.0.0.1:$(newport)
$Y data-server --listen $DA2 --data-dir $PWD/$D/data --token s3cr3t > $D/data2.log 2>&1 & DP2=$!
$Y meta-server --listen $MA2 --meta-dir $PWD/$D/central --token s3cr3t > $D/meta2.log 2>&1 & MP2=$!
sleep 1.2
if ! srvalive $DP2 $MP2; then
  bad "token-gated servers did not come up"; tail -2 $D/data2.log $D/meta2.log | sed 's/^/    /'
else
  $CBIN --meta-addr $MA2 --data-addr $DA2 --view alice --mount $PWD/$D/mnt_t \
    > $D/cli_t.log 2>&1 & TP=$!
  for _ in $(seq 1 16); do
    findmnt -rn --mountpoint "$PWD/$D/mnt_t" >/dev/null 2>&1 && break
    grep -qi 'refus\|denied\|token\|not authorized' $D/cli_t.log && break; sleep 0.25
  done
  if findmnt -rn --mountpoint "$PWD/$D/mnt_t" >/dev/null 2>&1; then
    bad "client mounted a token-gated cluster with NO token"
  else
    ok "no-token client refused the gated cluster"
  fi
  kp $TP; cln $D/mnt_t
  $CBIN --meta-addr $MA2 --data-addr $DA2 --view alice --token s3cr3t \
    --mount $PWD/$D/mnt_t > $D/cli_t2.log 2>&1 & TP=$!
  if mnt_gate $D/mnt_t; then
    chk "$(T cat $D/mnt_t/docs/a.txt 2>/dev/null)" "hello-client" \
      "client with the right token reads the gated tree"
  else
    bad "tokened client never mounted"; tail -2 $D/cli_t2.log | sed 's/^/    /'
  fi
  cln $D/mnt_t; kp $TP; wk $TP
fi
kp $MP2 $DP2; wk $MP2 $DP2
cln $D/mnt_k; kp $KP; wk $KP
kp $CP $GP $MP $DP; wk $CP $GP $MP $DP

# ---------------------------------------------------------------------- D) PG
PGHOST=${PGHOST:-127.0.0.1}; PGPORT=${PGPORT:-5433}
if (exec 3<>/dev/tcp/$PGHOST/$PGPORT) 2>/dev/null; then
  exec 3<&- 2>/dev/null || true
  echo "===== D) PostgreSQL control plane x external client ====="
  PGURI="postgres://grove:grovepw@${PGHOST}:${PGPORT}/grovefs"
  VIEW="cli$(date +%s)"
  DA3=127.0.0.1:$(newport); MA3=127.0.0.1:$(newport)
  mkdir -p $D/pgdata
  $Y data-server --listen $DA3 --data-dir $PWD/$D/pgdata > $D/pgdata.log 2>&1 & DP3=$!
  $Y view --meta-store "$PGURI" --meta-dir $PWD/$D/home create --view "$VIEW" \
    > $D/pgcreate.log 2>&1 \
    && ok "PG tier provisioned schema for view $VIEW" \
    || { bad "PG view create failed"; tail -3 $D/pgcreate.log | sed 's/^/    /'; }
  $Y meta-server --listen $MA3 --home $PWD/$D/home --meta-store "$PGURI" \
    > $D/pgmeta.log 2>&1 & MP3=$!
  sleep 1.5
  if ! srvalive $DP3 $MP3; then
    bad "PG-backed servers did not come up"; tail -2 $D/pgdata.log $D/pgmeta.log | sed 's/^/    /'
  else
    ok "PG-backed meta-server + data-server alive"
    $CBIN --meta-addr $MA3 --data-addr $DA3 --view "$VIEW" --mount $PWD/$D/mnt_pg \
      > $D/cli_pg.log 2>&1 & PGP=$!
    if mnt_gate $D/mnt_pg; then
      ok "client mounted a PG-governed view"
      T bash -c "echo pg-from-client > '$D/mnt_pg/f.txt'" \
        && chk "$(T cat $D/mnt_pg/f.txt)" "pg-from-client" \
        "client write/read round-trips against the PG tier"
      chk "$(find $D/pgdata -type f | wc -l)" "1" "PG tier still puts bytes on the data-server"
    else
      bad "client never mounted the PG view"; tail -3 $D/cli_pg.log | sed 's/^/    /'
    fi
    cln $D/mnt_pg; kp $PGP; wk $PGP
    kp $MP3 $DP3; wk $MP3 $DP3
    # leave no schema behind: a bare DROP would orphan rows in grovefs_refs
    $Y view --meta-store "$PGURI" rm --base --name "$VIEW" --data-dir $PWD/$D/pgdata \
      > $D/pgdrop.log 2>&1 \
      && ok "PG view $VIEW reclaimed through the CLI (ledger unbooked)" \
      || bad "PG view rm --base failed: $(tail -1 $D/pgdrop.log)"
  fi
else
  echo "===== D) PG tier SKIPPED (no PostgreSQL on $PGHOST:$PGPORT) ====="
fi

echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" = 0 ]
