#!/usr/bin/env python3
# M7m: bisect which shell construct makes brush (PID5 W3) recurse. Each probe is
# a child `sh -c` so a crash kills only the child; PID1 shell survives. A probe
# that yields its echo marker is SAFE; one that produces no marker (+ EL0 fault
# in the serial log) is the TRIGGER.
import subprocess, os, sys, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
def d(*a, t=60):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def log(*a): print(*a, flush=True)
PROBES = [
    ("baseline",   "sh -c 'echo MARK_baseline'"),
    ("trapEXIT",   "sh -c 'trap true EXIT; echo MARK_trapEXIT'"),
    ("fn_trap",    "sh -c 'cleanup(){ :; }; trap cleanup EXIT INT TERM; echo MARK_fn_trap'"),
    ("cmdsubst",   "sh -c 'echo MARK_$(echo cmdsubst)'"),
    ("arith",      "sh -c 'i=0; i=$((i+1)); echo MARK_arith$i'"),
    ("bg_amp",     "sh -c 'true & echo MARK_bg'"),
    ("drs_usage",  "sh /usr/bin/dbus-run-session"),
]
def main():
    subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
    booted=False
    for _ in range(3):
        out=d("start","aarch64","uefi-tcg",t=220)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")): booted=True; break
    if not booted: log("FATAL no boot"); return
    d("login","root","root",t=45)
    for name,cmd in PROBES:
        out=deansi(d("cmd",cmd,"12"))
        marker = f"MARK_{name}" if name not in ("cmdsubst","arith","drs_usage") else "MARK_"
        got = ("MARK_" in out) or ("usage:" in out) or ("busd binary not found" in out)
        log(f"[{name}] {'OK' if got else 'NO-MARKER (possible TRIGGER)'} :: {out.strip()[-160:]!r}")
    log("=== serial tail (faults?) ===")
    slog=d("log",t=30)
    for ln in deansi(slog).splitlines():
        if any(k in ln for k in ("EL0 Fault","[BT]","[VMA]","recursion","PID=")): log("  "+ln)
    subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True)
if __name__=="__main__": main()
