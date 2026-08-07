#!/usr/bin/env python3
# Gap-2 instrumentation run. Boot fresh, login root, launch the full COSMIC
# session, drain serial long enough for >=60 [G2SUM] samples, screenshot twice to
# confirm the freeze still reproduces, then dump every [G2*] line.
import subprocess, sys, os, time, threading, re, socket, json
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP="/tmp/leandros-qmp.sock"; SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/g2-run")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG  = sys.argv[2] if len(sys.argv) > 2 else "g2"
DRAIN= int(sys.argv[3]) if len(sys.argv) > 3 else 190
SHOTS= [int(x) for x in sys.argv[4].split(",")] if len(sys.argv) > 4 else [70,160]
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
    except Exception: return False
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
def main():
    log(f"==== G2 run {ARCH} tag={TAG} drain={DRAIN} shots={SHOTS} {time.ctime()} ====")
    try: os.remove(SERIAL)
    except OSError: pass
    env={"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    booted=False; out=""
    for attempt in range(1,3):
        log(f"#### BOOT {attempt} ####"); clean()
        out=d("start",ARCH,"uefi",t=220,env=env)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): booted=True; break
    if not booted: log("NO BOOT"); log(out[-2000:]); clean(); return
    d("login","root","root",t=45)
    threading.Thread(target=lambda: d("session",str(DRAIN),"sh /bin/start-cosmic-leandros &",t=DRAIN+40),daemon=True).start()
    log(f"[session launched; draining {DRAIN}s]")
    t0=time.time()
    shot_paths=[]
    for i,when in enumerate(SHOTS):
        dt=when-(time.time()-t0)
        if dt>0: time.sleep(dt)
        mx,my=(300+300*i, 200+150*i)
        mouse(int(mx*0x7fff/1280 if ARCH=="aarch64" else mx*0x7fff/1920),
              int(my*0x7fff/800 if ARCH=="aarch64" else my*0x7fff/1080))
        time.sleep(0.5)
        ppm=f"{OUT}/g2-{ARCH}-{TAG}-t{when}.ppm"
        d("screenshot",ppm,t=40); shot_paths.append(ppm)
        img=readppm(ppm)
        log(f"[t={when:3d}] shot {'ok' if img else 'FAILED'} {ppm}")
    # Compare the panel bar region across the two shots: identical bytes => frozen.
    if len(shot_paths)>=2:
        a=readppm(shot_paths[0]); b=readppm(shot_paths[-1])
        if a and b:
            w,h,pa=a; _,_,pb=b
            bar_a=pa[:w*40*3]; bar_b=pb[:w*40*3]
            log(f"[FREEZE CHECK] panel bar rows0-39 identical across shots: {bar_a==bar_b}")
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/g2-{ARCH}-{TAG}-serial.txt","w").write(ct[-4000000:])
        g2=[l for l in ct.splitlines() if "[G2" in l]
        open(f"{OUT}/g2-{ARCH}-{TAG}-g2lines.txt","w").write("\n".join(g2))
        log(f"--- [G2*] lines: {len(g2)} ---")
        for k in ("[G2MEMFD]","[G2IMP]","[G2ACQ]","[G2MAP]","[G2FALL]","[G2SUM]","[G2MSF]"):
            log(f"  {k} x{sum(1 for l in g2 if k in l)}")
        log("--- non-SUM G2 lines (all) ---")
        for l in g2:
            if "[G2SUM]" not in l: log("  "+l.strip())
        sums=[l for l in g2 if "[G2SUM]" in l]
        log(f"--- [G2SUM] first 8 / last 8 of {len(sums)} ---")
        for l in sums[:8]: log("  "+l.strip())
        log("  ...")
        for l in sums[-8:]: log("  "+l.strip())
        uniq=set()
        for l in sums:
            m=re.search(r"sum=(\S+)", l)
            if m: uniq.add(m.group(1))
        log(f"--- distinct sum= values across {len(sums)} samples: {len(uniq)} ---")
        log("--- session signals ---")
        for k in ("leandros-applet","committed 220x32","entering event loop","cosmic-panel",
                  "GL Renderer","Broken pipe","PANEL MAIN ERR","panic","EL0 Fault"):
            log(f"  '{k}' x{ct.count(k)}")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== G2 run DONE ====")
if __name__=="__main__": main()
