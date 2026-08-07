#!/usr/bin/env python3
# M7b: trace busd directly under the kernel ring. Start busd via `m7repro armexec`
# (arms the trace for busd's tgid), drive a real zbus client (w1client) that
# triggers the per-peer socket_reader spawn, then `m7repro dump` the ring.
import subprocess, sys, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL_LOG = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
MODE = sys.argv[2] if len(sys.argv) > 2 else "uefi"
TAG = sys.argv[3] if len(sys.argv) > 3 else "busd"

BUSD = "/usr/libexec/busd"
CONF = "/usr/share/dbus-1/session.conf"
ADDR = "unix:path=/run/user/0/bus"

def d(*a, t=200):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"
def log(*a): print(*a, flush=True)
def clean():
    d("stop", t=30); subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)

def main():
    log(f"==== M7b busd-trace {ARCH} {MODE} {TAG} {time.ctime()} ====")
    booted = False
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, MODE, t=200)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("FATAL no boot"); clean(); return
    d("login", "root", "root", t=45)
    d("cmd", "mkdir -p /run/user/0; export XDG_RUNTIME_DIR=/run/user/0; rm -f /run/user/0/bus; echo SETUP", t=10)
    # start busd traced (armexec arms + execve busd; tgid preserved)
    log("[starting busd via m7repro armexec]")
    d("cmd", f"/bin/m7repro armexec {BUSD} --config {CONF} --address {ADDR} >/tmp/busd.log 2>&1 &", t=10)
    d("cmd", "sleep 3; echo BUSD_UP; tail -3 /tmp/busd.log", t=12)
    marker = f"M7B-BUSD-{TAG}"
    d("cmd", f"echo {marker}", t=6)
    # drive the real client — this triggers busd's per-peer socket_reader spawn
    log("[running w1client]")
    out = d("cmd", f"export DBUS_SESSION_BUS_ADDRESS={ADDR}; /bin/w1client", t=20)
    log("=== w1client output ===")
    log(out[-1500:])
    verdict = "PASS" if "SUCCESS" in out or "CONNECTED" in out else ("W1-REPRO" if "WATCHDOG" in out else "UNKNOWN")
    log(f"[verdict] {verdict}")
    # give busd a moment to settle into the stuck park, then dump the ring
    d("cmd", "sleep 1; echo PRE_DUMP", t=8)
    log("[dumping ring]")
    d("cmd", "/bin/m7repro dump", t=40)
    d("cmd", "echo POST_DUMP; tail -5 /tmp/busd.log", t=10)
    # capture serial window since our marker
    try:
        with open(SERIAL_LOG, "r", errors="replace") as f: data = f.read()
        idx = data.rfind(marker)
        window = data[idx:] if idx >= 0 else data[-60000:]
        dst = f"{OUT}/m7b-busd-{ARCH}-{TAG}.log"
        with open(dst, "w") as g: g.write(window)
        log(f"[serial window -> {dst} ({len(window)}B)]")
    except Exception as e:
        log(f"[serial err] {e}")
    clean(); log("==== busd-trace DONE ====")

if __name__ == "__main__": main()
