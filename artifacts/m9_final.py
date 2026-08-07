#!/usr/bin/env python3
"""M9 final verification — ONE blocking command, no watchers, no pollers.

Runs every remaining piece of work in PRIORITY order and appends each result to
RESULTS as soon as it is known, so a run that is cut short still leaves honest
partial results on disk:

  1. x86_64 full regression suite  (carries the new nested_epoll subtest — the
     single most important number; a subtest that only passes on one arch is a
     half-landed fix)
  2. aarch64 full regression suite
  3. aarch64 panel/clock run with the PRISTINE, un-instrumented panel binary and
     DBG_SERIAL_WRITE=false, closing the "measured with diagnostics on" caveat
  4. x86_64 panel/clock run (TCG, long settle) — nice-to-have

Each phase is a plain blocking subprocess call. Nothing here waits on another
process's log file.
"""
import os
import re
import subprocess
import sys
import time

ART = os.path.expanduser("~/code/leandros-artifacts")
RESULTS = os.path.expanduser(
    "/private/tmp/claude-501/-Users-forain-code-leandros/"
    "07b19cad-edf7-479d-84b0-21f06bc8ec0a/scratchpad/m9-final-results.txt")

BASELINE = {
    "vfstest": 36, "drmsmoke": 22, "scmtest": 25, "wakepolltest": 10,
}


def emit(line):
    print(line, flush=True)
    with open(RESULTS, "a") as f:
        f.write(line + "\n")


def kill_all():
    for pat in ("qemu-syste[m]",):
        subprocess.run(["pkill", "-9", "-f", pat], capture_output=True)
    time.sleep(3)


def run(tag, argv, timeout):
    """Blocking. Returns the combined output text (also written to a log)."""
    emit(f"\n########## {tag} :: start {time.ctime()} ##########")
    log = os.path.join(os.path.dirname(RESULTS), f"m9final-{tag}.log")
    t0 = time.time()
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        out = (r.stdout or "") + (r.stderr or "")
        rc = r.returncode
    except subprocess.TimeoutExpired as e:
        out = ((e.stdout or b"").decode(errors="replace")
               if isinstance(e.stdout, bytes) else (e.stdout or ""))
        out += "\n(TIMEOUT)"
        rc = -1
    with open(log, "w") as f:
        f.write(out)
    emit(f"{tag}: rc={rc} elapsed={int(time.time()-t0)}s log={log}")
    return out


def report_suite(arch, out):
    """Pull the per-test PASS/FAIL lines out of m9_regress.py's output."""
    rows = re.findall(r"\[(\w+)\] PASS=(\d+) FAIL=(\d+) done=(\w+)", out)
    if not rows:
        emit(f"  !! {arch}: NO test result lines parsed — suite did not run")
        return
    emit(f"  --- {arch} suite ---")
    for name, p, f, done in rows:
        p, f = int(p), int(f)
        note = ""
        if name in BASELINE and p != BASELINE[name]:
            note = f"  << DEVIATION (baseline {BASELINE[name]}/0)"
        if name == "epolltest":
            note = "  << nested_epoll subtest lives here (baseline 8, now 9)"
        if f:
            note += "  << NONZERO FAIL"
        emit(f"    {name:16s} PASS={p:3d} FAIL={f:3d} done={done}{note}")


def clockcheck(tag, arch, shots):
    """Hash the centred clock block of each screenshot; changing == ticking."""
    import hashlib
    out = os.path.expanduser("~/code/leandros-artifacts/notes/m9-panelgate")

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

    emit(f"  --- {tag} clock block ({arch}) ---")
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
    open(RESULTS, "w").write(f"==== M9 FINAL VERIFICATION {time.ctime()} ====\n")
    os.chdir(ART)

    # 1. x86_64 suite — highest priority (x86_64 epolltest / nested_epoll).
    kill_all()
    out = run("x86_64-suite", ["python3", "-u", "m9_regress.py", "x86_64", "uefi", "m9f"],
              timeout=3000)
    report_suite("x86_64", out)

    # 2. aarch64 suite.
    kill_all()
    out = run("aarch64-suite", ["python3", "-u", "m9_regress.py", "aarch64", "uefi", "m9f"],
              timeout=2400)
    report_suite("aarch64", out)

    # 3. aarch64 panel/clock, PRISTINE binaries + DBG_SERIAL_WRITE=false.
    kill_all()
    out = run("aarch64-panel", ["python3", "-u", "m9_panelgate.py", "aarch64", "m9p", "220", "info"],
              timeout=700)
    for l in out.splitlines():
        if "bar identical" in l or "shot" in l:
            emit("    " + l.strip())
    clockcheck("m9p", "aarch64", [65, 100, 118, 170])

    # 4. x86_64 panel/clock (TCG, long settle) — nice-to-have.
    kill_all()
    out = run("x86_64-panel",
              ["python3", "-u", "m9_panelgate_slow.py", "x86_64", "m9x", "480", "info"],
              timeout=1400)
    for l in out.splitlines():
        if "bar identical" in l or "shot" in l:
            emit("    " + l.strip())
    clockcheck("m9x", "x86_64", [200, 300, 340, 430])

    kill_all()
    emit(f"\n==== M9 FINAL VERIFICATION DONE {time.ctime()} ====")


if __name__ == "__main__":
    sys.exit(main())
