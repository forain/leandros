#!/usr/bin/env python3
"""gpu-session-loop.py <n> <tag> [virgl|venus] — Linux host, x86_64/KVM.

Boots N graphical sessions in a row and, per boot, counts host GPU faults
(`journalctl -k`: amdgpu page fault / ring timeout / reset / wedged). Per boot:
start with the GPU device, serial root login, log `leandro` in at the greeter,
then ROUNDS rounds of Super+W x2, Super + "te" + Esc (APPS=1 also opens
cosmic-term (Super+T), runs ls, toggles the app library and closes it each round). Saves serial,
QEMU stderr, a screenshot and any host fault lines under
$LEANDROS_LOOP_OUT/<tag>/ (default /tmp/leandros-gpu-loop).

Written for lane hostgpufault (2026-09-27): a guest session must never fault the
host GPU, and this is the soak that checks it.
"""
import os, sys, time, re, subprocess
os.environ.setdefault("LEANDROS_RUN_ID", "gpuloop")
os.environ.setdefault("LEANDROS_VNC_PORT", "5941")
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../..")))
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver
N = int(sys.argv[1]); TAG = sys.argv[2]; GPU = sys.argv[3] if len(sys.argv) > 3 else "virgl"
ROUNDS = int(os.environ.get("ROUNDS", "8"))
OUT = os.path.join(os.environ.get("LEANDROS_LOOP_OUT", "/tmp/leandros-gpu-loop"), TAG); os.makedirs(OUT, exist_ok=True)
def log(*a): print(time.strftime("%H:%M:%S"), *a, flush=True)
def key(k): driver._monitor_send(f"sendkey {k}"); time.sleep(0.25)
def typ(s):
    for ch in s: key(ch)
def faults(since):
    o = subprocess.run(["journalctl", "-k", "--no-pager", "--since", since], capture_output=True, text=True).stdout
    return [l for l in o.splitlines() if "amdgpu" in l and ("fault" in l or "timeout" in l or "wedged" in l or "reset" in l)]
tot = 0
for i in range(N):
    since = time.strftime("%Y-%m-%d %H:%M:%S")
    log(f"=== boot {i+1}/{N} gpu={GPU}")
    subprocess.run([sys.executable, f"{REPO}/.claude/skills/run-leandros/driver.py", "start", "x86_64", f"--{GPU}"], capture_output=True, timeout=400)
    subprocess.run([sys.executable, f"{REPO}/.claude/skills/run-leandros/driver.py", "login", "root", "root"], capture_output=True, timeout=120)
    time.sleep(45)
    typ("leandro"); key("ret")
    time.sleep(40)
    for r in range(ROUNDS):
        key("meta_l-w"); time.sleep(3); key("meta_l-w"); time.sleep(2)
        key("meta_l"); time.sleep(2); typ("te"); time.sleep(1); key("esc"); time.sleep(1)
        if os.environ.get("APPS"):
            key("meta_l-t"); time.sleep(8)          # cosmic-term (wgpu client)
            typ("ls"); key("spc"); key("minus"); typ("la"); key("spc"); key("slash"); typ("bin"); key("ret"); time.sleep(3)
            key("meta_l-a"); time.sleep(3); key("esc"); time.sleep(1)
            key("meta_l-q"); time.sleep(3)
    time.sleep(10)
    subprocess.run([sys.executable, f"{REPO}/.claude/skills/run-leandros/driver.py", "login", "root", "root"], capture_output=True, timeout=120)
    try:
        o = driver._serial_send('n=0; w=0; g=0; while IFS= read -r l; do case "$l" in *"Selected: AdapterInfo"*) w=$((w+1));; esac; case "$l" in *"backend: Gl"*|*"Gl backend"*) g=$((g+1));; esac; case "$l" in *anick*|*"context is lost"*|*"context lost"*|*"device lost"*|*"DeviceLost"*) n=$((n+1)); echo "ERR $l";; esac; done < /var/log/greetd.log; echo "ALIVE wgpu=$w gl=$g err=$n"', timeout=40)
        alive = [l for l in o.splitlines() if l.startswith("ALIVE") or l.startswith("ERR")][-6:]
    except Exception as e: alive = False
    try: subprocess.run([sys.executable, f"{REPO}/.claude/skills/run-leandros/driver.py", "screenshot", f"{OUT}/boot{i+1}.ppm"], capture_output=True, timeout=60)
    except Exception: pass
    f = faults(since)
    subprocess.run(["cp", driver.SERIAL_LOG, f"{OUT}/serial{i+1}.log"])
    try: subprocess.run(["cp", driver.QEMU_STDERR_LOG, f"{OUT}/stderr{i+1}.log"])
    except Exception: pass
    if f:
        tot += 1
        open(f"{OUT}/fault{i+1}.txt", "w").write("\n".join(f))
    try: driver._serial_send("cp /var/log/greetd.log /tmp/g.log", timeout=10)
    except Exception: pass
    log(f"boot {i+1}: host_fault_lines={len(f)} guest_alive={alive} boots_with_fault={tot}")
    for l in f[:4]: log("   ", l[:200])
    subprocess.run([sys.executable, f"{REPO}/.claude/skills/run-leandros/driver.py", "stop"], capture_output=True, timeout=120)
    time.sleep(3)
log(f"DONE boots={N} boots_with_fault={tot}")
