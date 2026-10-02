#!/usr/bin/env python3
"""runtests.py <arch> <tag> <cmd>...  — headless (no GPU) boot, root login, run each cmd, print output.
RUNTESTS_VIRGL=1 boots with --virgl instead (for drmsmoke and other GPU tests)."""
import os, sys, time, subprocess
os.environ.setdefault("LEANDROS_RUN_ID", "fftest")
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../..")))
DRV = f"{REPO}/.claude/skills/run-leandros/driver.py"
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver
arch, tag, cmds = sys.argv[1], sys.argv[2], sys.argv[3:]
OUT = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"tests-{tag}")
os.makedirs(OUT, exist_ok=True)
subprocess.run([sys.executable, DRV, "start", arch] + (["--virgl"] if os.environ.get("RUNTESTS_VIRGL") else []),
               capture_output=True, timeout=600)
subprocess.run([sys.executable, DRV, "login", "root", "root"], capture_output=True, timeout=180)
res = open(f"{OUT}/results.txt", "w")
for c in cmds:
    t = 400
    try:
        o = driver._serial_send(c + "; echo RC=$?", timeout=t)
    except BaseException as e:
        o = f"<error {e}>"
    lines = [l for l in o.splitlines() if not l.startswith("[FORK]")]
    txt = "\n".join(lines)
    rc = [l for l in lines if l.startswith("RC=")]
    print(f"=== {c}: {rc[-1] if rc else 'RC=?'}", flush=True)
    res.write(f"=== {c}\n{txt}\n"); res.flush()
subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial.log"])
subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
