#!/usr/bin/env python3
# M7n final regression sweep after the execve signal-handler-reset fix +
# EL0 backtrace facility (gated false). vfstest FIRST. scmtest via scmrun.py
# (its "-> " diagnostics trip the driver prompt heuristic). Stock-image tests.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN = os.path.expanduser("~/code/leandros-artifacts/scmrun.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-tcg"

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def scm(cmd, dur):
    try:
        r = subprocess.run(["python3", SCMRUN, cmd, str(dur)], capture_output=True, text=True, timeout=dur+30)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return "(TIMEOUT scmrun)"
def log(*a): print(*a, flush=True)
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',s)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

TESTS = [
    ("vfstest",     "vfstest",        95),
    ("wakepolltest","wakepolltest",   70),
    ("f2fstest",    "f2fstest",       60),
    ("epolltest",   "epolltest",      40),
    ("polltest",    "polltest",       40),
    ("pthreadtest", "pthreadtest",    45),
    ("sigtest",     "sigtest",        40),
    ("timertest",   "timertest",      40),
    ("idletest",    "idletest 0",     40),
    ("drmsmoke",    "drmsmoke",       50),
    ("evtest2",     "evtest2",        40),
    ("waittest",    "waittest",       55),
]
def main():
    log(f"==== M7n REGRESS {ARCH} {MODE} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted: log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    # Warm-ups: stabilize the reedline prompt so the FIRST real test isn't eaten
    # by the post-login prompt race, and prove the shell is live.
    for _ in range(5):
        d("cmd", "echo WARMUP", "8")
    d("cmd", "true", "6")
    results = {}
    for name, cmd, t in TESTS:
        o = deansi(d("cmd", f"{cmd} > /tmp/{name}.txt 2>&1; echo {name}_RC=$?; tail -10 /tmp/{name}.txt", t=t))
        log(f"\n===== {name} ====="); log(o[-1600:])
        m = re.search(rf"{name}_RC=(\d+)", o)
        rc = m.group(1) if m else "?"
        verdict = f"rc={rc}"
        pf = re.search(r"pass[=: ]+(\d+).*fail[=: ]+(\d+)", o.lower())
        if pf: verdict += f" pass={pf.group(1)} fail={pf.group(2)}"
        results[name] = verdict
    # scmtest via scmrun.py (20/20 expected)
    log("\n===== scmtest (scmrun.py) =====")
    so = deansi(scm("scmtest", 40)); log(so[-2000:])
    sm = re.search(r"(\d+)\s*/\s*(\d+)\s*pass|pass[=: ]+(\d+).*(?:of|/)\s*(\d+)", so.lower())
    results["scmtest"] = "see-log" + (f" {sm.group(0)}" if sm else "")
    log("\n==== SUMMARY ====");
    for name,_,_ in TESTS: log(f"  {name}: {results.get(name,'?')}")
    log(f"  scmtest: {results.get('scmtest','?')}")
    clean(); log("==== M7n REGRESS DONE ====")
if __name__ == "__main__": main()
