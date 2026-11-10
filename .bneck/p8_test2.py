#!/usr/bin/env python3
# USAGE §6.3/§12.1 states two things about `--engine-idle-secs` on this backend
# ("Spec P2 约束 4 的重新判定"): it has nothing but memory to reclaim, and the
# session gate still protects a mounted view. A manual probe on this host measured
# both (0 eviction lines while mounted at a 3s threshold; `evicted 1 idle
# engine(s)` after the unmount), so the claim is true — but it lived only in a
# throwaway script. Registering it as section 13 makes it a re-runnable judgement
# and adds the third half the docs promise: the evicted view reopens transparently.
F = '/data/grovefs/grovefs/.build-bin/test_fdb.sh'
ANCHOR = 'for q in $MP2 $MP3; do kill $q 2>/dev/null; wait $q 2>/dev/null; done'

BLOCK = r'''
echo "===== 13) --engine-idle-secs here: memory reclaimed, session gate held, view reopens ====="
# Spec P2 re-adjudicated this knob for the third backend: with no per-view
# connection pool there is nothing to check back in, so eviction retires an
# in-process engine (block allocator, readdir checkpoints, last-touch stamp) and
# the next op reopens it. Three assertions, on a dedicated server pair so the
# 3s threshold cannot be confused with the main run's default:
#   a) a MOUNTED view is never evicted (the `subs` gate is not backend-specific);
#   b) after the unmount the idle engine IS retired;
#   c) the same view serves its data again through a fresh mount — the property
#      that makes the knob invisible to clients.
META_EV=127.0.0.1:15196
DATA_EV=127.0.0.1:15197
for a in $META_EV $DATA_EV; do
    (echo >/dev/tcp/${a%%:*}/${a##*:}) 2>/dev/null \
        && { bad "port $a is already listening; section 13 would talk to a stranger's server"; }
done
V_EV="e$(date +%s)"          # its own subspace: nothing here touches sections 1-12
mkdir -p $S/ev/data $S/ev/home $S/mnt_ev
$Y view --meta-store "$FDBURI" --meta-dir $S/ev/home create --view "$V_EV" > $S/ev/create.log 2>&1 \
    && ok "provisioned a scratch subspace for the eviction check" \
    || { bad "scratch subspace failed"; cat $S/ev/create.log; }
$Y data-server --listen $DATA_EV --data-dir $S/ev/data > $S/ev/data.log 2>&1 & DPE=$!; BG="$BG $DPE"
$Y meta-server --listen $META_EV --home $S/ev/home --meta-store "$FDBURI" \
    --engine-idle-secs 3 > $S/ev/meta.log 2>&1 & MPE=$!; BG="$BG $MPE"
wait_port $META_EV || { bad "section 13's meta-server never bound"; tail -3 $S/ev/meta.log; }
wait_port $DATA_EV || { bad "section 13's data-server never bound"; tail -3 $S/ev/data.log; }
grep -q "metadata backend foundationdb" $S/ev/meta.log \
    && ok "the probe server banners the foundationdb backend" || bad "probe server banner missing: $(head -3 $S/ev/meta.log)"
$Y --home $S/ev/home mount --view "$V_EV" -m $S/mnt_ev --meta-server $META_EV \
    --data-servers $DATA_EV --foreground > $S/ev/mnt.log 2>&1 & MNE=$!; BG="$BG $MNE"
mnt_gate $S/mnt_ev || { bad "the probe view never mounted"; cat $S/ev/mnt.log; }
T sh -c "printf x > $S/mnt_ev/f" && ok "wrote through the probe mount" || bad "probe mount not writable"
sleep 8                      # 3s threshold, comfortably exceeded, WHILE the session is held
chk "$(grep -c 'evicted' $S/ev/meta.log)" "0" "a mounted view is not evicted on this backend either — the session gate is not backend-specific"
fusermount -u $S/mnt_ev 2>/dev/null; kill $MNE 2>/dev/null; wait $MNE 2>/dev/null
for _ in $(seq 1 40); do grep -q 'evicted' $S/ev/meta.log && break; sleep 0.5; done
grep -q 'evicted 1 idle engine' $S/ev/meta.log \
    && ok "once the session is gone the idle engine IS retired (what is reclaimed here is memory, not a connection pool)" \
    || bad "no eviction line after the idle threshold: $(tail -3 $S/ev/meta.log)"
$Y --home $S/ev/home mount --view "$V_EV" -m $S/mnt_ev --meta-server $META_EV \
    --data-servers $DATA_EV --foreground > $S/ev/mnt2.log 2>&1 & MNE2=$!; BG="$BG $MNE2"
mnt_gate $S/mnt_ev || { bad "the evicted view did not reopen on the next mount"; cat $S/ev/mnt2.log; }
chk "$(T cat $S/mnt_ev/f)" "x" "the reopened view serves the same object — eviction is invisible to clients"
fusermount -u $S/mnt_ev 2>/dev/null; kill $MNE2 2>/dev/null; wait $MNE2 2>/dev/null
kill $MPE $DPE 2>/dev/null; wait $MPE 2>/dev/null; wait $DPE 2>/dev/null'''


def main():
    txt = open(F, encoding='utf-8').read()
    assert '===== 13) --engine-idle-secs' not in txt, 'already applied'
    lines = txt.split('\n')
    idx = [i for i, l in enumerate(lines) if l == ANCHOR]
    assert len(idx) == 1, 'anchor hit %d' % len(idx)
    lines[idx[0] + 1:idx[0] + 1] = BLOCK.split('\n')
    open(F, 'w', encoding='utf-8').write('\n'.join(lines))
    print('test_fdb.sh: section 13 (idle-eviction semantics) added')


main()
