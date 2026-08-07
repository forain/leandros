#!/usr/bin/env python3
# M7z3 idle-fade validation of the leandros-applet 1Hz re-commit tick.
#   mode=noinput : override CosmicIdle screen_off_time to a short value, inject
#                  ZERO input after session-up, verify the fade fires on time
#                  (top-bar teal g drops) — validates applet-tick -> comp repaint
#                  -> is_inhibited refresh -> idle timer arms -> fade.
#   mode=input   : same short timeout but inject pointer motion every 3s; screen
#                  must stay BRIGHT (timer keeps resetting).
import subprocess, sys, os, time, threading, re, socket, json
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP="/tmp/leandros-qmp.sock"; SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
MODE = sys.argv[1] if len(sys.argv)>1 else "noinput"
IDLE_MS = sys.argv[2] if len(sys.argv)>2 else "6000"
ARCH="aarch64"; DRAIN=200
SHOTS=[50,58,66,74,82,90,100,115,130,150,175,195]
os.makedirs(OUT,exist_ok=True)
def d(*a,t=260,env=None):
    e=dict(os.environ); e.update(env or {})
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t,env=e); return (r.stdout or"")+(r.stderr or"")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
def qmp(cmds):
    try:
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(5); s.connect(QMP)
        f=s.makefile("rwb"); f.readline(); f.write(b'{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        o=[]
        for c in cmds: f.write((json.dumps(c)+"\n").encode()); f.flush(); o.append(f.readline().decode(errors="replace").strip())
        s.close(); return all("return" in x for x in o)
    except Exception: return False
def arm(x,y):
    ev=lambda ax,v:{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":ax,"value":v}}]}}
    return qmp([ev("x",x),ev("y",y)])
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
    aat=lambda x,y:(pix[(y*w+x)*3],pix[(y*w+x)*3+1],pix[(y*w+x)*3+2])
    return {"center":aat(w//2,h//2),"top":aat(w//2,16),"q3":aat(3*w//4,3*h//4)}
def main():
    log(f"==== M7z3 idle mode={MODE} idle={IDLE_MS} {time.ctime()} ====")
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
    log(f"[draining; MODE={MODE}]")
    t0=time.time(); faded=None; stayed_bright=True
    for w in SHOTS:
        dt=w-(time.time()-t0)
        if dt>0: time.sleep(dt)
        if MODE=="input":
            # inject motion each shot window to keep it awake
            arm(0x3000 if w%2 else 0x5000, 0x3000 if w%2 else 0x5000)
        p=f"{OUT}/m7z3-idle-{MODE}-t{w}.ppm"; d("screenshot",p,t=40); s=sample(p)
        mark=""
        if s:
            g=s["top"][1]
            if g<150:
                mark=" <== DIMMED/FADED"
                if faded is None: faded=w
                if MODE=="input": stayed_bright=False
        log(f"[t={w:3d}] "+(" ".join(f"{k}={v}" for k,v in s.items()) if s else "(no ppm)")+mark)
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/m7z3-idle-{MODE}-serial.txt","w").write(ct[-1000000:])
        for k in ("tick: 1000ms","loginctl lock-session","lock-session","cosmic_idle] command","Idled","fade"):
            n=ct.count(k)
            if n: log(f"  serial '{k}' x{n}")
    except Exception as e: log("serr",e)
    log(f"RESULT mode={MODE} faded_at={faded} stayed_bright={stayed_bright}")
    clean(); log("==== DONE ====")
if __name__=="__main__": main()
