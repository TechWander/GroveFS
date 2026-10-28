#!/usr/bin/env bash
# perf_migrate_scale.sh — metadata-only scale measurement for
# `grovefs view export` / `view import` (the numbers quoted in the paper).
#
# What it measures: exporting a multi-thousand-node view tree to the portable
# JSONL file, re-importing it into a DIFFERENT store path, and then serving the
# imported tree over FUSE. No content bytes are transported by export/import,
# so the byte half is placed with `cp -al` here (a same-machine stand-in for
# `rsync -a -H blocks/` on a real migration).
#
# Usage:  bash .build-bin/perf_migrate_scale.sh [NODES_PER_DIR] [NDIRS]
set -u
cd "$(dirname "$0")/.." || exit 1
Y=./target/release/grovefs
PERF=.migtest/perf
NFILES=${1:-100}        # files per directory
NDIRS=${2:-50}          # number of directories
[ -x "$Y" ] || { echo "build first: cargo build --release"; exit 1; }

rm -rf "$PERF"
mkdir -p "$PERF/src_store" "$PERF/dst_store" "$PERF/mnt_dst" "$PERF/seeds"

echo "== seed $((NFILES * NDIRS)) distinct objects in $NDIRS directories =="
python3 - "$PERF/seeds" "$NDIRS" "$NFILES" <<'PY'
import os, sys, hashlib
root, ndirs, nfiles = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
for d in range(1, ndirs + 1):
    p = os.path.join(root, f"obs{d}")
    os.makedirs(p, exist_ok=True)
    for i in range(1, nfiles + 1):
        with open(os.path.join(p, f"img{i:03d}.fits"), "wb") as f:
            f.write(hashlib.sha256(f"{d}-{i}".encode()).digest() * 64)
PY

echo "== register the tree into a physical base view (zero-copy hardlinks) =="
t0=$(date +%s.%N)
$Y --home "$PERF/src_store" import --view big --source "$PERF/seeds" | tail -1
t1=$(date +%s.%N)
printf '   import-onboarding: %.1fs\n' "$(echo "$t1 - $t0" | bc)"

echo "== export metadata of the $((NFILES * NDIRS))-object view =="
t0=$(date +%s.%N)
$Y --home "$PERF/src_store" view export --view big --out "$PERF/big.jsonl" | head -1
t1=$(date +%s.%N)
printf '   export: %.2fs\n' "$(echo "$t1 - $t0" | bc)"
n_export=$(wc -l < "$PERF/big.jsonl")
sz_export=$(stat -c %s "$PERF/big.jsonl")
sz_db=$(stat -c %s "$PERF/src_store/meta/big.db")
printf '   jsonl: %d lines (header + nodes), %d bytes; view db %d bytes (%.2fx)\n' \
  "$n_export" "$sz_export" "$sz_db" \
  "$(echo "$sz_export / $sz_db" | bc -l)"

echo "== place the content bytes in the target store (rsync -a -H on a real move) =="
t0=$(date +%s.%N)
cp -al "$PERF/src_store/blocks" "$PERF/dst_store/"
t1=$(date +%s.%N)
printf '   blocks placement: %.1fs\n' "$(echo "$t1 - $t0" | bc)"

echo "== import into a NEW store at a different absolute path =="
t0=$(date +%s.%N)
$Y --home "$PERF/dst_store" view import --view big --in "$PERF/big.jsonl" | head -1
t1=$(date +%s.%N)
printf '   import: %.2fs\n' "$(echo "$t1 - $t0" | bc)"

echo "== mount the imported tree and verify it =="
$Y --home "$PERF/dst_store" mount --view big -m "$PERF/mnt_dst" >/dev/null 2>&1
for _ in $(seq 1 100); do grep -q "$PERF/mnt_dst" /proc/self/mounts && break; sleep 0.2; done
t0=$(date +%s.%N)
seen=$(find "$PERF/mnt_dst" -type f | wc -l)
t1=$(date +%s.%N)
printf '   full enumeration of %s files: %.2fs\n' "$seen" "$(echo "$t1 - $t0" | bc)"
a=$(md5sum "$PERF/mnt_dst/seeds/obs7/img042.fits" | cut -d' ' -f1)
b=$(md5sum "$PERF/seeds/obs7/img042.fits" | cut -d' ' -f1)
[ "$a" = "$b" ] && echo "   content identical through the imported view" || echo "   CONTENT MISMATCH"
echo "   mode/owner after import: $(stat -c '%a uid=%u' "$PERF/mnt_dst/seeds/obs7/img042.fits")"
$Y umount -m "$PERF/mnt_dst" >/dev/null 2>&1
echo "== done (scratch kept under $PERF) =="
