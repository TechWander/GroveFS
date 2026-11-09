set -u
cd /data/grovefs/grovefs
Y=./target/release/grovefs
rm -rf .f63; mkdir -p .f63/home .f63/src .f63/mnt
export GROVEFS_HOME=$PWD/.f63/home
printf 'first\nsecond\n' > .f63/src/f.txt
$Y init --view base >/dev/null
$Y import --view base --source $PWD/.f63/src >/dev/null
$Y view fork --name f --base base | head -1
$Y mount --view f --mountpoint .f63/mnt --foreground > .f63/mnt.log 2>&1 &
PM=$!
for i in $(seq 20); do mountpoint -q .f63/mnt && break; sleep 0.3; done
M=.f63/mnt
echo "--- 0) files in fork mount:"; find $M -type f
F=$(find $M -type f | head -1)
echo "--- 0b) baseline content of $F:"; cat "$F"
echo "--- 1) shell append to EXISTING file (expect rc=0 FALSE SUCCESS, bytes NOT landed):"
sh -c "echo injected >> '$F'"; echo "rc=$?"
sync; sleep 0.5; echo "    content after append:"; cat "$F"
echo "--- 2) python: write then fsync then close (failure timing ladder):"
python3 - "$F" <<'PY'
import os, sys
p = sys.argv[1]
fd = os.open(p, os.O_WRONLY | os.O_APPEND)
try:
    n = os.write(fd, b'via-write\n')
    print(f"  write() returned {n}   <-- kernel page cache accepted it")
except OSError as e:
    print(f"  write() errno={e.errno} {os.strerror(e.errno)}")
try:
    os.fsync(fd); print("  fsync() ok  <-- UNEXPECTED")
except OSError as e:
    print(f"  fsync() errno={e.errno} {os.strerror(e.errno)}   <-- first visible failure")
try:
    os.close(fd); print("  close() ok")
except OSError as e:
    print(f"  close() errno={e.errno} {os.strerror(e.errno)}")
PY
echo "--- 3) O_TRUNC open on EXISTING file:"
python3 - "$F" <<'PY'
import os, sys
try:
    fd = os.open(sys.argv[1], os.O_WRONLY | os.O_TRUNC)
    print("  O_TRUNC open ok <-- UNEXPECTED"); os.close(fd)
except OSError as e:
    print(f"  O_TRUNC open errno={e.errno} {os.strerror(e.errno)}   <-- fails at open")
PY
echo "--- 4) bytes actually on the object (blocks side):"; find .f63/home/blocks -type f | xargs -r cat
fusermount -u .f63/mnt; kill $PM 2>/dev/null; wait 2>/dev/null
