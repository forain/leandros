#!/usr/bin/env python3
# m9: read the panel's own render-gate telemetry off the serial TX.
#
# Needs the DIAGNOSTIC cosmic-panel build (apply_panel_diag.py) and kernel
# DBG_SERIAL_WRITE = true. Emits three [UCK] counters:
#   m9 applet_commit  — the applet's commits reaching the panel's embedded server
#   m9 layer_frame_cb — cosmic-comp's frame callbacks for the panel's layer surface
#   m9 render_gate    — space_event / is_dirty / has_frame at each render() entry
import subprocess, sys, os, time, threading, re, socket, json

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
QMP = "/tmp/leandros-qmp.sock"
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-panelgate")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "m9u"
DRAIN = int(sys.argv[3]) if len(sys.argv) > 3 else 180
SHOTS = [70, 165]
os.makedirs(OUT, exist_ok=True)


def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(2)


def main():
    log(f"==== m9 UCK panel telemetry {ARCH} tag={TAG} drain={DRAIN} {time.ctime()} ====")
    try:
        os.remove(SERIAL)
    except OSError:
        pass
    env = {"LEANDROS_QEMU_EXTRA": f"-qmp unix:{QMP},server,nowait"}
    booted = False; out = ""
    for attempt in range(1, 3):
        log(f"#### BOOT {attempt} ####"); clean()
        out = d("start", ARCH, "uefi", t=220, env=env)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True; break
    if not booted:
        log("NO BOOT"); log(out[-2000:]); clean(); return
    d("login", "root", "root", t=45)
    threading.Thread(target=lambda: d("session", str(DRAIN), "sh /bin/start-cosmic-leandros &",
                                      t=DRAIN + 40), daemon=True).start()
    log(f"[session launched; draining {DRAIN}s]")

    t0 = time.time()
    for when in SHOTS:
        dt = when - (time.time() - t0)
        if dt > 0:
            time.sleep(dt)
        d("screenshot", f"{OUT}/{TAG}-{ARCH}-t{when}.ppm", t=40)
        log(f"[t={when:3d}] shot")

    time.sleep(4)
    try:
        data = open(SERIAL, errors="replace").read()
        ct = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', data))
        open(f"{OUT}/{TAG}-{ARCH}-serial.txt", "w").write(ct)
        uck = re.findall(r"\[UCK\][^\n]*", ct)
        open(f"{OUT}/{TAG}-{ARCH}-uck.txt", "w").write("\n".join(uck))
        log(f"--- [UCK] lines total={len(uck)} ---")
        for k in ("applet_commit", "layer_frame_cb", "render_gate", "WaitConfigure",
                  "configure_panel_layer", "CONFIGURE received"):
            hits = [u for u in uck if k in u]
            log(f"  === {k}: {len(hits)} lines")
            for h in hits[:6]:
                log("     " + h.strip()[:150])
            if len(hits) > 12:
                log("     ...")
            for h in hits[-6:] if len(hits) > 6 else []:
                log("     " + h.strip()[:150])
        log("--- signals ---")
        for k in ("committed 220x32", "entering event loop", "Failed to render",
                  "panicked", "Out of memory", "EL0 Fault"):
            log(f"  '{k}' x{ct.count(k)}")
    except Exception as e:
        log(f"[serial err] {e}")
    clean()
    log("==== m9 UCK DONE ====")


if __name__ == "__main__":
    main()
