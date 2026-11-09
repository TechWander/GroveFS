#!/bin/bash
# 集群侧取证串行驱动: 一次只跑一个电池, 避免并发互相污染.
# 用法: bash .build-bin/vm_evidence_run.sh [audit|1|2|3|4 ...]
#   不带参数 = 完整序列: audit(前置) -> battery_1..4 -> audit(后置)
# 日志落在宿主 /tmp/vm_evidence/<name>.log, 结果行落在 /tmp/vm_evidence/status
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
export LC_ALL=C
D=/tmp/vm_evidence
mkdir -p "$D"
: > "$D/status"

run() { # run <name> <script>
  local n=$1 f=$2
  echo ">>> $n start $(date +%H:%M:%S)"
  timeout 900 bash "$f" > "$D/$n.log" 2>&1
  local rc=$?
  echo "$n rc=$rc end $(date +%H:%M:%S) bytes=$(wc -c < "$D/$n.log")" >> "$D/status"
  echo "<<< $n rc=$rc"
}

if [ $# -eq 0 ]; then
  set -- audit 1 2 3 4 audit_after
fi
for a in "$@"; do
  case "$a" in
    audit)       run audit_before .build-bin/vm_audit.sh ;;
    audit_after) run audit_after  .build-bin/vm_audit.sh ;;
    *)           run battery_$a   ".build-bin/vm_battery_$a.sh" ;;
  esac
done
echo "DONE $(date +%H:%M:%S)" >> "$D/status"
