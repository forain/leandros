#!/usr/bin/env python3
# M4f final regression: fresh boot, vfstest FIRST (dirty-image discipline), then the
# watched suite, then kmscube -D animation check (2 screenshots, must differ) — all in
# ONE boot to avoid double-booting slow TCG. arch + mode args.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-hvf"
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def dcmd(c, t=120):
    o = d("cmd", c, t=t); log(f"\n$ {c}\n{o.strip()[-1600:]}"); return o
def boot():
    for attempt in range(1, 6):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False
def main():
    os.makedirs(OUT, exist_ok=True)
    log(f"==== M5f REGRESSION {ARCH} {MODE} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    # vfstest FIRST on fresh image (discipline rule 5)
    dcmd("vfstest", t=180)
    for t in ["drmsmoke","scmtest","epolltest","evtest2","polltest","sigtest","timertest","waittest","idletest"]:
        dcmd(t, t=150)
    # kmscube -D animation check (2 screenshots ~1.5s apart, must differ)
    log("---- kmscube -D animation ----")
    dcmd("setsid kmscube -D /dev/dri/card0 >/tmp/kc.log 2>&1 &", t=8)
    dcmd("sleep 5; echo ===KCLOG===; tail -n 6 /tmp/kc.log", t=15)
    d("screenshot", f"{OUT}/m5f-kmscube-{ARCH}-1.ppm", t=30)
    dcmd("sleep 1.5", t=6)
    d("screenshot", f"{OUT}/m5f-kmscube-{ARCH}-2.ppm", t=30)
    try:
        a=open(f"{OUT}/m5f-kmscube-{ARCH}-1.ppm",'rb').read()
        b=open(f"{OUT}/m5f-kmscube-{ARCH}-2.ppm",'rb').read()
        diff=sum(1 for x,y in zip(a,b) if x!=y)
        log(f"KMSCUBE FRAME_DIFF_BYTES={diff} (of {min(len(a),len(b))}) -> {'ANIMATING' if diff>10000 else 'STATIC/NO-RENDER'}")
    except Exception as e:
        log(f"kmscube diff err {e}")
    clean()
    log("==== REGRESSION DONE ====")
if __name__ == "__main__": main()
