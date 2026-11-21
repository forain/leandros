#!/usr/bin/env python3
"""flatsession.py <arch> <outdir> [gui|probe:<script>] [--venus]

GPU boot (virgl default, --venus optional), greeter login as leandro, then in ONE session: apk add flatpak,
flatpak install --user Flatseal (skipped if present), GUI `flatpak run` with WAYLAND_DEBUG=1 (90 s), checks.
Session env (XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, ...) is captured from a live session process: the
first of cosmic-session/cosmic-comp/cosmic-panel/... whose /proc/<pid>/environ has a DBUS_SESSION_BUS_ADDRESS.
Results land in <outdir>/flat.out. Host Mesa 26.x may SEGV QEMU at stop with --virgl: results before it are valid."""
import os, subprocess, sys, time
os.environ.setdefault("LEANDROS_RUN_ID", "flatwin")
os.environ.setdefault("LEANDROS_VNC_BASE", "5977")
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
    import re  # drop kernel console lines (FORK/MMAP spam would crowd out the tail)
    o = "\n".join(l for l in o.splitlines() if (not re.match(r"^\[ *\d+\.\d+\]", l) or re.search(r"(wl_|xdg_|zwp_|wp_|zxdg_|ext_|zwlr_)\w*[@#.]", l)) and not re.search(r"\[(FORK|MMAP-BIG|fslock|pcache)\]", l))
    R.write(f"### {time.strftime('%T')} {c[:200]}\n{o[-30000:]}\n"); R.flush()
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
    "flatpak run --log-session-bus --env=WAYLAND_DEBUG=1 --env=G_MESSAGES_DEBUG=all --env=GDK_DEBUG=opengl,dmabuf,vulkan-disable --env=GSK_RENDERER=ngl --env=GSK_DEBUG=renderer --env=EGL_LOG_LEVEL=debug --env=MESA_DEBUG=1 --env=LIBGL_DEBUG=verbose com.github.tchx84.Flatseal > /tmp/fgui.log 2>&1; echo EXIT=$? > /tmp/fgui.exit"])
put("/tmp/portal.sh", [
    "#!/bin/sh", "set -a; . /tmp/env.txt; set +a",
    "export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unix:path=/run/user/1000/bus}",
    "G_MESSAGES_DEBUG=all /usr/libexec/xdg-desktop-portal -r -v > /tmp/portal.log 2>&1"])
sh("apk add python3 py3-pillow 2>&1 | tail -n 1; which cosmic-screenshot; cosmic-screenshot --help 2>&1 | head -n 20", 400)
put("/tmp/shot.sh", ["#!/bin/sh", "set -a; . /tmp/env.txt; set +a", "export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}", "mkdir -p /tmp/shots; rm -f /tmp/shots/*",
    "timeout 60 cosmic-screenshot --interactive=false --save-dir /tmp/shots 2>&1 | tail -n 5; for f in /tmp/shots/*.png; do mv $f /tmp/shots-$1.png; done; ls -l /tmp/shots-*"])
sh("chmod 755 /tmp/shot.sh; chmod 1777 /tmp; su leandro -c '/tmp/shot.sh base' 2>&1", 120)
sh("su leandro -c '/tmp/portal.sh > /dev/null 2>&1 &'; sleep 3; wc -l /tmp/portal.log; head -n 5 /tmp/portal.log", 30)
sh("rm -f /tmp/fgui.exit; su leandro -c '/tmp/fgui.sh > /tmp/fgui.pre 2>&1 &' ; sleep 1; cat /tmp/fgui.pre; echo started", 30)
ser.pump(100 * scale)
put("/tmp/diag.sh", [
    "#!/bin/sh", "echo EXIT=$(cat /tmp/fgui.exit 2>/dev/null); wc -l /tmp/fgui.log",
    "grep -a -v -E '^(B|C|F)[-0-9]*: |^\\*|^[[:space:]]*$' /tmp/fgui.log > /tmp/fg.clean",
    "echo APPLINES; wc -l < /tmp/fg.clean; tail -n 14 /tmp/fg.clean | cut -c1-200",
    "echo WLCOUNT; for k in get_xdg_surface get_toplevel set_title ack_configure attach commit; do echo $k=$(grep -a -c \"$k\" /tmp/fg.clean)n; done",
    "echo GLINFO; grep -a -i -E 'opengl|renderer|MESA|ZINK|EGL|GL_|llvmpipe|virgl|swrast|dri2' /tmp/fg.clean | head -n 10 | cut -c1-200",
    "echo BUFTYPE; for k in wl_shm.create_pool wl_shm_pool.create_buffer zwp_linux_dmabuf_v1 create_immed zwp_linux_buffer_params_v1 wl_drm wp_viewport; do echo $k=$(grep -a -c \"$k\" /tmp/fg.clean); done",
    "echo GLSTR; grep -a -i -E 'GL_RENDERER|renderer|virgl|llvmpipe|softpipe|swrast|zink|using|Mesa|EGL|dri|gl ' /tmp/fg.clean | head -n 25 | cut -c1-220",
    "echo SANDBOX; ls -l /dev/dri; flatpak --user info --show-permissions com.github.tchx84.Flatseal | head -n 20; flatpak --user list --runtime | head -n 12",
    "echo WLTAIL; grep -a -E 'wl_|xdg_' /tmp/fgui.log | tail -n 25 | cut -c1-200",
    "echo PROC; for c in /proc/[0-9]*/comm; do n=$(cat $c); case $n in gjs*) d=${c%/comm}; grep -E 'State' $d/status;; esac; done"])
