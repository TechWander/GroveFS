#!/usr/bin/env bash
# e2e for the presentation overlay (`mount --layout`): ONE stored view,
# DIFFERENT lenses per client. Covers hide / only / map (relocate) / link
# (alias), write-through to the real entries, lens-side deletes hitting the
# source, and the synthesized-dir creation guard. Assertion-based like the
# other mount suites; SUMMARY line for run_all.sh.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
rm -rf .ltest; mkdir -p .ltest/home .ltest/mnt_base .ltest/mnt_a .ltest/mnt_b .ltest/mnt_c .ltest/mnt_d
export GROVEFS_HOME=$PWD/.ltest/home

cat > .ltest/team.manifest <<'EOF'
mkdir docs
mkdir docs/2026
touch docs/2026/a.md
mkdir photos
mkdir photos/private
touch photos/private/x.jpg
mkdir photos/public
touch photos/public/y.jpg
mkdir data
mkdir data/raw
touch data/raw/big.bin
mkdir shared
mkdir shared/lib
touch shared/lib/lib.a
EOF
$Y init --view team --manifest .ltest/team.manifest >/dev/null

# Layout rule files: each is one client's lens over the SAME stored tree.
cat > .ltest/a.layout <<'EOF'
# client A: only the docs subtree matters
only /docs
EOF
cat > .ltest/b.layout <<'EOF'
# client B: everything, minus the private album
hide /photos/private
EOF
cat > .ltest/c.layout <<'EOF'
# client C: raw data lives under /team, the report lands in /inbox
map /data/raw /team/raw
map /docs/2026/a.md /inbox/report.md
EOF
cat > .ltest/d.layout <<'EOF'
# client D: project view aliases the shared lib
link /shared/lib /proj/mylib
EOF

$Y mount --view team --mountpoint .ltest/mnt_base --strict --foreground > .ltest/base.log 2>&1 &
$Y mount --view team --mountpoint .ltest/mnt_a   --strict --layout .ltest/a.layout --foreground > .ltest/a.log 2>&1 &
$Y mount --view team --mountpoint .ltest/mnt_b   --strict --layout .ltest/b.layout --foreground > .ltest/b.log 2>&1 &
$Y mount --view team --mountpoint .ltest/mnt_c   --strict --layout .ltest/c.layout --foreground > .ltest/c.log 2>&1 &
$Y mount --view team --mountpoint .ltest/mnt_d   --strict --layout .ltest/d.layout --foreground > .ltest/d.log 2>&1 &
PIDS=$(jobs -p)
sleep 1.5

cleanup(){ fusermount -u .ltest/mnt_base .ltest/mnt_a .ltest/mnt_b .ltest/mnt_c .ltest/mnt_d 2>/dev/null; kill $PIDS 2>/dev/null; }
trap cleanup EXIT

BASE=.ltest/mnt_base; A=.ltest/mnt_a; B=.ltest/mnt_b; C=.ltest/mnt_c; D=.ltest/mnt_d
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }
T(){ timeout 5 "$@"; }

# Give the source data real content on the base (unlensed) mount.
T sh -c "printf 'report-body' > $BASE/docs/2026/a.md"
T sh -c "printf 'rawdata123'  > $BASE/data/raw/big.bin"

echo "===== 1) baseline (no lens) shows the stored tree as-is ====="
chk "$(T ls $BASE | tr '\n' ' ')" "data docs photos shared " "unlensed root listing"

echo "===== 2) only: client A sees just the whitelisted subtree ====="
chk "$(T ls $A | tr '\n' ' ')" "docs " "A root is trimmed to /docs"
chk "$(T ls $A/docs | tr '\n' ' ')" "2026 " "A sees docs/2026"
[ -e "$A/photos" ] && bad "A leaks photos" || ok "A cannot see /photos"
T mkdir $A/newtop 2>.ltest/a.err
grep -qE "Operation not permitted|不允许的操作" .ltest/a.err && ok "A write into pass-through root refused (EPERM)" || bad "A mkdir at root should be EPERM: $(cat .ltest/a.err)"
T sh -c "printf 'note' > $A/docs/2026/note.md" && ok "A writes inside the shown subtree" || bad "A write into shown subtree failed"
chk "$(T ls $BASE/docs/2026 | tr '\n' ' ')" "a.md note.md " "lensed write landed on the real tree"

