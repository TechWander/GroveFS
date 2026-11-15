#!/usr/bin/env bash
# Shared helpers for the `.build-bin/test_*.sh` e2e suites. This file is
# SOURCED, never run: it holds the two small process/mount primitives several
# suites would otherwise copy verbatim.
#
# kp/wk: never hand `kill` an empty or zero placeholder. PID 0 means the
# CALLER'S WHOLE PROCESS GROUP, so a suite whose server pids stay unset on one
# code path would signal the harness that launched it — and print a green
# SUMMARY on its way out with exit 143 (this ate a real debugging hour).
kp(){ local p; for p in "$@"; do case "$p" in ''|0|*[!0-9]*) continue ;; esac; kill "$p" 2>/dev/null || true; done; }
wk(){ local p; for p in "$@"; do case "$p" in ''|0|*[!0-9]*) continue ;; esac; wait "$p" 2>/dev/null || true; done; }

# mnt_gate DIR: a mount that never came up must abort the section, not let the
# following writes land in a bare directory and print green numbers about
# nothing.
mnt_gate(){ local mp="$PWD/$1"; for _ in $(seq 1 60); do
    findmnt -rn --mountpoint "$mp" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }
