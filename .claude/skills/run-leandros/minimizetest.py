#!/usr/bin/env python3
"""minimizetest.py -- minimize and un-hide a window under COSMIC (virgl session).

Logs in through the greeter as leandro (cosmic-panel wrapped with WAYLAND_DEBUG=1 so its
wire trace is in /tmp/cosmic-panel-1000.log), opens cosmic-term with Super+T, then:
  1. `mintest min <app_id>` (zcosmic_toplevel_manager.set_minimized, the only minimize path
     cosmic-comp has) and checks zcosmic_toplevel_info reports `minimized`, not `activated`;
  2. computes the dock's minimize-applet entry from the panel's layer-surface geometry in
     the wire trace (the "Dock" get_layer_surface / configure / set_margin and its pill's
     wl_region), and CLICKS it with the pointer (QMP), the way the applet is used;
  3. checks the toplevel is no longer minimized and is activated, then types `touch` into
     it and checks the shell ran it (keyboard focus really is on the restored window).
    CS_REPO=<repo> CS_OUT=<dir> LEANDROS_RUN_ID=x python3 minimizetest.py
"""
import os, re, sys, time
R = os.environ["CS_REPO"]
sys.path.insert(0, R + "/.claude/skills/run-leandros")
import importlib.util
spec = importlib.util.spec_from_file_location("cb", R + "/.claude/skills/run-leandros/coldstart-bench.py")
cb = importlib.util.module_from_spec(spec); spec.loader.exec_module(cb)
import driver
ARCH = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
APP = "com.system76.CosmicTerm"
W, H = 1280, 800
ENV = "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1; "
PANELLOG = "/tmp/cosmic-panel-1000.log"
RES = []


def check(name, ok, detail=""):
    RES.append(bool(ok))
    cb.log("CHECK", name, "PASS" if ok else "FAIL", detail)
    return ok


def tl():
    """The target toplevel's states from `mintest list`, or None."""
    o = cb.SER.cmd(ENV + "mintest list", 40)
    m = re.search(r"TL app_id=" + re.escape(APP) + r" title=.*? states=(\S+)", o)
    cb.log("TL", o[-300:].replace("\n", " | ") if not m else m.group(0))
    return m.group(1).split(",") if m else None


def wait_state(pred, secs):
    end = time.time() + secs
    st = None
    while time.time() < end:
        st = tl()
        if st is not None and pred(st):
            return st
        time.sleep(1)
    return st


def dock_geometry():
    """(x, y, w, h) of the dock's pill from the panel's wire trace: its layer surface
    (get_layer_surface ... "Dock") and the last configure/margin for it, plus the
    wl_region the panel sets as that surface's input region (the pill)."""
    o = cb.SER.cmd(
        f"L={PANELLOG}; echo @@LS; grep 'get_layer_surface.*\"Dock\"' $L | tail -n 1 | cut -c1-200", 40)
    m = re.search(r"get_layer_surface\(new id zwlr_layer_surface_v1#(\d+), wl_surface#(\d+)", o)
    if not m:
        return None, o
    ls, surf = m.group(1), m.group(2)
    o = cb.SER.cmd(
        f"L={PANELLOG}; echo @@GEO; grep -E 'zwlr_layer_surface_v1#{ls}\\.(configure|set_margin|set_size)' $L | tail -n 4 | cut -c1-120; "
        f"echo @@IR; grep -E 'wl_surface#{surf}\\.set_input_region' $L | tail -n 1 | cut -c1-120; echo @@END", 40)
    m = re.search(r"wl_surface#%s\.set_input_region\(wl_region#(\d+)\)" % surf, o)
    if m:   # the pill: the add() calls of the region the Dock surface last set as input region
        o += cb.SER.cmd(f"echo @@REG; grep -E 'wl_region#{m.group(1)}\\.add' {PANELLOG} | tail -n 2 | cut -c1-120; echo @@END2", 40)
    return (ls, surf), o


