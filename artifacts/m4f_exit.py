#!/usr/bin/env python3
# Robust M4 exit with PERSISTENT serial reader (QEMU serial server=on,wait=off drops
# output when no client is connected — the settle window lost all UXTR before).
# Sequence: boot/login/launch m4run via driver.py (serial), THEN attach our own
# persistent serial reader for the whole settle+QMP window (screenshots=monitor sock,
# QMP=qmp sock, no serial conflict). Evidence: UXTR CON/ACC/SND/RCV + wlclient lines.
import subprocess, sys, os, time, shutil, socket, select, threading
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = os.path.expanduser("~/code/leandros-artifacts/m4-client/qmp.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m4-screenshots")
SERIAL_SOCK = "/tmp/leandros-serial.sock"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi-hvf"
SETTLE = int(sys.argv[3]) if len(sys.argv) > 3 else 150
TAG = f"{ARCH}-{MODE.replace('uefi-','').replace('uefi','tcg')}"
CAP = f"{OUT}/m4f-{TAG}-serial.log"
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def qmp(*a):
    try:
        r = subprocess.run(["python3", QMP, *a], capture_output=True, text=True, timeout=15)
        log(f"QMP {' '.join(a)} -> {(r.stdout or r.stderr).strip()[-120:]}")
    except Exception as e: log(f"QMP {' '.join(a)} ERR {e!r}")
def shot(name):
    d("screenshot", f"{OUT}/m4e-r-{TAG}-{name}.ppm", t=30); log(f"[shot] {name}")
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def boot():
    for attempt in range(1, 7):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####"); clean()
        os.environ["LEANDROS_QEMU_EXTRA"] = "-qmp unix:/tmp/leandros-qmp.sock,server,nowait"
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False

def find_window(ppm_path):
    """Locate the composited client window (distinct gradient vs lavender ~(204,204,229))
    in a P6 ppm. Returns (cx, cy, W, H) screen coords or None. Uses per-row/col
    foreground density so the small cursor doesn't skew the bounding box."""
    try:
        f = open(ppm_path, "rb")
        assert f.readline().strip() == b"P6"
        w, h = map(int, f.readline().split()); f.readline()
        data = f.read(w*h*3)
    except Exception as e:
        log(f"[find_window] {e}"); return None
    def fg(i):
        return abs(data[i]-204)+abs(data[i+1]-204)+abs(data[i+2]-229) > 120
    rows = [0]*h; cols = [0]*w
    for y in range(0, h, 2):
        base = y*w*3
        for x in range(0, w, 2):
            if fg(base + x*3):
                rows[y] += 1; cols[x] += 1
    ry = [y for y in range(h) if rows[y] > 60]
    rx = [x for x in range(w) if cols[x] > 60]
    if not ry or not rx: return None
    return ((min(rx)+max(rx))//2, (min(ry)+max(ry))//2, w, h)

_stop = threading.Event()
def reader():
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(SERIAL_SOCK); s.setblocking(False)
    except Exception as e:
        log(f"[reader] connect err {e}"); return
    f = open(CAP, "ab")
    while not _stop.is_set():
        if select.select([s], [], [], 0.3)[0]:
            try: c = s.recv(8192)
            except BlockingIOError: continue
            except Exception: break
            if not c: break
            f.write(c); f.flush()
            if b"\x1b[6n" in c:
                try: s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                except Exception: pass
    try: s.close()
    except Exception: pass
    f.close()

def main():
    os.makedirs(OUT, exist_ok=True)
    open(CAP, "wb").close()
    log(f"==== M4 EXIT ROBUST2 {ARCH} {MODE} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-120:])
    d("cmd", "brush /bin/m4run &", t=8)
    th = threading.Thread(target=reader, daemon=True); th.start()   # persistent serial capture
    log(f"...persistent reader attached; settling {SETTLE}s...")
    time.sleep(SETTLE)
    shot("B-client")
    log("---- CRIT2 cursor via QMP tablet ----")
    qmp("move","6000","6000"); time.sleep(4); shot("C-cursor1")
    qmp("move","26000","20000"); time.sleep(4); shot("D-cursor2")
    log("---- CRIT3 key via QMP (locate window in B, move over it + click for kbd focus) ----")
    win = find_window(f"{OUT}/m4e-r-{TAG}-B-client.ppm")
    if win:
        cx, cy, w, h = win
        tx, ty = int(cx*32767/w), int(cy*32767/h)
        log(f"[window] center screen=({cx},{cy}) -> tablet=({tx},{ty})")
    else:
        tx, ty = 16383, 16383; log("[window] NOT found, clicking screen center")
    qmp("move", str(tx), str(ty)); time.sleep(3)   # pointer over the composited client window
    qmp("click"); time.sleep(4)                     # click -> anvil sets keyboard focus to that surface
    shot("E0-focusclick")
    qmp("key","a"); time.sleep(4); qmp("key","b"); time.sleep(6)
    shot("E-key")
    _stop.set(); th.join(timeout=5)
    # summarize
    txt = open(CAP, errors='replace').read()
    hits = [l for l in txt.splitlines() if any(k in l for k in ("UXTR","wlclient","M4RUN","KEY code","focus","ENTER","EVK"))]
    log(f"--- serial evidence ({len(hits)} lines, cap={len(txt)}B) ---")
    for l in hits[-120:]: log("  "+l.strip()[:200])
    log("==== M4 EXIT ROBUST2 DONE ====")
if __name__ == "__main__": main()
