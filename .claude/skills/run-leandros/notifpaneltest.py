#!/usr/bin/env python3
"""notifpaneltest.py -- cosmic-panel respawns the notifications applet after a non-zero exit CODE.

Upstream (launch-pad): an applet that exits with a code is restarted (exponential backoff); one killed by a
signal is not. The applet binary is wrapped by a sh script that logs start/exit uptimes and exits 7.
Checks: 1. applet running; 2. kill the real applet -> wrapper exits 7 -> a NEW applet appears (gate).
Info (not gates): signal path (kill the wrapper), cosmic-notifications<->panel handshake errors and
start-time delta, SOCK_STREAM 64 B round trip from `scmtest bench`.
    CS_REPO=<repo> CS_OUT=<dir> LEANDROS_RUN_ID=x python3 notifpaneltest.py
"""
import os, re, sys, time
R = os.environ["CS_REPO"]
sys.path.insert(0, R + "/.claude/skills/run-leandros")
import importlib.util
spec = importlib.util.spec_from_file_location("cb", R + "/.claude/skills/run-leandros/coldstart-bench.py")
cb = importlib.util.module_from_spec(spec); spec.loader.exec_module(cb)
import driver
ARCH = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
PANELLOG = "/tmp/cosmic-panel-1000.log"
LIB = "/usr/libexec/lrf/cosmic-applet-notifications"
RES = []


def check(name, ok, detail=""):
    RES.append(bool(ok))
    cb.log("CHECK", name, "PASS" if ok else "FAIL", detail)
    return ok


def sh(c, t=30):
    # The end marker is computed by the shell, so it never matches the (redrawn) echo of a
    # long command line; output is what follows the last echo of the marker expression.
    o = cb.SER.cmd(c + "; echo @@E$((6*7))", t, wait=r"@@E42")
    o = o.split("@@E$((6*7))")[-1]
    # drop kernel console lines interleaved on the serial line
    return "\n".join(l for l in o.split("\n") if "tgid=" not in l and not re.match(r"\s*\[ *\d+\.\d+\]", l) and "cache]" not in l)


def tail(o, n=300):
    return o[-n:].replace("\n", " | ")


def applet_pids():
    """pids whose cmdline holds the real applet path (the wrapper's cmdline is `sh <orig path>`, not LIB)."""
    o = sh("for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline 2>/dev/null | grep -q '" + LIB +
           "' && echo APID ${p#/proc/}; done; echo @@E", 40)
    return re.findall(r"^APID (\d+)", o, re.M)


def wlog():
    return sh("cat /tmp/notifwrap.log; echo @@E", 20)


def starttime(comm):
    o = sh("for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = " + comm +
           " ] && echo ST ${p#/proc/} $(cut -d' ' -f22 $p/stat); done; echo @@E", 40)
    m = re.search(r"^ST (\d+) (\d+)", o, re.M)
    return (m.group(1), int(m.group(2))) if m else (None, None)


