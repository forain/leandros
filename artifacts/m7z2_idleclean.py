#!/usr/bin/env python3
# Clean Issue-2 demo on the healthy (raised-pool) session:
#   Phase B: inject input every 3s → screen stays BRIGHT (idle timer keeps resetting)
#   Phase C: stop input → screen FADES to black (Idled fires, cosmic-idle fade + DPMS)
import subprocess, sys, os, time, threading, re, socket, json
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
MON="/tmp/leandros-monitor.sock"; QMP="/tmp/leandros-qmp.sock"; SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH="aarch64"; IDLE_MS = sys.argv[1] if len(sys.argv)>1 else "5000"; DRAIN=150
os.makedirs(OUT, exist_ok=True)
def d(*a,t=260,env=None):
    e=dict(os.environ); e.update(env or {})
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t,env=e); return (r.stdout or"")+(r.stderr or"")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def hmp(cmd):
    try:
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(4); s.connect(MON)
        time.sleep(0.2); s.recv(65536); s.sendall((cmd+"\n").encode()); time.sleep(0.2); s.recv(65536); s.close(); return "ok"
    except Exception as e: return f"(hmp {e})"
def qmp(cmds):
    try:
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(5); s.connect(QMP)
        f=s.makefile("rwb"); f.readline(); f.write(b'{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        o=[]
        for c in cmds: f.write((json.dumps(c)+"\n").encode()); f.flush(); o.append(f.readline().decode(errors="replace").strip())
        s.close(); return all("return" in x for x in o)
    except Exception as e: return False
def arm():
    ev=lambda x,y:{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":"x","value":x}},{"type":"abs","data":{"axis":"y","value":y}}]}}
    hmp("sendkey a"); return qmp([ev(0x3000,0x3000),ev(0x5000,0x5000)])
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
def shot(tag,when):
    p=f"{OUT}/m7z2-idleclean-{tag}-t{when}.ppm"; d("screenshot",p,t=40); s=sample(p)
    log(f"[t={when:3d}] "+(" ".join(f"{k}={v}" for k,v in s.items()) if s else "(no ppm)")); return p
def main():
    log(f"==== idleclean idle={IDLE_MS} {time.ctime()} ====")
    try: os.remove(SERIAL)
    except OSError: pass
    env={"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    for _ in range(2):
        clean(); out=d("start",ARCH,"uefi",t=220,env=env)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): break
    else: log("no boot"); clean(); return
    d("login","root","root",t=45)
    cfg="/root/.config/cosmic/com.system76.CosmicIdle/v1"
    d("session","4",f"mkdir -p {cfg}",f"printf 'Some({IDLE_MS})' > {cfg}/screen_off_time","echo OK",t=40)
    threading.Thread(target=lambda: d("session",str(DRAIN),"sh /bin/start-cosmic-leandros &",t=DRAIN+40),daemon=True).start()
    log("[draining]")
    t0=time.time()
    def waituntil(w):
        dt=w-(time.time()-t0)
        if dt>0: time.sleep(dt)
    # baseline (session settled, no idle yet)
    waituntil(52); shot("base",52)
    # Phase B: continuous input t=54..92 -> must stay BRIGHT
    log("--- Phase B: continuous input (expect BRIGHT) ---")
    for w in range(54,93,3):
        waituntil(w); ok=arm()
        if w in (60,78,90): shot("inputON",w)
    # Phase C: stop input t=93.. -> must FADE
    log("--- Phase C: no input (expect FADE to dark) ---")
    for w in (100,108,116,124,132,140):
        waituntil(w); shot("idle",w)
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        for k in ("loginctl lock-session","lock-session","cosmic_idle] command","Idled","fade"):
            n=ct.count(k)
            if n: log(f"  serial '{k}' x{n}")
    except Exception as e: log("serr",e)
    clean(); log("==== DONE ====")
if __name__=="__main__": main()
