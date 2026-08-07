#!/usr/bin/env python3
# M7m: confirm RUST_LOG activates brush's recursive tracing dispatch. Each probe
# a child sh -c. A probe with RUST_LOG set that yields no marker (+fault) => the
# tracing subscriber re-entrancy is the W3 recursion; RUST_LOG is the trigger.
import subprocess, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
def d(*a, t=60):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def deansi(s): return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',s))
def log(*a): print(*a, flush=True)
PROBES = [
    ("no_rustlog",   "sh -c 'echo MARK_a'"),
    ("rustlog_info", "RUST_LOG=info sh -c 'echo MARK_b'"),
    ("rustlog_trace","RUST_LOG=trace sh -c 'echo MARK_c'"),
    ("rustlog_warn", "RUST_LOG=warn sh -c 'echo MARK_d'"),
    ("info_drs",     "RUST_LOG=info sh /usr/bin/dbus-run-session"),
    ("info_export_child","sh -c 'export RUST_LOG=info; echo MARK_f'"),
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
        got = ("MARK_" in out) or ("usage:" in out)
        log(f"[{name}] {'OK' if got else 'NO-MARKER => TRIGGER'} :: {out.strip()[-140:]!r}")
    log("=== serial tail (faults?) ===")
    for ln in deansi(d("log",t=30)).splitlines():
        if any(k in ln for k in ("EL0 Fault","[BT] 0 ret","[VMA]*","PID=5")): log("  "+ln)
    subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True)
if __name__=="__main__": main()
