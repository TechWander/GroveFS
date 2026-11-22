#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Mount-example checker (part of `doccheck.sh`, section 5).

A mount resolves BOTH cluster coordinates ONLY from the deployment's manager —
there is no way left to name a meta-/data-service by hand (the `--meta-server` /
`--data-servers` flags were removed). So a documented `mount` example that does
not go through `--manager` is a documented example that does not RUN: clap
rejects the typed-address flags, and a bare mount fails closed on NO_COORDINATES.

usage: mountcheck.py [REPO_ROOT]
Exit code: 0 = every example bootstraps from a manager; 1 = at least one does not.

Accepted WITHOUT a visible `--manager`, in this order:
  * the refusal demo itself (the line right after prints the `NO_COORDINATES`
    opening `a mount resolves its cluster coordinates`);
  * the manager is named on a continuation line (joined below) or through
    a `${…[@]}` array / `$GROVE_A_KEY` indirection or an elision marker (`…`);
  * a config-file / env context: `GROVEFS_MANAGER` appears within the previous
    30 lines (that env knob alone satisfies the bootstrap);
  * prose: a markdown table cell, an HTML `<td>`/`<p>` run that only names the
    shape of the command; a comparative line that says the old coordinate flags
    were `removed` / `已移除` (it is describing the deletion, not dialing).
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
            nearby = "\n".join(lines[j + 1:j + 4])
            ok = ("--manager" in cmd
                  or re.search(r"\$\{[A-Za-z_][A-Za-z0-9_]*\[@\]\}", cmd)
                  or "…" in cmd or "$GROVE_A_KEY" in cmd
                  or "GROVEFS_MANAGER" in ctx
                  or "a mount resolves its cluster coordinates" in nearby
                  or "removed" in nearby or "已移除" in nearby
                  or re.match(r"\s*\|", lines[i]) or "<td>" in lines[i]
                  or "<p" in lines[i])
            if not ok:
                bad.append("%s:%d: %s" % (f, i + 1, re.sub(r"\s+", " ", cmd)[:170]))
            i = j
        i += 1

print("mount 示例未经 manager 引导: %d" % len(bad))
for b in bad:
    print("  " + b)
print("判据: 每条 `grovefs mount` 示例都必须经 --manager(或 GROVEFS_MANAGER/env、"
      "续行/变量间接)引导坐标;手敲 --meta-server/--data-servers 已从 mount 移除,"
      "示例里再出现它就是一条被 clap 拒绝的命令。")
sys.exit(1 if bad else 0)