echo "===== 3) hide: client B trims one album, keeps the rest ====="
chk "$(T ls $B/photos | tr '\n' ' ')" "public " "B sees only photos/public"
[ -e "$B/photos/private/x.jpg" ] && bad "B leaks private album" || ok "B cannot reach photos/private"
chk "$(T cat $B/photos/public/y.jpg | wc -c | tr -d ' ')" "0" "B reads remaining entries normally"

echo "===== 4) map: client C reorganizes the same bytes ====="
chk "$(T ls $C | tr '\n' ' ')" "data docs inbox photos shared team " "C root carries the lens scaffolding"
chk "$(T ls $C/data | tr '\n' ' ')" "" "raw vacated its source spot for C"
chk "$(T ls $C/team | tr '\n' ' ')" "raw " "C finds raw under /team"
chk "$(T cat $C/team/raw/big.bin)" "rawdata123" "relocated path reads the same object"
T sh -c "printf 'more' > $C/team/raw/new2.bin" && ok "C writes through the relocated dir" || bad "C write-through failed"
chk "$(T ls $BASE/data/raw | tr '\n' ' ')" "big.bin new2.bin " "write-through visible on the real tree"
chk "$(T cat $C/inbox/report.md)" "report-body" "file relocated to /inbox keeps content"

echo "===== 5) lens deletes act on the real entries ====="
T rm $C/inbox/report.md && ok "C removes the relocated name" || bad "C rm report.md failed"
[ -e "$BASE/docs/2026/a.md" ] && bad "source a.md survived the lens delete" || ok "source entry a.md removed via the lens"

echo "===== 6) link: client D aliases without moving ====="
chk "$(T ls $D/shared | tr '\n' ' ')" "lib " "D still sees the source spot (alias, not move)"
chk "$(T ls $D/proj | tr '\n' ' ')" "mylib " "D sees the alias under /proj"
i1=$(T stat -c '%i' $D/shared/lib); i2=$(T stat -c '%i' $D/proj/mylib)
chk "$i2" "$i1" "alias and source share one inode"

echo "===== 7) synthesized dirs reject direct creation ====="
T mkdir $C/team 2>.ltest/c.err
if grep -qE "File exists|文件已存在|Operation not permitted|不允许的操作" .ltest/c.err; then ok "mkdir over scaffolding refused"; else bad "mkdir /team should be refused: $(cat .ltest/c.err)"; fi
T ls "$C/team/raw" >/dev/null && ok "relocated dir traversable" || bad "relocated dir not traversable"

echo "===== 8) bad layout file fails fast ====="
echo "bogus /x" > .ltest/bad.layout
$Y mount --view team --mountpoint .ltest/mnt_e --layout .ltest/bad.layout --foreground > .ltest/e.log 2>&1
grep -q "unknown layout verb" .ltest/e.log && ok "invalid rules rejected at mount time" || bad "invalid rules not rejected"

echo "===== 9) server-side derived views: --view <name>, no client rules file ====="
META=127.0.0.1:50162; DATA=127.0.0.1:50163
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
$Y meta-server --db-path .ltest/home --listen $META > .ltest/meta_srv.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --root .ltest/home > .ltest/data_srv.log 2>&1 &
DSP=$!
wait_port $META && wait_port $DATA || bad "derived-view servers failed to start"

# define TWO derived views over the SAME physical base tree 'team'
$Y view --db-path .ltest/home define --name docsonly  --base team --rules .ltest/a.layout > .ltest/def1.log 2>&1 \
  && ok "view define docsonly" || bad "define docsonly: $(cat .ltest/def1.log)"
$Y view --db-path .ltest/home define --name trimphotos --base team --rules .ltest/b.layout > .ltest/def2.log 2>&1 \
  && ok "view define trimphotos" || bad "define trimphotos: $(cat .ltest/def2.log)"
# guard rails at define time: name must differ from base, rules must parse
$Y view --db-path .ltest/home define --name team --base team --rules .ltest/a.layout > .ltest/def3.log 2>&1
[ $? -ne 0 ] && ok "define rejects a name equal to its base" || bad "define should reject name==base"
$Y view --db-path .ltest/home list > .ltest/vlist.log 2>&1
grep -q "docsonly' -> base 'team'" .ltest/vlist.log && grep -q "trimphotos" .ltest/vlist.log \
  && ok "view list shows both derived views with their base" || bad "view list wrong: $(cat .ltest/vlist.log)"

