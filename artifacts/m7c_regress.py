#!/usr/bin/env python3
# M7c final regression sweep. vfstest FIRST. Stock-image tests only (no injected
# binaries). wakepolltest covers the futex fix (xthread_futex_timed_wake subtest).
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
TESTS = [
    ("vfstest",     "vfstest",        95),
    ("f2fstest",    "f2fstest",       60),
    ("wakepolltest","wakepolltest",   60),
    ("epolltest",   "epolltest",      40),
    ("polltest",    "polltest",       40),
    ("pthreadtest", "pthreadtest",    45),
    ("sigtest",     "sigtest",        40),
    ("timertest",   "timertest",      40),
    ("scmtest",     "scmtest",        40),
    ("idletest",    "idletest 0",     40),
    ("drmsmoke",    "drmsmoke",       50),
    ("evtest2",     "evtest2",        40),
    ("waittest",    "waittest",       50),
]
def main():
    log(f"==== M7c REGRESS {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    results = {}
    for name, cmd, t in TESTS:
        o = deansi(d("cmd", f"{cmd} > /tmp/{name}.txt 2>&1; echo {name}_RC=$?; tail -8 /tmp/{name}.txt", t=t))
        log(f"\n===== {name} =====")
        log(o[-1400:])
        m = re.search(rf"{name}_RC=(\d+)", o)
        rc = m.group(1) if m else "?"
        verdict = f"rc={rc}"
        pf = re.search(r"pass[=: ]+(\d+).*fail[=: ]+(\d+)", o.lower())
        if pf: verdict += f" pass={pf.group(1)} fail={pf.group(2)}"
        results[name] = verdict
    log("\n==== SUMMARY ====")
    for name,_,_ in TESTS: log(f"  {name}: {results.get(name,'?')}")
    clean(); log("==== M7c REGRESS DONE ====")
if __name__ == "__main__": main()