try:
    cb.log("start", cb.drv("start", ARCH, "--virgl")[-200:].replace("\n", " | "))
    cb.log("login", cb.drv("login", "root", "root", timeout=240)[-80:].replace("\n", " | "))
    cb.SER = cb.Serial()
    sh("mkdir -p /usr/libexec/lrf", 15)
    if os.environ.get("NP_WRAP_PANEL", "1") == "1":   # exec wrappers: timestamped RUST_LOG=info logs of both handshake ends
      sh("[ -e /usr/libexec/lrf/cosmic-panel ] || mv /bin/cosmic-panel /usr/libexec/lrf/cosmic-panel; "
       "printf '#!/bin/sh\\nRUST_LOG=info exec /usr/libexec/lrf/cosmic-panel \"$@\" >>/tmp/cosmic-panel-$(id -u).log 2>&1\\n' > /bin/cosmic-panel; "
       "chmod 755 /bin/cosmic-panel", 30)
    if os.environ.get("NP_WRAP_DAEMON", "1") == "1":
      sh("A=$(command -v cosmic-notifications); echo NDAEMON=$A; [ -e /usr/libexec/lrf/cosmic-notifications ] || mv $A /usr/libexec/lrf/cosmic-notifications; "
       "printf '#!/bin/sh\\nRUST_LOG=info exec /usr/libexec/lrf/cosmic-notifications \"$@\" >>/tmp/cosmic-notifications-$(id -u).log 2>&1\\n' > $A; chmod 755 $A; ls -l $A", 30)
    o = sh("A=$(command -v cosmic-applet-notifications); echo ORIG=$A; command -v cut; "
           "[ -e " + LIB + " ] || mv $A " + LIB + "; "
           "printf '%s\\n' '#!/bin/sh' 'echo \"start $$ $(cut -d\" \" -f1 /proc/uptime)\" >> /tmp/notifwrap.log; "
           + LIB + " \"$@\"; rc=$?; echo \"exit $rc $(cut -d\" \" -f1 /proc/uptime)\" >> /tmp/notifwrap.log; exit 7' > $A; "
           "chmod 755 $A; : > /tmp/notifwrap.log; chmod 666 /tmp/notifwrap.log; ls -l $A; cat $A", 30)
    cb.log("WRAP", tail(o, 500))
    t0 = time.time()
    while "cosmic-greeter" not in sh("cat /proc/[0-9]*/comm", 30) and time.time() - t0 < 240:
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
            up = "cosmic-panel" in sh("cat /proc/[0-9]*/comm", 30)
        cb.log("LOGIN try", t, up)
        if up: break
    cb.log("desktop up", up)
    time.sleep(25)

    pids = applet_pids()
    cb.log("WRAPLOG", tail(wlog()))
    check("notif applet running", bool(pids), f"pids={pids}")

    # Crash path: kill the real applet; wrapper exits 7; panel should restart it.
    if pids:
        old = set(pids)
        sh(f"kill -KILL {pids[0]}", 15)
        t1 = time.time(); new = []
        while time.time() - t1 < 15:
            time.sleep(1)
            new = [p for p in applet_pids() if p not in old]
            if new: break
        w = wlog()
        starts = re.findall(r"^start (\d+) ([\d.]+)", w, re.M)
        exits = re.findall(r"^exit (\d+) ([\d.]+)", w, re.M)
        delay = None
        if len(starts) >= 2 and exits:
            delay = round(float(starts[1][1]) - float(exits[0][1]), 3)
        check("respawned after exit code", bool(new) and len(starts) >= 2, f"new={new} starts={len(starts)} exits={exits[:2]} delay_s={delay}")
        cb.log("RESPAWN DELAY s", delay)
        cb.log("PANELLOG restart", tail(sh(f"grep -aE 'exited with code|restarting|sleeping for' {PANELLOG} | tail -n 6 | cut -c1-200; echo @@E", 20), 700))
        if not new:
            sh_pid = sh("for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = cosmic-panel ] && echo PP ${p#/proc/}; done", 30)
            cb.log("EVIDENCE", tail(sh("grep -E '^(Name|State|Threads)' /proc/$(echo '" + sh_pid + "' | grep -o 'PP [0-9]*' | head -n1 | cut -c4-)/status; "
                   "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline 2>/dev/null | grep -q notifications && echo $p $(grep State $p/status); done; echo @@E", 30), 700))

        # Signal path (info only): kill the wrapper sh, then the real applet.
        cur = applet_pids()
        if cur:
            par = sh(f"cut -d' ' -f4 /proc/{cur[0]}/stat; echo @@E", 15)
            m = re.search(r"^(\d+)\s*$", par, re.M)
            if m:
                sh(f"kill -KILL {m.group(1)}; kill -KILL {cur[0]}", 15)
                time.sleep(20)
                back = applet_pids()
                cb.log("SIGNAL PATH", "respawned" if back else "not respawned (expected)", f"wrapper={m.group(1)} now={back}")
            else:
                cb.log("SIGNAL PATH", "no parent pid", par[-60:])

    # Handshake info: short commands only (long serial command lines get mangled).
    HS = {}
    for f in (PANELLOG, "/tmp/cosmic-session-1000.log", "/tmp/cosmic-notifications-1000.log"):
        o = sh(f"grep -aE 'otifications daemon|on fd|panel dbus|start notif|notifications applet' {f} | cut -c1-160 | head -n 8", 30)
        cb.log("HS", f, tail(o, 1400))
        HS[f] = o
    # Handshake gap: the daemon arms a 1 s timeout at "Connecting to daemon on fd"; the panel
    # starts its side at "Connecting to notifications daemon" (both UTC, same guest clock).
    import datetime
    def ts(pat, txt):
        txt = re.sub(r"\x1b(\[[0-9;]*m)?", "", txt)   # tracing's ANSI colours (partly stripped)
        m = re.search(r"(\d{4}-\d\d-\d\dT[\d:.]+)Z\s+INFO.*" + pat, txt)
        return datetime.datetime.fromisoformat(m.group(1)) if m else None
    d = ts("Connecting to daemon on fd", HS.get("/tmp/cosmic-notifications-1000.log", ""))
    p = ts("Connecting to notifications daemon", HS.get(PANELLOG, ""))
    cb.log("HANDSHAKE GAP s", round((p - d).total_seconds(), 3) if d and p else None, "(daemon times out at 1.0)")
    np, ns = starttime("cosmic-notifica")
    pp, ps = starttime("cosmic-panel")
    cb.log("STARTTIME", f"notifications pid={np} ticks={ns} panel pid={pp} ticks={ps}",
           f"delta_ms={(ns - ps) * 10 if ns is not None and ps is not None else None} (CLK_TCK=100 assumed)")

    # Socket latency info.
    o = sh("/bin/scmtest bench 2000 2>&1 | grep -a 'IPCBENCH stream size=64 ' | cut -c1-160; echo @@E", 240)
    cb.log("IPCBENCH", tail(o, 400))
finally:
    cb.log("stop", cb.drv("stop")[-100:])
ok = len(RES) >= 2 and all(RES)
cb.log("RESULT", "PASS" if ok else "FAIL", RES)
sys.exit(0 if ok else 1)