# client mounts by NAME only (no --layout, no rules file, NOT --strict —
# proving the server-side multi-bucket invalidation push works)
mkdir -p .ltest/mnt_srv .ltest/mnt_srvbase
$Y mount --view docsonly -m .ltest/mnt_srv --meta $META --data $DATA --foreground > .ltest/srv.log 2>&1 &
SP=$!
$Y mount --view team -m .ltest/mnt_srvbase --meta $META --data $DATA --foreground > .ltest/srvb.log 2>&1 &
BP2=$!
sleep 2
SB=.ltest/mnt_srvbase; SV=.ltest/mnt_srv
chk "$(T ls $SV | tr '\n' ' ')" "docs " "derived view trimmed server-side for this name"
chk "$(T ls $SV/photos 2>/dev/null | tr '\n' ' ')" "" "photos invisible through the derived view"
chk "$(T ls $SB | tr '\n' ' ')" "data docs photos shared " "base name still shows the full tree"
# A virtual (derived) view mounts READ-ONLY by contract: the M_OPEN ack flagged
# the derived name and the client applied the kernel RO mount itself.
grep -q "read-only" .ltest/srv.log \
  && ok "derived mount came up read-only (handshake flag)" || bad "derived mount should be RO: $(cat .ltest/srv.log)"

# cross-client freshness WITHOUT --strict: base write -> derived viewer converges
# via the base-bucket subscription of the derived-view subscriber
T sh -c "printf 'fresh' > $SB/docs/2026/fresh.md" && ok "base client writes into shared tree" || bad "base write failed"
T ls $SV/docs/2026 | grep -q fresh.md && ok "derived view sees base write immediately (push)" || bad "derived view missed base write"
# WRITES THROUGH THE DERIVED NAME ARE REFUSED: a virtual view is a read-only
# shape; every mutation belongs to the original (base) view.
T sh -c "printf 'lens' > $SV/docs/2026/fromlens.md" 2>/dev/null \
  && bad "derived write should be refused" || ok "derived view refuses writes (EROFS)"
[ ! -e "$SB/docs/2026/fromlens.md" ] && ok "refused write left the base tree untouched" || bad "phantom entry landed on base"
T rm $SV/docs/2026/fresh.md 2>/dev/null \
  && bad "derived delete should be refused" || ok "derived view refuses deletes"
[ -e "$SB/docs/2026/fresh.md" ] && ok "base entry survived the refused lens delete" || bad "delete leaked through the gate"
# the base (original) view takes those writes fine, on the same server/session
T sh -c "printf 'lens' > $SB/docs/2026/fromlens.md" && ok "base view still writable on the same server" || bad "base write failed"
T rm $SB/docs/2026/fromlens.md && T rm $SB/docs/2026/fresh.md

# HOT RELOAD: re-define docsonly BROADER while SV stays mounted — the server
# re-checks the definition's mtime per op, so the live mount converges without
# a remount (bounded by the client's <=2s attr/dentry TTL, no --strict here)
cat > .ltest/broader.layout <<'EOF'
# widen the same view name: show docs AND photos
only /docs
only /photos
EOF
$Y view --db-path .ltest/home define --name docsonly --base team --rules .ltest/broader.layout > .ltest/def5.log 2>&1 \
  && ok "re-define broadens the derived view" || bad "re-define: $(cat .ltest/def5.log)"
sleep 5
chk "$(T ls $SV | tr '\n' ' ')" "docs photos " "live mount picked up the broadened shape (no remount)"
# narrow it back — hot reload works in both directions
$Y view --db-path .ltest/home define --name docsonly --base team --rules .ltest/a.layout > .ltest/def6.log 2>&1
sleep 5
chk "$(T ls $SV | tr '\n' ' ')" "docs " "live mount narrowed back after the next edit"
# after a hot swap the read-only contract still holds, and base writes show
# through the live virtual shape (fresh definition, same mount)
T sh -c "printf 'hot' > $SB/docs/2026/hot.md" && ok "base write lands after hot reload" || bad "base write after reload failed"
T ls $SV/docs/2026 | grep -q hot.md && ok "derived viewer sees the base write after reload" || bad "derived missed post-reload base write"
T sh -c "printf 'x' > $SV/docs/2026/ro_after_reload.md" 2>/dev/null \
  && bad "derived writable after hot reload" || ok "read-only contract survives hot reload"

# a bad definition is refused at mount time (fail fast, server resolves it)
echo "bogus /x" > .ltest/broken.rules
$Y view --db-path .ltest/home define --name broken --base team --rules .ltest/broken.rules > .ltest/def4.log 2>&1
! grep -q "defined derived view" .ltest/def4.log && ok "define rejects non-parseable rules" || bad "define accepted bad rules"

