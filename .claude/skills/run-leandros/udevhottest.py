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
# A usb-mouse at boot: an xhci with no device at boot never saw a hot-add here
# (no [USB] line for it; separate bug), and it also exercises coldplug enumeration.
os.environ["LEANDROS_QEMU_EXTRA"] = "-device qemu-xhci,id=xhs -device usb-mouse,bus=xhs.0,id=m0"
ok = True
def comp_pid():
    out = cb.SER.cmd("for p in /proc/[0-9]*; do case $(cat $p/comm 2>/dev/null) in cosmic-comp) echo CPID=${p#/proc/};; esac; done; echo END", 30)
    m = re.findall(r"CPID=(\d+)", out)
    return m[0] if m else None
def serial_log():
    try: return open(os.environ["CS_OUT"] + "/serial.log", errors="replace").read()
    except Exception: return ""
def comp_regs(pid, since):
    # The evdev server logs every open of an input node with the opener's pid;
    # /proc/<pid>/fd listings are unreliable here.
    return set(re.findall(r"\[EVDEV\] register dev=(\d+) open=\w+ pid=%s\b" % pid, serial_log()[since:]))
def comp_alive():
    return "cosmic-comp" in cb.SER.cmd("cat /proc/[0-9]*/comm", 30)
def type_marker(name, tag):
    cb.typ(f"touch /tmp/{tag}"); cb.key("ret", 0); time.sleep(4)
    return "MISSING" not in cb.SER.cmd(f"ls /tmp/{tag} || echo MIS''SING", 20)
try:
    cb.log("start", cb.drv("start", os.environ.get("LEANDROS_TEST_ARCH", "aarch64"), "--virgl")[-200:].replace("\n", " | "))
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
    cpid = comp_pid(); cb.log("cosmic-comp pid", cpid)
    mark = len(serial_log())
    cb.log("device_add", cb.drv("qmp", "device_add", json.dumps({"driver": "usb-kbd", "bus": "xhs.0", "id": "hk"}), timeout=60)[-120:])
    time.sleep(8)
    new = comp_regs(cpid, mark) if cpid else set()
    if new: cb.log("PASS (a) cosmic-comp registered new evdev node(s)", sorted(new))
    else: ok = False; cb.log("FAIL (a) no [EVDEV] register for cosmic-comp pid", cpid, "after hot-add")
    if type_marker("uh1", "udevhota"): cb.log("PASS (b) keys via usb-kbd reached the terminal")
    else: ok = False; cb.log("FAIL (b) /tmp/udevhota not created")
    cb.save("2-typed")
    cb.log("device_del", cb.drv("qmp", "device_del", json.dumps({"id": "hk"}), timeout=60)[-120:])
    time.sleep(8)
    if not comp_alive(): ok = False; cb.log("FAIL (c) cosmic-comp died")
    if type_marker("uh2", "udevhotb"): cb.log("PASS (c) keys after unplug reached the terminal")
    else: ok = False; cb.log("FAIL (c) /tmp/udevhotb not created")
    cb.save("3-after")
finally:
    cb.log("stop", cb.drv("stop")[-100:])
    cb.log("RESULT", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
