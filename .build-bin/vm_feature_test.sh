#!/bin/bash
# One-click functional tour of GroveFS against the live VM cluster.
# Non-destructive reads + semantics probes + a mint-then-revoke API-key round-trip.
# Assumes vm_redeploy.sh all + vm_add_demo_views.sh have run (alice/team-fork/nomore mounted).
#
#   bash vm_feature_test.sh
#
# Uploads a probe to the client, runs it, then scores each tagged line. Exit 0 iff all pass.
set -uo pipefail
SSH="ssh -o BatchMode=yes -o ConnectTimeout=8"
CLIENT=gfs-client

cat > /tmp/_ft_probe.sh <<'PROBE'
#!/bin/bash
G=/usr/local/bin/grovefs; DB=/home/hanjun/meta
A=/home/hanjun/mnt; F=/home/hanjun/mnt_fork; L=/home/hanjun/mnt_lens
# A. POSIX/FUSE on base alice
cd "$A" || exit 9
head -c 200000 /dev/urandom > _t.bin
sz0=$(stat -c %s _t.bin); : > _t.bin; sz1=$(stat -c %s _t.bin)
echo "A1_trunc:$sz0,$sz1"
ln _t.bin _hl2 2>/dev/null; echo "A2_nlink:$(stat -c %h _t.bin)"
ln -sfn README.txt _sl; echo "A3_link:$(readlink _sl)"
python3 - "$A" <<'PY'
import os,fcntl,sys
p=sys.argv[1]+"/_lock"; open(p,"wb").write(b"x"*8)
fa=os.open(p,os.O_RDWR); fb=os.open(p,os.O_RDWR)
fcntl.flock(fa,fcntl.LOCK_EX)
try:
    fcntl.flock(fb,fcntl.LOCK_EX|fcntl.LOCK_NB); print("A4_flock=GRANTED_BAD")
except OSError: print("A4_flock=EAGAIN_OK")
fcntl.flock(fa,fcntl.LOCK_UN); os.close(fa); os.close(fb)
PY
dd if=_t.bin of=/dev/null bs=64k iflag=direct count=1 status=none 2>/dev/null; echo "A5_odirect:$?"
rm -f _t.bin _hl2 _sl _lock
# B. views
echo "B1_fork:$(head -c 14 "$F/README.txt" 2>&1)"
(echo zzz >> "$F/README.txt" 2>/dev/null && echo "B2_cwrite=BAD") || echo "B2_cwrite=EROFS_OK"
mkdir "$F/_nd$$" 2>/dev/null && echo "B3_mkdir=OK" || echo "B3_mkdir=BAD"; rmdir "$F/_nd$$" 2>/dev/null
echo "B4_lens_dsets:$(ls "$L" | grep -c datasets)"
echo "B5_lens_readme:$(ls "$L" | grep -c README.txt)"
(echo z > "$L/_x$$" 2>/dev/null && echo "B6_lwrite=BAD") || echo "B6_lwrite=RO_OK"; rm -f "$L/_x$$" 2>/dev/null
# C. api key round-trip (local db-path op, no token needed)
OUT=$($G apikey --meta-dir "$DB" create --label _ftest 2>&1)
ID=$(printf '%s\n' "$OUT" | sed -n 's/^[[:space:]]*id:[[:space:]]*//p' | head -1)
echo "C1_created:${ID:-none}"
$G apikey --meta-dir "$DB" set-ro --id "$ID" >/dev/null 2>&1 && echo "C2_setro=OK" || echo "C2_setro=BAD"
$G apikey --meta-dir "$DB" revoke --id "$ID" >/dev/null 2>&1 && echo "C3_revoke=OK" || echo "C3_revoke=BAD"
# D. data plane
$G view --meta-dir "$DB" export --view alice --out /tmp/_al.json 2>&1 | tail -1 | sed 's/^/D1_export:/'
echo "D2_rows:$(wc -l < /tmp/_al.json 2>/dev/null)"; rm -f /tmp/_al.json
sudo sh -c 'set -a; . /etc/grovefs/cluster.env; set +a; /usr/local/bin/grovefs du --view alice --total' 2>&1 | tail -1 | sed 's/^/D3_du:/'
# E. caps echo
for u in grovefs-mount-alice grovefs-mount-team-fork; do
  printf 'E_caps[%s]= ' "$u"; sudo journalctl -u "$u".service -o cat --no-pager 2>/dev/null | grep 'caps granted' | tail -1 | grep -o 'ATOMIC_O_TRUNC=[a-z]*'
done
PROBE

scp -q /tmp/_ft_probe.sh $CLIENT:/tmp/_ft_probe.sh
R=$($SSH $CLIENT 'bash /tmp/_ft_probe.sh 2>&1; rm -f /tmp/_ft_probe.sh' 2>&1 | grep -vE 'setlocale|Pseudo-terminal|stdin: is')

pass=0; fail=0
chk() { if printf '%s\n' "$R" | grep -qE "$2"; then printf '  [PASS] %s\n' "$1"; pass=$((pass+1)); else printf '  [FAIL] %s  (raw: %s)\n' "$1" "$(printf '%s\n' "$R" | grep -F "${3:-$2}" | head -1)"; fail=$((fail+1)); fi; }

echo "================ A. POSIX / FUSE semantics (base alice) ================"
chk "O_TRUNC via '>' leaves size 0 (atomic path)"      '^A1_trunc:[0-9]+,0$'  'A1_trunc:'
chk "hard link nlink=2"                                '^A2_nlink:2$'          'A2_nlink:'
chk "symlink target round-trip"                        '^A3_link:README.txt$'  'A3_link:'
chk "flock LOCK_NB contention refused (EAGAIN)"        'A4_flock=EAGAIN_OK'    'A4_flock'
chk "O_DIRECT read ok"                                 '^A5_odirect:0$'        'A5_odirect:'
echo "================ B. View governance (fork / lens) ================"
chk "fork reads base content"                          '^B1_fork:hello grovefs$' 'B1_fork:'
chk "fork content write refused (EROFS)"               'B2_cwrite=EROFS_OK'      'B2_cwrite'
chk "fork structure write allowed (mkdir)"             'B3_mkdir=OK'             'B3_mkdir'
chk "lens hides /datasets"                             '^B4_lens_dsets:0$'       'B4_lens_dsets:'
chk "lens hides /README.txt"                           '^B5_lens_readme:0$'      'B5_lens_readme:'
chk "derived lens is read-only"                        'B6_lwrite=RO_OK'         'B6_lwrite'
echo "================ C. API key mint / set-ro / revoke ================"
chk "apikey mint produced an id"                       '^C1_created:[0-9a-f]{8}' 'C1_created:'
chk "apikey set-ro in place"                           'C2_setro=OK'             'C2_setro'
chk "apikey revoke"                                    'C3_revoke=OK'            'C3_revoke'
echo "================ D. Data plane (export / du) ================"
chk "view export produced node rows"                   '^D2_rows:[1-9]'          'D2_rows:'
chk "du total (striped, non-zero)"                      '^D3_du:.*[0-9]'          'D3_du:'
echo "================ E. Kernel-adaptive capability echo ================"
chk "ATOMIC_O_TRUNC negotiated+echoed (rw mount)"      'E_caps.*ATOMIC_O_TRUNC=true' 'E_caps'
echo "================ raw probe ================"
printf '%s\n' "$R" | sed 's/^/    /'
echo "================================================================"
echo "RESULT: PASS=$pass FAIL=$fail"
rm -f /tmp/_ft_probe.sh
[ "$fail" -eq 0 ]
