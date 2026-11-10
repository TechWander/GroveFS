#!/usr/bin/env python3
"""Move `--meta-store` from after the `view` action to the `view` group, where the
CLI actually accepts it (verified against the current binary: `view create
--meta-store …` exits with "unexpected argument '--meta-store'", while
`view --meta-store … create` works).  Dry run by default; --apply writes."""
import re
import sys

APPLY = "--apply" in sys.argv
FILES = ["USAGE.md", "docs.html"]

# Only the `view` group is affected: `du --view main --meta-store …` and
# `grovefs import --meta-store …` are top-level commands that do take the flag
# themselves, so the lookbehind rejects `--view` and the action set is explicit.
ACTION = r"(create|list|export|import|rm|define|alias|fork|graft)"
URI = r"[A-Za-z0-9:/~_.+\-?=&%…@#]{1,90}"
pat = re.compile(r"(?<![-A-Za-z0-9_])view " + ACTION + r" (--meta-store(?: " + URI + r")?)")

# Two prose spots carry the flag with no URI at all; rewriting them mechanically
# would read badly, so they get an explicit wording.
MANUAL = [
    ("docs.html", "(视图 schema 由 view create --meta-store 显式建,无需预迁移)",
     "(视图 schema 由 view --meta-store … create 显式建,无需预迁移)"),
    ("docs.html", "schema 由 <code>view create --meta-store</code> 显式建出",
     "schema 由 <code>view --meta-store … create</code> 显式建出"),
]

hits = 0
for path in FILES:
    src = open(path, encoding="utf-8").read()
    out_lines = []
    for n, line in enumerate(src.split("\n"), 1):
        new = line
        for f, old, repl in MANUAL:
            if f == path and old in new:
                new = new.replace(old, repl)
        new, cnt = pat.subn(lambda m: f"view {m.group(2)} {m.group(1)}", new)
        if new != line:
            hits += cnt
            for m in pat.finditer(line):
                a = max(0, m.start() - 45)
                print(f"--- {path}:{n}\n  旧: …{line[a:m.end() + 45]}…\n  新: …{new[a:a + (m.end() - m.start()) + 90]}…")
        out_lines.append(new)
    if APPLY:
        open(path, "w", encoding="utf-8").write("\n".join(out_lines))

print(f"{'APPLIED' if APPLY else 'DRY RUN'}: {hits} rewritten spots")