# the SAME read-only contract applies to a LOCAL (--home) derived mount: no
# server involved, the definition itself forces kernel RO
mkdir -p .ltest/mnt_ld
$Y mount --view docsonly -m .ltest/mnt_ld --foreground > .ltest/ld.log 2>&1 &
LDP=$!
sleep 1.5
grep -q "read-only" .ltest/ld.log \
  && ok "local derived mount notes read-only" || bad "local derived mount: $(cat .ltest/ld.log)"
T ls .ltest/mnt_ld | grep -q docs && ok "local derived mount renders the shape" || bad "local derived listing wrong"
T sh -c "printf 'x' > .ltest/mnt_ld/docs/2026/ld.md" 2>/dev/null \
  && bad "local derived write should be refused" || ok "local derived mount is read-only too"
fusermount -u .ltest/mnt_ld 2>/dev/null; kill $LDP 2>/dev/null; wait $LDP 2>/dev/null

$Y view --db-path .ltest/home rm --name docsonly > .ltest/rm1.log 2>&1 \
  && ok "view rm drops the definition" || bad "view rm: $(cat .ltest/rm1.log)"
$Y view --db-path .ltest/home list > .ltest/vlist2.log 2>&1
! grep -q "docsonly" .ltest/vlist2.log && ok "view list no longer shows removed view" || bad "removed view still listed"

fusermount -u $SV $SB 2>/dev/null
kill $SP $BP2 $MSP $DSP 2>/dev/null
wait $SP $BP2 $MSP $DSP 2>/dev/null

echo "===== 10) pure alias (name governance): local --home first ====="
# `alias <real>` carries NO rules: the alias IS the real view under a public
# name — full tree, WRITABLE — and the governed real name stops being a local
# mount entry point (mirrors the meta-server contract without a wire).
$Y view --db-path .ltest/home alias --name pub --real team > .ltest/al1.log 2>&1 \
  && ok "view alias pub -> team" || bad "alias pub: $(cat .ltest/al1.log)"
$Y view --db-path .ltest/home list > .ltest/vlist3.log 2>&1
grep -q "alias 'pub' -> real view 'team'" .ltest/vlist3.log \
  && ok "view list shows the alias with its real view" || bad "alias missing from list: $(cat .ltest/vlist3.log)"
# define guards around the alias slot
$Y view --db-path .ltest/home define --name pub --base team --rules .ltest/a.layout > .ltest/ag1.log 2>&1
[ $? -ne 0 ] && ok "define refuses to shadow an existing alias name" || bad "define overwrote an alias"
# the derived-view guard needs a LIVE derived definition: docsonly was rm'd at
# the end of section 9, trimphotos is still defined
$Y view --db-path .ltest/home alias --name pub --real trimphotos > .ltest/ag2.log 2>&1
[ $? -ne 0 ] && ok "alias refuses a derived-view target" || bad "alias accepted a derived target"
# mount by ALIAS: full shape and writable
mkdir -p .ltest/mnt_al
grep -q "trimphotos" .ltest/ag2.log && ok "guard names the refused target view" || bad "guard message unclear: $(cat .ltest/ag2.log)"
$Y mount --view pub -m .ltest/mnt_al --foreground > .ltest/al_mount.log 2>&1 &
ALP=$!
sleep 1.5
chk "$(T ls .ltest/mnt_al | tr '\n' ' ')" "data docs photos shared " "alias mount shows the FULL tree (no shape, unlike a derived view)"
T sh -c "printf 'aw' > .ltest/mnt_al/docs/2026/aliaswrite.md" && ok "alias mount is WRITABLE (name governance, not shape)" || bad "alias mount refused writes"
T cat .ltest/mnt_al/docs/2026/aliaswrite.md | grep -q aw && ok "alias write reads back" || bad "alias write lost"
# mounting the governed REAL name locally is refused (the alias is the entry)
$Y mount --view team -m .ltest/mnt_al --foreground 2> .ltest/al_denied.log
[ $? -ne 0 ] && grep -q "governed by an alias" .ltest/al_denied.log \
  && ok "local mount of the governed real name is refused" || bad "real name mounted despite governance: $(cat .ltest/al_denied.log)"
T rm .ltest/mnt_al/docs/2026/aliaswrite.md
fusermount -u .ltest/mnt_al 2>/dev/null; kill $ALP 2>/dev/null; wait $ALP 2>/dev/null

