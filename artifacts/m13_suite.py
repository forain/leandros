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

The completion marker is emitted as `echo "M13""RC=$?"`, which the shell prints
as M13RC=0 but which is typed as M13""RC=. That is not cosmetic. scmrun stops
reading at the first sight of the marker, the tty echoes every character it is
sent, and a marker spelled literally in the command therefore matched the echo
of the command itself -- closing every window about a second after the command
was typed, no matter how large the budget. The result was not an empty log but
a short one: vfstest's 36 subtests split 16 into its own window and 20 into the
next, each following row read the PREVIOUS row's exit status, and four tests
reported none at all. A suite that shifts exit statuses across test boundaries
can report a pass for a test that never ran, so the marker must be something
the echo cannot contain. scmrun now refuses an echoable marker outright.
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

# (command, read-seconds). Every command gets the same `echo "M13""RC=$?"`
# marker appended, so there is no per-test marker to get wrong.
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
    ("vfstest", 300),
    ("drmsmoke", 300),
    # Expected RED here: venustest needs `driver.py start x86_64 --venus`
    # (virtio-gpu-gl-pci,venus=on) and this suite boots plain uefi, so the
    # capset it probes is absent and it reports ~32 failures against a 108/0
    # baseline. Kept in the list rather than dropped, because before the marker
    # fix this row read "NO EXIT STATUS READ BACK" and the red was invisible.
    ("venustest", 240),
    ("scmtest", 300),
    ("wakepolltest", 240),
    ("forktest", 180),
    ("epolltest", 240),
    ("polltest", 240),
    ("waittest", 300),
    ("sigtest", 180),
    ("timertest", 240),
    ("memtest", 180),
    ("f2fstest", 300),
]

# Optional argv[3]: comma-separated subset, for re-running only the rows whose
# exit status a previous pass failed to read back. vfstest is excluded from any
# subset by default -- it must run exactly once per image (its own xattrs make
# a second run on the same image report a false xattr_list_f2fs failure).
if len(sys.argv) > 3 and sys.argv[3]:
    want = set(sys.argv[3].split(","))
    TESTS = [t for t in TESTS if t[0] in want]

# The marker is printed by the shell but never typed, so the tty echo of the
# command cannot contain it. See the module docstring.
RC_CMD = '; echo "M13""RC=$?"'
RC_MARK = "M13RC="

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
    ctl = scm("nosuchbinary_xyz42" + RC_CMD, 25, RC_MARK)
    m = re.search(r"M13RC=(\d+)", ctl)
    log(f"[control] nosuchbinary_xyz42 -> M13RC={m.group(1) if m else '??'}")
    if not m or m.group(1) == "0":
        log("[control] FAILED — aborting rather than reporting a green suite.")
        teardown()
        sys.exit(3)

    results = {}
    for cmd, dur in TESTS:
        txt = scm(cmd + RC_CMD, dur, RC_MARK)
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
