#!/usr/bin/env bash
# Recompute the package's per-file checksums after a text change, repack the
# tarball, and print the new outer fingerprint so the docs can quote it.
set -eu
P=/data/grovefs/grovefs/dist/grovefs-fdb-experimental-linux-x86_64
cd "$P"
find . -type f ! -name SHA256SUMS.txt | sort | sed 's|^\./||' | xargs sha256sum > SHA256SUMS.txt
sha256sum -c SHA256SUMS.txt
cd ..
rm -f grovefs-fdb-experimental-linux-x86_64.tar.gz
tar czf grovefs-fdb-experimental-linux-x86_64.tar.gz grovefs-fdb-experimental-linux-x86_64
sha256sum grovefs-fdb-experimental-linux-x86_64.tar.gz > grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256
cat grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256
stat -c 'size=%s bytes' grovefs-fdb-experimental-linux-x86_64.tar.gz
echo PKG_REPACK_DONE