sh("su leandro -c '/tmp/shot.sh after' 2>&1; grep -a -i screenshot /tmp/portal.log | tail -n 12 | cut -c1-200", 120)
put("/tmp/pix.py", ["from PIL import Image, ImageChops",
    "import hashlib, sys",
    "a=Image.open('/tmp/shots-base.png').convert('RGB'); b=Image.open('/tmp/shots-after.png').convert('RGB')",
    "print('sizes', a.size, b.size)",
    "d=ImageChops.difference(a,b).convert('L').point(lambda v:255 if v>8 else 0)",
    "print('diff_bbox', d.getbbox(), 'diff_px', sum(1 for v in d.getdata() if v))",
    "bb=d.getbbox()",
    "if bb:",
    "  c=b.crop(bb); print('crop_colors', len(c.getcolors(1<<24) or []), 'crop_sha', hashlib.sha1(c.tobytes()).hexdigest())",
    "print('after_colors', len(b.getcolors(1<<24) or []), 'base_sha', hashlib.sha1(a.tobytes()).hexdigest()[:12], 'after_sha', hashlib.sha1(b.tobytes()).hexdigest()[:12])"])
sh("ls -l /tmp/shots-*.png; python3 /tmp/pix.py 2>&1 | tail -n 12", 300)
sh("for c in /proc/[0-9]*/comm; do case $(cat $c) in gjs*) ;; *) continue;; esac; d=${c%/comm}; echo GJSMAPS ${d#/proc/} $(cat $c); ls -l $d/fd | grep -a -E 'dri|dma|drm'; grep -a -o -E '[^ ]*(gallium|_dri|vulkan|lvp|virtio|swrast|libEGL|libGL|libgbm)[^ ]*' $d/maps | sort -u | head -n 20; done", 60)
sh("/tmp/diag.sh 2>&1", 90)
# logs: portal, proxy/bus traffic, who owns stderr of busd/portal/proxy, gjs thread state, vfs lines
put("/tmp/logs.sh", [
    "#!/bin/sh",
    "echo PORTAL; wc -l < /tmp/portal.log; grep -a -v -E '^[[:space:]]*$' /tmp/portal.log | tail -n 45 | cut -c1-220",
    "echo PORTAL-ERR; grep -a -i -E 'warn|error|critical|denied|fail|flatpak-info|proc/' /tmp/portal.log | head -n 25 | cut -c1-220",
    "echo BUSTRAFFIC; grep -a -E '^(B|C|F)[-0-9]*: ' /tmp/fgui.log | wc -l; grep -a -i -E 'vfs|Error|Timeout|Settings|Registry|Inhibit' /tmp/fgui.log | grep -a -E '^(B|C|F)[-0-9]*: ' | head -n 30 | cut -c1-220",
    "echo BUSTAIL; grep -a -E '^(B|C|F)[-0-9]*: ' /tmp/fgui.log | tail -n 15 | cut -c1-200",
    "echo APPERR; grep -a -i -E 'warn|error|critical|timeout|fail|Gjs|Gtk-|Gdk-|dconf|vfs' /tmp/fg.clean | head -n 30 | cut -c1-220",
    "echo FDS; for n in busd xdg-desktop-portal xdg-dbus-proxy xdg-desktop-portal-cosmic gjs; do for c in /proc/[0-9]*/comm; do [ \"$(cat $c 2>/dev/null)\" = $n ] || continue; d=${c%/comm}; echo $n ${d#/proc/} fd1=$(readlink $d/fd/1) fd2=$(readlink $d/fd/2) ppid=$(grep PPid $d/status | cut -f2); done; done",
    "echo THREADS; for c in /proc/[0-9]*/comm; do n=$(cat $c 2>/dev/null); case $n in gjs*|xdg-dbus-prox*|xdg-desktop-p*|busd) d=${c%/comm}; for t in $d/task/*; do echo $n ${d#/proc/} ${t#$d/task/} $(cat $t/comm 2>/dev/null) st=$(grep State $t/status | cut -f2) wchan=$(cat $t/wchan 2>/dev/null) sc=$(cat $t/syscall 2>/dev/null | cut -c1-40); done;; esac; done | head -n 60",
    "echo SVC; ls /usr/share/dbus-1/services | grep -i -E 'gtk|vfs|gvfs|portal|document'; ls /run/user/1000 | head -n 30"])
sh("/tmp/logs.sh 2>&1", 120)
put("/tmp/pxy.sh", [
    "#!/bin/sh", "set -a; . /tmp/env.txt; set +a",
    "rm -f /tmp/px.sock; xdg-dbus-proxy $DBUS_SESSION_BUS_ADDRESS /tmp/px.sock --filter --talk=org.freedesktop.DBus --log > /tmp/px.log 2>&1 &",
    "sleep 2; ls -l /tmp/px.sock",
    "for a in $DBUS_SESSION_BUS_ADDRESS unix:path=/tmp/px.sock; do echo ADDR $a; export DBUS_SESSION_BUS_ADDRESS=$a",
    "  timeout 40 gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus -m org.freedesktop.DBus.StartServiceByName org.gtk.vfs.Daemon 0 2>&1 | head -n 3; echo rc=$?",
    "done", "echo PXLOG; cut -c1-200 /tmp/px.log | tail -n 20"])
sh("apk add glib 2>&1 | tail -n 1; which gdbus; su leandro -c /tmp/pxy.sh 2>&1", 150)
sh("sleep 1; /tmp/logs.sh 2>&1 | head -n 0; for f in /run/user/1000/*.log /tmp/*busd*.log /var/log/*busd*; do [ -f $f ] && { echo == $f; tail -n 20 $f | cut -c1-200; }; done", 60)
R.close()
drv("stop", t=60)
print("FLATSESSION done", flush=True)
