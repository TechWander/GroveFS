#!/usr/bin/env bash
# Re-run the FDB档 of every parameterised suite against the CURRENT tree, so the
# P4/P5/P8 judgement criteria rest on measurements taken now rather than on the
# numbers recorded when each suite was first parameterised. Sequential on
# purpose: they all mount, and parallel runs would collide on ports.
cd /data/grovefs/grovefs || exit 1
export PKG_CONFIG="$PWD/.build-bin/pkg-config" LC_ALL=C LANG=C
for pair in \
    "LOCKS_BACKEND .build-bin/test_locks.sh" \
    "LAYOUT_BACKEND .build-bin/test_layout.sh" \
    "GOVCHECK_BACKEND .build-bin/test_govcheck.sh" \
    "GOVTIME_BACKEND .build-bin/test_govtime.sh" \
    "APIKEY_BACKEND .build-bin/test_apikey.sh" \
    "MIGRATE_BACKEND .build-bin/test_migrate.sh" \
    "IMPORT_BACKEND .build-bin/test_import.sh"; do
    set -- $pair
    echo "===== $1=fdb  $2"
    env "$1=fdb" timeout 600 bash "$2" 2>&1 | grep -E "^SUMMARY|^  FAIL|pass=.*fail=" | tail -3
    echo "      rc=${PIPESTATUS[0]}"
done
