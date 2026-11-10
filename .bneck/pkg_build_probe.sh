#!/usr/bin/env bash
# Does the BUILD really need pkg-config + libfuse headers, as the README once
# claimed? Poison the lookup (PKG_CONFIG=/nonexistent) and build both shapes from
# a clean target dir, then read the product's DT_NEEDED. No pkg-config shim, no
# FDB_CLIENT_LIB_PATH beyond the one the fdb feature needs to link libfdb_c.
set -u
cd /data/grovefs/grovefs
SRC=/data/grovefs/.fdbbin
t0=$(date +%s)
echo "### 0) dependency graph: does any crate ask pkg-config for fuse?"
cargo tree --features fdb 2>/dev/null | grep -ciE 'pkg-config|libfuse-sys|fuse-rs' || true
echo "fuser features in this build (default must be empty):"
grep -nA3 '^\[features\]' "$(ls -d $HOME/.cargo/registry/src/*/fuser-0.18.0 2>/dev/null | head -1)/Cargo.toml" 2>/dev/null | head -6
echo

echo "### 1) default release build, PKG_CONFIG=/nonexistent"
rm -rf target/pp_plain
if env -i PATH="$HOME/.cargo/bin:/usr/bin:/bin" HOME="$HOME" \
        PKG_CONFIG=/nonexistent CARGO_HOME="$HOME/.cargo" \
        cargo build --release --target-dir target/pp_plain > /tmp/pp_plain.log 2>&1; then
    echo "  rc=0  wall=$(( $(date +%s) - t0 ))s"
else
    echo "  rc=FAIL — tail:"; tail -12 /tmp/pp_plain.log
fi
echo "  DT_NEEDED:"; objdump -p target/pp_plain/release/grovefs 2>/dev/null | awk '/NEEDED/{print "   ",$2}'

t1=$(date +%s)
echo "### 2) --features fdb release build, same poison (+ FDB_CLIENT_LIB_PATH for the link step)"
rm -rf target/pp_fdb
if env -i PATH="$HOME/.cargo/bin:/usr/bin:/bin" HOME="$HOME" \
        PKG_CONFIG=/nonexistent CARGO_HOME="$HOME/.cargo" \
        FDB_CLIENT_LIB_PATH="$SRC/lib" FDB_CLIENT_INCLUDE_PATH="$SRC/include" \
        cargo build --release --features fdb --target-dir target/pp_fdb > /tmp/pp_fdb.log 2>&1; then
    echo "  rc=0  wall=$(( $(date +%s) - t1 ))s"
else
    echo "  rc=FAIL — tail:"; tail -12 /tmp/pp_fdb.log
fi
echo "  DT_NEEDED:"; objdump -p target/pp_fdb/release/grovefs 2>/dev/null | awk '/NEEDED/{print "   ",$2}'
echo "  any fuse lib linked? $(objdump -p target/pp_fdb/release/grovefs 2>/dev/null | grep -ci fuse)"
echo "  product runs without a fuse package at all? $(env -i PATH=/usr/bin:/bin LD_LIBRARY_PATH=$SRC/lib target/pp_fdb/release/grovefs --version 2>&1 | head -1)"
echo "PKG_BUILD_PROBE_DONE"
