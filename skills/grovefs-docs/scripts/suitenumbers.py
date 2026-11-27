#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Suite-count drift scanner (part of `doccheck.sh`, section 6).

Any number written into prose is a fragile assertion: adding an `ok` line to a
suite silently invalidates every document that restates its total (this has
misfired repeatedly in this repo - see the note at the bottom). So the truth is
never typed by hand: it is parsed from the per-stage logs of the last real board
run (`run_all.sh` writes `.build-bin/logs/<timestamp>/<stage>.log`).

A `SUMMARY: pass=N` line from a run that then went red is NOT a suite total - it
is the number of assertions reached before the run stopped - so the truth for
each suite is its `pass` in the NEWEST board run where that suite came out
`fail=0`. Suites that are red on the newest board are listed as unverified
rather than compared, so a half-finished run can never silently redefine a
documented count (nor let one pass by being compared against its own abort).

usage: suitenumbers.py [REPO_ROOT] [--selftest]
  BOARD_LOG_DIR=<dir>   compare against one specific run instead of scanning
                        `.build-bin/logs/*` for the newest GREEN run per suite
  --selftest            bump every restated number in a throwaway copy of the
                        docs and prove each of the three rules turns red
Exit code: 0 = every restated number matches; 1 = at least one drift.
"""
import io
import os
import re
import sys

SELFTEST = "--selftest" in sys.argv
argv = [a for a in sys.argv[1:] if not a.startswith("--")]
ROOT = argv[0] if argv else "."
FILES = ["README.md", "README.en.md", "USAGE.md", "USAGE.en.md",
         "docs/docs.html", "docs/deploy.html"]

def is_run(d):
    return os.path.isdir(d) and any(f.endswith(".log") for f in os.listdir(d))


# BOARD_LOG_DIR may name ONE run directory or the logs root (the default); the
# logs root is what the `--selftest` pass below reuses, so injection is judged
# against exactly the same truth set as the real comparison.
src = os.environ.get("BOARD_LOG_DIR") or os.path.join(ROOT, ".build-bin", "logs")
if not os.path.isdir(src):
    print("skip: no board run found (%s 不存在；先跑 .build-bin/run_all.sh)" % src)
    sys.exit(0)
if is_run(src):
    runs = [src]
else:
    runs = [os.path.join(src, d) for d in sorted(os.listdir(src)) if is_run(os.path.join(src, d))]
    if not runs:
        print("skip: no board run found (%s 下没有整板目录)" % src)
        sys.exit(0)


def read_run(d):
    """-> {stage: (pass, fail)} plus the unit lib-test total."""
    got, lib = {}, None
    for name in sorted(os.listdir(d)):
        if not name.endswith(".log"):
            continue
        txt = io.open(os.path.join(d, name), encoding="utf-8",
                      errors="replace").read()
        m = re.search(r"SUMMARY: pass=(\d+) fail=(\d+)", txt)
        if not m:
            m = re.search(r"SUMMARY \w+: (\d+) passed(?:, (\d+) failed)?", txt)
        if m:
            got[name[:-4]] = (int(m.group(1)), int(m.group(2) or 0))
            continue
        m = re.search(r"test result: ok\. (\d+) passed", txt)
        if m and name[:-4] == "unit":
            lib = int(m.group(1))
    return got, lib


# Walk runs oldest -> newest: a suite's truth is its `pass` in the newest run
# where it came out `fail=0`; a later RED run never redefines the total.
truth, stages, unit, red_newest = {}, [], None, {}
newest = runs[-1]
for run in runs:
    got, lib = read_run(run)
    unit = lib if lib is not None else unit
    for stage, (p, f) in got.items():
        if stage not in stages:
            stages.append(stage)
        if f == 0:
            truth[stage] = p
        elif run == newest:
            red_newest[stage] = (p, f)

print("board runs: %d   比对基准: 每套件最近一次 fail=0 的实跑%s"
      % (len(runs), "(BOARD_LOG_DIR 指定单次)" if len(runs) == 1 else ""))
print("newest run: %s" % newest)
print("stages: %d   suites with a SUMMARY: %d   assertions: %d%s"
      % (len(stages), len(truth), sum(truth.values()),
         "   unit lib tests: %s" % unit if unit else ""))
if red_newest:
    print("最新板上未绿的套件(%d): %s"
          % (len(red_newest),
             ", ".join("%s pass=%d fail=%d" % (s, p, f)
                       for s, (p, f) in sorted(red_newest.items()))))
    print("  -> 红板的 pass 只数到中断处,不是总数: 上述套件的文档计数取自它们最"
          "后一次 GREEN 的实跑,未被最新板复验。")

EXTRA = ["forksrv", "gcremote", "pg_parallel"]
NAMES = "|".join(sorted([re.escape(s) for s in truth] + EXTRA))
# A suite that SELF-SKIPs in this build (an optional feature that is off) reports
# pass=0, which is not a truth to compare against; its documented numbers came
# from a run with the feature on. Reported separately, never as drift.
UNVERIFIABLE = set(s for s, n in truth.items() if n == 0)
TOTAL = re.compile(r"`(%s)`[^`\n]{0,20}?(?:套件|suite)?[^0-9\n]{0,24}(\d{1,4})\s*"
                   r"(?:条断言|断言|assertions?)" % NAMES)
DETAIL = re.compile(r"\b(%s) (\d{1,4})\b" % NAMES)
# `TOTAL` needs the name NEXT TO the number, which is exactly what a long
# single-line paragraph does not do: README's "full regression" bullet packs
# every suite into one line, each with its own "(…; 42 assertions)" tail. So a
# second pass attributes EVERY "N 条断言 / N assertions" to the LAST suite name
# quoted before it on that line - the reading a human would give it.
NAMEPOS = re.compile(r"`(%s)`|<code>(%s)</code>" % (NAMES, NAMES))
LOOSE = re.compile(r"(\d{1,4})\s*(?:条断言|断言|assertions?)\b")
ANYTOK = re.compile(r"`([^`\n]+)`|<code>([^<\n]+)</code>")
SUBSET = r"第\s*\d+(?:/\d+)?\s*节|sections? \d"

bad, unverified, reported = [], set(), set()
for f in FILES:
    path = os.path.join(ROOT, f)
    if not os.path.exists(path):
        continue
    for i, line in enumerate(io.open(path, encoding="utf-8").read().split("\n"), 1):
        for m in TOTAL.finditer(line):
            name, num = m.group(1), m.group(2)
            reported.add((f, i, num))
            if name in UNVERIFIABLE:
                unverified.add("%s:%d  %s: doc=%s (stage SELF-SKIP in this build)"
                               % (f, i, name, num))
                continue
            # "`layout` 套件第 10/11 节 29 条断言" is a SUBSET claim, not a restatement
            # of the suite total; comparing it would be a false red. The marker has to
            # sit between the name and the number, or a line that merely mentions
            # another section would hide real drift.
            if re.search(SUBSET, line[max(0, m.start(2) - 18):m.start(2)]):
                continue
            if name in truth and int(num) != truth[name]:
                bad.append("%s:%d  %s: doc=%s truth=%s" % (f, i, name, num, truth[name]))
        for m in LOOSE.finditer(line):
            num = m.group(1)
            if (f, i, num) in reported:
                continue          # the adjacent rule already judged this one
            # "49 assertions = 39 OK + 10 SKIP + 0 FAIL" is a breakdown of one
            # claim into three answers, not a restatement of a stage total.
            if re.match(r"\s*=", line[m.end():]):
                continue
            # Nearest quoted thing wins: `.bneck/pg_view_teardown.sh` (44
            # assertions) is a standalone script's own count, so the suite name
            # two clauses back is not its owner.
            quoted_any = [x for x in ANYTOK.finditer(line) if x.start() < m.start()]
            if quoted_any:
                last = quoted_any[-1].group(1) or quoted_any[-1].group(2)
                if last.endswith(".sh"):
                    continue
            quoted = [x for x in NAMEPOS.finditer(line) if x.start() < m.start()]
            if not quoted:
                continue
            name = quoted[-1].group(1) or quoted[-1].group(2)
            if name in UNVERIFIABLE or re.search(SUBSET, line[max(0, m.start() - 18):m.start()]):
                continue
            if name in truth and int(num) != truth[name]:
                bad.append("%s:%d  %s (行内最近套件名): doc=%s truth=%s"
                           % (f, i, name, num, truth[name]))
        # the compact "name NN / name NN" form only where the line says what it is
        if "断言明细" in line or re.search(r"assertion (breakdown|details)", line):
            body = re.sub(r"<[^>]+>", " ", line)
            for name, num in DETAIL.findall(body):
                if name in truth and int(num) != truth[name]:
                    bad.append("%s:%d  %s (明细): doc=%s truth=%s"
                               % (f, i, name, num, truth[name]))

def _selftest(base):
    """Prove the gate can fire: copy the six docs, bump EVERY restated number by
    one, and require all three rules (adjacent / in-line attribution / 明细) to
    report drift. Without this the scanner's exemptions (subset markers, `.sh`
    owners, `= 39 OK + 10 SKIP` breakdowns) are one over-broad regex away from a
    permanently green check that checks nothing."""
    import shutil
    import subprocess
    import tempfile
    tmp = tempfile.mkdtemp(prefix="suitenumbers-selftest-")
    for f in FILES:
        src = os.path.join(ROOT, f)
        if not os.path.exists(src):
            continue
        txt = io.open(src, encoding="utf-8").read()
        txt = re.sub(r"(\d{1,4})(\s*(?:条断言|断言|assertions))",
                     lambda m: "%d%s" % (int(m.group(1)) + 1, m.group(2)), txt)
        out = []
        for line in txt.split("\n"):
            if "断言明细" in line or re.search(r"assertion (breakdown|details)", line):
                line = re.sub(r"\b(%s) (\d{1,4})\b" % NAMES,
                              lambda m: "%s %d" % (m.group(1), int(m.group(2)) + 1), line)
            out.append(line)
        dst = os.path.join(tmp, f)
        if os.path.dirname(dst):
            os.makedirs(os.path.dirname(dst), exist_ok=True)
        io.open(dst, "w", encoding="utf-8").write("\n".join(out))
    proc = subprocess.run([sys.executable, os.path.abspath(__file__), tmp],
                          env=dict(os.environ, BOARD_LOG_DIR=base),
                          stdout=subprocess.PIPE)
    got = proc.stdout.decode("utf-8", "replace")
    shutil.rmtree(tmp, ignore_errors=True)
    missing = []
    if proc.returncode == 0:
        missing.append("注入后仍退出 0")
    for label, probe in (("挨着名字", re.compile(
            r"\n  \S+:\d+  [a-z_]+: doc=\d+ truth=\d+", re.M)),
                         ("行内最近名", "行内最近套件名"),
                         ("明细行", "(明细)")):
        hit = probe.search(got) if hasattr(probe, "search") else probe in got
        if not hit:
            missing.append(label)
    print("selftest: %s" % ("OK — 三条归属规则都对注入红了" if not missing
                            else "失败 — 没触发: %s" % ", ".join(missing)))
    if missing:
        for l in got.split("\n"):
            if l.strip().startswith(("drift", "board")):
                print("  " + l.strip())
    return 1 if missing else 0


if SELFTEST:
    sys.exit(_selftest(src))

print("drift: %d" % len(bad))
for b in bad:
    print("  " + b)
for u in sorted(set(unverified)):
    print("  note: " + u)
print("判据: 文档里每句「X 套件 N 条断言」都必须等于该套件最近一次 GREEN 实跑的 pass 数"
      "(红板的 pass 只数到中断处); "
      "名字不挨着也算(归给行内它前面最近的那个套件名)；\n"
      "      改套件计数的那次改动必须同时改文档并重跑整板。")
sys.exit(1 if bad else 0)
