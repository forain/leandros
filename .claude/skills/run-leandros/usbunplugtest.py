#!/usr/bin/env python3
"""USB mass-storage unplug / re-plug, and background readiness (L8.2).

Phase 1 (unplug): two 32 MiB sticks with distinct random contents on a
second qemu-xhci (root stays on virtio). A loop of reads runs on stick A;
QMP `device_del` removes A while a read is in flight. Expected:
  - the reader fails (non-zero exit) within UNPLUG_EIO_S seconds,
  - A's /dev node disappears, the shell keeps answering (echo round trip),
  - stick B still reads correctly (the xHCI and the other port are fine),
  - after `blockdev-add` + `device_add` of the same image, a /dev/sd* node
    with A's contents comes back within REPLUG_S seconds.
Phase 2 (not ready, `--notready` or default both): boot with the fault knob
`usbms.notready_ms=12000` (every LUN reports NOT READY / becoming ready for
12 s). The boot must not wait for it; the disk must appear later (logged
"became ready ... in the background") and read correctly.

Usage: LEANDROS_RUN_ID=<lane> python3 usbunplugtest.py [aarch64|x86_64] [unplug|notready|fastreplug|hub|hid|all]
"""
import hashlib, json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
UNPLUG_EIO_S = 6.0
REPLUG_S = 20.0
MAX_RTT = 3.0


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def sh(cmd, t=60):
    try:
        return drv("run", cmd, str(t), timeout=t + 60)[1]
    except subprocess.TimeoutExpired:
        return ""


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


def boot(arch, env):
    drv("stop", timeout=60)
    rc, out = drv("start", arch, timeout=300, env=env)
    if rc != 0:
        print("FAIL: boot\n" + out[-800:]); return False
    rc, out = drv("login", "root", "root", "240", timeout=400, env=env)
    if rc != 0:
        print("FAIL: login\n" + out[-800:]); return False
    return True


def node_hashes():
    """{node: sha256 of its first 4 MiB} for every /dev/sd? disk."""
    out = sh("for d in /dev/sd?; do [ -e $d ] && echo \"N $d $(dd if=$d bs=256k count=16 iflag=fullblock 2>/dev/null | sha256sum)\"; done", 90)
    return {m.group(1): m.group(2) for m in re.finditer(r"N (/dev/sd\w) ([0-9a-f]{64})", out)}


def make_img(path):
    data = os.urandom(32 * MIB)
    with open(path, "wb") as f: f.write(data)
    return hashlib.sha256(data[:4 * MIB]).hexdigest()


def rtt_ok():
    t = time.time()
    out = sh("echo PONG", 30)
    dt = time.time() - t
    return "PONG" in out and dt <= MAX_RTT, dt