try:
    cb.log("start", cb.drv("start", ARCH, "--virgl")[-200:].replace("\n", " | "))
    cb.log("login", cb.drv("login", "root", "root", timeout=240)[-80:].replace("\n", " | "))
    cb.SER = cb.Serial()
    # Wrap cosmic-panel so its Wayland wire trace is kept (before the session starts).
    cb.SER.cmd("mkdir -p /usr/libexec/lrf; [ -e /usr/libexec/lrf/cosmic-panel ] || mv /bin/cosmic-panel /usr/libexec/lrf/cosmic-panel; "
               "printf '#!/bin/sh\\nWAYLAND_DEBUG=1 exec /usr/libexec/lrf/cosmic-panel \"$@\" >>/tmp/cosmic-panel-$(id -u).log 2>&1\\n' > /bin/cosmic-panel; "
               "chmod 755 /bin/cosmic-panel; ls -l /bin/mintest | cut -c1-60", 30)
    t0 = time.time()
    while "cosmic-greeter" not in cb.SER.cmd("cat /proc/[0-9]*/comm", 30) and time.time() - t0 < 240:
        time.sleep(3)
    time.sleep(int(os.environ.get("MT_GREETER_PAINT", "60")))
    for t in range(4):
        cb.click(830, 309); cb.key("ctrl-a"); cb.key("backspace")
        cb.typ("leandro"); time.sleep(2); cb.click(948, 309); time.sleep(2)
        cb.key("ret", 0)
        t1 = time.time()
        up = False
        while time.time() - t1 < 90 and not up:
            time.sleep(5)
            up = "cosmic-panel" in cb.SER.cmd("cat /proc/[0-9]*/comm", 30)
        cb.log("LOGIN try", t, up)
        if up: break
    check("desktop up", up)
    time.sleep(25)  # panel and dock first paint
    cb.key("meta_l-t", 0)
    t1 = time.time()
    while "cosmic-term" not in cb.SER.cmd("cat /proc/[0-9]*/comm", 30) and time.time() - t1 < 90:
        time.sleep(3)
    st = wait_state(lambda s: "activated" in s, 60)
    check("term open+activated", st is not None and "activated" in st, str(st))

    # Control: typing reaches the (unminimized) window, so a later miss is the restore's fault.
    o = ""
    for attempt in range(3):
        cb.typ("touch /tmp/pre"); cb.key("ret", 0)
        for _ in range(8):
            time.sleep(2)
            o = cb.SER.cmd("ls /tmp/pre", 15)
            if re.search(r"^/tmp/pre", o, re.M): break
        if re.search(r"^/tmp/pre", o, re.M): break
    check("control: typing reaches term before minimize", re.search(r"^/tmp/pre", o, re.M) is not None, o[-60:].replace("\n", " | "))
    cb.log("MIN", cb.SER.cmd(ENV + f"mintest min {APP}", 40)[-250:].replace("\n", " | "))
    st = wait_state(lambda s: "minimized" in s, 15)
    check("minimized", st is not None and "minimized" in st and "activated" not in st, str(st))
    time.sleep(5)  # let the applet pick the toplevel up and the dock relayout

    ids, o = dock_geometry()
    cb.log("DOCKGEO", ids, o[-700:].replace("\n", " | "))
    # Dock layer surface: anchored bottom, full output width minus margins, height h; the
    # pill (input region) is centred in it; the minimize applet's entry is its LAST item.
    cfg = re.findall(r"configure\(\d+, (\d+), (\d+)\)", o)
    mar = re.findall(r"set_margin\((-?\d+), (-?\d+), (-?\d+), (-?\d+)\)", o)
    reg = re.findall(r"wl_region#\d+\.add\((-?\d+), (-?\d+), (\d+), (\d+)\)", o)
    check("dock geometry in wltrace", bool(cfg and reg), f"cfg={cfg[-1:]} margin={mar[-1:]} region={reg[-1:]}")
    if cfg and reg:
        sw, sh = int(cfg[-1][0]), int(cfg[-1][1])
        top, right, bottom, left = (int(x) for x in mar[-1]) if mar else (0, 0, 0, 0)
        rx, ry, rw, rh = (int(x) for x in reg[-1])
        sx, sy = left, H - bottom - sh          # surface origin on the output
        item = rh                                # entries are square, pill height wide
        cx, cy = sx + rx + rw - item / 2, sy + ry + rh / 2
        cb.log("CLICK at", cx, cy, f"surface=({sx},{sy},{sw},{sh}) region=({rx},{ry},{rw},{rh})")
        cb.save("minimized")
        cb.click(cx, cy)
        st = wait_state(lambda s: "minimized" not in s, 20)
        check("un-hidden by applet click", st is not None and "minimized" not in st, str(st))
        st = wait_state(lambda s: "activated" in s, 10)
        check("restored window activated", st is not None and "activated" in st, str(st))
        cb.SER.cmd("rm -f /tmp/minok", 15)
        o = ""
        for attempt in range(3):       # cosmic-term's shell may still be starting: retype
            cb.typ("touch /tmp/minok"); cb.key("ret", 0)
            for _ in range(8):
                time.sleep(2)
                o = cb.SER.cmd("ls /tmp/minok", 15)
                if re.search(r"^/tmp/minok", o, re.M): break
            if re.search(r"^/tmp/minok", o, re.M): break
        check("keyboard focus on restored window", re.search(r"^/tmp/minok", o, re.M) is not None, o[-80:].replace("\n", " | "))
        cb.save("restored")
finally:
    cb.log("stop", cb.drv("stop")[-100:])
ok = bool(RES) and all(RES)
cb.log("RESULT", "PASS" if ok else "FAIL", RES)
sys.exit(0 if ok else 1)
