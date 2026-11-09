set -u
B=/data/grovefs/.frok; rm -rf "$B"; mkdir -p "$B/home" "$B/base" "$B/fmnt"
export GROVEFS_HOME="$B/home"; G=./target/release/grovefs
"$G" init --view base >/dev/null 2>&1
"$G" mount --view base --mountpoint "$B/base" --foreground >/dev/null 2>&1 & BP=$!; sleep 2
echo "content" > "$B/base/f.txt"; sync; sleep 1
"$G" umount "$B/base" >/dev/null 2>&1; kill "$BP" 2>/dev/null; sleep 1
"$G" view fork --name fview --base base 2>&1 | head -1
"$G" mount --view fview --mountpoint "$B/fmnt" --foreground --stats >"$B/fmnt.log" 2>&1 & FP=$!; sleep 2
echo "caps: $(grep 'FUSE caps granted' "$B/fmnt.log")"
if : > "$B/fmnt/f.txt" 2>"$B/e1"; then echo "trunc: rc=0 UNEXPECTED"; else echo "trunc: rc=? err=$(cat "$B/e1")"; fi
if echo x >> "$B/fmnt/f.txt" 2>"$B/e2"; then echo "append: rc=0 UNEXPECTED"; else echo "append: err=$(cat "$B/e2")"; fi
echo "read: $(cat "$B/fmnt/f.txt")"
echo "size-after-failed-trunc: $(stat -c%s "$B/fmnt/f.txt")"
"$G" umount "$B/fmnt" >/dev/null 2>&1; kill "$FP" 2>/dev/null; sleep 1; fusermount -u "$B/fmnt" 2>/dev/null; rm -rf "$B"
