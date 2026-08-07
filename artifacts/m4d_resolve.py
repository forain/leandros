#!/usr/bin/env python3
# M4d slow-vs-stuck resolver. ONE backgrounded command.
#  Phase 1: try HVF boot up to N times; if it boots, run anvil + wlclient, measure
#           first-frame wall clock + whether anvil accepts/services the client (UXTR).
#  Phase 2: if HVF never boots, fall through to TCG long-window anvil poll.
# All output to stdout (captured by the caller to notes/m4d-resolve.log).
import subprocess, socket, time, select, sys, os, re

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL = "/tmp/leandros-serial.sock"
HVF_ATTEMPTS = 4

def log(*a):
    print(*a, flush=True)

def driver(*args, timeout=200):
    try:
        r = subprocess.run(["python3", DRIVER, *args], capture_output=True, text=True, timeout=timeout)
        return r.returncode, (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired as e:
        return 124, f"(driver.py {' '.join(args)} TIMEOUT after {timeout}s)"

def kill_qemu():
    driver("stop", timeout=30)
    subprocess.run(["pkill", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)

# ---------- persistent serial helpers ----------
class Serial:
    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        for _ in range(80):
            try:
                self.s.connect(SERIAL); break
            except OSError:
                time.sleep(0.2)
        else:
            raise RuntimeError("serial connect fail")
        self.s.setblocking(False)
        self.buf = b""
    def _dsr(self, chunk):
        if b"\x1b[6n" in chunk:
            self.s.setblocking(True)
            self.s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
            self.s.setblocking(False)
    def _drain(self, echo=True):
        self.s.setblocking(False)
        try:
            while select.select([self.s], [], [], 0.1)[0]:
                c = self.s.recv(4096)
                if not c: break
                self._dsr(c); self.buf += c
                if echo: sys.stdout.buffer.write(c); sys.stdout.flush()
        except Exception:
            pass
    def send(self, line, pad=True, echo=True):
        # Blessed robust delivery (mirrors driver.py _serial_send): drain, send a
        # bare CR, WAIT for the prompt '#' to redraw, only THEN write the command
        # in 8-byte chunks with a 2-space head-drop pad. Without the prompt sync
        # the first chunk is silently dropped and the command head is eaten.
        self._drain(echo)
        self.s.setblocking(True)
        try:
            self.s.sendall(b"\r")
        except Exception:
            pass
        self.s.setblocking(False)
        end = time.time() + 2.5
        sync = b""
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    c = self.s.recv(4096)
                except BlockingIOError:
                    continue
                if not c: break
                self._dsr(c); sync += c; self.buf += c
                if echo: sys.stdout.buffer.write(c); sys.stdout.flush()
                if b"#" in sync[-40:]:
                    break
        time.sleep(0.06)
        self.s.setblocking(True)
        p = (("  " if pad else "") + line + "\n").encode()
        for i in range(0, len(p), 8):
            self.s.sendall(p[i:i+8]); time.sleep(0.025)
        self.s.setblocking(False)
    def read_until(self, markers, timeout, echo=True):
        end = time.time() + timeout
        acc = b""
        while time.time() < end:
            if select.select([self.s], [], [], 0.2)[0]:
                try:
                    c = self.s.recv(4096)
                except BlockingIOError:
                    continue
                if not c:
                    break
                if echo:
                    sys.stdout.buffer.write(c); sys.stdout.flush()
                self._dsr(c); acc += c; self.buf += c
                if markers and any(m in acc for m in markers):
                    return acc
        return acc
    def close(self):
        try: self.s.close()
        except Exception: pass

def guest_login():
    # Use driver.py's proven-robust login (opens+closes its own serial), then
    # our persistent Serial() connects afterwards for the anvil phase.
    log("\n--- login (driver.py) ---")
    rc, out = driver("login", "root", "root", timeout=45)
    log(out[-300:])
    return ("# " in out) or ("$ " in out) or ("brush" in out)

def guest_cmd(ser, line, wait=8, markers=None):
    ser.send(line)  # send() self-syncs on the prompt
    return ser.read_until(markers or [b"# ", b"$ "], wait)

def run_anvil_experiment(ser, accel, anvil_wait, client_wait):
    log(f"\n===== ANVIL EXPERIMENT ({accel}) =====")
    guest_cmd(ser, "mkdir -p /run/user/0")
    guest_cmd(ser, "export ANVIL_DRM_DEVICE=/dev/dri/card0")
    guest_cmd(ser, "export SMITHAY_USE_LEGACY=1")
    guest_cmd(ser, "export XDG_RUNTIME_DIR=/run/user/0")
    guest_cmd(ser, "echo RTDIR=[$XDG_RUNTIME_DIR]")
    ser.read_until([b"# "], 4)
    log(f"\n--- launch anvil (t0) ---")
    t0 = time.time()
    ser.send("anvil --tty-udev >/tmp/anvil.log 2>&1 &")
    ser.read_until([b"# ", b"]"], 5)
    # poll anvil.log line count for forward progress past "Creating new Output"
    last = -1
    first_growth_past_output = None
    output_line = None
    deadline = time.time() + anvil_wait
    while time.time() < deadline:
        time.sleep(8)
        ser.read_until([b"# "], 3)
        ser.send("wc -l /tmp/anvil.log")
        out = ser.read_until([b"# "], 6)
        m = re.search(rb"(\d+)\s+/tmp/anvil\.log", out)
        n = int(m.group(1)) if m else -1
        el = time.time() - t0
        log(f"[poll {accel}] t+{el:5.1f}s  anvil.log lines={n}")
        # detect the Creating-new-Output line index once
        if output_line is None and n >= 1:
            ser.send("grep -n 'Creating new Output' /tmp/anvil.log")
            g = ser.read_until([b"# "], 6)
            gm = re.search(rb"(\d+):", g)
            if gm:
                output_line = int(gm.group(1))
                log(f"[poll {accel}] 'Creating new Output' at line {output_line}")
        if output_line and n > output_line and first_growth_past_output is None:
            first_growth_past_output = el
            log(f"[poll {accel}] *** FORWARD PROGRESS past 'Creating new Output' at t+{el:.1f}s (lines {n}>{output_line}) => SLOW-NOT-STUCK")
            break
        last = n
    # now run the client regardless — the decisive functional test
    log(f"\n--- launch wlclient ({accel}) ---")
    guest_cmd(ser, "export WAYLAND_DISPLAY=wayland-1")
    guest_cmd(ser, "echo ENV=[$XDG_RUNTIME_DIR][$WAYLAND_DISPLAY]")
    ser.read_until([b"# "], 4)
    tc = time.time()
    ser.send("wlclient >/tmp/wl.log 2>&1 &")
    log(f"--- watching UXTR for {client_wait}s (ACC/SND/RCV = anvil services client) ---")
    ser.read_until([b"ROUNDTRIP-DONE-MARKER-NEVER"], client_wait)  # just pump+echo, capturing UXTR
    client_serviced = b"UXTR ACC" in ser.buf
    log(f"\n[{accel}] anvil accepted client (UXTR ACC seen)= {client_serviced}")
    log("\n--- wl.log ---")
    guest_cmd(ser, "cat /tmp/wl.log", wait=12)
    log("\n--- anvil.log tail ---")
    ser.read_until([b"# "], 4)
    ser.send("tail -n 8 /tmp/anvil.log")
    ser.read_until([b"# "], 10)
    # screenshot (monitor socket, independent of serial)
    rc, so = driver("screenshot", f"/tmp/m4d-{accel}-anvil.ppm", timeout=30)
    log(f"\n[screenshot {accel}] {so.strip()}")
    return {"first_growth": first_growth_past_output, "client_serviced": client_serviced}

def try_hvf_boot():
    for attempt in range(1, HVF_ATTEMPTS+1):
        log(f"\n########## HVF BOOT attempt {attempt}/{HVF_ATTEMPTS} ##########")
        kill_qemu()
        rc, out = driver("start", "aarch64", "uefi-hvf", timeout=150)
        tail = out[-500:]
        log(tail)
        if ("Login prompt ready" in out) or ("login: " in out) or ("Shell ready" in out):
            log(f"########## HVF BOOTED (attempt {attempt}) ##########")
            return True
        log(f"HVF boot attempt {attempt} failed (no login prompt — likely virtio-input hang)")
    return False

def main():
    log(f"==== M4D RESOLVE start {time.ctime()} ====")
    # -------- Phase 1: HVF --------
    if try_hvf_boot():
        try:
            if not guest_login():
                log("HVF login failed; retrying login once")
                guest_login()
            ser = Serial()
            res = run_anvil_experiment(ser, "hvf", anvil_wait=150, client_wait=45)
            ser.close()
            log(f"\n==== HVF RESULT: {res} ====")
            if res["client_serviced"] or res["first_growth"] is not None:
                log("==== VERDICT: SLOW-NOT-STUCK (HVF renders/services). HVF is a viable exit vehicle. ====")
                log("==== DONE (HVF path succeeded) ====")
                return
            else:
                log("==== HVF booted but anvil did NOT service client / no progress — genuinely stuck? Falling to TCG for cross-check. ====")
        except Exception as e:
            log(f"HVF experiment error: {e!r} — falling through to TCG")
    else:
        log(f"\n==== HVF never booted in {HVF_ATTEMPTS} attempts — falling through to TCG long-window ====")
    # -------- Phase 2: TCG long window --------
    kill_qemu()
    rc, out = driver("start", "aarch64", "uefi-tcg", timeout=200)
    log(out[-500:])
    if not (("Login prompt ready" in out) or ("login: " in out) or ("Shell ready" in out)):
        log("==== FATAL: TCG boot also failed. ====")
        return
    guest_login()
    ser = Serial()
    res = run_anvil_experiment(ser, "tcg", anvil_wait=2100, client_wait=60)  # ~35 min anvil window
    ser.close()
    log(f"\n==== TCG RESULT: {res} ====")
    if res["first_growth"] is not None or res["client_serviced"]:
        log("==== VERDICT: SLOW-NOT-STUCK under TCG (forward progress observed). ====")
    else:
        log("==== VERDICT: no forward progress in TCG window — candidate STUCK (verify CPU%). ====")
    log("==== DONE ====")

if __name__ == "__main__":
    main()
