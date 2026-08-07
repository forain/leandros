#!/usr/bin/env python3
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
    ("exec_repl",   "sh -c 'exec /bin/echo MARK_execrepl'"),
    ("fd3_redir",   "sh -c 'echo hi 3>/tmp/fd3; echo MARK_fd3'"),
    ("nested_exec", "sh -c 'exec /bin/sh -c \"echo MARK_nested\"'"),
    ("subsh_fd3_bg","sh -c 'echo $$ >/tmp/pf; /bin/sh -c \"echo MARK_sub\" 3>/tmp/rf & wait'"),
    # full launcher but replace cosmic with echo: brush runs real dbus-run-session
    # (busd spawn + poll + exec), execing /bin/echo instead of cosmic-session.
    ("drs_full_echo","RUST_LOG=info sh /usr/bin/dbus-run-session -- /bin/echo MARK_drsfull"),
    # the actual launcher, foreground:
    ("real_launcher","sh /bin/start-cosmic-leandros"),
]
def main():
    subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
    booted=False
    for _ in range(3):
        out=d("start","aarch64","uefi-tcg",t=220)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")): booted=True; break
    if not booted: log("FATAL no boot"); return
    d("login","root","root",t=45)
    d("cmd","export XDG_RUNTIME_DIR=/run/user/0","6")
    for name,cmd in PROBES:
        tt = 40 if name in ("drs_full_echo","real_launcher") else 12
        out=deansi(d("cmd",cmd,str(tt)))
        got = ("MARK_" in out)
        log(f"[{name}] {'OK' if got else 'NO-MARKER => TRIGGER?'} :: {out.strip()[-160:]!r}")
    log("=== serial tail (faults?) ===")
    for ln in deansi(d("log",t=30)).splitlines():
        if any(k in ln for k in ("EL0 Fault","[BT] 0 ret","[VMA]*","ELR=0000000001516B04")): log("  "+ln)
    subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True)
if __name__=="__main__": main()