# --- gapless name rotation (--retire): the leak-response workflow ---
# A leaked public name must swap in ONE step with no window where the real
# view 'team' becomes mountable by its own name.
$Y view --db-path .ltest/home alias --name pubnew --real team --retire pub > .ltest/rot1.log 2>&1 \
  && grep -q "gapless" .ltest/rot1.log \
  && ok "gapless rotation pub -> pubnew (--retire)" || bad "rotation failed: $(cat .ltest/rot1.log)"
$Y view --db-path .ltest/home list > .ltest/vlist_rot.log 2>&1
grep -q "alias 'pubnew'" .ltest/vlist_rot.log && ! grep -q "alias 'pub'" .ltest/vlist_rot.log \
  && ok "list shows the new alias only (old name retired)" || bad "list after rotation: $(cat .ltest/vlist_rot.log)"
# the leaked OLD name no longer reaches the real tree
mkdir -p .ltest/mnt_old
$Y mount --view pub -m .ltest/mnt_old --foreground > .ltest/rot_old.log 2>&1 &
OLDP=$!; sleep 1.5
[ ! -d .ltest/mnt_old/docs ] && ok "leaked old alias no longer resolves to the real tree" || bad "old alias still exposes team"
fusermount -u .ltest/mnt_old 2>/dev/null; kill $OLDP 2>/dev/null; wait $OLDP 2>/dev/null; rm -rf .ltest/mnt_old
# the retired mount lazily created an empty 'pub.db' namespace — drop it so the
# name is free again (a retired public name never reaches the real tree anyway)
rm -f .ltest/home/meta/pub.db
# the NEW name is the live entry: full tree
mkdir -p .ltest/mnt_new
$Y mount --view pubnew -m .ltest/mnt_new --foreground > .ltest/rot_new.log 2>&1 &
NEWP=$!; sleep 1.5
chk "$(T ls .ltest/mnt_new | tr '\n' ' ')" "data docs photos shared " "rotated alias pubnew serves the full tree"
fusermount -u .ltest/mnt_new 2>/dev/null; kill $NEWP 2>/dev/null; wait $NEWP 2>/dev/null; rm -rf .ltest/mnt_new
# --retire guards: never drop a derived shape def or a non-alias
$Y view --db-path .ltest/home alias --name rr1 --real team --retire trimphotos > .ltest/rotg1.log 2>&1
[ $? -ne 0 ] && grep -qiE "not a pure alias" .ltest/rotg1.log \
  && ok "--retire refuses a derived view (would drop a shape def)" || bad "retire guard (derived): $(cat .ltest/rotg1.log)"
$Y view --db-path .ltest/home alias --name rr2 --real team --retire nosuch > .ltest/rotg2.log 2>&1
[ $? -ne 0 ] && ok "--retire refuses a non-existent alias" || bad "retire guard (missing): $(cat .ltest/rotg2.log)"
# restore the entry name for the remote section (rotate pubnew -> pub)
$Y view --db-path .ltest/home alias --name pub --real team --retire pubnew > .ltest/rot2.log 2>&1 \
  && ok "rotate back to pub for the remote section" || bad "rotate back: $(cat .ltest/rot2.log)"

echo "===== 11) pure alias over a meta-server: real name refused, alias writable ====="
META2=127.0.0.1:50164; DATA2=127.0.0.1:50165
$Y meta-server --db-path .ltest/home --listen $META2 > .ltest/meta_srv2.log 2>&1 &
MSP2=$!
$Y data-server --listen $DATA2 --root .ltest/home > .ltest/data_srv2.log 2>&1 &
DSP2=$!
wait_port $META2 && wait_port $DATA2 || bad "alias servers failed to start"
mkdir -p .ltest/mnt_ra .ltest/mnt_rr
# alias mount through the server: full tree, writable (M_OPEN resolved the name)
$Y mount --view pub -m .ltest/mnt_ra --meta $META2 --data $DATA2 --foreground > .ltest/ra.log 2>&1 &
RAP=$!
sleep 2
chk "$(T ls .ltest/mnt_ra | tr '\n' ' ')" "data docs photos shared " "remote alias mount shows the full tree"
T sh -c "printf 'rw' > .ltest/mnt_ra/docs/2026/ralias.md" && ok "remote alias mount is writable" || bad "remote alias write refused: $(cat .ltest/ra.log)"
# mounting the REAL name through the same server: refused while governed
$Y mount --view team -m .ltest/mnt_rr --meta $META2 --data $DATA2 --foreground > .ltest/rr.log 2>&1 &
RRP=$!
sleep 1.5
[ ! -d .ltest/mnt_rr/docs ] && grep -qiE "rejected|refused|Permission|denied|token" .ltest/rr.log \
  && ok "meta-server refuses mounts of the governed real name" || bad "real name mounted remotely: $(cat .ltest/rr.log)"
