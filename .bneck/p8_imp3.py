#!/usr/bin/env python3
# P8 part 3 of the import tier: sections 6 and 7.
#
# Section 6 is the one the suite's title is really about — imported bytes served by
# a meta-server and shaped by a derived view — and it re-runs on FoundationDB
# almost line for line, because the deployment is already a server pair: the only
# change is that the pair belongs to the run (EARLY) instead of being started here,
# and that the control plane the definition is written to is the cluster root.
#
# Section 7 is the guard set. Two of the four guards run BEFORE the backend is
# consulted (the source must exist and be a directory), so they are shared; the
# other two are where refuse-not-downgrade lives, and this tier states the
# precedence out loud instead of quietly expecting a different message.
import io

P = '/data/grovefs/grovefs/.build-bin/test_import.sh'
s = io.open(P, encoding='utf-8').read()
orig = s

def rep(old, new, n=1):
    global s
    assert s.count(old) == n, f'anchor x{s.count(old)}: {old[:70]!r}'
    s = s.replace(old, new, n)

# ---- 6) servers + a derived view over the moved bytes ------------------------
rep('''META=127.0.0.1:15230; DATA=127.0.0.1:15231
wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
$Y meta-server --meta-dir .itest/home --listen $META > .itest/meta_srv.log 2>&1 &
MSP=$!
$Y data-server --listen $DATA --data-dir .itest/home > .itest/data_srv.log 2>&1 &
DSP=$!
wait_port $META && wait_port $DATA || bad "import-suite servers failed to start"
''',
'''wait_port(){ local a=$1; for _ in $(seq 1 100); do (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null && return 0; sleep 0.05; done; return 1; }
if [ "$EARLY" != 1 ]; then
    $Y meta-server --meta-dir .itest/home --listen $META > .itest/meta_srv.log 2>&1 &
    MSP=$!
    $Y data-server --listen $DATA --data-dir .itest/home > .itest/data_srv.log 2>&1 &
    DSP=$!
else
    # The pair `fdb_up` started serves EVERY view of its control plane, and the
    # trees it points at are the cluster's, so a second pair over a second root
    # would only split the definitions (which stay files — C6) between two
    # authorities. The suite's own section 2 already mounted through this pair.
    ok "the run's existing meta/data server pair is the one under test"
fi
wait_port $META && wait_port $DATA || bad "import-suite servers failed to start"
''')

rep('''$Y view --meta-dir .itest/home define --name albumview --base legacy --rules .itest/album.layout > .itest/def.log 2>&1 \\
  && ok "define derived view over the imported base" || bad "define failed: $(cat .itest/def.log)"

mkdir -p .itest/mnt_r .itest/mnt_v
$Y mount --view legacy   -m .itest/mnt_r --meta-server $META --data-servers $DATA --foreground > .itest/r.log 2>&1 &
RP=$!
$Y mount --view albumview -m .itest/mnt_v --meta-server $META --data-servers $DATA --foreground > .itest/v.log 2>&1 &
VP=$!
sleep 2
R=.itest/mnt_r; V=.itest/mnt_v
''',
'''$Y view $VDIR define --name albumview --base $VBASE --rules .itest/album.layout > .itest/def.log 2>&1 \\
  && ok "define derived view over the imported base" || bad "define failed: $(cat .itest/def.log)"

mkdir -p .itest/mnt_r .itest/mnt_v
$Y mount --view $VIEW    -m .itest/mnt_r --meta-server $META --data-servers $DATA --foreground > .itest/r.log 2>&1 &
RP=$!
$Y mount --view albumview -m .itest/mnt_v --meta-server $META --data-servers $DATA --foreground > .itest/v.log 2>&1 &
VP=$!
sleep 2
R=.itest/mnt_r; V=.itest/mnt_v
if [ "$BACKEND" = fdb ]; then
    mnt_gate .itest/mnt_r || bad ".itest/mnt_r never came up"
    mnt_gate .itest/mnt_v || bad ".itest/mnt_v never came up"
fi
''')