def phase_unplug(arch, run_id, env):
    ok = True
    ia, ib = (os.path.join(REPO, f"usbunplug-{x}-{run_id}.img") for x in "ab")
    ha, hb = make_img(ia), make_img(ib)
    env = dict(env)
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device qemu-xhci,id=xhs "
        f"-drive if=none,id=sta,format=raw,file={ia} -device usb-storage,bus=xhs.0,drive=sta,id=usba "
        f"-drive if=none,id=stb,format=raw,file={ib} -device usb-storage,bus=xhs.0,drive=stb,id=usbb")
    try:
        if not boot(arch, env): return False
        h = node_hashes()
        na = next((n for n, x in h.items() if x == ha), None)
        nb = next((n for n, x in h.items() if x == hb), None)
        print(f"nodes before unplug: {sorted(h)} A={na} B={nb}")
        if not na or not nb:
            print("FAIL: sticks not both readable at boot"); return False
        sh(f"sh -c 'rm -f /tmp/src; while :; do dd if={na} of=/dev/null bs=64k 2>/tmp/sd.err || "
           f"{{ echo SRC=$? > /tmp/src; break; }}; done' </dev/null >/dev/null 2>&1 & true", 20)
        time.sleep(2)
        t_del = time.time()
        rc, out = drv("qmp", "device_del", json.dumps({"id": "usba"}), timeout=60)
        print(f"device_del rc={rc} {out.strip()[-120:]}")
        src = None
        while time.time() - t_del < 60:
            m = re.search(r"SRC=(\d+)", sh("cat /tmp/src 2>/dev/null", 20))
            if m: src = int(m.group(1)); break
            time.sleep(0.5)
        dt = time.time() - t_del
        print(f"in-flight reader ended: rc={src} after {dt:.1f} s")
        if src is None or src == 0 or dt > UNPLUG_EIO_S:
            ok = False; print(f"FAIL: reader did not fail with an error within {UNPLUG_EIO_S} s")
        good, rtt = rtt_ok()
        print(f"shell round trip after unplug: {rtt:.1f} s")
        if not good: ok = False; print("FAIL: shell unresponsive after unplug")
        time.sleep(1)
        h = node_hashes()
        print(f"nodes after unplug: {sorted(h)}")
        if na in h or ha in h.values():
            ok = False; print(f"FAIL: {na} still present after unplug")
        if hb not in h.values():
            ok = False; print("FAIL: stick B no longer reads correctly")
        print("err: " + sh("cat /tmp/sd.err | tail -n 2", 20).strip()[-160:])
        rc1, o1 = drv("qmp", "blockdev-add", json.dumps({"driver": "raw", "node-name": "sta2",
                     "file": {"driver": "file", "filename": ia}}), timeout=60)
        rc2, o2 = drv("qmp", "device_add", json.dumps({"driver": "usb-storage", "bus": "xhs.0",
                     "drive": "sta2", "id": "usba2"}), timeout=60)
        print(f"blockdev-add rc={rc1} device_add rc={rc2} {o2.strip()[-120:]}")
        t_add, back = time.time(), None
        while time.time() - t_add < REPLUG_S:
            h = node_hashes()
            back = next((n for n, x in h.items() if x == ha), None)
            if back: break
            time.sleep(1)
        print(f"after re-plug: A={back} in {time.time() - t_add:.1f} s, nodes {sorted(h)}")
        if not back:
            ok = False; print("FAIL: re-plugged stick did not come back readable")
        if hb not in h.values():
            ok = False; print("FAIL: stick B broken after re-plug")
        for l in [l for l in serial_log(run_id).splitlines() if "[USB-MSC]" in l or "[USB]" in l][-8:]:
            print(l.strip()[-160:])
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (ia, ib):
            try: os.unlink(f)
            except OSError: pass
    return ok


def phase_notready(arch, run_id, env):
    ok = True
    ia = os.path.join(REPO, f"usbunplug-nr-{run_id}.img")
    ha = make_img(ia)
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    bimg = os.path.join(REPO, f"usbunplug-boot-{run_id}.img")
    if os.path.exists(bimg): os.unlink(bimg)
    if subprocess.run(["cp", "-c", src, bimg]).returncode != 0:
        import shutil; shutil.copyfile(src, bimg)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), bimg, "usbms.notready_ms=12000"],
                   check=True, stdout=subprocess.DEVNULL)
    env = dict(env)
    env["LEANDROS_DISK_IMAGE"] = bimg
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device qemu-xhci,id=xhs "
        f"-drive if=none,id=sta,format=raw,file={ia} -device usb-storage,bus=xhs.0,drive=sta,id=usba")
    try:
        if not boot(arch, env): return False
        back, t0 = None, time.time()
        while time.time() - t0 < 60:
            back = next((n for n, x in node_hashes().items() if x == ha), None)
            if back: break
            time.sleep(2)
        log = serial_log(run_id)
        bg = [l for l in log.splitlines() if "in the background" in l]
        for l in bg[-2:]: print(l.strip()[-160:])
        print(f"not-ready stick readable: {back}")
        if not back:
            ok = False; print("FAIL: not-ready stick never became readable")
        if not any("became ready" in l for l in bg):
            ok = False; print("FAIL: no background readiness line (boot waited, or the knob is missing)")
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (ia, bimg):
            try: os.unlink(f)
            except OSError: pass
    return ok


