#!/usr/bin/env python3
"""M18 regression + port-exhaustion reproducer, Linux box, x86_64/KVM.

One fresh boot. `nosuchbinary_xyz42` first, confirmed FAILING, or nothing below
is falsifiable. `vfstest` exactly once against this image. Each binary is read
by its own `failures = N` trailer, never by counting ': PASS'.

The last step is the reproducer: N background processes, each holding an open
descriptor on the f2fs mount, then one ordinary `ls` of a directory that
certainly exists. On the pre-fix kernel that `ls` answers "Out of memory"; the
point is that no compositor, no Wayland and no D-Bus are involved.
"""
import os, re, subprocess, sys, time

REPO = "/home/forain/Projects/leandros"
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{REPO}/artifacts/scmrun.py"
OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m18-regress"
ARCH = "x86_64"

TESTS = [("vfstest", 90), ("scmtest", 60), ("wakepolltest", 55), ("forktest", 30),
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
    print(d("start", ARCH, "uefi", "--venus")[-500:], flush=True)
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
        rows.append((cmd, trailer, len(txt)))
        print(f"  {cmd:14s} failures={trailer if trailer is not None else 'NO TRAILER'}"
              f"  bytes={len(txt)}", flush=True)

    print("\n=== REPRODUCER: port exhaustion without a compositor ===", flush=True)
    # Each background `sleep` is a process that execs from the f2fs mount and so
    # takes an IPC reply port for the life of the task. `ls` afterwards is an
    # ordinary read of a directory that exists.
    burst = ("i=0; while [ $i -lt 60 ]; do sleep 200 & i=$((i+1)); done; echo BURST_DONE")
    r1 = strip(scm(burst, 70))
    open(f"{OUT}/repro-burst.txt", "w").write(r1)
    r2 = strip(scm("ls /usr/share/X11/xkb", 25))
    open(f"{OUT}/repro-ls.txt", "w").write(r2)
    print(r2[-900:], flush=True)
    print(f"\n  'Out of memory' in the ls output: {'YES' if 'Out of memory' in r2 else 'no'}",
          flush=True)
    print(f"  'rules' (a real entry) in the ls output: {'YES' if 'rules' in r2 else 'no'}",
          flush=True)
    # Which ceiling the kernel named, if any. These reports are always on.
    ser = open('/tmp/leandros-serial.log','rb').read().decode('utf-8','replace')
    for pat in ('port table FULL', 'fd-table pool FULL', 'task table FULL',
                'no reply port for this task'):
        print(f"  kernel named {pat!r}: {ser.count(pat)}", flush=True)

    print("\n==== SUMMARY ====", flush=True)
    for cmd, tr, n in rows:
        print(f"  {cmd:14s} failures={tr}", flush=True)
    d("stop", t=60)


if __name__ == "__main__":
    main()
