#!/usr/bin/env python3
"""flatsession.py <arch> <outdir> [gui|probe:<script>] [--venus]

GPU boot (virgl default, --venus optional), greeter login as leandro, then in ONE session: apk add flatpak,
flatpak install --user Flatseal (skipped if present), GUI `flatpak run` with WAYLAND_DEBUG=1 (90 s), checks.
Session env (XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, ...) is captured from a live session process: the
first of cosmic-session/cosmic-comp/cosmic-panel/... whose /proc/<pid>/environ has a DBUS_SESSION_BUS_ADDRESS.
Results land in <outdir>/flat.out. Host Mesa 26.x may SEGV QEMU at stop with --virgl: results before it are valid."""
import os, subprocess, sys, time
os.environ.setdefault("LEANDROS_RUN_ID", "flatgui")
os.environ.setdefault("LEANDROS_VNC_BASE", "5961")
SK = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, SK)
import gstsession as g  # noqa: E402
driver, drv, log, key, typ = g.driver, g.drv, g.log, g.key, g.typ

arch, out = sys.argv[1], sys.argv[2]
MODE = sys.argv[3] if len(sys.argv) > 3 else "gui"
GPU = "--venus" if "--venus" in sys.argv else "--virgl"
os.makedirs(out, exist_ok=True)
scale = driver.wait_scale(arch)
for what, src, var in (("boot", f"leandros-limine-{arch}.img", "LEANDROS_DISK_IMAGE"),
                       ("root", f"f2fs-data0-{arch}.img", "LEANDROS_ROOT_IMAGE")):
    img = g.clone_path(arch, what)
    try: os.unlink(img)
    except OSError: pass
    s = os.path.join(g.REPO, src)
    if subprocess.run(["cp", "--reflink=auto", s, img], capture_output=True).returncode != 0 and \
       subprocess.run(["cp", "-c", s, img], capture_output=True).returncode != 0:
        subprocess.run(["cp", s, img], check=True)
    os.environ[var] = img
if drv("start", arch, GPU, t=900) != 0:
    drv("stop", t=60); sys.exit("boot failed")
if drv("login", "root", "root", str(int(60 * scale)), t=900) != 0:
    sys.exit("serial login failed")
ser = driver._Serial()
R = open(f"{out}/flat.out", "w")
def sh(c, t=60):
    o = driver.serial_run(c, timeout=t * scale, arch=arch, ser=ser)["output"]
    R.write(f"### {time.strftime('%T')} {c[:200]}\n{o[-12000:]}\n"); R.flush()
    return o
def put(path, lines):  # write a guest file via base64 (the guest shell is brush: quoting-fragile)
    import base64
    b = base64.b64encode(("\n".join(lines) + "\n").encode()).decode()
    sh(f"echo {b} | base64 -d > {path}; chmod 755 {path}; wc -c < {path}", 30)
argv = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'
def have(name):
    return any(os.path.basename(l.strip()).startswith(name) for l in sh(argv, 90).splitlines())
def wait_for(name, pred, timeout):
    t = time.time()
    while time.time() - t < timeout * scale:
        if pred(): log(f"{name}: after {time.time()-t:.0f} s"); return True
        ser.pump(4)
    log(f"{name}: NOT ready"); return False

sh("apk add flatpak 2>&1 | tail -n 1; su leandro -c 'flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo; echo ra rc=$?'", 400)
sh("su leandro -c 'flatpak --user list | grep -q Flatseal && echo already || flatpak install --user -y --noninteractive flathub com.github.tchx84.Flatseal > /tmp/fi.log 2>&1; echo inst rc=$?; tr \"\\r\" \"\\n\" < /tmp/fi.log | grep -v -E \"Installing [0-9]\" | tail -n 6'", 1500)
if MODE.startswith("probe:"):
    L = [l for l in open(MODE[6:]).read().splitlines() if l.strip()]
    for l in [l for l in L if l.startswith("@")]: sh(l[1:], 30)
    put("/tmp/p.sh", [l for l in L if not l.startswith("@")])
    sh("su leandro -c '/bin/sh /tmp/p.sh' 2>&1 | grep -a -v -E '^\\[ *[0-9]+\\.'", 400)
    R.close(); drv("stop", t=60); print("FLATSESSION done", flush=True); sys.exit(0)