def qmp_multi(cmds, run_id):
    """Send several QMP commands over ONE connection, back-to-back, then read
    the replies. Returns (replies, [send timestamps])."""
    import socket
    path = open(f"/tmp/leandros-{run_id}-qmp-sockpath").read().strip()
    sk = socket.socket(socket.AF_UNIX); sk.settimeout(15); sk.connect(path)
    buf = b""
    def rd():
        nonlocal buf
        while True:
            if b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if line.strip(): return json.loads(line)
                continue
            d = sk.recv(65536)
            if not d: raise OSError("qmp closed")
            buf += d
    rd()
    sk.sendall(b'{"execute":"qmp_capabilities"}\n')
    while "return" not in (m := rd()) and "error" not in m: pass
    ts = []
    for c, a in cmds:
        sk.sendall((json.dumps({"execute": c, "arguments": a}) + "\n").encode())
        ts.append(time.time())
    reps = []
    while len(reps) < len(cmds):
        m = rd()
        if "return" in m or "error" in m: reps.append(m)
    sk.close()
    return reps, ts


def phase_fastreplug(arch, run_id, env):
    ok = True
    ia, ib, ic = (os.path.join(REPO, f"usbunplug-fr{x}-{run_id}.img") for x in "abc")
    ha, hb, hc = make_img(ia), make_img(ib), make_img(ic)
    env = dict(env)
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device qemu-xhci,id=xhs "
        f"-drive if=none,id=sta,format=raw,file={ia} -device usb-storage,bus=xhs.0,port=1,drive=sta,id=usba "
        f"-drive if=none,id=stb,format=raw,file={ib} -device usb-storage,bus=xhs.0,port=2,drive=stb,id=usbb")
    try:
        if not boot(arch, env): return False
        h = node_hashes()
        na = next((n for n, x in h.items() if x == ha), None)
        print(f"nodes before swap: {sorted(h)} A={na}")
        if not na or hb not in h.values():
            print("FAIL: sticks not both readable at boot"); return False
        rc, o = drv("qmp", "blockdev-add", json.dumps({"driver": "raw", "node-name": "stc",
                    "file": {"driver": "file", "filename": ic}}), timeout=60)
        print(f"blockdev-add stc rc={rc}")
        reps, ts = qmp_multi([("device_del", {"id": "usba"}),
                              ("device_add", {"driver": "usb-storage", "bus": "xhs.0", "port": "1",
                                              "drive": "stc", "id": "usbc"})], run_id)
        print(f"fast swap gap between device_del and device_add: {(ts[1] - ts[0]) * 1000:.1f} ms; replies {reps}")
        if any("error" in r for r in reps):
            print("FAIL: QEMU rejected the swap commands (test bug)"); return False
        t0, back, h = time.time(), None, {}
        while time.time() - t0 < 20:
            time.sleep(2)
            h = node_hashes()
            back = next((n for n, x in h.items() if x == hc), None)
            if back and ha not in h.values(): break
        print(f"after swap: C={back} A_present={ha in h.values()} B_present={hb in h.values()} nodes {sorted(h)} ({time.time() - t0:.1f} s)")
        if ha in h.values(): ok = False; print("FAIL: stale stick A still readable after swap")
        if not back: ok = False; print("FAIL: stick C never appeared (swap missed)")
        if hb not in h.values(): ok = False; print("FAIL: stick B broken")
        for l in [l for l in serial_log(run_id).splitlines() if "[USB" in l][-8:]:
            print(l.strip()[-160:])
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (ia, ib, ic):
            try: os.unlink(f)
            except OSError: pass
    return ok


