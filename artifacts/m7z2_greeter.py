#!/usr/bin/env python3
# Issue 1 verification: boot, launch cosmic session, watch for cosmic-greeter
# EMFILE crash-loop vs. staying alive. Dumps [XFDS] fd-census lines (kernel
# instrumentation) and greeter/EMFILE markers. Fresh full-rebuild image must be
# in place at f2fs-data0 already.
import subprocess, sys, os, time, threading, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
DRAIN = int(sys.argv[2]) if len(sys.argv) > 2 else 160
TAG = sys.argv[3] if len(sys.argv) > 3 else "greeter"
SHOTS = [55, 80, 110, 140]
os.makedirs(OUT, exist_ok=True)

def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def sample(path):
    try: data = open(path,"rb").read()
    except OSError: return None
    if not data.startswith(b"P6"): return None
    idx=2; f=[]
    while len(f)<3:
        while idx<len(data) and data[idx:idx+1].isspace(): idx+=1
        s=idx
        while idx<len(data) and not data[idx:idx+1].isspace(): idx+=1
        f.append(int(data[s:idx]))
    w,h,_=f; idx+=1; pix=data[idx:]
    at=lambda x,y:(pix[(y*w+x)*3],pix[(y*w+x)*3+1],pix[(y*w+x)*3+2])
    return (w,h,{"center":at(w//2,h//2),"top":at(w//2,16),"low":at(w//2,int(h*0.75))})

def main():
    log(f"==== m7z2 greeter {ARCH} drain={DRAIN} tag={TAG} {time.ctime()} ====")
    try: os.remove(SERIAL_LOG)
    except OSError: pass
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start", ARCH, "uefi", t=220)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted:
        log("no boot"); log(out[-1500:]); clean(); return
    d("login","root","root", t=45)
    threading.Thread(target=lambda: d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &", t=DRAIN+40), daemon=True).start()
    log(f"[session draining {DRAIN}s]")
    t0=time.time()
    for when in SHOTS:
        dt=when-(time.time()-t0)
        if dt>0: time.sleep(dt)
        ppm=f"{OUT}/m7z2-{ARCH}-{TAG}-t{when}.ppm"; d("screenshot", ppm, t=40)
        s=sample(ppm)
        log(f"[t={when:3d}] " + (" ".join(f"{k}={v}" for k,v in s[2].items()) if s else "(no ppm)"))
    time.sleep(4)
    try:
        data=open(SERIAL_LOG,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/m7z2-{ARCH}-{TAG}-serial.txt","w").write(ct[-2000000:])
        # count greeter starts/restarts and EMFILE
        for k in ("greeter","No file descriptors","os error 24","EMFILE","panic","XFDS",
                  "Idled","GL Renderer","committed","Starting: /bin/cosmic-greeter"):
            n=ct.count(k)
            if n: log(f"  serial '{k}' x{n}")
        # print the [XFDS] lines (the fd census per execve)
        xf=[l for l in ct.splitlines() if "[XFDS]" in l]
        log(f"  --- [XFDS] lines: {len(xf)} (showing max-survIno + last 12) ---")
        # highlight the ones with biggest sweptIno / survIno
        def field(l,name):
            m=re.search(name+r"=(0x[0-9a-fA-F]+|\d+)", l)
            return int(m.group(1),0) if m else 0
        xf_sorted=sorted(xf, key=lambda l: field(l,"sweptIno")+field(l,"survIno"), reverse=True)
        for l in xf_sorted[:6]: log("   MAX", l.strip())
        for l in xf[-12:]: log("   ", l.strip())
        # greeter context lines
        gl=[l for l in ct.splitlines() if "greeter" in l.lower()]
        log(f"  --- greeter lines: {len(gl)} (last 15) ---")
        for l in gl[-15:]: log("   ", l.strip()[:160])
    except Exception as e: log("serr", e)
    clean(); log("==== DONE ====")

if __name__=="__main__": main()
