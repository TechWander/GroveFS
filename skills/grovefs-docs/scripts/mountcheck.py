#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Mount-example checker (part of `doccheck.sh`, section 5).

A mount that names neither service is now a hard error, so a documented example
without coordinates is a documented example that does not run.

usage: mountcheck.py [REPO_ROOT]
Exit code: 0 = every example carries its coordinates; 1 = at least one does not.

Accepted WITHOUT coordinates, in this order:
  * the refusal demo itself (the line right after prints `a mount needs BOTH`);
  * the command names them on a continuation line (joined below) or through
    `${GFS[@]}` / env indirection;
  * a config-file / env context: `GROVEFS_META_SERVER` or `GROVEFS_MANAGER`
    appears within the previous 30 lines;
  * prose: a markdown table cell, an HTML `<td>`/`<p>` run that only names the
    shape of the command, or an elision marker (`…`).
"""
import io
import os
import re
import sys

ROOT = sys.argv[1] if len(sys.argv) > 1 else "."
FILES = ["README.md", "README.en.md", "USAGE.md", "USAGE.en.md",
         "docs/docs.html", "docs/deploy.html"]
INVOC = re.compile(r"(grovefs[\w.]*|\$\w+|\"[^\"]*grovefs[\w.]*\")\s+mount\s")
CONT = re.compile(r"\\\s*$")

bad = []
for f in FILES:
    path = os.path.join(ROOT, f)
    if not os.path.exists(path):
        continue
    lines = io.open(path, encoding="utf-8").read().split("\n")
    i = 0
    while i < len(lines):
        if INVOC.search(lines[i]):
            cmd, j = lines[i], i
            while CONT.search(cmd) and j + 1 < len(lines):
                j += 1
                cmd += " " + lines[j]
            ctx = "\n".join(lines[max(0, i - 30):i])
            ok = ("--meta-server" in cmd or "--manager" in cmd
                  or "${GFS[@]}" in cmd or "…" in cmd or "$GROVE_A_KEY" in cmd
                  or "GROVEFS_META_SERVER" in ctx or "GROVEFS_MANAGER" in ctx
                  or "a mount needs BOTH" in "\n".join(lines[j + 1:j + 4])
                  or re.match(r"\s*\|", lines[i]) or "<td>" in lines[i]
                  or "<p" in lines[i])
            if not ok:
                bad.append("%s:%d: %s" % (f, i + 1, re.sub(r"\s+", " ", cmd)[:170]))
            i = j
        i += 1

print("mount 示例缺坐标: %d" % len(bad))
for b in bad:
    print("  " + b)
print("判据: 每条 `grovefs mount` 示例都必须带 --meta-server(或 --manager),"
      "否则示例本身就是那条被拒绝的命令。")
sys.exit(1 if bad else 0)
