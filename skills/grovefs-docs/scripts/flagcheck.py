#!/usr/bin/env python3
"""文档命令行示例的**按子命令**参数对账。

§2 的集合成员检查只问「这个 flag 在整棵 help 树里存在吗」,于是两类判断它给不出:
某条子命令根本没有的 flag 被写进它的示例(实测 clap 当场报 `unexpected
argument`),以及 `.bneck` 配方脚本自己的合法参数被读成臆造参数。这里逐条示例问
「这个 flag 写在**哪条命令**上、那条命令接不接它」——真值取
`<bin> <命令路径> --help` 的 Options 段,因为 clap 在那里列出的正是这条命令实际
接受的一组参数:全局参数(如 `--home`)出现在每个子命令的 Options 段里,父命令特
有的非全局参数(`view --meta-dir`)不出现在子命令(`view move`)的段里 —— 与运行
时的接受形状一致(两条都在真二进制上核过)。

示例里的命令起点认二进制 token(`grovefs`、`./target/release/grovefs`、
`ExecStart=/usr/local/bin/grovefs`)和文档里定义的替身(`$Y`、`$G`),并且只认
后面确实走到了一条已知子命令的那些 —— 散文里的 `postgres://…/grovefs`、
`grovefs_control.view_placement` 因此不会被当成一条命令。

用法: python3 flagcheck.py <仓库根> [--selftest]
退出码: 0=示例参数全部对得上;1=有跨子命令误配 / help 树取不到 / 自测没拉响。
"""
from __future__ import annotations

import bisect
import os
import re
import subprocess
import sys

DOC_FILES = [
    "README.md",
    "README.en.md",
    "USAGE.md",
    "USAGE.en.md",
    "docs/docs.html",
    "docs/deploy.html",
]

# 文档里的占位与别家工具的参数:不该拿去跟任何 grovefs 命令比对。
FOREIGN = {
    "delete-after", "delete-before", "delete-during", "exclude", "include",
    "inplace", "progress", "partial", "times", "whole-file", "verbose",
    "nocapture", "ignored", "no-pager", "no-default-features", "permanent",
    "add-rich-rule", "zone", "exec", "xxx",
}

# 命令起点:二进制名(可带路径)或文档里定义的替身。前后标点在外面剥。
ANCHOR = re.compile(r"(?:[\w./-]*grovefs(?:\.exe)?|\$Y(?![\w-])|\$G(?![\w-]))")
# 一条示例的终止符:反引号/表格竖线/分命令/重定向/注释/变量展开,以及下面
# doc_text() 给 HTML 代码片段打的边界标记。
STOP_CHARS = set("`|;&#\x02\x03")
FLAG = re.compile(r"--([a-z][a-z0-9-]*)")
# clap 的 Options 段:短参行缩进 2、长参行缩进 6;下面 10 空格缩进的说明文字里
# 也写着别的命令的参数,所以必须按缩进把它们排掉。
OPTION_DEF = re.compile(r"^(?:  (?:-[A-Za-z], )?|      )--([a-z][a-z0-9-]*)\b(.*)$")
COMMAND_ROW = re.compile(r"^  ([a-z][a-z0-9-]*)\b")
CJK = re.compile(r"[\u3000-\u30ff\u4e00-\u9fff，。、（）；：]")
TRIM = "`\"'()[]*<>=" + "\x02\x03"


def is_anchor(tok: str) -> bool:
    """这个 token 是 grovefs 二进制本身,而不是含 `grovefs` 字样的路径/表名。"""
    t = tok.strip(TRIM)
    if t in ("$Y", "$G"):
        return True
    if "://" in t:
        return False
    last = t.rsplit("/", 1)[-1]
    return last in ("grovefs", "grovefs.exe")


def own_options(text: str):
    """这一段命令自己的 Options 段:(参数名 → 是否带值)。"""
    out = {}
    inside = False
    for line in text.splitlines():
        if line == "Options:":
            inside = True
            continue
        if inside and line and not line[0].isspace():
            break  # EXAMPLES 之类的下一节
        if inside:
            m = OPTION_DEF.match(line)
            if m:
                # `--home <HOME>` / `--rules [FILE]` 带值,`--keep-source` 不带。
                out[m.group(1)] = bool(re.search(r"[\[<]", m.group(2)))
    return out


def subcommands(text: str) -> list:
    out = []
    inside = False
    for line in text.splitlines():
        if line == "Commands:":
            inside = True
            continue
        if inside and line and not line[0].isspace():
            break
        if inside:
            m = COMMAND_ROW.match(line)
            # `help` 是 clap 自带的,`<bin> help --help` 会报错,不走它。
            if m and m.group(1) != "help":
                out.append(m.group(1))
    return out


