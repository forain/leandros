#!/usr/bin/env python3
# Foreground cosmic-comp capture: boot, own serial, run compfg (cosmic-comp
# straight to serial, RUST_BACKTRACE=full), log everything, screenshot.
import subprocess, sys, os, time, socket, select, threading
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_SOCK = "/tmp/leandros-serial.sock"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
DUR = int(sys.argv[3]) if len(sys.argv) > 3 else 75
LAUNCHER = sys.argv[4] if len(sys.argv) > 4 else "compfg"
CAP = f"{OUT}/m5b-{LAUNCHER}-{ARCH}-serial.log"
def log(*a): print(*a, flush=True)
def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
def boot():
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####"); clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready","login: ","Shell ready")):
            log(f"#### BOOTED ({attempt}) ####"); return True
    return False
_stop = threading.Event(); _sock = None
def reader():
    global _sock
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(SERIAL_SOCK); s.setblocking(False)
    except Exception as e:
        log(f"[reader] connect err {e}"); return
    _sock = s; f = open(CAP, "ab")
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
    f.close()
def send(line):
    for _ in range(50):
        if _sock is not None: break
        time.sleep(0.1)
    if _sock is None: log(f"[send] no socket: {line}"); return
    _sock.sendall((line + "\n").encode()); log(f"[send] {line}"); time.sleep(0.6)
def main():
    os.makedirs(OUT, exist_ok=True); open(CAP, "wb").close()
    log(f"==== M5 FG {ARCH} {MODE} dur={DUR} {time.ctime()} ====")
    if not boot(): log("FATAL no boot"); return
    log(d("login","root","root", t=45)[-160:])
    th = threading.Thread(target=reader, daemon=True); th.start(); time.sleep(1)
    send(f"brush /bin/{LAUNCHER}")
    slept = 0
    while slept < DUR:
        time.sleep(15); slept += 15
        d("screenshot", f"{OUT}/m5-fg-{ARCH}-t{slept}.ppm", t=30); log(f"[shot] t{slept}")
    _stop.set(); th.join(timeout=5)
    txt = open(CAP, errors="replace").read()
    log(f"--- serial cap {len(txt)}B ---")
    for l in txt.splitlines():
        s = l.strip()
        if s and "Task::new_kernel" not in s and "clean allocation" not in s:
            log("  " + s[:180])
    log("==== M5 FG DONE ====")
    clean()
if __name__ == "__main__": main()
