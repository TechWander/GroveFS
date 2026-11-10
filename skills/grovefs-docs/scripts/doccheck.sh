#!/usr/bin/env bash
# GroveFS 文档口径检查 —— 改过 README.md / USAGE.md / docs.html 后跑一遍。
# 用法: bash skills/grovefs-docs/scripts/doccheck.sh [仓库根目录]
#   GROVEFS_BIN=/path/to/grovefs  指定用来跑 --help 的二进制(默认 target/release/grovefs)
# 退出码: 0=干净; 1=有需处理的命中或结构错误。
set -uo pipefail

# 用 readlink -f 解链:.qoder/skills/ 下是符号链,不解会算错仓库根
ROOT="${1:-$(cd "$(dirname "$(readlink -f "$0")")/../../.." && pwd)}"
DOCS=("$ROOT/README.md" "$ROOT/USAGE.md" "$ROOT/docs.html")
BIN="${GROVEFS_BIN:-$ROOT/target/release/grovefs}"
rc=0
say() { printf '\n== %s ==\n' "$1"; }

# ---------- 1. 变更史叙述 / 已移除开关名 ----------
# 规矩:文档只陈述当前版本。白名单只放行三类合法内容——当前版本内的真实回退语义、
# 运维回滚动作、「未实现(诚实标注)」负空间清单,以及口径条款本身。
HIST='已从代码|已从 CLI|已移除|已被移除|不再支持|不再提供|strict-view|ALLOW_LAZY_VIEW|改造前|改造后|旧版本|与旧版|旧行为|退回旧|新增了|现已支持|已落地|已经落地|gained|now also|now travel|fall back to the old'
WL='未实现|回滚|老服务端|不含该能力位|退化为独立拷贝|退回全量列表|退回通用|招魂|变更叙述|文档口径|不会注入|快照'
say "1. 变更史叙述扫描(应为 0 需处理项)"
hit=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  if printf '%s' "$line" | grep -qE "$WL"; then
    printf '  [放行] %s\n' "${line:0:140}"
  else
    printf '  [需处理] %s\n' "${line:0:140}"; hit=1
  fi
done < <(grep -nE "$HIST" "${DOCS[@]}" 2>/dev/null)
[ "$hit" = 1 ] && rc=1

# 前后对比数字只能靠启发式,且「两种当前档位的差别」(async RPO≈500ms vs --sync-repl 0)
# 属合法行为描述 —— 因此本项不影响退出码,只列出来供人逐条判断。
say "1b. 疑似前后对比数字(逐条人工确认,不影响退出码)"
grep -nE '[0-9](\.[0-9]+)?(s|ms|分钟|倍)[^a-zA-Z]{0,3}(→|->|降到|提到)|降到[ ]?[0-9]|倍提升|基线' "${DOCS[@]}" 2>/dev/null \
  | grep -vE '未实现|文档口径' | sed 's/^/  /'

# ---------- 2. 参数对账 ----------
say "2. 参数对账(文档里的 --xxx 是否真存在于 --help)"
real=$(mktemp); docf=$(mktemp); seen=$(mktemp); dtxt=$(mktemp); neg=$(mktemp)
trap 'rm -f "$real" "$docf" "$seen" "$dtxt" "$neg"' EXIT
orph=/tmp/grovefs-doc-orphan-flags.txt
: > "$seen"
# 递归走 help 树:根 → 每个子命令 → 孙命令(view create 之类)。
# 注意:子命令清单先读进数组再递归,否则递归会截断正在被循环读取的临时文件。
walk() {
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
    printf '%s %s\n' "$path" "$c" >> "$seen"
    walk "$path $c"
  done
}
if [ -x "$BIN" ]; then
  walk ""
  sort -u -o "$real" "$real"
  # 文档侧:先剥掉 docs.html 的 <style> 块(CSS 自定义属性 --accent/--card/… 不是参数)
  for f in "${DOCS[@]}"; do sed '/<style>/,/<\/style>/d' "$f"; done > "$dtxt"
  grep -oE -- '--[a-z][a-z0-9-]*' "$dtxt" | sed 's/^--//' | sort -u > "$docf"
  # 负空间清单(明说「未实现」的那几行)里点名的参数不算臆造
  grep -hE '未实现|别在当前版本上期待' "${DOCS[@]}" 2>/dev/null \
    | grep -oE -- '--[a-z][a-z0-9-]*' | sed 's/^--//' | sort -u > "$neg"
  comm -23 "$docf" "$real" > "$orph"
  comm -23 "$orph" "$neg" > "${orph}.2" && mv "${orph}.2" "$orph"
  # 噪声两类:别家工具的参数(rsync/cargo/systemd/journalctl/firewalld/pg_dump/fdbcli…),
  # 以及文档里的占位与通配写法(--xxx、--bw-*)。
  FOREIGN='delete-after|delete-before|delete-during|exclude|include|inplace|info|progress|partial|times|whole-file|verbose|release|nocapture|bench|features|ignored|now|since|until|no-pager|reload|enable|disable|status|full|state|format|permanent|add-rich-rule|zone|type|set-profile|daemon-reload|json-pretty|exec|xxx|bw-'
  nf=$(grep -cvE "^($FOREIGN)$" "$orph" || true)
  printf '  真实参数 %d 个,文档参数 %d 个,差集 %d 项(其中非外部工具/非负空间 %d 项)\n' \
    "$(wc -l < "$real")" "$(wc -l < "$docf")" "$(wc -l < "$orph")" "$nf"
  printf '  非外部工具、非占位的差集需逐条定性(应为空;完整差集见 %s):\n' "$orph"
  grep -vE "^($FOREIGN)$" "$orph" | sed 's/^/    --/'
  printf '  判据:出现在「未实现(诚实标注)」清单里的名字 = 合法(负空间说明);\n'
  printf '  当前版本根本没有的 grovefs 参数名被当真的写 = 需删改。\n'
  [ "$nf" -gt 0 ] && rc=1
  [ "$(wc -l < "$real")" -lt 40 ] && { printf '  [警告] 真实参数过少,walker 可能没走全(检查 %s)\n' "$BIN"; rc=1; }
