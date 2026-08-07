#!/usr/bin/env python3
# Gap-2 follow-up: multi-pool sampler. Checksums the applet pool AND the panel's
# own bar pools on one shared timeline, to split
#   (1) panel's embedded compositor caches the applet texture, vs
#   (2) the panel never repaints/re-presents its own bar.
# Same screendump discipline as g2_run.py: two well-separated shots, byte-compare
# the bar rows, so "the screen was frozen during the window" is evidence.
import subprocess, sys, os, time, threading, re, socket, json, itertools
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP="/tmp/leandros-qmp.sock"; SERIAL="/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/g2-run")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG  = sys.argv[2] if len(sys.argv) > 2 else "g2b"
DRAIN= int(sys.argv[3]) if len(sys.argv) > 3 else 200
SHOTS= [int(x) for x in sys.argv[4].split(",")] if len(sys.argv) > 4 else [70,175]
os.makedirs(OUT, exist_ok=True)

def d(*a,t=260,env=None):
    e=dict(os.environ); e.update(env or {})
    try: r=subprocess.run(["python3",DRIVER,*a],capture_output=True,text=True,timeout=t,env=e); return (r.stdout or"")+(r.stderr or"")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {a})"
def log(*a): print(*a,flush=True)
def clean(): d("stop",t=30); subprocess.run(["pkill","-9","-f","qemu-syste[m]"],capture_output=True); time.sleep(2)
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

def analyze_sums(raw):
    """Per-idx [G2SUM] statistics on the shared t= timeline."""
    recs={}
    for m in re.finditer(r"\[G2SUM\] t=0x([0-9a-f]+) idx=0x([0-9a-f]+) np=0x([0-9a-f]+) p0=0x([0-9a-f]+) vlen=0x([0-9a-f]+) sum=(0x[0-9a-f]+)", raw):
        t=int(m.group(1),16); idx=int(m.group(2),16)
        recs.setdefault(idx,[]).append((t,int(m.group(5),16),m.group(6)))
    out={}
    for idx,s in sorted(recs.items()):
        sums=[x[2] for x in s]
        trans=[(s[i+1][0]) for i in range(len(s)-1) if sums[i]!=sums[i+1]]
        runs=[(k,len(list(g))) for k,g in itertools.groupby(sums)]
        out[idx]={"n":len(s),"t0":s[0][0],"t1":s[-1][0],"vlen":s[0][1],
                  "distinct":len(set(sums)),"transitions":len(trans),
                  "first_change_t":trans[0] if trans else None,
                  "last_change_t":trans[-1] if trans else None,
                  "longest_static_run":max(r[1] for r in runs),
                  "samples":s}
    return out

def main():
    log(f"==== G2 run2 (multi-pool) {ARCH} tag={TAG} drain={DRAIN} shots={SHOTS} {time.ctime()} ====")
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
    t0=time.time(); shot_paths=[]
    for i,when in enumerate(SHOTS):
        dt=when-(time.time()-t0)
        if dt>0: time.sleep(dt)
        mx,my=(300+300*i, 200+150*i)
        mouse(int(mx*0x7fff/1280 if ARCH=="aarch64" else mx*0x7fff/1920),
              int(my*0x7fff/800 if ARCH=="aarch64" else my*0x7fff/1080))
        time.sleep(0.5)
        ppm=f"{OUT}/g2b-{ARCH}-{TAG}-t{when}.ppm"
        d("screenshot",ppm,t=40); shot_paths.append((when,ppm))
        log(f"[t={when:3d}] shot {'ok' if readppm(ppm) else 'FAILED'} {ppm}")
    log("--- FREEZE CHECK (screendump byte-compare) ---")
    if len(shot_paths)>=2:
        a=readppm(shot_paths[0][1]); b=readppm(shot_paths[-1][1])
        if a and b:
            w,h,pa=a; _,_,pb=b
            bar_a,bar_b = pa[:w*40*3], pb[:w*40*3]
            log(f"  bar rows 0-39   identical t{shot_paths[0][0]} vs t{shot_paths[-1][0]}: {bar_a==bar_b}")
            # the applet block sits centred in the bar; compare just its columns
            x0,x1=(w-220)//2,(w+220)//2
            blk_a=b"".join(pa[(y*w+x0)*3:(y*w+x1)*3] for y in range(0,32))
            blk_b=b"".join(pb[(y*w+x0)*3:(y*w+x1)*3] for y in range(0,32))
            log(f"  applet block    identical: {blk_a==blk_b}")
            log(f"  whole screen    identical: {pa==pb}")
    time.sleep(3)
    try:
        data=open(SERIAL,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/g2b-{ARCH}-{TAG}-serial.txt","w").write(ct[-6000000:])
        g2=[l for l in ct.splitlines() if "[G2" in l]
        open(f"{OUT}/g2b-{ARCH}-{TAG}-g2lines.txt","w").write("\n".join(g2))
        log("--- non-SUM G2 lines (ACQ/MAP/IMP/MEMFD/FALL, excl. kind=0x4 noise) ---")
        for l in g2:
            if "[G2SUM]" in l: continue
            if "[G2FALL]" in l and "kind=0x4" in l: continue
            log("  "+l.strip())
        log("--- PER-POOL [G2SUM] ANALYSIS (shared t= timeline, 0.5 Hz) ---")
        st=analyze_sums(ct)
        for idx,v in st.items():
            verdict=("CONSTANT" if v["transitions"]==0 else
                     f"ADVANCED ({v['transitions']} transitions)")
            log(f"  idx=0x{idx:x} vlen=0x{v['vlen']:x} samples={v['n']} "
                f"t=[0x{v['t0']:x}..0x{v['t1']:x}] distinct={v['distinct']} "
                f"longest_static_run={v['longest_static_run']} -> {verdict}")
            if v["transitions"]:
                log(f"      first change t=0x{v['first_change_t']:x}  "
                    f"last change t=0x{v['last_change_t']:x}  "
                    f"(sampler ends t=0x{v['t1']:x})")
        for idx,v in st.items():
            log(f"--- idx=0x{idx:x}: first 6 / last 6 of {v['n']} samples ---")
            for t,vl,s in v["samples"][:6]: log(f"    t=0x{t:x} sum={s}")
            log("    ...")
            for t,vl,s in v["samples"][-6:]: log(f"    t=0x{t:x} sum={s}")
        log("--- session signals ---")
        for k in ("leandros-applet","committed 220x32","entering event loop",
                  "cosmic-panel","GL Renderer","Broken pipe","PANEL MAIN ERR",
                  "panic","EL0 Fault","Rendering space"):
            log(f"  '{k}' x{ct.count(k)}")
    except Exception as e: log(f"[serial err] {e}")
    clean(); log("==== G2 run2 DONE ====")
if __name__=="__main__": main()
