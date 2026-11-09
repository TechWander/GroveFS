#!/bin/bash
# 仓库侧回归: clippy + 集成测试, 输出照抄进报告.
set -uo pipefail
cd /data/grovefs/grovefs
export PKG_CONFIG="$PWD/.build-bin/pkg-config"
echo "##### cargo clippy --release --all-targets #####"
cargo clippy --release --all-targets 2>&1 | tail -30
echo "CLIPPY_RC=${PIPESTATUS[0]}"
echo "##### cargo test --release --tests #####"
cargo test --release --tests 2>&1 | grep -E '^(test |running|error|warning: unused|test result|failures:)' | tail -80
echo "TEST_RC=${PIPESTATUS[0]}"
