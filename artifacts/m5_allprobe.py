import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = "uefi-hvf" if ARCH == "aarch64" else "uefi"
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return "(TIMEOUT)"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
print(f"==== ALLPROBE {ARCH} {time.ctime()} ====", flush=True)
ok = False
for a in range(1, 8):
    clean(); out = d("start", ARCH, MODE, t=175)
    if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
        ok = True; print(f"BOOTED {a}", flush=True); break
if ok:
    print(d("login","root","root", t=45)[-100:], flush=True)
    for c in ("capprobe", "drmprobe"):
        print(d("cmd", c, t=30), flush=True)
clean(); print("==== DONE ====", flush=True)
