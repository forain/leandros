#!/usr/bin/env python3
"""Port-table exhaustion, reproduced with no compositor, no Wayland, no D-Bus.

`ls` of a directory that certainly exists answers "Out of memory (os error 12)"
because the IPC port table has no bucket left for the task's reply port, and
every VFS call to a mounted filesystem needs one.

The load is `N` background `sleep`s, which is all it takes: each is a task that
holds one bucket from its exec until it exits. The shipped `LIVE_BUCKETS` is
512 and brush runs out of its own descriptors long before 512 background jobs,
so this script is meant to be run against a kernel built with a scaled-down
`LIVE_BUCKETS` -- the point is the mechanism and the errno, not the constant.

usage: m18_repro.py <outdir> <n_background_jobs>
"""
import os, re, subprocess, sys, time

REPO = "/home/forain/Projects/leandros"
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{REPO}/artifacts/scmrun.py"
OUT = sys.argv[1]
N = int(sys.argv[2]) if len(sys.argv) > 2 else 20
os.makedirs(OUT, exist_ok=True)


def scm(cmd, dur):
    r = subprocess.run(["python3", SCMRUN, cmd, str(dur)],
                       capture_output=True, text=True, timeout=dur + 60)
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", (r.stdout or "") + (r.stderr or ""))


subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
time.sleep(2)
print(subprocess.run(["python3", DRIVER, "start", "x86_64"], capture_output=True,
                     text=True, timeout=260).stdout[-300:], flush=True)
subprocess.run(["python3", DRIVER, "login", "root", "root"], capture_output=True,
               text=True, timeout=90)

ctl = scm("nosuchbinary_xyz42", 12)
open(f"{OUT}/control.txt", "w").write(ctl)
if not re.search(r"not found|No such file|cannot", ctl, re.I):
    sys.exit(">>> POSITIVE CONTROL DID NOT FAIL — aborting")
print(">>> CONTROL OK\n", flush=True)

before = scm("ls /usr/share/X11/xkb", 20)
open(f"{OUT}/ls-before.txt", "w").write(before)
print(f"BEFORE the burst: 'rules' present = {'rules' in before}, "
      f"'Out of memory' = {'Out of memory' in before}", flush=True)

burst = f"i=0; while [ $i -lt {N} ]; do sleep 300 & i=$((i+1)); done; echo BURST_DONE"
b = scm(burst, 40)
open(f"{OUT}/burst.txt", "w").write(b)
print(f"burst: EMFILE lines = {b.count('No file descriptors available')}", flush=True)

after = scm("ls /usr/share/X11/xkb", 20)
open(f"{OUT}/ls-after.txt", "w").write(after)
print(f"AFTER  the burst: 'rules' present = {'rules' in after}, "
      f"'Out of memory' = {'Out of memory' in after}", flush=True)
print(after[-500:], flush=True)

ser = open("/tmp/leandros-serial.log", "rb").read().decode("utf-8", "replace")
open(f"{OUT}/serial.log", "w").write(ser)
for pat in ("port table FULL", "fd-table pool FULL", "task table FULL",
            "no reply port for this task"):
    print(f"  kernel named {pat!r}: {ser.count(pat)}", flush=True)

subprocess.run(["python3", DRIVER, "stop"], capture_output=True, text=True, timeout=60)
