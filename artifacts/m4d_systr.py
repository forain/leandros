#!/usr/bin/env python3
# Capture the SYSTR spin histogram: boot HVF, launch anvil (/bin/gorun), then hold a
# persistent serial reader during the spin so kernel SYSTR/UXTR lines are captured
# (they are discarded when no serial client is connected). Dumps a syscall-number
# histogram at the end.
import subprocess, socket, select, time, os, sys, re
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL = "/tmp/leandros-serial.sock"
def d(*a, t=200):
    try: r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return "(TIMEOUT)"
def log(*a): print(*a, flush=True)

# clean + boot HVF (retry)
booted = False
for att in range(1, 5):
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
    os.environ["LEANDROS_QEMU_EXTRA"] = "-qmp unix:/tmp/leandros-qmp.sock,server,nowait"
    out = d("start","aarch64","uefi-hvf", t=150)
    if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
        booted = True; log(f"BOOTED attempt {att}"); break
    log(f"boot {att} failed")
if not booted: log("FATAL no boot"); sys.exit(1)
log(d("login","root","root", t=45)[-150:])
log("launch anvil /bin/gorun ...")
d("cmd","brush /bin/gorun &", t=8)
time.sleep(30)  # reach desktop + spin
# screenshot to confirm desktop
d("screenshot","/tmp/m4d-systr-anvil.ppm", t=30)
log("connecting persistent serial reader for 35s (SYSTR capture) ...")
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(40):
    try: s.connect(SERIAL); break
    except OSError: time.sleep(0.2)
s.setblocking(False)
buf = b""
end = time.time() + 35
while time.time() < end:
    if select.select([s],[],[],0.3)[0]:
        try: c = s.recv(65536)
        except BlockingIOError: continue
        if not c: break
        buf += c
        if b"\x1b[6n" in c:
            s.setblocking(True); s.sendall(b"\x1b[24;1R"*c.count(b"\x1b[6n")); s.setblocking(False)
s.close()
# raw SYSTR sample lines (num + pid + elr)
raw = re.findall(rb"SYSTR num=[0-9a-fx]+ pid=[0-9a-fx]+(?: elr=[0-9a-fx]+)?", buf)
log("\n==== raw SYSTR samples (last 25) ====")
for r in raw[-25:]:
    log("  " + r.decode())
# histogram SYSTR num=
nums = re.findall(rb"SYSTR num=([0-9a-fx]+) pid=([0-9a-fx]+)", buf)
elrs = re.findall(rb"SYSTR num=[0-9a-fx]+ pid=[0-9a-fx]+ elr=([0-9a-fx]+)", buf)
from collections import Counter as _C
log(f"ELR histogram: {_C(e.decode() for e in elrs).most_common(8)}")
from collections import Counter
hist = Counter()
pids = Counter()
for n,p in nums:
    hist[n.decode()] += 1; pids[p.decode()] += 1
log(f"\n==== SYSTR total samples={len(nums)} (window 35s) ====")
NAME = {"0x16":"epoll_pwait","0x49":"ppoll","0x1d":"ioctl","0x71":"clock_gettime",
        "0x7c":"sched_yield","0x62":"futex","0x56":"timerfd_settime","0xd4":"recvmsg",
        "0xd3":"sendmsg","0x40":"write","0x3f":"read","0x65":"nanosleep","0x48":"pselect6",
        "0x15":"epoll_ctl","0xca":"accept","0x0":"?"}
for n,cnt in hist.most_common(15):
    log(f"  num={n} ({NAME.get(n,'?')})  x{cnt}")
log(f"  pids: {dict(pids.most_common(6))}")
uxtr = re.findall(rb"UXTR ([A-Z]+) pid=([0-9a-fx]+)", buf)
log(f"  UXTR tags: {Counter(t.decode() for t,_ in uxtr)}")
# PCSAMP EL0/EL1 spin localizer
pc = re.findall(rb"PCSAMP (EL[01]) elr=([0-9a-fA-Fx]+)(?: pid=([0-9a-fA-Fx]+))?", buf)
elcnt = Counter(e.decode() for e,_,_ in pc)
log(f"\n==== PCSAMP total={len(pc)}  EL split={dict(elcnt)} ====")
def norm(h):
    v = int(h, 16); return f"0x{v:x}"
pchist = Counter(f"{e.decode()} {norm(a.decode())}" for e,a,_ in pc)
for k,c in pchist.most_common(20):
    log(f"  {k}  x{c}")
pidhist = Counter(norm(p.decode()) for _,_,p in pc if p)
log(f"  PID histogram: {pidhist.most_common(10)}")
picks = re.findall(rb"PICK pid=(\d+)", buf)
log(f"\n==== PICK total={len(picks)}  dispatched-pid histogram: {Counter(p.decode() for p in picks).most_common(12)} ====")
ylds = re.findall(rb"YLD (\S+) pid=(\d+)", buf)
log(f"==== YLD total={len(ylds)}  (reason,pid) histogram: {Counter((r.decode(),p.decode()) for r,p in ylds).most_common(15)} ====")
log("==== DONE ====")
