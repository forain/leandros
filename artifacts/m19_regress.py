#!/usr/bin/env python3
"""M19 regression — the aarch64 half of M18's suite, on a freshly built image.

One fresh boot against an image generated immediately before it, so `vfstest`
runs exactly once against it (the O_TRUNC/xattr residue recorded in the
open-issues list makes a second run on the same image fail by construction).
`nosuchbinary_xyz42` first, confirmed FAILING, or nothing below is falsifiable.

Every binary is read by its OWN `failures = N` trailer or, for `vfstest` which
has none, by counting its `: FAIL` lines. Counting `': PASS'` would report a
truncated run as a clean one.

The tail is M18's reproducer, kept because on the FIXED kernel it is its own
control: the same burst that used to reach the port ceiling now stops at
brush's descriptor limit (errno 24) and names no kernel table at all.

usage: m19_regress.py <outdir> [arch]
"""
import os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{HERE}/scmrun.py"
OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m19-regress"
ARCH = sys.argv[2] if len(sys.argv) > 2 else "aarch64"

TESTS = [("vfstest", 120), ("scmtest", 60), ("wakepolltest", 55), ("forktest", 30),
         ("epolltest", 35), ("polltest", 35), ("waittest", 45), ("sigtest", 30),
         ("timertest", 35), ("memtest", 30), ("venustest", 90)]

os.makedirs(OUT, exist_ok=True)


def d(*a, t=260):
    r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
    return (r.stdout or "") + (r.stderr or "")


def scm(cmd, dur):
    r = subprocess.run(["python3", SCMRUN, cmd, str(dur)],
                       capture_output=True, text=True, timeout=dur + 60)
    return (r.stdout or "") + (r.stderr or "")


def strip(t):
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", re.sub(r"\x1b[=>78]", "", t))


def main():
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(2)
    print(d("start", ARCH)[-500:], flush=True)
    print(d("login", "root", "root", t=90)[-300:], flush=True)

    ctl = strip(scm("nosuchbinary_xyz42", 12))
    open(f"{OUT}/control.txt", "w").write(ctl)
    if not re.search(r"not found|No such file|cannot", ctl, re.I):
        sys.exit(">>> POSITIVE CONTROL DID NOT FAIL — aborting, nothing below is falsifiable")
    print(">>> CONTROL OK (nosuchbinary_xyz42 reported failing)\n", flush=True)

    rows = []
    for cmd, dur in TESTS:
        txt = strip(scm(cmd, dur))
        open(f"{OUT}/{cmd}.txt", "w").write(txt)
        m = re.findall(r"failures\s*=\s*(\d+)", txt)
        trailer = m[-1] if m else None
        # vfstest ships no trailer; its FAILs are the only reading of it.
        fails = len(re.findall(r":\s*FAIL", txt))
        done = "done ---" in txt
        rows.append((cmd, trailer, fails, done, len(txt)))
        print(f"  {cmd:14s} trailer={trailer if trailer is not None else '-':>4}"
              f"  ':FAIL' lines={fails:3d}  saw-done={done}  bytes={len(txt)}", flush=True)

    print("\n=== M18 REPRODUCER against the FIXED kernel (its own control) ===", flush=True)
    burst = "i=0; while [ $i -lt 60 ]; do sleep 200 & i=$((i+1)); done; echo BURST_DONE"
    open(f"{OUT}/repro-burst.txt", "w").write(strip(scm(burst, 70)))
    r2 = strip(scm("ls /usr/share/X11/xkb", 25))
    open(f"{OUT}/repro-ls.txt", "w").write(r2)
    print(r2[-700:], flush=True)
    print(f"  'Out of memory' in ls output : {'YES' if 'Out of memory' in r2 else 'no'}", flush=True)
    print(f"  'rules' (a real entry) there : {'YES' if 'rules' in r2 else 'no'}", flush=True)
    print(f"  errno 24 (brush's own limit) : "
          f"{'YES' if 'No file descriptors' in r2 or 'error 24' in r2 else 'no'}", flush=True)

    ser = open('/tmp/leandros-serial.log', 'rb').read().decode('utf-8', 'replace')
    for pat in ('port table FULL', 'fd-table pool FULL', 'task table FULL',
                'no reply port for this task', '[EXC] EL0 Fault!'):
        print(f"  kernel named {pat!r}: {ser.count(pat)}", flush=True)

    print("\n==== SUMMARY ====", flush=True)
    for cmd, tr, fails, done, _ in rows:
        print(f"  {cmd:14s} failures={tr if tr is not None else f'(no trailer) FAILs={fails}'}"
              f"  done={done}", flush=True)
    d("stop", t=60)


if __name__ == "__main__":
    main()