def phase_hub(arch, run_id, env):
    ok = True
    ia, ib = (os.path.join(REPO, f"usbunplug-hub{x}-{run_id}.img") for x in "ab")
    ha, hb = make_img(ia), make_img(ib)
    env = dict(env)
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device qemu-xhci,id=xhs "
        "-device usb-hub,bus=xhs.0,port=1,id=hub1 "
        f"-drive if=none,id=sta,format=raw,file={ia} -device usb-storage,bus=xhs.0,port=1.1,drive=sta,id=usba "
        f"-drive if=none,id=stb,format=raw,file={ib} -device usb-storage,bus=xhs.0,port=2,drive=stb,id=usbb")
    try:
        if not boot(arch, env): return False
        h = node_hashes()
        na = next((n for n, x in h.items() if x == ha), None)
        print(f"nodes at boot: {sorted(h)} A(behind hub)={na} B_present={hb in h.values()}")
        if not na:
            print("FAIL: stick behind the hub not readable at boot")
            for l in [l for l in serial_log(run_id).splitlines() if "hub" in l.lower()][-4:]:
                print(l.strip()[-160:])
            return False
        rc, o = drv("qmp", "device_del", json.dumps({"id": "usba"}), timeout=60)
        print(f"device_del rc={rc}")
        t0, gone = time.time(), False
        while time.time() - t0 < 6:
            time.sleep(1)
            h = node_hashes()
            if ha not in h.values(): gone = True; break
        print(f"A gone={gone} after {time.time() - t0:.1f} s")
        if not gone: ok = False; print("FAIL: node behind hub still present after unplug")
        good, rtt = rtt_ok()
        print(f"shell round trip: {rtt:.1f} s")
        if not good: ok = False; print("FAIL: shell unresponsive")
        if hb not in node_hashes().values(): ok = False; print("FAIL: stick B broken")
        rc1, _ = drv("qmp", "blockdev-add", json.dumps({"driver": "raw", "node-name": "sta2",
                     "file": {"driver": "file", "filename": ia}}), timeout=60)
        rc2, o2 = drv("qmp", "device_add", json.dumps({"driver": "usb-storage", "bus": "xhs.0", "port": "1.1",
                     "drive": "sta2", "id": "usba2"}), timeout=60)
        print(f"blockdev-add rc={rc1} device_add rc={rc2} {o2.strip()[-100:]}")
        t_add, back = time.time(), None
        while time.time() - t_add < REPLUG_S:
            h = node_hashes()
            back = next((n for n, x in h.items() if x == ha), None)
            if back: break
            time.sleep(1)
        print(f"after re-plug behind hub: A={back} in {time.time() - t_add:.1f} s")
        if not back: ok = False; print("FAIL: re-plugged stick behind hub did not come back")
        for l in [l for l in serial_log(run_id).splitlines() if "[USB" in l][-6:]:
            print(l.strip()[-160:])
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        for f in (ia, ib):
            try: os.unlink(f)
            except OSError: pass
    return ok


def key_events(run_id, dev, node="all", secs=1.5, inject=True):
    """Inject key 'a' down/up (no "device": QEMU routes to the newest keyboard handler; `dev` is informational) while a cat in the guest
    records `node` ("all": every /dev/input/eventN -- a USB keyboard with its own node
    reaches userspace only there, event0 then feeds just the kernel console); returns the
    number of bytes captured (24 per input_event)."""
    nodes = "/dev/input/event*" if node == "all" else node
    sh(f"rm -f /tmp/ev.out /tmp/ev.pid; touch /tmp/ev.out; for n in {nodes}; do sh -c \"cat $n >> /tmp/ev.out 2>/dev/null & echo \\$! >> /tmp/ev.pid\" </dev/null >/dev/null 2>&1; done; true", 20)
    time.sleep(0.5)
    ev = lambda d: {"type": "key", "data": {"down": d, "key": {"type": "qcode", "data": "a"}}}
    reps, _ = qmp_multi([("input-send-event", {"events": [ev(True)]}),
                         ("input-send-event", {"events": [ev(False)]})] if inject else [("query-status", {})], run_id)
    if any("error" in r for r in reps): print(f"  input-send-event {dev} error: {reps}"); return -1
    time.sleep(secs)
    out = sh("kill $(cat /tmp/ev.pid) 2>/dev/null; sleep 0.2; wc -c < /tmp/ev.out", 20)
    m = re.findall(r"^\s*(\d+)\s*$", out, re.M)
    return int(m[-1]) if m else -1


def mouse_events(run_id, dev, node="/dev/input/event1", secs=1.5):
    sh(f"rm -f /tmp/ms.out; sh -c 'cat {node} > /tmp/ms.out 2>/dev/null & echo $! > /tmp/ms.pid' </dev/null >/dev/null 2>&1; true", 20)
    time.sleep(0.5)
    reps, _ = qmp_multi([("input-send-event", {"events": [
        {"type": "rel", "data": {"axis": "x", "value": 20}},
        {"type": "rel", "data": {"axis": "y", "value": 10}}]})], run_id)
    if any("error" in r for r in reps): print(f"  mouse input-send-event error: {reps}"); return -1
    time.sleep(secs)
    out = sh("kill $(cat /tmp/ms.pid) 2>/dev/null; wc -c < /tmp/ms.out", 20)
    m = re.findall(r"^\s*(\d+)\s*$", out, re.M)
    return int(m[-1]) if m else -1


