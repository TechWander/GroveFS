#!/usr/bin/env bash
# Attribute correctness for CREATE / MKDIR / SYMLINK replies that now carry the
# row inline (no follow-up GETATTR). Exits non-zero on any mismatch.
set -u
export LC_ALL=C   # `stat -c %F` is localized otherwise
umask 022         # so the expected modes below are the defaults, not this shell's
cd "$(dirname "$0")/.." || exit 1
R=.probe_attr; BIN=./target/release/grovefs; PORT=$((17600 + RANDOM % 300))
rm -rf $R; mkdir -p $R/home $R/mnt
"$BIN" view --meta-dir $R/home create --view v1 >/dev/null || exit 1
"$BIN" meta-server --meta-dir $R/home --listen 127.0.0.1:$PORT --token t \
    </dev/null >$R/meta.log 2>&1 &
MP=$!
sleep 1.5
"$BIN" mount --view v1 -m $R/mnt --meta-server 127.0.0.1:$PORT --token t \
    --foreground --threads 2 </dev/null >$R/mnt.log 2>&1 &
sleep 1.5
ls $R/mnt >/dev/null 2>&1 || { echo MOUNT_FAILED; exit 1; }

fail=0
mkdir -p $R/mnt/d/sub                     || fail=1
: > $R/mnt/a.txt                          || fail=1
ln -s a.txt $R/mnt/l                      || fail=1
ln $R/mnt/a.txt $R/mnt/h.txt              || fail=1
chmod 640 $R/mnt/a.txt                    || fail=1

echo "--- ls -l ---"
ls -l $R/mnt

check() { # desc expected actual
    if [ "$2" = "$3" ]; then echo "  OK  $1 = $3"; else echo "  BAD $1: expected $2 got $3"; fail=1; fi
}
check 'a.txt type'  'regular empty file' "$(stat -c %F $R/mnt/a.txt)"
check 'a.txt mode'  '640'          "$(stat -c %a $R/mnt/a.txt)"
check 'd type'      'directory'    "$(stat -c %F $R/mnt/d)"
check 'd mode'      '755'          "$(stat -c %a $R/mnt/d)"
check 'd nlink'     '3'            "$(stat -c %h $R/mnt/d)"
check 'l type'      'symbolic link' "$(stat -c %F $R/mnt/l)"
check 'l target'    'a.txt'        "$(readlink $R/mnt/l)"
check 'h inode'     "$(stat -c %i $R/mnt/a.txt)" "$(stat -c %i $R/mnt/h.txt)"
check 'a nlink'     '2'            "$(stat -c %h $R/mnt/a.txt)"

# Re-read through the kernel from a fresh page: attrs must match what CREATE said.
sync
check 'a size after trunc' '0' "$(stat -c %s $R/mnt/a.txt)"

cd - >/dev/null
fusermount -u $R/mnt 2>/dev/null
kill $MP 2>/dev/null
rm -rf $R
[ $fail -eq 0 ] && echo 'ATTRIBUTE CHECK PASSED' || echo 'ATTRIBUTE CHECK FAILED'
exit $fail
