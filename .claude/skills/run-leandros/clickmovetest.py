#!/usr/bin/env python3
"""clickmovetest.py -- do clicks batched with / interleaved with pointer motion reach a
Wayland client under COSMIC (virgl)? Logs in via the greeter, starts
`WAYLAND_DEBUG=1 cosmic-term`, sends N clicks still / batched / interleaved over QMP and
counts wl_pointer.button lines per phase (cosmic-term binds 3 wl_pointers: 3 lines per event).
    CS_REPO=<repo> CS_OUT=<dir> LEANDROS_RUN_ID=x python3 clickmovetest.py   (CM_N=50)
"""
import os, re, sys, time
R = os.environ["CS_REPO"]
sys.path.insert(0, R + "/.claude/skills/run-leandros")
import importlib.util
spec = importlib.util.spec_from_file_location("cb", R + "/.claude/skills/run-leandros/coldstart-bench.py")
cb = importlib.util.module_from_spec(spec); spec.loader.exec_module(cb)
import driver
N = int(os.environ.get("CM_N", "50"))
CX, CY = int(os.environ.get("CM_X", "640")), int(os.environ.get("CM_Y", "450"))
W, H = 1280, 800
def a(px, py): return [{"type": "abs", "data": {"axis": "x", "value": int(px*32767/W)}},
                       {"type": "abs", "data": {"axis": "y", "value": int(py*32767/H)}}]
def btn(d): return [{"type": "btn", "data": {"down": d, "button": "left"}}]
COUNT = "cut -d'(' -f1 /tmp/wd.log | tr -d '0-9 []' | sort | uniq -c"
ARCH = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
COUNTS = {}
def count(tag):
    o = cb.SER.cmd(COUNT, 60); cb.log("COUNT", tag, o[-12000:].replace("\n", " | "))
    m = re.search(r"(\d+)\s+\S*wl_pointer\S*\.button", o)
    COUNTS[tag] = int(m.group(1)) if m else 0
def phase(tag, fn):
    so = driver._qmp_open()
    send = lambda ev: driver._qmp_command(so, "input-send-event", {"events": ev})
    for i in range(N):
        fn(send, i)
    so.close(); time.sleep(3); count(tag); cb.save(tag)
def still(send, i):
    send(a(CX, CY)); time.sleep(0.15); send(btn(True)); time.sleep(0.08); send(btn(False)); time.sleep(0.25)
def batched(send, i):
    send(a(CX, CY) + btn(True)); time.sleep(0.03); send(a(CX + 50, CY + 30) + btn(False)); time.sleep(0.25)
def interleaved(send, i):
    for k in range(4): send(a(CX - 40 + 10*k, CY + 3*k)); time.sleep(0.008)
    send(btn(True))
    for k in range(4): send(a(CX + 10*k, CY + 12 + 3*k)); time.sleep(0.008)
    send(btn(False))
    for k in range(4): send(a(CX + 40 - 10*k, CY + 24)); time.sleep(0.008)
    time.sleep(0.2)
try:
    cb.log("start", cb.drv("start", ARCH, "--virgl")[-200:].replace("\n", " | "))
    cb.log("login", cb.drv("login", "root", "root", timeout=240)[-80:].replace("\n", " | "))
    cb.SER = cb.Serial()
    time.sleep(100); cb.log("quiet", cb.settle_quiet(120))
    for t in range(4):
        cb.click(830, 309); cb.key("ctrl-a"); cb.key("backspace")
        cb.typ("leandro"); time.sleep(2); cb.click(948, 309); time.sleep(2); cb.save(f"1-typed{t}")
        cb.key("ret", 0); time.sleep(40)
        up = "cosmic-panel" in cb.SER.cmd("cat /proc/[0-9]*/comm", 30)
        cb.log("LOGIN try", t, up)
        if up: break
    cb.save("1-desktop")
    cb.key("meta_l-t", 0); time.sleep(25); cb.save("2-term")
    cb.SER.cmd("echo 'WAYLAND_DEBUG=1 exec cosmic-term 2>/tmp/wd.log' > /tmp/wd.sh; chmod 755 /tmp/wd.sh", 15)
    cb.typ("sh /tmp/wd.sh"); cb.key("ret", 0); time.sleep(30); cb.save("3-term2")
    cb.log("PS", cb.SER.cmd("ls -l /tmp/wd.log; wc -l /tmp/wd.log", 20)[-300:].replace("\n", " | "))
    count("base")
    for tag, fn in (("still", still), ("batched", batched), ("interleaved", interleaved), ("still2", still)):
        phase(tag, fn)
finally:
    cb.log("stop", cb.drv("stop")[-100:])
# Verdict: every phase must deliver all N press+release pairs (>= 2N button lines, one
# wl_pointer binding at least), and batched/interleaved must match the still phase.
order = ["base", "still", "batched", "interleaved", "still2"]
d = [COUNTS.get(b, 0) - COUNTS.get(a, 0) for a, b in zip(order, order[1:])]
ok = len(COUNTS) == 5 and all(x >= 2 * N for x in d) and len(set(d)) == 1
cb.log("DELTAS", d, "RESULT", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
