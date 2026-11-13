#!/usr/bin/env bash
# GroveFS Repo Wiki 漂移检查 —— 代码改了、wiki 没跟上时跑一遍。
#
# 为什么需要它（wiki 的 IDE 自动更新为何不可靠）：
#   Repo Wiki 绑定「打开的工作区文件夹」做提交增量。本仓库工作区根 /data/grovefs
#   不是 git 仓库（代码库在其下一层 grovefs/），于是 IDE 把它归到魔术分支
#   `Q0DeR-MaG1C-BrAnCh-Fo2-NoN-GiT-Or-No-C0MmIt`（非 git / 无提交），
#   元数据基线 last_commit_id 恒为全零、增量任务表 0 行 —— 提交驱动的自动重扫
#   从未被武装。要真正自动更新：① 直接把代码目录作为工作区打开，或 ② 在 IDE
#   Wiki 面板手动「重新生成」。在此之前，用本脚本以「代码真值」为准钉住 wiki 事实。
#
# 用法: bash skills/grovefs-docs/scripts/wikicheck.sh [仓库根目录]
#   GROVEFS_BIN=/path/to/grovefs  指定跑 --help 的二进制(默认 <code>/target/release/grovefs)
#   WIKI_DIR=/path/to/.qoder/repowiki/zh/content  指定 wiki 正文目录
# 退出码: 0=干净; 1=有需处理的漂移。
set -uo pipefail

ROOT="${1:-$(cd "$(dirname "$(readlink -f "$0")")/../../.." && pwd)}"   # grovefs 仓库根(代码所在)
WS="$(dirname "$ROOT")"                                                 # 工作区根(.qoder 所在)
BIN="${GROVEFS_BIN:-$ROOT/target/release/grovefs}"
WIKI="${WIKI_DIR:-$(find "$WS/.qoder/repowiki" -maxdepth 3 -type d -name content 2>/dev/null | head -1)}"
rc=0
say(){ printf '\n== %s ==\n' "$1"; }

# ---------- 0. 定位 ----------
say "0. 目标定位"
[ -n "$WIKI" ] && [ -d "$WIKI" ] || { echo "  找不到 wiki content 目录(设 WIKI_DIR)"; exit 1; }
echo "  wiki 正文: $WIKI"
echo "  代码仓库根: $ROOT"
[ -x "$BIN" ] && echo "  二进制: $BIN" || echo "  [警告] 二进制不可执行,§2 参数对账将跳过: $BIN"

# ---------- 1. 自动更新状态诊断(信息项,不影响退出码) ----------
say "1. 自动更新状态诊断(信息项)"
if git -C "$WS" rev-parse --show-toplevel >/dev/null 2>&1; then
  echo "  工作区根 $WS 是 git 仓库 —— 提交驱动增量可能生效"
else
  echo "  工作区根 $WS 不是 git 仓库 → 提交增量无法计算(本脚本替代之)"
fi
meta="$(find "$WIKI/../meta" -name repowiki-metadata.json 2>/dev/null | head -1)"
if [ -n "$meta" ]; then
  base=$(grep -oE '"last_commit_id"[^,]*' "$meta" | sed 's/.*: *"//;s/"//')
  [ "${base:-}" = "0000000000000000000000000000000000000000" ] || [ -z "$base" ] \
    && echo "  基线 last_commit_id 为占位零值(未锚定真实提交)" \
    || echo "  基线 last_commit_id = ${base:0:12}…"
fi
echo "  → 结论:wiki 需靠手动重生成或本脚本收口,勿依赖 IDE 自动更新"