# d) plain `flatpak run` as leandro (no session): command mode
sh("su leandro -c 'flatpak run --command=sh com.github.tchx84.Flatseal -c \"echo inside; id\" 2>&1 | tail -n 3; echo run rc=$?'", 300)
if MODE != "gui":
    R.close(); drv("stop", t=60); print("FLATSESSION done", flush=True); sys.exit(0)

wait_for("greeter", lambda: have("cosmic-greeter"), 150)
ser.pump(4 * scale); typ("leandro"); key("ret")
wait_for("panel", lambda: have("cosmic-panel"), 150)
ser.pump(10 * scale)
# Env capture: script in the guest, diagnostics per candidate process (size of environ, has DBUS?)
put("/tmp/cap.sh", [
    "#!/bin/sh", ": > /tmp/env.txt",
    "for n in cosmic-session cosmic-comp cosmic-panel cosmic-launcher cosmic-applets dbus-daemon; do",
    "  for c in /proc/[0-9]*/comm; do",
    "    [ \"$(cat $c 2>/dev/null)\" = \"$n\" ] || continue; d=${c%/comm}; p=${d#/proc/}",
    "    sz=$(wc -c < $d/environ 2>/dev/null); nb=$(tr '\\0' '\\n' < $d/environ 2>/dev/null | grep -c .)",
    "    echo \"cand $n pid=$p environ_bytes=$sz lines=$nb uid=$(grep ^Uid $d/status | cut -f2)\"",
    "    if tr '\\0' '\\n' < $d/environ 2>/dev/null | grep -q '^DBUS_SESSION_BUS_ADDRESS=.'; then",
    "      tr '\\0' '\\n' < $d/environ | grep -E '^(XDG_RUNTIME_DIR|HOME|USER|DBUS_SESSION_BUS_ADDRESS|XDG_SESSION_[A-Z]*|XDG_CURRENT_DESKTOP|LANG|WAYLAND_DISPLAY)=' > /tmp/env.txt",
    "      echo \"USING $n pid=$p\"; break 2; fi",
    "  done",
    "done",
    "[ -s /tmp/env.txt ] && echo CAPTURED || echo CAPTURE-FAILED", "chmod 644 /tmp/env.txt; cat /tmp/env.txt; ls /run/user/1000"])
sh("/tmp/cap.sh 2>&1", 90)
put("/tmp/fgui.sh", [
    "#!/bin/sh", "set -a; [ -s /tmp/env.txt ] && . /tmp/env.txt; set +a",
    ": ${XDG_RUNTIME_DIR:=/run/user/1000}; : ${DBUS_SESSION_BUS_ADDRESS:=unix:path=$XDG_RUNTIME_DIR/bus}; export XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS",
    "export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}",
    "echo BUS=$DBUS_SESSION_BUS_ADDRESS",
    "flatpak run --log-session-bus --env=WAYLAND_DEBUG=1 --env=GDK_DEBUG=opengl --env=GSK_DEBUG=renderer com.github.tchx84.Flatseal > /tmp/fgui.log 2>&1; echo EXIT=$? > /tmp/fgui.exit"])
sh("rm -f /tmp/fgui.exit; su leandro -c '/tmp/fgui.sh > /tmp/fgui.pre 2>&1 &' ; sleep 1; cat /tmp/fgui.pre; echo started", 30)
ser.pump(100 * scale)
put("/tmp/diag.sh", [
    "#!/bin/sh", "echo EXIT=$(cat /tmp/fgui.exit 2>/dev/null); wc -l /tmp/fgui.log",
    "grep -a -v -E '^(B|C|F)[-0-9]*: |^\\*|^[[:space:]]*$' /tmp/fgui.log > /tmp/fg.clean",
    "echo APPLINES; wc -l < /tmp/fg.clean; tail -n 14 /tmp/fg.clean | cut -c1-200",
    "echo WLCOUNT; for k in get_xdg_surface get_toplevel set_title ack_configure attach commit; do echo $k=$(grep -a -c \"$k\" /tmp/fg.clean)n; done",
    "echo GLINFO; grep -a -i -E 'opengl|renderer|MESA|ZINK|EGL|GL_|llvmpipe|virgl|swrast|dri2' /tmp/fg.clean | head -n 10 | cut -c1-200",
    "echo PROC; for c in /proc/[0-9]*/comm; do n=$(cat $c); case $n in gjs*) d=${c%/comm}; grep -E 'State' $d/status;; esac; done"])
sh("/tmp/diag.sh 2>&1", 90)
R.close()
drv("stop", t=60)
print("FLATSESSION done", flush=True)