# ---- 7) guards ---------------------------------------------------------------
rep('''echo "===== 7) guards ====="
$Y import --view legacy --source .itest/does-not-exist > .itest/g1.log 2>&1
[ $? -ne 0 ] && grep -qi "no such file" .itest/g1.log && ok "missing source rejected" || bad "missing source should fail: $(cat .itest/g1.log)"
$Y import --view legacy --source .itest/src/docs/quarter.txt > .itest/g2.log 2>&1
[ $? -ne 0 ] && grep -q "must be a directory" .itest/g2.log && ok "file source rejected" || bad "file source should fail: $(cat .itest/g2.log)"
# default prefix = source dir name
$Y import --view legacy --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1 \\
  && [ -e .itest/home/meta/legacy.db ] && ok "explicit alternate prefix imports" || bad "alt prefix import failed: $(cat .itest/g3.log)"
# importing INTO a derived (virtual) name would write a shadow db nothing reads
$Y import --view albumview --source .itest/src > .itest/g4.log 2>&1
[ $? -ne 0 ] && grep -qF "derived (virtual) view" .itest/g4.log \\
  && ok "import refuses a derived (virtual) view name" || bad "import should refuse derived name: $(cat .itest/g4.log)"
''',
'''echo "===== 7) guards ====="
# The two input guards are backend-blind on purpose: the tool checks what it was
# pointed at before it asks which deployment it is in, so an `fdb://` invocation
# reports a missing or non-directory source with the SAME words. That ordering is
# asserted, not assumed — a refusal that came first would hide a bad --source.
$Y import $MSARG --view $VIEW --source .itest/does-not-exist > .itest/g1.log 2>&1
[ $? -ne 0 ] && grep -qi "no such file" .itest/g1.log \\
  && ok "missing source rejected (before the backend is even consulted)" \\
  || bad "missing source should fail: $(cat .itest/g1.log)"
! grep -q "local SQLite view db" .itest/g1.log \\
  && ok "a bad --source is reported as the source problem it is, not as a backend refusal" \\
  || bad "the input guards lost to the backend check: $(cat .itest/g1.log)"
$Y import $MSARG --view $VIEW --source .itest/src/docs/quarter.txt > .itest/g2.log 2>&1
[ $? -ne 0 ] && grep -q "must be a directory" .itest/g2.log && ok "file source rejected" || bad "file source should fail: $(cat .itest/g2.log)"
if [ "$BACKEND" = fdb ]; then
    # What the remaining two guards become here: the backend boundary is decided
    # before the control plane is consulted, so an alt-prefix import AND an import
    # onto a derived name are both refused with the STORE's words. The layout
    # guards are not weakened — they are unreachable, and the shadow-tree rule
    # they protect (`meta/<view>.db` under a virtual name) has no form to take in
    # a cluster, where a name is only ever a subspace or a definition.
    $Y import $MSARG --view $VIEW --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/g3.log \\
      && ok "an alt-prefix import is refused too (a different prefix does not make it legal)" \\
      || bad "alt prefix should refuse on fdb://: $(cat .itest/g3.log)"
    $Y import $MSARG --view albumview --source .itest/src > .itest/g4.log 2>&1
    [ $? -ne 0 ] && grep -q "local SQLite view db" .itest/g4.log \\
      && ok "an import onto a derived name is stopped at the backend boundary" \\
      || bad "derived name should refuse on fdb://: $(cat .itest/g4.log)"
    [ ! -e .itest/fdbhome/meta/albumview.db ] && [ ! -e .itest/fdbhome/meta/$VIEW.db ] \\
      && ok "no shadow tree appeared in the cluster root either" || bad "a refused import wrote a local db"
else
    # default prefix = source dir name
    $Y import --view legacy --source .itest/src/docs --prefix /docs2 > .itest/g3.log 2>&1 \\
      && [ -e .itest/home/meta/legacy.db ] && ok "explicit alternate prefix imports" || bad "alt prefix import failed: $(cat .itest/g3.log)"
    # importing INTO a derived (virtual) name would write a shadow db nothing reads
    $Y import --view albumview --source .itest/src > .itest/g4.log 2>&1
    [ $? -ne 0 ] && grep -qF "derived (virtual) view" .itest/g4.log \\
      && ok "import refuses a derived (virtual) view name" || bad "import should refuse derived name: $(cat .itest/g4.log)"
fi
''')

io.open(P, 'w', encoding='utf-8').write(s)
print('import part3 ok, %d -> %d bytes' % (len(orig), len(s)))