# ---------- 2. 参数对账:代码真值 ↔ wiki ----------
# 规矩:凡 mount / data-server 实际接受的长选项,wiki 正文必须提到(漏=新旋钮没同步);
# 反向:CLI 参考页当成 grovefs 选项写的 --xxx,必须真存在(外部工具/占位除外)。
if [ -x "$BIN" ]; then
  say "2. 参数对账(代码 --help 真值 ↔ wiki 正文)"
  real=$(mktemp); seen=$(mktemp)
  trap 'rm -f "$real" "$seen"' EXIT
  walk(){
    local path="$1" out c
    out=$("$BIN" $path --help 2>&1) || return 0
    printf '%s\n' "$out" | grep -oE -- '--[a-z][a-z0-9-]*' | sed 's/^--//' | sort -u >> "$real"
    local -a subs=()
    while IFS= read -r c; do [ -n "$c" ] && subs+=("$c"); done < <(
      printf '%s\n' "$out" | sed -n '/^Commands:/,/^\(Options\|Arguments\):/p' \
      | awk 'NR>1 && $1 ~ /^[a-z][a-z0-9-]*$/ {print $1}')
    for c in "${subs[@]:-}"; do
      [ -n "$c" ] || continue
      case " $(tr '\n' ' ' < "$seen") " in *" $path $c "*) continue ;; esac
      printf '%s %s\n' "$path" "$c" >> "$seen"; walk "$path $c"
    done
  }
  walk ""; sort -u -o "$real" "$real"
  echo "  真实 flag 总数: $(wc -l < "$real")"

  # 2a. 正向缺口:数据面两命令的 flag,wiki 完全没提 = 需处理
  miss=0
  for sub in mount data-server; do
    "$BIN" $sub --help 2>&1 | grep -oE -- '--[a-z][a-z0-9-]*' | sed 's/^--//' | sort -u | \
    while read -r f; do
      [ -z "$f" ] && continue
      grep -rq -- "--$f" "$WIKI" || echo "MISS $sub $f"
    done
  done > /tmp/wikicheck-miss.txt
  if [ -s /tmp/wikicheck-miss.txt ]; then
    echo "  [需处理] 数据面 flag 在 wiki 中无踪迹:"; sed 's/^/    /' /tmp/wikicheck-miss.txt; miss=1; rc=1
  else
    echo "  [OK] mount / data-server 的每个长选项 wiki 均有提及"
  fi

  # 2b. 反向幽灵(信息项,不影响退出码):wiki 是无状态生成的散文,Markdown 锚点/
  # 断行截断/「未实现」负空间句都会造成假阳性,故只列出不纳入退出码(同 doccheck §1b)。
  say "2b. 疑似幽灵参数(逐条人工确认,不影响退出码)"
  EXT='rsync|systemd|mount|umount|bash|sh|cargo|git|pg_dump|pg_restore|ps|ls|cat|find|stat|strace|dd|lvm|mdadm|scp|ssh|curl|wget|install|timeout|sort|awk|sed|grep|xargs|chmod|chown|mkdir|rm|cp|mv|touch|truncate|df|du|nohup|force'
  # 先取差集,再剔除「与任一真实 flag 互为前缀」的 token(消除 --bw- vs --bw-read、--sync vs --sync-repl、锚点 --data-servers- 等)
  raworph=$(grep -rhoE -- '--[a-z][a-z0-9-]+' "$WIKI/命令行工具参考" 2>/dev/null | sed 's/^--//' | sort -u | comm -23 - "$real")
  ORPH=""
  while IFS= read -r o; do
    [ -n "$o" ] || continue
    echo "$o" | grep -qwE "$EXT" && continue
    clash=0
    while IFS= read -r r; do
      # 任一方向互为前缀即视为同一 flag 的提取伪影(o 截断或 r 带后缀)
      [ "${r#"$o"}" != "$r" ] && { clash=1; break; }   # r 以 o 开头
      [ "${o#"$r"}" != "$o" ] && { clash=1; break; }   # o 以 r 开头
    done < "$real"
    [ "$clash" = 1 ] || ORPH="${ORPH}${o}\n"
  done <<< "$raworph"
  if [ -n "$ORPH" ]; then
    printf '%b' "$ORPH" | sed '/^$/d' | tee /tmp/grovefs-wiki-orphan-flags.txt | sed 's/^/    --/'
  else
    echo "    (无)"
  fi
fi

# ---------- 3. 事实漂移模式(新代码已改口径、wiki 若留旧说法即命中) ----------
say "3. 事实漂移模式(应为 0)"
# 每条正则配一句说明(两数组一一对应,避免与正则里的 | 交替冲突)。
# 新增特性时往这里补一条旧口径模式,并把新旋钮名加入 §3b 的必提清单(或依赖 §2a 自动覆盖)。
RXS=(
  '按固定 ?stripe|固定 ?stripe ?大小|按固定大小的 ?stripe|stripe 为固定|分片大小：默认 16|默认 ?16 ?MiB ?才分块|≥ ?16 ?MiB ?才分块'
  'status=0, val=0, data=\[\]'
  '顺序不符会被 stat 检出|必须保留 N 个 data-server 及顺序|列表顺序与台数一变'
)
WHYS=(
  '分片尺寸现由 --stripe-bytes/GROVEFS_STRIPE_BYTES 逐对象可配,非写死 16MiB'
  'OP_OPEN 握手 ack 的 val 现回传 member_id(NO_MEMBER_ID=u64::MAX 哨兵),不再恒 0'
  '位序约束已不再是唯一口径:成员都带 --member-id 时按身份排序放置(顺序无关、集合不可变),此句未区分两种模式'
)
for i in "${!RXS[@]}"; do
  rx="${RXS[$i]}"; why="${WHYS[$i]}"
  hits=$(grep -rnE "$rx" "$WIKI" 2>/dev/null)
  if [ -n "$hits" ]; then
    printf '  [需处理] %s\n' "$why"
    printf '%s\n' "$hits" | sed 's/^/    /' | cut -c1-160
    rc=1
  fi
done
[ "$rc" = 0 ] && echo "  [OK] 未发现写死尺寸/旧握手等失效口径"

  # 单独的整页级检查:描述当前统一格式的核心页若通篇没有 [:fp],视为未同步
say "3b. 核心页是否已体现统一布局键与成员身份"
# 页名 : 该页必须出现的关键词
CHECKS=(
  "存储后端/分片存储后端.md|fp|member-id|stripe-bytes"
  "数据服务/对象存储系统/分片存储系统.md|fp|member-id|stripe-bytes"
  "存储后端/存储后端接口.md|fp|member-id"
  "数据服务/网络通信协议.md|member_id"
  "命令行工具参考/服务器命令/data-server 命令.md|member-id"
)
for entry in "${CHECKS[@]}"; do
  IFS='|' read -r f kw1 kw2 kw3 <<< "$entry"
  [ -f "$WIKI/$f" ] || continue
  missing=()
  for kw in "$kw1" "$kw2" "$kw3"; do
    [ -n "$kw" ] || continue
    grep -q -- "$kw" "$WIKI/$f" || missing+=("$kw")
  done
  if [ "${#missing[@]}" = 0 ]; then echo "  [OK] $f"; else
    printf '  [需处理] %s 缺少关键词: %s\n' "$f" "${missing[*]}"; rc=1
  fi
done

say "结论"
if [ "$rc" = 0 ]; then echo "  OK —— wiki 与代码事实一致"; else echo "  需处理(见上面 [需处理])"; fi
exit "$rc"
