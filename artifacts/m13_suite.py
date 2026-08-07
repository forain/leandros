#!/usr/bin/env python3
"""M13 regression suite: fresh image, boot, log in, run the suite exactly once.

    python3 artifacts/m13_suite.py <arch> [outdir]

Counting rule: every one of these binaries RETURNS its own failure count from
main (vfstest/src/main.rs:701 `failures`, drmsmoke:1180, waittest:112-122), so
the authoritative number is the exit status, not a count of `: PASS` lines. A
harness that grepped `: PASS` once reported waittest as 5/0 against a
4-subtest source. We therefore ask the shell for `$?` and, separately, list the
named `: FAIL` lines so the two can be cross-footed against each other.

vfstest runs FIRST and exactly once per image: it leaves xattrs behind that
make a second run on the same image report a false xattr_list_f2fs failure
(O_TRUNC clears data but not xattrs, and /data survives reboots).
"""
import os
import re
import subprocess
import sys
import time

REPO = os.path.expanduser("~/code/leandros")
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{REPO}/scripts/scmrun.py"

ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
OUT = sys.argv[2] if len(sys.argv) > 2 else f"/tmp/m13/suite-{ARCH}"
os.makedirs(OUT, exist_ok=True)
LOGF = open(f"{OUT}/m13-suite-{ARCH}.log", "w", buffering=1)

# (command, read-seconds, completion marker or None)
#
# Budgets are deliberately generous. A first pass at half these numbers came
# back with five "NO EXIT STATUS READ BACK" rows on aarch64 and seven on
# x86_64 -- none of which were failures: drmsmoke was still sitting in its
# "holding gradient for screenshot" pause and waittest had printed only its
# first subtest when the read window closed. A truncated log greps clean, so a
# short budget manufactures reds. Every command below ends with `echo M13RC=$?`
# and the reader stops on that marker, so the ceiling only costs time when
# something really is stuck.
TESTS = [
    ("vfstest", 240, "--- vfstest done ---"),
    ("drmsmoke", 240, None),
    ("venustest", 200, None),
    ("scmtest", 240, None),
    ("wakepolltest", 240, None),
    ("forktest", 150, None),
    ("epolltest", 180, None),
    ("polltest", 180, None),
    ("waittest", 240, "--- waittest done ---"),
    ("sigtest", 150, None),
    ("timertest", 200, None),
    ("memtest", 150, None),
    ("f2fstest", 240, None),
]

# Optional argv[3]: comma-separated subset, for re-running only the rows whose
# exit status a previous pass failed to read back. vfstest is excluded from any
# subset by default -- it must run exactly once per image (its own xattrs make
# a second run on the same image report a false xattr_list_f2fs failure).
if len(sys.argv) > 3 and sys.argv[3]:
    want = set(sys.argv[3].split(","))
    TESTS = [t for t in TESTS if t[0] in want]

ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][B0]")


def log(m=""):
    print(m, flush=True)
    LOGF.write(m + "\n")


def d(*a, t=360):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True,
                           text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def clean(raw):
    out = []
    for line in ANSI.sub("", raw).split("\n"):
        out.append(line.split("\r")[-1])
    return "\n".join(out)


def scm(cmd, dur, marker):
    args = ["python3", SCMRUN, cmd, str(dur)]
    if marker:
        args.append(marker)
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=dur + 90)
        return clean((r.stdout or "") + (r.stderr or ""))
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {cmd})"


def teardown():
    d("stop", t=90)
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(3)


def main():
    log(f"===== M13 suite {ARCH} ===== {time.strftime('%F %T')}")
    for attempt in (1, 2):
        teardown()
        out = d("start", ARCH, "uefi", t=400)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            log(f"[boot] up on attempt {attempt}")
            break
        log(f"[boot] attempt {attempt} failed")
    else:
        log("[boot] NO BOOT")
        sys.exit(2)
    d("login", "root", "root", t=90)

    # Positive control: a binary that must NOT exist, confirmed failing, so a
    # clean-looking suite cannot come from a console that runs nothing.
    ctl = scm("nosuchbinary_xyz42; echo M13RC=$?", 25, "M13RC=")
    m = re.search(r"M13RC=(\d+)", ctl)
    log(f"[control] nosuchbinary_xyz42 -> M13RC={m.group(1) if m else '??'}")
    if not m or m.group(1) == "0":
        log("[control] FAILED — aborting rather than reporting a green suite.")
        teardown()
        sys.exit(3)

    results = {}
    for cmd, dur, marker in TESTS:
        txt = scm(f"{cmd}; echo M13RC=$?", dur, "M13RC=")
        open(f"{OUT}/{cmd}.log", "w").write(txt)
        rc = re.search(r"M13RC=(\d+)", txt)
        fails = re.findall(r"^\s*([A-Za-z0-9_./-]+):\s*FAIL", txt, re.M)
        passes = len(re.findall(r"^\s*[A-Za-z0-9_./-]+:\s*PASS", txt, re.M))
        results[cmd] = {"rc": int(rc.group(1)) if rc else None,
                        "named_fails": fails, "pass_lines": passes}
        log(f"  {cmd:14s} exit(failures)={results[cmd]['rc']}  "
            f"named FAIL={len(fails)} {fails[:6]}  (PASS lines={passes})")

    log("\n===== SUMMARY =====")
    bad = []
    for k, v in results.items():
        if v["rc"] is None:
            log(f"  {k:14s} NO EXIT STATUS READ BACK — unproven")
            bad.append(k)
        elif v["rc"] != 0:
            log(f"  {k:14s} FAILURES={v['rc']} {v['named_fails']}")
            bad.append(k)
        else:
            log(f"  {k:14s} clean")
    log(f"\n  reds: {bad if bad else 'none'}")
    teardown()


if __name__ == "__main__":
    main()
