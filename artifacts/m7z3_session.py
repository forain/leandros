#!/usr/bin/env python3
# M7z3 final session verification. Boot fresh, login root, launch the full COSMIC
# session, drain serial, screenshot at t≈45/90/150 (aarch64) with QMP pointer moves
# between shots. Pixel-verify panel bar (dark full-width + teal 220px centered block),
# wallpaper present, cursor moves. Grep serial for the four new components staying
# alive, the applet 1Hz tick, and error signatures.
import subprocess, sys, os, time, threading, re, socket, json
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP="/tmp/leandros-qmp.sock"; SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG  = sys.argv[2] if len(sys.argv) > 2 else "sess"
DRAIN= int(sys.argv[3]) if len(sys.argv) > 3 else 175
SHOTS= [int(x) for x in sys.argv[4].split(",")] if len(sys.argv) > 4 else [45,90,150]
os.makedirs(OUT, exist_ok=True)

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
    except Exception as e: return False
def mouse(x,y):
    ev=lambda ax,v:{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":ax,"value":v}}]}}
    return qmp([ev("x",x),ev("y",y)])
def readppm(p):
    try: data=open(p,"rb").read()
    except OSError: return None
    if not data.startswith(b"P6"): return None
    idx=2; f=[]
    while len(f)<3:
        while idx<len(data) and data[idx:idx+1].isspace(): idx+=1
        s=idx
        while idx<len(data) and not data[idx:idx+1].isspace(): idx+=1
        f.append(int(data[s:idx]))
    w,h,_=f; idx+=1; return (w,h,data[idx:])
def at(img,x,y):
    w,h,pix=img; i=(y*w+x)*3; return (pix[i],pix[i+1],pix[i+2])
def is_teal(c): return c[1]>150 and c[2]>140 and c[0]<110 and c[1]>c[0] and c[2]>c[0]
def teal_extent(img,y):
    w,h,pix=img; run=[]; best=(0,0,0)
    x=0
    while x<w:
        if is_teal(at(img,x,y)):
            s=x
            while x<w and is_teal(at(img,x,y)): x+=1
            if x-s>best[2]: best=(s,x-1,x-s)
        else: x+=1
    return best  # (start,end,width)
def analyze(p):
    img=readppm(p)
    if not img: return None
    w,h,_=img
    top=at(img,w//2,16); center=at(img,w//2,h//2); q3=at(img,3*w//4,3*h//4)
    ext=teal_extent(img,16)  # scan bar row for teal block
    ctr=(ext[0]+ext[1])//2 if ext[2]>0 else -1
    return {"w":w,"h":h,"top":top,"center":center,"q3":q3,"block":ext,"blockctr":ctr,"imgcenter":w//2}
def main():
    log(f"==== M7z3 session {ARCH} tag={TAG} drain={DRAIN} shots={SHOTS} {time.ctime()} ====")
    try: os.remove(SERIAL)
    except OSError: pass
    env={"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    booted=False
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,"uefi",t=220,env=env)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("NO BOOT"); log(out[-1500:]); clean(); return
    d("login","root","root",t=45)
    threading.Thread(target=lambda: d("session",str(DRAIN),"sh /bin/start-cosmic-leandros &",t=DRAIN+40),daemon=True).start()
    log(f"[session launched; draining {DRAIN}s]")
    t0=time.time()
    for i,when in enumerate(SHOTS):
        dt=when-(time.time()-t0)
        if dt>0: time.sleep(dt)
        # move pointer to a distinct spot before each shot (verify cursor moves)
        mx,my=(300+300*i, 200+150*i)
        mvok=mouse(int(mx*0x7fff/1280 if ARCH=="aarch64" else mx*0x7fff/1920),
                   int(my*0x7fff/800 if ARCH=="aarch64" else my*0x7fff/1080))
        time.sleep(0.5)
        ppm=f"{OUT}/m7z3-{ARCH}-{TAG}-t{when}.ppm"
        d("screenshot",ppm,t=40); a=analyze(ppm)
        if a:
            log(f"[t={when:3d}] {a['w']}x{a['h']} top={a['top']} center={a['center']} q3={a['q3']} "
                f"tealblock={a['block']} blkctr={a['blockctr']} imgctr={a['imgcenter']} mv={mvok}")
        else:
            log(f"[t={when:3d}] (no ppm) mv={mvok}")
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/m7z3-{ARCH}-{TAG}-serial.txt","w").write(ct[-1200000:])
        log("--- serial signal counts ---")
        for k in ("leandros-applet","tick: 1000ms","entering event loop","committed 220x32",
                  "cosmic-panel","GL Renderer","softpipe","com.system76.CosmicAppletTime",
                  "cosmic-workspaces","cosmic-greeter","cosmic-files-applet","cosmic-idle",
                  "cosmic-bg","Rendering space"):
            n=ct.count(k)
            if n: log(f"  '{k}' x{n}")
        log("--- error signatures (want 0) ---")
        for k in ("No such file or directory","No file descriptors available","EMFILE",
                  "Unknown id","Broken pipe","PANEL MAIN ERR","EL0 Fault","panic",
                  "failed to start process","os error 24","code 101","restart"):
            n=ct.count(k)
            log(f"  '{k}' x{n}")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== session run DONE ====")
if __name__=="__main__": main()