def help_tree(binary: str):
    """命令路径 → {参数名: 带值},整棵树靠递归 `--help` 取。"""
    tree = {}
    queue = [""]
    while queue:
        path = queue.pop(0)
        if path in tree:
            continue
        r = subprocess.run(
            [binary] + (path.split() if path else []) + ["--help"],
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            # 取不到 help 就等于对不了账,必须吵出来而不是静默少一条命令。
            raise RuntimeError(f"`{binary} {path} --help` 退出码 {r.returncode}")
        tree[path] = own_options(r.stdout)
        for c in subcommands(r.stdout):
            child = (path + " " + c).strip()
            if child not in tree:
                queue.append(child)
    return tree


def doc_text(root: str, rel: str) -> str:
    """文档正文:HTML 去标签并解实体,Markdown 原样。行号必须与磁盘一致,所以
    删掉的东西里带几个换行就补回几个;代码片段(<code>)换成边界标记,因为一行
    表格文字里常并列写着两条命令与两个参数。"""
    raw = open(os.path.join(root, rel), encoding="utf-8", errors="replace").read()
    if rel.endswith(".html"):
        keep_nl = lambda m: "\n" * m.group(0).count("\n") + " "
        raw = re.sub(r"<style>.*?</style>", keep_nl, raw, flags=re.S)
        raw = raw.replace("<code>", "\x02").replace("</code>", "\x03")
        raw = re.sub(r"<[^>]+>", keep_nl, raw)
        for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"),
                     ("&quot;", '"'), ("&#39;", "'"), ("&nbsp;", " ")):
            raw = raw.replace(a, b)
    return raw


def tokens_from(text: str, i: int, line_of):
    """从起点 i 吃到这一条示例结束,返回 [(token, 行号)](续行算同一条示例)。"""
    out = []
    while i < len(text):
        ch = text[i]
        if ch in " \t":
            i += 1
            continue
        if ch == "\\n" and text[i + 1:i + 2] == "\n":
            i += 2
            continue
        if ch in "\n\\":
            break
        if ch in STOP_CHARS:
            break
        j = i
        while j < len(text) and text[j] not in " \t\n\\":
            if text[j] in STOP_CHARS:
                break
            j += 1
        if j == i:  # 兜底:任何吃不掉的字符都结束这条示例,不要原地打转
            break
        tok = text[i:j]
        if out and is_anchor(tok.strip(TRIM)):
            break  # 下一个调用起点就是下一条示例,不要把两条读成一条
        cur = line_of(i)
        m = CJK.search(tok)
        if m:
            if m.start():
                out.append((tok[: m.start()], cur))
            break
        out.append((tok, cur))
        i = j
    return out


def parse_example(text: str, start: int, tree, line_of):
    """([（命令路径组, 参数名, 行号）], 是否走到了子命令) —— 走不到任何子命令就
    返回空,宁可不判也不把散文里的路径当一条命令。

    路径组是个元组:文档常用 `export/import` 这种简写一行说两条命令,那种情况下
    参数必须**两条都接**才算对得上(否则等于用一条示例掩盖了另一条跑不通)。
    """
    toks = tokens_from(text, start, line_of)
    paths = ("",)
    seen_cmd = False
    records = []
    k = 0
    while k < len(toks):
        tok = toks[k][0]
        if tok.startswith("--"):
            m = FLAG.match(tok)
            if not m:
                k += 1
                continue
            name = m.group(1)
            if not seen_cmd and name not in tree[""]:
                # 还没走到子命令就冒出根不接受的参数:这不是命令行,是散文。
                return [], False
            records.append((paths, name, toks[k][1]))
            k += 1
            if all(tree[p].get(name) for p in paths) and "=" not in tok \
                    and k < len(toks):
                k += 1  # 参数值不是命令名
            continue
        cand = [(p + " " + tok).strip() for p in paths]
        if all(c in tree for c in cand):
            paths = tuple(sorted(set(cand)))
            seen_cmd = True
            k += 1
            continue
        if tok.count("/") == 1 and not tok.startswith("/"):
            head, tail = tok.split("/", 1)
            alt = [(p + " " + x).strip() for p in paths for x in (head, tail)]
            if head and tail and all(a in tree for a in alt):
                paths = tuple(sorted(set(alt)))
                seen_cmd = True
                k += 1
                continue
        elif tok in tree and " " not in tok and tok:
            # 散文里并列写两条命令(`grovefs view define + mount --view V`):
            # 第二个裸命令名就是另一条示例的起点,后面的参数得归到它名下。
            paths = (tok,)
            seen_cmd = True
        k += 1
    return records, seen_cmd


def iter_examples(root: str, tree):
    """逐份文档产出 (文件, 行号, 那一行的原文, 记录)。"""
    for rel in DOC_FILES:
        p = os.path.join(root, rel)
        if not os.path.exists(p):
            continue
        text = doc_text(root, rel)
        nl = [i for i, ch in enumerate(text) if ch == "\n"]
        src = text.split("\n")
        line_of = lambda pos: bisect.bisect_right(nl, pos) + 1
        for m in ANCHOR.finditer(text):
            s = m.start()
            while s > 0 and not text[s - 1].isspace():
                s -= 1
            e = m.end()
            while e < len(text) and not text[e].isspace() and text[e] not in STOP_CHARS:
                e += 1
            if not is_anchor(text[s:e]):
                continue
            records, saw_cmd = parse_example(text, m.end(), tree, line_of)
            if not saw_cmd or not records:
                # 走不到子命令 = 散文;走到了子命令但一个参数都没写 = 没得对账。
                continue
            lineno = records[0][2]
            yield rel, lineno, src[lineno - 1].strip(), records


