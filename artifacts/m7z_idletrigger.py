#!/usr/bin/env python3
# Decisive idle-fade test: does the fade fire AFTER real input arms the idle
# timer, and if so what COLOR does our softpipe stack render the single-pixel
# fade overlay? screen_off_time short; inject proven virtio-keyboard sendkey +
# tablet abs motion once to arm/reset, then go idle and densely screenshot.
import subprocess, sys, os, time, threading, re, socket, json
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
MON = "/tmp/leandros-monitor.sock"; QMP = "/tmp/leandros-qmp.sock"
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m7z-screenshots")
ARCH = "aarch64"; IDLE_MS = sys.argv[1] if len(sys.argv) > 1 else "6000"
DRAIN = 150; TAG = sys.argv[2] if len(sys.argv) > 2 else "trig"
ARM_AT = 62
SHOTS = [55, 64, 68, 70, 72, 74, 78, 84, 92, 105, 125]
os.makedirs(OUT, exist_ok=True)

def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t, env=e)
    return (r.stdout or "") + (r.stderr or "")
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def hmp(cmd):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(4); s.connect(MON)
        time.sleep(0.2); s.recv(65536)
        s.sendall((cmd + "\n").encode()); time.sleep(0.3)
        r = s.recv(65536).decode(errors="replace"); s.close(); return r.strip()[:80]
    except Exception as e: return f"(hmp err {e})"
def qmp(cmds):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(5); s.connect(QMP)
        f = s.makefile("rwb"); f.readline()
        f.write(b'{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        out = []
        for c in cmds:
            f.write((json.dumps(c)+"\n").encode()); f.flush(); out.append(f.readline().decode(errors="replace").strip())
        s.close(); return out
    except Exception as e: return [f"(qmp err {e})"]

def arm_input():
    # keyboard (proven path) + several tablet abs motions = seat activity
    ks = [hmp("sendkey a"), hmp("sendkey b"), hmp("sendkey shift")]
    ev = lambda x,y: {"execute":"input-send-event","arguments":{"events":[
        {"type":"abs","data":{"axis":"x","value":x}},{"type":"abs","data":{"axis":"y","value":y}}]}}
    qs = qmp([ev(0x1000,0x1000), ev(0x6000,0x3000), ev(0x3000,0x6000), ev(0x4000,0x4000)])
    return f"keys={ks} qmp_ok={all('return' in x for x in qs)}"

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
    return (w,h,{"center":at(w//2,h//2),"q1":at(w//4,h//4),"q3":at(3*w//4,3*h//4),"top":at(w//2,16)})

def main():
    log(f"==== idle-trigger idle={IDLE_MS}ms tag={TAG} {time.ctime()} ====")
    try: os.remove(SERIAL_LOG)
    except OSError: pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    for attempt in range(1,3):
        clean(); out = d("start", ARCH, "uefi", t=220, env=env)
        if any(m in out for m in ("Login prompt ready","login:","Shell ready")): break
    else:
        log("no boot"); clean(); return
    d("login","root","root", t=45)
    cfgdir="/root/.config/cosmic/com.system76.CosmicIdle/v1"
    d("session","4", f"mkdir -p {cfgdir}", f"printf 'Some({IDLE_MS})' > {cfgdir}/screen_off_time", "echo OK", t=40)
    threading.Thread(target=lambda: d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &", t=DRAIN+40), daemon=True).start()
    log(f"[session draining {DRAIN}s]")
    t0=time.time(); armed=False
    for when in SHOTS:
        if not armed and when>=ARM_AT:
            log(f"[t~{when}] ARM INPUT:", arm_input()); armed=True
        dt=when-(time.time()-t0)
        if dt>0: time.sleep(dt)
        ppm=f"{OUT}/m7z-{ARCH}-{TAG}-t{when}.ppm"; d("screenshot", ppm, t=40)
        s=sample(ppm)
        log(f"[t={when:3d}] " + (" ".join(f"{k}={v}" for k,v in s[2].items()) if s else "(no ppm)"))
    time.sleep(3)
    try:
        data=open(SERIAL_LOG,errors="replace").read()
        ct=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',re.sub(r'\x1b[=>78]','',data))
        open(f"{OUT}/m7z-{ARCH}-{TAG}-serial.txt","w").write(ct[-1200000:])
        for k in ("Idled","Resumed","fade","single","output_power","Overlay","layer_surface","protocol error","fatal"):
            n=ct.count(k)
            if n: log(f"  serial '{k}' x{n}")
    except Exception as e: log("serr", e)
    clean(); log("==== DONE ====")

if __name__=="__main__": main()