else
  printf '  [跳过] 找不到可执行 %s(用 GROVEFS_BIN 指定)\n' "$BIN"
fi

# ---------- 3. docs.html 结构 ----------
say "3. docs.html 标签闭合"
python3 - "$ROOT/docs.html" <<'PY' || rc=1
import sys, html.parser
VOID = {"area","base","br","col","embed","hr","img","input","link","meta","param","source","track","wbr"}
class P(html.parser.HTMLParser):
    def __init__(self): super().__init__(convert_charrefs=True); self.st=[]; self.err=[]
    def handle_starttag(self, t, a):
        if t not in VOID: self.st.append((t, self.getpos()))
    def handle_endtag(self, t):
        if t in VOID: return
        if not self.st: self.err.append("多余 </%s> @%s" % (t, self.getpos())); return
        if self.st[-1][0] == t: self.st.pop(); return
        for i in range(len(self.st)-1, -1, -1):
            if self.st[i][0] == t:
                self.err.append("跨层闭合 </%s> @%s, 未闭: %s" % (t, self.getpos(),
                    [x[0] for x in self.st[i+1:]])); del self.st[i:]; break
        else: self.err.append("无开始标签 </%s> @%s" % (t, self.getpos()))
p = P(); p.feed(open(sys.argv[1], encoding="utf-8").read())
print("  unclosed:", [f"{t}@{pos}" for t, pos in p.st] or "[]")
print("  errors:", p.err or "none")
sys.exit(1 if p.st or p.err else 0)
PY

# ---------- 4. 新增用字校验(抓自打错字) ----------
say "4. 新增用字校验(文档历史上没出现过的汉字 = 逐个确认是否错字)"
if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  python3 - "$ROOT" <<'PY' || rc=1
import subprocess, sys, re
root = sys.argv[1]
files = ["README.md", "USAGE.md", "docs.html"]
diff = subprocess.run(["git", "-C", root, "diff", "-U0", "--"] + files,
                      capture_output=True, text=True).stdout
added = [l[1:] for l in diff.splitlines() if l.startswith("+") and not l.startswith("+++")]
if not added:
    print("  无文档改动,跳过")
    sys.exit(0)
new = set()
for a in added:
    new |= set(re.findall(r"[\u4e00-\u9fff]", a))
old = set()
for f in files:
    r = subprocess.run(["git", "-C", root, "show", f"HEAD:{f}"], capture_output=True, text=True)
    if r.returncode == 0:
        old |= set(re.findall(r"[\u4e00-\u9fff]", r.stdout))
odd = sorted(new - old)
print("  本次新增行里的生面孔汉字:", "".join(odd) or "(无)")
if odd:
    print("  -> 命中不等于错字(新写的句子自然带新字),逐字看一眼再收工;确认无误即可忽略。")
PY
else
  printf '  [跳过] 不在 git 仓库内\n'
fi

say "结论"
if [ "$rc" = 0 ]; then printf '  OK\n'; else printf '  需处理(见上面 [需处理] / 结构错误 / 参数差集)\n'; fi
exit "$rc"