def hid_log(run_id, words):
    return [l.strip()[-160:] for l in serial_log(run_id).splitlines()
            if "[USB-HID]" in l and any(w in l.lower() for w in words)]


def phase_hid(arch, run_id, env):
    ok = True
    env = dict(env)
    env["LEANDROS_QEMU_EXTRA"] = ("-device qemu-xhci,id=xhs -device usb-kbd,bus=xhs.0,id=kbd1 "
                                  "-device usb-kbd,bus=xhs.0,id=kbd2 -device usb-mouse,bus=xhs.0,id=mouse1")
    try:
        if not boot(arch, env): return False
        print("input nodes at boot: " + " ".join(sh("ls /dev/input", 20).split()))
        ctl = key_events(run_id, "none", inject=False)
        n1, n2 = key_events(run_id, "kbd1"), key_events(run_id, "kbd2")
        print(f"(a) control (no injection) bytes: {ctl}")
        # a cat of event0 reads a fixed initial burst on open (ctl); injected keys add 24 B/event on top
        if n1 <= ctl or n2 <= ctl:
            print("FAIL: baseline keys not delivered (step a)"); return False
        print(f"(a) baseline key bytes on event0: kbd1={n1} kbd2={n2}")
        rc, o = drv("qmp", "device_del", json.dumps({"id": "kbd1"}), timeout=60)
        print(f"device_del kbd1 rc={rc}")
        t0, lines = time.time(), []
        while time.time() - t0 < 3:
            time.sleep(0.5)
            lines = hid_log(run_id, ("removed", "disconnected"))
            if lines: break
        print(f"(b) removal log lines: {lines[-2:]}")
        if not lines: ok = False; print("FAIL: no [USB-HID] removed/disconnected log line within 3 s")
        print("input nodes after removal: " + " ".join(sh("ls /dev/input", 20).split()))
        k2 = key_events(run_id, "kbd2")
        print(f"(b) kbd2 after kbd1 removal: {k2}")
        if k2 <= ctl: ok = False; print("FAIL: kbd2 keys lost after kbd1 removal")
        print(f"(b) mouse1 rel motion bytes on event1 (informational): {mouse_events(run_id, 'mouse1')}")
        def kbd_lines():
            return [l.strip()[-120:] for l in serial_log(run_id).splitlines()
                    if "Keyboard" in l and ("[USB]" in l or "[USB-HID]" in l) and "removed" not in l]
        before = len(kbd_lines())
        rc, o = drv("qmp", "device_add", json.dumps({"driver": "usb-kbd", "bus": "xhs.0", "id": "kbd1"}), timeout=60)
        print(f"device_add kbd1 rc={rc} {o.strip()[-100:]}")
        t0, new = time.time(), []
        while time.time() - t0 < 10:
            time.sleep(1)
            new = kbd_lines()[before:]
            if new: break
        print(f"(c) enumeration lines after re-add ({time.time() - t0:.1f} s): {new[-2:]}")
        if not new:
            ok = False; print("FAIL: re-added kbd1 never enumerated (no new keyboard [USB]/[USB-HID] line within 10 s)")
        n1 = key_events(run_id, "kbd1")
        print(f"(c) kbd1 keys after re-add: {n1}")
        if n1 <= ctl: ok = False; print("FAIL: re-added kbd1 keys not delivered")
        # (e) per-device node appears for a new keyboard
        s0 = set(sh("ls /dev/input", 20).split())
        rc, o = drv("qmp", "device_add", json.dumps({"driver": "usb-kbd", "bus": "xhs.0", "id": "kbd3"}), timeout=60)
        print(f"device_add kbd3 rc={rc}")
        t0, node = time.time(), None
        while time.time() - t0 < 10 and not node:
            time.sleep(1)
            cur = [x for x in sh("ls /dev/input", 20).split() if re.fullmatch(r"event\d+", x) and x not in s0]
            if cur: node = "/dev/input/" + sorted(cur)[0]
        if not node:
            ok = False; print("FAIL: (e) no new eventN appeared for kbd3 within 10 s"); return ok
        info = sh(f"evtest2 info {node}", 20)
        print(f"(e) {info.strip()[-160:]}")
        if "USB Keyboard" in info and re.search(r"bus=3\b", info): print(f"PASS: (e) per-device node {node} name/bus")
        else: ok = False; print(f"FAIL: (e) node {node} info lacks 'USB Keyboard'/bus=3")
        # (f) keys on the new node; userspace event0 no longer gets them
        # (the libudev stand-in hotplugs the node into libinput)
        # QEMU routes injected keys to the newest *active* keyboard handler; a
        # usb-kbd becomes active once the guest polls it, so let it settle.
        time.sleep(1.5)
        kn = key_events(run_id, "kbd3", node=node)
        k0 = key_events(run_id, "kbd3", node="/dev/input/event0")
        print(f"(f) kbd3 key bytes: {node}={kn} event0={k0} (ctl={ctl})")
        if kn > 0: print("PASS: (f) keys on per-device node")
        else: ok = False; print("FAIL: (f) no key bytes on per-device node")
        if k0 == 0: print("PASS: (f) keys not mirrored to userspace event0")
        else: ok = False; print("FAIL: (f) keys still mirrored on event0")
        # (g) unplug: reader sees ENODEV + POLLHUP, node vanishes
        sh(f"rm -f /tmp/gone.out; sh -c 'evtest2 gone {node} 15 > /tmp/gone.out 2>&1 & ' </dev/null >/dev/null 2>&1; true", 20)
        time.sleep(1)
        rc, o = drv("qmp", "device_del", json.dumps({"id": "kbd3"}), timeout=60)
        print(f"device_del kbd3 rc={rc}")
        gone = ""
        t0 = time.time()
        while time.time() - t0 < 12:
            time.sleep(1)
            gone = sh("cat /tmp/gone.out", 20)
            if "GONE-READ" in gone or "GONE-TIMEOUT" in gone: break
        print(f"(g) {' | '.join(gone.split(chr(10))[-4:])[-200:]}")
        m = re.search(r"GONE-READ errno=19 revents=0x([0-9a-f]+)", gone)
        if m and int(m.group(1), 16) & 0x10: print("PASS: (g) reader got ENODEV and POLLHUP")
        else: ok = False; print("FAIL: (g) expected GONE-READ errno=19 with POLLHUP")
        ls = sh("ls /dev/input", 20).split()
        if os.path.basename(node) not in ls: print("PASS: (g) node absent from /dev/input")
        else: ok = False; print("FAIL: (g) node still listed after unplug")
        info2 = sh(f"evtest2 info {node}", 20)
        if "err=2" in info2: print("PASS: (g) open of removed node ENOENT")
        else: ok = False; print(f"FAIL: (g) info after removal: {info2.strip()[-100:]}")
        # (h) remaining keyboard still delivers
        k0 = key_events(run_id, "kbd2")
        print(f"(h) key bytes after kbd3 removal: {k0} (ctl={ctl})")
        if k0 > ctl: print("PASS: (h) remaining keyboard delivers")
        else: ok = False; print("FAIL: (h) keys lost after kbd3 removal")
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
    return ok


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
    which = sys.argv[2] if len(sys.argv) > 2 else "all"
    run_id = os.environ.get("LEANDROS_RUN_ID", "x")
    env = dict(os.environ)
    ok = True
    if which in ("unplug", "all"):
        r = phase_unplug(arch, run_id, env); ok &= r
        print("unplug phase: " + ("PASS" if r else "FAIL"))
    if which in ("notready", "all"):
        r = phase_notready(arch, run_id, env); ok &= r
        print("notready phase: " + ("PASS" if r else "FAIL"))
    for name, fn in (("fastreplug", phase_fastreplug), ("hub", phase_hub), ("hid", phase_hid)):
        if which in (name, "all"):
            r = fn(arch, run_id, env); ok &= r
            print(f"{name} phase: " + ("PASS" if r else "FAIL"))
    print("PASS usbunplug" if ok else "FAIL usbunplug")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
