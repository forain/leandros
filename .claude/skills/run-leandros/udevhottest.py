#!/usr/bin/env python3
"""udevhottest.py -- does a USB keyboard hot-added to a running COSMIC session (virgl)
reach libinput through its own /dev/input/eventN, and does unplugging it leave the
session healthy? Logs in via the greeter, opens a terminal, then over QMP:
  (a) device_add usb-kbd -> cosmic-comp opens the new /dev/input/eventN (libudev
      stand-in enumeration + hotplug monitor; red while the monitor never fired),
  (b) keys sent (QEMU routes them to the newest keyboard, the usb-kbd) reach the
      terminal: `echo UDEVHOT1 > /tmp/uh1`,
  (c) device_del -> cosmic-comp drops the fd, stays alive, and keys from the
      remaining keyboard still reach the terminal (`/tmp/uh2`).
    CS_REPO=<repo> CS_OUT=<dir> LEANDROS_RUN_ID=x LEANDROS_VNC_PORT=59xx python3 udevhottest.py
"""
import json, os, re, sys, time
R = os.environ["CS_REPO"]
sys.path.insert(0, R + "/.claude/skills/run-leandros")
import importlib.util
spec = importlib.util.spec_from_file_location("cb", R + "/.claude/skills/run-leandros/coldstart-bench.py")
cb = importlib.util.module_from_spec(spec); spec.loader.exec_module(cb)
os.environ["LEANDROS_QEMU_EXTRA"] = "-device qemu-xhci,id=xhs"
ok = True
def comp_nodes():
    out = cb.SER.cmd("for p in /proc/[0-9]*; do grep -qx cosmic-comp $p/comm 2>/dev/null && ls -l $p/fd 2>/dev/null; done | grep -o 'event[0-9]*' | sort -u | tr '\\n' ' '; echo END", 30)
    m = re.findall(r"((?:event\d+ )*)END", out)
    return set(m[-1].split()) if m else set()
def comp_alive():
    return "cosmic-comp" in cb.SER.cmd("cat /proc/[0-9]*/comm", 30)
def type_marker(name, tag):
    cb.typ(f"echo {tag} > /tmp/{name}"); cb.key("ret", 0); time.sleep(4)
    return tag in cb.SER.cmd(f"cat /tmp/{name}", 20)
try:
    cb.log("start", cb.drv("start", "aarch64", "--virgl")[-200:].replace("\n", " | "))
    cb.log("login", cb.drv("login", "root", "root", timeout=240)[-80:].replace("\n", " | "))
    cb.SER = cb.Serial()
    time.sleep(100); cb.log("quiet", cb.settle_quiet(120))
    for t in range(4):
        cb.click(830, 309); cb.key("ctrl-a"); cb.key("backspace")
        cb.typ("leandro"); time.sleep(2); cb.click(948, 309); time.sleep(2)
        cb.key("ret", 0); time.sleep(40)
        up = "cosmic-panel" in cb.SER.cmd("cat /proc/[0-9]*/comm", 30)
        cb.log("LOGIN try", t, up)
        if up: break
    cb.key("meta_l-t", 0); time.sleep(25); cb.save("1-term")
    before = comp_nodes(); cb.log("comp nodes before", sorted(before))
    cb.log("device_add", cb.drv("qmp", "device_add", json.dumps({"driver": "usb-kbd", "bus": "xhs.0", "id": "hk"}), timeout=60)[-120:])
    time.sleep(8)
    after = comp_nodes(); new = after - before
    cb.log("comp nodes after add", sorted(after))
    if new: cb.log("PASS (a) cosmic-comp opened", sorted(new))
    else: ok = False; cb.log("FAIL (a) cosmic-comp opened no new event node")
    if type_marker("uh1", "UDEVHOT1"): cb.log("PASS (b) keys via usb-kbd reached the terminal")
    else: ok = False; cb.log("FAIL (b) UDEVHOT1 not in /tmp/uh1")
    cb.save("2-typed")
    cb.log("device_del", cb.drv("qmp", "device_del", json.dumps({"id": "hk"}), timeout=60)[-120:])
    time.sleep(8)
    gone = comp_nodes(); cb.log("comp nodes after del", sorted(gone))
    if new and new & gone: ok = False; cb.log("FAIL (c) cosmic-comp still holds", sorted(new & gone))
    if not comp_alive(): ok = False; cb.log("FAIL (c) cosmic-comp died")
    if type_marker("uh2", "UDEVHOT2"): cb.log("PASS (c) keys after unplug reached the terminal")
    else: ok = False; cb.log("FAIL (c) UDEVHOT2 not in /tmp/uh2")
    cb.save("3-after")
finally:
    cb.log("stop", cb.drv("stop")[-100:])
    cb.log("RESULT", "PASS" if ok else "FAIL")
