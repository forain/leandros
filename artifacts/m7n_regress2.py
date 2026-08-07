#!/usr/bin/env python3
# M7n reliable regression: boot+login via driver, then run each test via
# scmrun.py (fixed-duration raw serial reader — immune to the prompt heuristic
# that dropped verbose tests). vfstest FIRST.
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN = os.path.expanduser("~/code/leandros-artifacts/scmrun.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def scm(cmd, dur):
    # Retry the connect race (single-server serial socket): scmrun does one
    # connect with no backoff, so a lingering prior close can refuse it.
    for attempt in range(4):
        try:
            r = subprocess.run(["python3", SCMRUN, cmd, str(dur)], capture_output=True, text=True, timeout=dur+40)
            out = (r.stdout or "") + (r.stderr or "")
            if "ConnectionRefused" not in out and "Connection refused" not in out:
                return out
        except subprocess.TimeoutExpired:
            return "(TIMEOUT scmrun)"
        time.sleep(3)
    return out
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

TESTS = [
    ("vfstest",     "vfstest",       35),
    ("wakepolltest","wakepolltest",  35),
    ("f2fstest",    "f2fstest",      30),
    ("polltest",    "polltest",      20),
    ("timertest",   "timertest",     25),
    ("idletest",    "idletest 0",    25),
    ("drmsmoke",    "drmsmoke",      30),
    ("evtest2",     "evtest2",       25),
    ("waittest",    "waittest",      30),
    ("scmtest",     "scmtest",       35),
]
def verdict(name, out):
    o = deansi(out)
    # Count PASS/FAIL tokens and look for explicit SUMMARY/ALL PASS.
    passes = len(re.findall(r'\bPASS\b', o))
    fails  = len(re.findall(r'\bFAIL\b', o))
    summ = ""
    m = re.search(r'(\d+)\s*/\s*(\d+)', o)
    if m: summ = f" ({m.group(0)})"
    allp = "ALL PASS" in o.upper() or "ALL TESTS PASS" in o.upper()
    err  = bool(re.search(r'\bpanic\b|\[EXC\]|EL0 Fault|assertion failed', o))
    return f"PASS={passes} FAIL={fails}{summ}{' ALLPASS' if allp else ''}{' ERR!' if err else ''}"
def main():
    log(f"==== M7n REGRESS2 {ARCH} {MODE} {time.ctime()} ====")
    booted=False
    for a in range(1,3):
        log(f"#### BOOT {a} ####"); clean()
        out=d("start",ARCH,MODE,t=220)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True;break
    if not booted: log("FATAL no boot"); clean(); return
    d("login","root","root",t=45)
    # Sacrificial warm-ups: the FIRST couple of commands after login are eaten by
    # the reedline post-login prompt race; absorb them so vfstest captures clean.
    scm("echo WARM1", 6); scm("echo WARM2", 6)
    results={}
    for name,cmd,dur in TESTS:
        out=scm(cmd,dur)
        log(f"\n===== {name} ====="); log(deansi(out)[-1800:])
        results[name]=verdict(name,out)
        time.sleep(2)  # let the serial socket fully close before the next connect
    log("\n==== SUMMARY ====")
    for name,_,_ in TESTS: log(f"  {name}: {results[name]}")
    clean(); log("==== REGRESS2 DONE ====")
if __name__=="__main__": main()