kill $RRP 2>/dev/null; wait $RRP 2>/dev/null
# writes through the alias are the real tree: a fresh alias mount sees them
T cat .ltest/mnt_ra/docs/2026/ralias.md | grep -q rw && ok "alias write reads back through the wire" || bad "alias remote write lost"
T rm .ltest/mnt_ra/docs/2026/ralias.md
# --- gapless rotation over the wire: 'team' never becomes mountable ---
$Y view --db-path .ltest/home alias --name pubr --real team --retire pub > .ltest/rot3.log 2>&1 \
  && grep -q gapless .ltest/rot3.log \
  && ok "remote: gapless rotation pub -> pubr (--retire)" || bad "remote rotation: $(cat .ltest/rot3.log)"
# right after rotation the real name is STILL refused (governed by pubr) — zero window
$Y mount --view team -m .ltest/mnt_rr --meta $META2 --data $DATA2 --foreground > .ltest/rr3.log 2>&1 &
RR3=$!; sleep 1.5
[ ! -d .ltest/mnt_rr/docs ] && grep -qiE "token|denied|permission|refus" .ltest/rr3.log \
  && ok "remote: real name stays refused across the rotation (no gap)" || bad "gap during rotation: $(cat .ltest/rr3.log)"
kill $RR3 2>/dev/null; wait $RR3 2>/dev/null
# the rotated name serves the full tree
mkdir -p .ltest/mnt_rp
$Y mount --view pubr -m .ltest/mnt_rp --meta $META2 --data $DATA2 --foreground > .ltest/rp.log 2>&1 &
RPP=$!; sleep 2
chk "$(T ls .ltest/mnt_rp | tr '\n' ' ')" "data docs photos shared " "remote: rotated alias pubr serves the full tree"
fusermount -u .ltest/mnt_rp 2>/dev/null; kill $RPP 2>/dev/null; wait $RPP 2>/dev/null
# rotate back so the next steps (which use 'pub') still hold
$Y view --db-path .ltest/home alias --name pub --real team --retire pubr > /dev/null 2>&1
# removing the alias reopens the real name for NEW mounts
fusermount -u .ltest/mnt_ra 2>/dev/null; kill $RAP $MSP2 $DSP2 2>/dev/null; wait $RAP $MSP2 $DSP2 2>/dev/null
$Y view --db-path .ltest/home rm --name pub > .ltest/al_rm.log 2>&1 \
  && ok "view rm removes an alias definition too" || bad "alias rm: $(cat .ltest/al_rm.log)"
$Y meta-server --db-path .ltest/home --listen $META2 > .ltest/meta_srv3.log 2>&1 &
MSP3=$!
$Y data-server --listen $DATA2 --root .ltest/home > .ltest/data_srv3.log 2>&1 &
DSP3=$!
wait_port $META2 && wait_port $DATA2 || bad "post-rm servers failed to start"
$Y mount --view team -m .ltest/mnt_rr --meta $META2 --data $DATA2 --foreground > .ltest/rr2.log 2>&1 &
RRP2=$!
sleep 2
chk "$(T ls .ltest/mnt_rr | tr '\n' ' ')" "data docs photos shared " "real name mountable again after the alias is removed"
fusermount -u .ltest/mnt_rr 2>/dev/null
kill $RRP2 $MSP3 $DSP3 2>/dev/null; wait $RRP2 $MSP3 $DSP3 2>/dev/null
# import guard: an alias name is not a storage identity
mkdir -p .ltest/src_dir && touch .ltest/src_dir/f1
$Y view --db-path .ltest/home alias --name pub2 --real team > /dev/null 2>&1
$Y import --db-path .ltest/home --view pub2 --source .ltest/src_dir > .ltest/al_imp.log 2>&1
[ $? -ne 0 ] && grep -q "pure alias" .ltest/al_imp.log \
  && ok "import refuses an alias target (names the real view)" || bad "import guard wrong: $(cat .ltest/al_imp.log)"
$Y view --db-path .ltest/home rm --name pub2 > /dev/null 2>&1

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL LAYOUT CHECKS PASSED" || echo "SOME LAYOUT CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
