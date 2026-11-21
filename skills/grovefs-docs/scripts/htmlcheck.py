#!/usr/bin/env python3
"""GroveFS 文档 HTML 结构检查 —— 标签闭合 + 页内锚点 + 跨页引用。

用法:
  python3 skills/grovefs-docs/scripts/htmlcheck.py            # 查 docs/ 下两份 HTML
  python3 skills/grovefs-docs/scripts/htmlcheck.py a.html …   # 查指定文件
退出码: 0=干净; 1=有结构问题(未闭合标签 / 跨层闭合 / 悬空 #锚点 / 断链的兄弟页)。

为什么锚点也要查:文档页的目录全靠 href="#sNN" ↔ id="sNN" 手工配对,重写一节时
最容易改出「目录点下去没反应」——标签配平衡量不出这种错。
"""
import os
import re
import sys
from html.parser import HTMLParser

VOID = {
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
    "meta", "param", "source", "track", "wbr",
}


class Checker(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack = []
        self.errors = []

    def handle_starttag(self, tag, attrs):
        if tag not in VOID:
            self.stack.append((tag, self.getpos()[0]))

    def handle_endtag(self, tag):
        if tag in VOID:
            return
        if not self.stack:
            self.errors.append(f"line {self.getpos()[0]}: stray </{tag}>")
            return
        if self.stack[-1][0] == tag:
            self.stack.pop()
            return
        names = [s[0] for s in self.stack]
        if tag in names:
            while self.stack and self.stack[-1][0] != tag:
                t, ln = self.stack.pop()
                self.errors.append(
                    f"line {self.getpos()[0]}: </{tag}> closes unclosed <{t}> from line {ln}")
            self.stack.pop()
        else:
            t, ln = self.stack[-1]
            self.errors.append(
                f"line {self.getpos()[0]}: unmatched </{tag}> (stack top <{t}> from line {ln})")


def check(path):
    src = open(path, encoding="utf-8").read()
    p = Checker()
    p.feed(src)
    p.close()
    ids = set(re.findall(r'id="([^"]+)"', src))
    hrefs = set(re.findall(r'href="#([^"]+)"', src))
    dangling = sorted(hrefs - ids)
    here = os.path.basename(path)
    sibling = sorted({m for m in re.findall(r'href="([A-Za-z0-9._-]+\.html)(?:#[^"]*)?"', src)
                      if m != here})
    broken = [s for s in sibling if not os.path.exists(os.path.join(os.path.dirname(path), s))]
    dupes = sorted({i for i in re.findall(r'id="([^"]+)"', src)
                    if src.count(f'id="{i}"') > 1})
    print(f"{path}: lines={src.count(chr(10)) + 1} tag_errors={len(p.errors) + len(p.stack)} "
          f"dangling_anchors={len(dangling)} duplicate_ids={len(dupes)} broken_links={len(broken)}")
    for e in p.errors[:20]:
        print("  TAG " + e)
    for tag, ln in p.stack[:20]:
        print(f"  UNCLOSED <{tag}> from line {ln}")
    for d in dangling[:20]:
        print(f"  DANGLING #{d}")
    for d in dupes[:20]:
        print(f"  DUPLICATE id=\"{d}\"")
    for b in broken[:20]:
        print(f"  BROKEN LINK {b}")
    return len(p.errors) + len(p.stack) + len(dangling) + len(dupes) + len(broken)


def main(argv):
    if len(argv) > 1:
        targets = argv[1:]
    else:
        # realpath:.qoder/skills/ 下是符号链,不解会算错仓库根(同 doccheck.sh)
        here = os.path.dirname(os.path.realpath(__file__))
        root = os.path.dirname(os.path.dirname(os.path.dirname(here)))
        targets = [os.path.join(root, "docs", f) for f in ("docs.html", "deploy.html")]
    bad = 0
    for t in targets:
        bad += check(t)
    print("OK" if bad == 0 else f"需处理: {bad} 处结构问题")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