def check(root: str, tree) -> tuple:
    """(误配列表, 比对个数)。误配项是 (文件, 行号, 命令路径组, 参数名, 文案),
    自测直接拿结构判“该红的红了、不该红的没红”,不看文字子串。"""
    bad = []
    n = 0
    for rel, lineno, raw, records in iter_examples(root, tree):
        for paths, name, _ in records:
            if name in FOREIGN or name == "help":
                continue
            n += 1
            if all(name in tree[p] for p in paths):
                continue
            where = sorted(k for k, v in tree.items() if name in v)
            owner = "`%s`" % where[0] if where else ""
            kind = (f"它只是 {owner} 的参数" if where
                    else "整棵 help 树里都没有这个名字")
            shown = " / ".join("`%s`" % p for p in paths if p) or "(根)"
            bad.append((rel, lineno, paths, name,
                        f"{rel}:{lineno}: {shown} 不接 --{name}({kind})\n"
                        f"      原文:{raw[:150]}"))
    return bad, n


def selftest(root: str, tree) -> int:
    """证明闸门拉得响:对多条子命令注入「拿别的命令的参数写在本命令示例里」的
    误配,并同档注入它的正确归属 —— 误配必须全报红、正确的那几条必须全不报红。
    §2 的全局集合检查对这一类误配是绿的,所以这条自测不能归约成「名字存在吗」。"""
    import tempfile

    universe = set()
    for flags in tree.values():
        universe |= set(flags)
    universe -= FOREIGN
    universe.discard("help")
    pairs = []
    for b in sorted(p for p in tree if p):
        extra = sorted(universe - set(tree[b]))
        if not extra:
            continue
        flag = extra[0]
        src = next(a for a in sorted(tree) if a != b and flag in tree[a])
        pairs.append((b, flag, src))
    # 跳着取,覆到命令树的不止同一段,又不必把 54 条全跑一遍。
    picked = pairs[:: max(1, len(pairs) // 8)][:8]
    if not picked:
        print("  [需处理] help 树里找不到一对可注入的参数差异,自测无法进行")
        return 1
    with tempfile.TemporaryDirectory() as td:
        with open(os.path.join(td, "USAGE.md"), "w", encoding="utf-8") as fh:
            fh.write("# t\n")
            for dst, flag, src in picked:
                fh.write("\n```bash\ngrovefs %s --%s x\n```\n" % (dst, flag))
                fh.write("\n```bash\ngrovefs %s --%s x\n```\n" % (src, flag))
        saved = DOC_FILES[:]
        DOC_FILES[:] = ["USAGE.md"]
        try:
            bad, _ = check(td, tree)
        finally:
            DOC_FILES[:] = saved
    missed = []
    for dst, flag, src in picked:
        if not any(b[3] == flag and b[2] == (dst,) for b in bad):
            missed.append(f"`{dst}` 不接 --{flag} 没报红")
    false_pos = [b[4] for b in bad
                 if any(b[2] == (src,) and b[3] == flag for _, flag, src in picked)]
    if missed or false_pos:
        for m in missed:
            print(f"  [需处理] 注入自测没拉响:{m} —— 检查器没按子命令区分归属")
        for fp in false_pos:
            print(f"  [需处理] 注入自测误报正确归属的那条:{fp}")
        return 1
    print(f"  注入自测 OK:{len(picked)} 组误配全部报红、{len(picked)} 条正确归属全部"
          f"不报红(§2 的全局集合检查对这一类误配是绿的)")
    return 0


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    root = sys.argv[1]
    binary = os.environ.get(
        "GROVEFS_BIN", os.path.join(root, "target/release/grovefs"))
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        print(f"  [跳过] 找不到可执行 {binary}(用 GROVEFS_BIN 指定)")
        return 0
    try:
        tree = help_tree(binary)
    except RuntimeError as e:
        print(f"  [需处理] help 树取不全:{e}")
        return 1
    if "--selftest" in sys.argv:
        return selftest(root, tree)
    bad, n = check(root, tree)
    print(f"  help 树 {len(tree)} 条命令;文档示例里比对上 {n} 个「命令+参数」对")
    for _, _, _, _, msg in bad:
        print(f"  [需处理] {msg}")
    if bad:
        print("  判据:参数写在了不接它的子命令上(clap 会报 unexpected argument),"
              "\n  或者整棵 help 树里根本没有这个名字 —— 两者都要改文档。")
        return 1
    if n == 0:
        print("  [需处理] 一个示例参数都没比对上,检查器可能没吃到文档里的命令行")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
