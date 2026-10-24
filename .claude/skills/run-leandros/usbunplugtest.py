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

Usage: LEANDROS_RUN_ID=<lane> python3 usbunplugtest.py [aarch64|x86_64] [unplug|notready|all]
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
    print("PASS usbunplug" if ok else "FAIL usbunplug")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
