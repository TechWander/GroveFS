set -u
cd "$(dirname "$0")/.."
Y=./target/release/grovefs
BASE=$PWD/.setrotest/home
rm -rf .setrotest; mkdir -p .setrotest/home
export GROVEFS_HOME=$BASE
cat > .setrotest/t.manifest <<'M'
mkdir docs
touch docs/a.md
M
$Y init --view team --manifest .setrotest/t.manifest >/dev/null 2>&1 || { echo init FAIL; exit 1; }
$Y apikey --meta-dir "$BASE" create --label m --for mount --view team > .setrotest/ak.log 2>&1
ID=$(sed -n 's/^  id:    //p' .setrotest/ak.log); KEY=$(sed -n 's/^  key:   //p' .setrotest/ak.log)
$Y mount --view team -m .setrotest/mnt --api-key "$ID:$KEY" --foreground > .setrotest/mnt.log 2>&1 &
MP=$!
for i in $(seq 30); do mountpoint -q .setrotest/mnt && break; sleep 0.2; done
mountpoint -q .setrotest/mnt || { echo mount FAIL; cat .setrotest/mnt.log; exit 1; }
echo "--- baseline append (rw):"
bash -c "echo first >> .setrotest/mnt/docs/a.md"; echo "rc=$?"
$Y apikey --meta-dir "$BASE" set-ro --id "$ID" > .setrotest/setro.log 2>&1; cat .setrotest/setro.log
sleep 1.5
echo "--- after set-ro, shell append:"
bash -c "echo second >> .setrotest/mnt/docs/a.md" 2>.setrotest/err.txt; echo "rc=$? stderr=$(cat .setrotest/err.txt)"
sync
sleep 1.5
echo "--- content after append+recheck window:"; cat .setrotest/mnt/docs/a.md
echo "--- python write ladder after set-ro:"
python3 - <<'P'
import os
fd=os.open('.setrotest/mnt/docs/a.md', os.O_WRONLY|os.O_APPEND)
try:
    n=os.write(fd,b'x'); print(f"  write() ok returned {n}")
except OSError as e:
    print(f"  write() errno={e.errno}")
try:
    os.fsync(fd); print("  fsync() ok")
except OSError as e:
    print(f"  fsync() errno={e.errno}")
try:
    os.close(fd); print("  close() ok")
except OSError as e:
    print(f"  close() errno={e.errno}")
P
echo "--- final content (blocks truth):"; cat .setrotest/mnt/docs/a.md
fusermount -u .setrotest/mnt 2>/dev/null; kill $MP 2>/dev/null; wait $MP 2>/dev/null
rm -rf .setrotest
