#!/usr/bin/env bash
# Terminal check for the FDB delivery package: unpack the tarball somewhere fresh
# and prove, with a completely empty environment, that nothing outside the package
# is needed. Run as: bash .bneck/pkg_final.sh
set -eu
SRC=/data/grovefs/grovefs/dist
W=/tmp/pkgfinal
rm -rf "$W"; mkdir -p "$W"
cd "$W"
cp -f "$SRC/grovefs-fdb-experimental-linux-x86_64.tar.gz"{,.sha256} .
echo "=== 0) tarball fingerprint ==="
sha256sum -c grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256
tar xzf grovefs-fdb-experimental-linux-x86_64.tar.gz
cd grovefs-fdb-experimental-linux-x86_64
echo "=== A) per-file checksums in a freshly unpacked copy ==="
sha256sum -c SHA256SUMS.txt
echo "=== B) ldd with a completely empty environment ==="
env -i PATH=/usr/bin:/bin ldd ./grovefs
echo "=== C) --version + fdbcli status, empty env ==="
env -i PATH=/usr/bin:/bin ./grovefs --version
env -i PATH=/usr/bin:/bin FDB_CLUSTER_FILE=/data/grovefs/.fdbtest/test.cluster \
    ./fdb/fdbcli --exec "status minimal"
echo "=== D) no hardcoded dependency on the build machine's lib dir in the .so path ==="
env -i PATH=/usr/bin:/bin ldd ./grovefs | grep -c '/data/grovefs/.fdbbin' | sed 's/^/hits-on-build-machine-path: /'
echo PKG_FINAL_DONE
