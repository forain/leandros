#!/usr/bin/env python3
# M9 regression (m7v_regress.py + drmsmoke): fresh boot, login root, run vfstest FIRST, then scmtest (with
# the new mincore test), wakepolltest, and the core suite — via scmrun.py's
# persistent serial reader (driver.py cmd early-breaks on '-> ' diagnostics).
import subprocess, sys, os, time, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SCMRUN = os.path.expanduser("~/code/leandros-artifacts/scmrun.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
TAG  = sys.argv[3] if len(sys.argv) > 3 else "reg"

# (command, read-seconds)
TESTS = [
    ("vfstest", 60),
    ("drmsmoke", 60),
    ("scmtest", 55),
    ("wakepolltest", 55),
    ("forktest", 25),
    ("epolltest", 30),
    ("polltest", 30),
    ("waittest", 40),
    ("sigtest", 25),
    ("timertest", 60),
    ("memtest", 25),
]

def d(*a, t=220):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"

def scm(cmd, dur):
    try:
        r = subprocess.run(["python3", SCMRUN, cmd, str(dur)], capture_output=True, text=True, timeout=dur + 40)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {cmd})"

def log(*a): print(*a, flush=True)

SOCK = "/tmp/leandros-serial.sock"
PIDF = "/tmp/leandros-qemu.pid"

def clean():
    d("stop", t=30)
    # NOTE: the bracket makes the pattern non-self-matching. A plain
    # "qemu-system" also matches any wrapper shell whose command line mentions
    # it, which has repeatedly killed the harness itself.
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(2)

def vm_alive():
    """True iff the serial socket is there and the QEMU pid is still running."""
    if not os.path.exists(SOCK):
        return False
    try:
        pid = int(open(PIDF).read().strip())
    except Exception:
        return True  # no pidfile, trust the socket
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True

def death_report():
    out = []
    for p in ("/tmp/leandros-qemu-stderr.log", "/tmp/leandros-serial.log"):
        try:
            with open(p, "rb") as f:
                f.seek(0, 2); f.seek(max(0, f.tell() - 1200))
                out.append(f"--- tail {p} ---\n" + f.read().decode(errors="replace"))
        except Exception as e:
            out.append(f"--- {p}: {e} ---")
    return "\n".join(out)

def main():
    log(f"==== M9 regress {ARCH} {MODE} tag={TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=220)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("no boot"); log(out[-1500:]); clean(); return
    d("login", "root", "root", t=45)
    # sacrificial warm-up (first serial command after login sometimes drops head)
    scm("echo WARMUP", 4)

    summary = []
    for cmd, dur in TESTS:
        if not vm_alive():
            log(f"  [{cmd}] !! VM IS GONE before this test — aborting suite")
            log(death_report())
            summary.append((cmd, 0, 0, False))
            break
        txt = scm(cmd, dur)
        if not vm_alive():
            log(f"  [{cmd}] !! VM DIED DURING this test")
            log(death_report())
        clean_txt = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', txt))
        open(f"{OUT}/m9-{TAG}-{ARCH}-{cmd}.txt", "w").write(clean_txt)
        npass = len(re.findall(r'\bPASS\b', clean_txt))
        nfail = len(re.findall(r'\bFAIL\b', clean_txt))
        # test-specific done markers
        done = any(m in clean_txt for m in (f"{cmd} done", "--- ", "ALL", "results", "SUMMARY"))
        summary.append((cmd, npass, nfail, done))
        log(f"  [{cmd}] PASS={npass} FAIL={nfail} done={done}")
    log("==== SUMMARY ====")
    for cmd, p, f, dn in summary:
        log(f"  {cmd:16s} PASS={p:3d} FAIL={f:3d} {'OK' if f==0 and p>0 else 'CHECK'}")
    clean()
    log("==== regress DONE ====")

if __name__ == "__main__":
    main()
