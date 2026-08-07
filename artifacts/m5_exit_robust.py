#!/usr/bin/env python3
# M5 exit: cosmic-comp on KMS + wl_shm client composite + busd session bus.
# Pattern (proven in m4e): driver.py owns boot+login+screenshots (monitor sock),
# THIS script owns the serial socket for the compositor phase (persistent reader
# thread logging to CAP + answering ESC[6n; command sender on the same fd).
# Screenshots via driver.py screendump (monitor sock) — no serial conflict.
import subprocess, sys, os, time, socket, select, threading
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m5-screenshots")
SERIAL_SOCK = "/tmp/leandros-serial.sock"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else ("uefi-hvf" if ARCH == "aarch64" else "uefi")
COMP_SETTLE = int(sys.argv[3]) if len(sys.argv) > 3 else (150 if ARCH == "aarch64" else 230)
CLIENT_SETTLE = int(sys.argv[4]) if len(sys.argv) > 4 else 55
TAG = ARCH
CAP = f"{OUT}/m5-{TAG}-serial.log"

def log(*a): print(*a, flush=True)

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {' '.join(a)})"

def shot(name):
    d("screenshot", f"{OUT}/m5-{TAG}-{name}.ppm", t=30)
    log(f"[shot] {name}")

def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)

def boot():
    for attempt in range(1, 8):
        log(f"#### BOOT {attempt} ({ARCH} {MODE}) ####")
        clean()
        out = d("start", ARCH, MODE, t=175)
        if any(m in out for m in ("Login prompt ready", "login: ", "Shell ready", "> ")):
            log(f"#### BOOTED ({attempt}) ####")
            return True
        log(f"(boot {attempt} tail) " + out[-300:].replace("\n", " "))
    return False

_stop = threading.Event()
_sock = None
def reader():
    global _sock
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(SERIAL_SOCK)
        s.setblocking(False)
    except Exception as e:
        log(f"[reader] connect err {e}")
        return
    _sock = s
    f = open(CAP, "ab")
    while not _stop.is_set():
        if select.select([s], [], [], 0.3)[0]:
            try:
                c = s.recv(8192)
            except BlockingIOError:
                continue
            except Exception:
                break
            if not c:
                break
            f.write(c); f.flush()
            if b"\x1b[6n" in c:
                try: s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                except Exception: pass
    f.close()

def send(line):
    global _sock
    for _ in range(50):
        if _sock is not None: break
        time.sleep(0.1)
    if _sock is None:
        log(f"[send] no socket for: {line}"); return
    try:
        _sock.sendall((line + "\n").encode())
        log(f"[send] {line}")
    except Exception as e:
        log(f"[send] err {e}")
    time.sleep(0.6)

def settle(total, label):
    log(f"...settling {total}s ({label})...")
    step = 30
    elapsed = 0
    while elapsed < total:
        time.sleep(min(step, total - elapsed))
        elapsed += step
        shot(f"{label}-t{elapsed}")

def main():
    os.makedirs(OUT, exist_ok=True)
    open(CAP, "wb").close()
    log(f"==== M5 EXIT {ARCH} {MODE} comp={COMP_SETTLE}s client={CLIENT_SETTLE}s {time.ctime()} ====")
    if not boot():
        log("FATAL no boot"); return
    log(d("login", "root", "root", t=45)[-160:])
    th = threading.Thread(target=reader, daemon=True); th.start()
    time.sleep(1)
    # Phase 1: launch cosmic-comp under the D-Bus session bus.
    send("brush /bin/comprun &")
    settle(COMP_SETTLE, "comp")
    shot("A-comp-final")
    # Phase 2: wl_shm client composite.
    send("brush /bin/clientrun &")
    settle(CLIENT_SETTLE, "client")
    shot("B-composite")
    # Phase 3: evidence dump to serial.
    send("brush /bin/evrun")
    time.sleep(10)
    shot("C-after-ev")
    _stop.set(); th.join(timeout=5)
    txt = open(CAP, errors="replace").read()
    keys = ("COMPRUN", "CLIENTRUN", "COSMIC-EVIDENCE", "WL-CLIENT", "Cosmic starting",
            "Listening", "NameAcquired", "CosmicComp", "EGL", "software", "card0",
            "wayland socket", "New client", "Failed", "panic", "gpu", "udev", "wlclient")
    hits = [l for l in txt.splitlines() if any(k in l for k in keys)]
    log(f"--- serial evidence ({len(hits)} lines, cap={len(txt)}B) ---")
    for l in hits[-90:]:
        log("  " + l.strip()[:180])
    log("==== M5 EXIT DONE ====")

if __name__ == "__main__":
    main()
