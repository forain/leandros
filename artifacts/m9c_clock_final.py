#!/usr/bin/env python3
"""M9c — land + verify the sub-tick CLOCK_MONOTONIC fix on the Mac host.

ONE blocking command. No watchers, no pollers, no waiting on another process's
log file. Every phase is a plain blocking subprocess call and its result is
appended to RESULTS the moment it is known, so a run that is cut short still
leaves honest partial results on disk.

  1. x86_64 full suite on a FRESH f2fs image (vfstest runs exactly once)
  2. aarch64 full suite on a FRESH f2fs image
  3. aarch64 COSMIC panel/clock run on a FRESH image (pristine binaries)

The new `clock_monotonic_subtick` subtest lives in timertest, so its evidence
lines (clock_getres_ns / min_subtick_step_ns / sleep200ms_measured_ns) are
pulled out of the timertest log for both arches.
"""
import hashlib
import os
import re
import subprocess
import sys
import time

ART = os.path.expanduser("~/code/leandros-artifacts")
REPO = os.path.expanduser("~/code/leandros")
SCRATCH = ("/private/tmp/claude-501/-Users-forain-code-leandros/"
           "07b19cad-edf7-479d-84b0-21f06bc8ec0a/scratchpad")
RESULTS = os.path.join(SCRATCH, "m9c-clock-results.txt")
NOTES = os.path.expanduser("~/code/leandros-artifacts/notes")

BASELINE = {
    "vfstest": 36, "drmsmoke": 22, "scmtest": 25, "wakepolltest": 10,
    "forktest": 3, "epolltest": 9, "polltest": 6, "sigtest": 6,
    "timertest": 6, "memtest": 4,
}


def emit(line):
    print(line, flush=True)
    with open(RESULTS, "a") as f:
        f.write(line + "\n")


def kill_all():
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(3)


def run(tag, argv, timeout, cwd=ART):
    emit(f"\n########## {tag} :: start {time.ctime()} ##########")
    log = os.path.join(SCRATCH, f"m9c-{tag}.log")
    t0 = time.time()
    try:
        r = subprocess.run(argv, capture_output=True, text=True,
                           timeout=timeout, cwd=cwd)
        out = (r.stdout or "") + (r.stderr or "")
        rc = r.returncode
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        out += "\n(TIMEOUT)"
        rc = -1
    with open(log, "w") as f:
        f.write(out)
    emit(f"{tag}: rc={rc} elapsed={int(time.time() - t0)}s log={log}")
    return out


def fresh_image(arch):
    """Regenerate the f2fs data images so vfstest never sees a dirty one."""
    emit(f"  regenerating fresh f2fs image for {arch}")
    r = subprocess.run(["python3", "scripts/mkfs-f2fs-populated.py",
                        f"f2fs-data0-{arch}.img", arch],
                       capture_output=True, text=True, timeout=900, cwd=REPO)
    if r.returncode != 0:
        emit(f"  !! mkfs FAILED rc={r.returncode}: {(r.stderr or '')[-400:]}")
        return False
    subprocess.run(["cp", f"f2fs-data0-{arch}.img", f"f2fs-data1-{arch}.img"],
                   cwd=REPO, capture_output=True)
    return True


def report_suite(arch, out):
    rows = re.findall(r"\[(\w+)\] PASS=(\d+) FAIL=(\d+) done=(\w+)", out)
    if not rows:
        emit(f"  !! {arch}: NO test result lines parsed — suite did not run")
        return
    emit(f"  --- {arch} suite ---")
    for name, p, f, done in rows:
        p, f = int(p), int(f)
        note = ""
        if name in BASELINE and p != BASELINE[name]:
            note = f"  << DEVIATION (baseline {BASELINE[name]})"
        if f:
            note += "  << NONZERO FAIL"
        emit(f"    {name:16s} PASS={p:3d} FAIL={f:3d} done={done}{note}")


def report_clocktest(arch, tag="m9c"):
    """Pull the direct sub-tick clock evidence out of the timertest log."""
    p = f"{NOTES}/m9-{tag}-{arch}-timertest.txt"
    emit(f"  --- {arch} clock_monotonic_subtick evidence ---")
    if not os.path.exists(p):
        emit("    (no timertest log)")
        return
    txt = open(p, errors="replace").read()
    hits = 0
    for l in txt.splitlines():
        s = l.strip()
        if (s.startswith("clock_getres_ns=") or s.startswith("min_subtick_step_ns=")
                or s.startswith("sleep200ms_measured_ns=") or s.startswith("loop_span_ns=")
                or s.startswith("clock_monotonic_subtick:")):
            emit("    " + s)
            hits += 1
    if not hits:
        emit("    !! no clock evidence lines found — subtest did not run")


def readppm(p):
    d = open(p, "rb").read()
    if d[:2] != b"P6":
        return None
    i, fl = 2, []
    while len(fl) < 3:
        while d[i:i + 1].isspace():
            i += 1
        s = i
        while not d[i:i + 1].isspace():
            i += 1
        fl.append(int(d[s:i]))
    i += 1
    return fl[0], fl[1], d[i:]


def clockcheck(tag, arch, shots):
    out = f"{NOTES}/m9-panelgate"
    emit(f"  --- {tag} panel clock block ({arch}) ---")
    prev, seen = None, []
    for t in shots:
        p = f"{out}/{tag}-{arch}-t{t}.ppm"
        img = readppm(p) if os.path.exists(p) else None
        if not img:
            emit(f"    t{t:3d} (no screenshot)")
            continue
        w, h, px = img
        x0 = (w - 220) // 2
        crop = b"".join(px[(y * w + x0) * 3:(y * w + x0 + 220) * 3] for y in range(32))
        hh = hashlib.sha1(crop).hexdigest()[:12]
        nz = sum(1 for b in crop if b)
        emit(f"    t{t:3d} sha={hh} nonzero={nz} changed_from_prev={hh != prev}")
        seen.append(hh)
        prev = hh
    if len(seen) >= 2:
        emit(f"    VERDICT: {'TICKING' if len(set(seen)) > 1 else 'FROZEN'} "
             f"({len(set(seen))} distinct of {len(seen)} samples)")


def main():
    open(RESULTS, "w").write(f"==== M9c CLOCK VERIFICATION {time.ctime()} ====\n")

    # 1. x86_64 suite, fresh image.
    kill_all()
    fresh_image("x86_64")
    out = run("x86_64-suite",
              ["python3", "-u", "m9c_regress.py", "x86_64", "uefi", "m9c"],
              timeout=3600)
    report_suite("x86_64", out)
    report_clocktest("x86_64")

    # 2. aarch64 suite, fresh image.
    kill_all()
    fresh_image("aarch64")
    out = run("aarch64-suite",
              ["python3", "-u", "m9c_regress.py", "aarch64", "uefi", "m9c"],
              timeout=3000)
    report_suite("aarch64", out)
    report_clocktest("aarch64")

    # 3. aarch64 COSMIC panel/clock, fresh image, pristine binaries.
    kill_all()
    fresh_image("aarch64")
    out = run("aarch64-panel",
              ["python3", "-u", "m9_panelgate.py", "aarch64", "m9c", "220", "info"],
              timeout=800)
    for l in out.splitlines():
        if "bar identical" in l or "shot" in l:
            emit("    " + l.strip())
    clockcheck("m9c", "aarch64", [65, 100, 118, 170])

    kill_all()
    emit(f"\n==== M9c CLOCK VERIFICATION DONE {time.ctime()} ====")


if __name__ == "__main__":
    sys.exit(main())
