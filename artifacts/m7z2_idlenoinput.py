#!/usr/bin/env python3
# Cleanest Issue-2 test: healthy session, short screen_off, ZERO injected input,
# long observation. Expect the fade to fire and the screen to go dark/DPMS-off.
import subprocess, sys, os, time, threading, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH="aarch64"; IDLE_MS = sys.argv[1] if len(sys.argv)>1 else "5000"; DRAIN=200
SHOTS=[50,58,66,74,82,90,100,115,130,150,175,195]
os.makedirs(OUT,exist_ok=True)
def d(*a,t=260,env=None):
    e=dict(os.environ); e.update(env or {})
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t,env=e); return (r.stdout or"")+(r.stderr or"")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def sample(p):
    try: data=open(p,"rb").read()
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
    return {"center":at(w//2,h//2),"top":at(w//2,16),"q3":at(3*w//4,3*h//4)}
def main():
    log(f"==== idle NO-INPUT idle={IDLE_MS} {time.ctime()} ====")
    try: os.remove(SERIAL)
    except OSError: pass
    for _ in range(2):
        clean(); out=d("start",ARCH,"uefi",t=220)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): break
    else: log("no boot"); clean(); return
    d("login","root","root",t=45)
    cfg="/root/.config/cosmic/com.system76.CosmicIdle/v1"
    d("session","4",f"mkdir -p {cfg}",f"printf 'Some({IDLE_MS})' > {cfg}/screen_off_time","echo OK",t=40)
    threading.Thread(target=lambda: d("session",str(DRAIN),"sh /bin/start-cosmic-leandros &",t=DRAIN+40),daemon=True).start()
    log("[draining; NO input injected]")
    t0=time.time(); faded_at=None
    for w in SHOTS:
        dt=w-(time.time()-t0)
        if dt>0: time.sleep(dt)
        p=f"{OUT}/m7z2-noinput-t{w}.ppm"; d("screenshot",p,t=40); s=sample(p)
        mark=""
        if s and s["top"][1] < 150: mark=" <== FADED"  # teal g=214 normally; <150 = dimmed
        log(f"[t={w:3d}] "+(" ".join(f"{k}={v}" for k,v in s.items()) if s else "(no ppm)")+mark)
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        for k in ("loginctl lock-session","cosmic_idle] command","lock-session"):
            n=ct.count(k)
            if n: log(f"  serial '{k}' x{n}  (=> fade_done ran => Idled fired)")
    except Exception as e: log("serr",e)
    clean(); log("==== DONE ====")
if __name__=="__main__": main()
