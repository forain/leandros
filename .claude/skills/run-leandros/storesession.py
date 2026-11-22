#!/usr/bin/env python3
"""storesession.py <arch> <outdir> [--venus]  (cosmic-store Flatpak backend: libflatpak list+install, then GUI store run)

GPU boot (virgl default, --venus optional), greeter login as leandro, then in ONE session: apk add flatpak,
flatpak install --user Flatseal (skipped if present), GUI `flatpak run` with WAYLAND_DEBUG=1 (90 s), checks.
Session env (XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, ...) is captured from a live session process: the
first of cosmic-session/cosmic-comp/cosmic-panel/... whose /proc/<pid>/environ has a DBUS_SESSION_BUS_ADDRESS.
Results land in <outdir>/flat.out. Host Mesa 26.x may SEGV QEMU at stop with --virgl: results before it are valid."""
import imgclone
import os, subprocess, sys, time
os.environ.setdefault("LEANDROS_RUN_ID", "flatstore")
os.environ.setdefault("LEANDROS_VNC_BASE", "5979")
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
       subprocess.run([*imgclone.CPC, s, img], capture_output=True).returncode != 0:
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

# --- cosmic-store Flatpak backend session (lane/flatstore) ---------------------------------------
import base64, gzip
FA = "com.github.tchx84.Flatseal"
sh("ls -l /bin/cosmic-store /usr/lib/libflatpak.so.0 /usr/lib/libostree-1.so.1; /bin/cosmic-store --help 2>&1 | head -n 4; echo help rc=$?", 90)
# (1) libs/ldd-equivalent: the dynamic loader's view of the store binary
sh("LD_TRACE_LOADED_OBJECTS=1 /lib/ld-musl-" + ("x86_64" if arch == "x86_64" else "aarch64") + ".so.1 /bin/cosmic-store 2>&1 | grep -c 'not found'; LD_TRACE_LOADED_OBJECTS=1 /lib/ld-musl-" + arch + ".so.1 /bin/cosmic-store 2>&1 | grep -E 'flatpak|ostree|not found'", 60)
sh("apk add flatpak 2>&1 | tail -n 1; su leandro -c 'flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo; echo ra rc=$?; flatpak --user update --appstream 2>&1 | tail -n 2; echo upd rc=$?'", 600)
sh("grep -c '<component' /home/leandro/.local/share/flatpak/appstream/flathub/*/active/appstream.xml 2>/dev/null; ls /home/leandro/.local/share/flatpak/appstream/flathub/*/active/ 2>&1 | head", 60)
# (2) libflatpak smoke through our staged libs (binary built against the cross sysroot)
b = base64.b64encode(open("/tmp/flatstore-flatinst-" + arch, "rb").read()).decode()
sh(f"echo {b} | base64 -d > /tmp/flatinst; chmod 755 /tmp/flatinst; ls -l /tmp/flatinst", 60)
sh("su leandro -c '/tmp/flatinst list 2>&1 | tail -n 8'; echo list rc=$?", 600)
if not os.environ.get("STORE_SKIP_INSTALL"): sh("su leandro -c '/tmp/flatinst install " + FA + " > /tmp/fi.out 2>&1; echo inst rc=$?; grep -a -E \"OP-|INSTALL|FAIL|installed:\" /tmp/fi.out | tail -n 20; flatpak --user list 2>&1 | head'", 2400)
# (3) GUI: login, run the store with RUST_LOG, read the backend lines, one screenshot
wait_for("greeter", lambda: have("cosmic-greeter"), 150)
ser.pump(4 * scale); typ("leandro"); key("ret")
wait_for("panel", lambda: have("cosmic-panel"), 150)
ser.pump(10 * scale)
put("/tmp/cap.sh", [
    "#!/bin/sh", ": > /tmp/env.txt",
    "for n in cosmic-session cosmic-comp cosmic-panel; do",
    "  for c in /proc/[0-9]*/comm; do",
    "    [ \"$(cat $c 2>/dev/null)\" = \"$n\" ] || continue; d=${c%/comm}",
    "    if tr '\\0' '\\n' < $d/environ 2>/dev/null | grep -q '^DBUS_SESSION_BUS_ADDRESS=.'; then",
    "      tr '\\0' '\\n' < $d/environ | grep -E '^(XDG_RUNTIME_DIR|HOME|USER|DBUS_SESSION_BUS_ADDRESS|XDG_SESSION_[A-Z]*|XDG_CURRENT_DESKTOP|LANG|WAYLAND_DISPLAY)=' > /tmp/env.txt; break 2; fi",
    "  done", "done", "[ -s /tmp/env.txt ] && echo CAPTURED || echo CAPTURE-FAILED", "chmod 644 /tmp/env.txt"])
sh("/tmp/cap.sh 2>&1", 90)
put("/tmp/store.sh", ["#!/bin/sh", "set -a; . /tmp/env.txt; set +a", "export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}",
    "RUST_LOG=cosmic_store=debug,libflatpak=debug,warn timeout 200 /bin/cosmic-store > /tmp/store.log 2>&1; echo EXIT=$? >> /tmp/store.log"])
put("/tmp/shot.sh", ["#!/bin/sh", "set -a; . /tmp/env.txt; set +a", "export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}", "mkdir -p /tmp/shots; rm -f /tmp/shots/*",
    "timeout 60 cosmic-screenshot --interactive=false --save-dir /tmp/shots 2>&1 | tail -n 3; for f in /tmp/shots/*.png; do mv $f /tmp/shots-$1.png; done; ls -l /tmp/shots-$1.png; sha1sum /tmp/shots-$1.png"])
sh("chmod 755 /tmp/store.sh /tmp/shot.sh; chmod 1777 /tmp; su leandro -c '. /tmp/env.txt; export DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR; /usr/libexec/leandros-sysbus portal screenshot 60' 2>&1 | tail -n 2; rm -f /tmp/screenshot-*.png", 120)
sh("su leandro -c '/tmp/store.sh &'; sleep 1; echo started", 30)
ser.pump(90 * scale)
sh("wc -l /tmp/store.log; tail -n 30 /tmp/store.log | cut -c1-220; for p in $(pidof cosmic-store); do grep -E 'State|Threads' /proc/$p/status; for t in /proc/$p/task/*; do echo $(cat $t/comm) $(cat $t/wchan 2>/dev/null) $(grep State $t/status | cut -f2); done | sort | uniq -c | head -20; done", 90)
sh("su leandro -c '/tmp/shot.sh store' 2>&1", 120)
ser.pump(40 * scale)
sh("grep -a -E 'cosmic_store|flatpak|Flatpak' /tmp/store.log | cut -c1-220 | tail -n 40; ps | grep -c cosmic-store", 90)
R.close(); drv("stop", t=60); print("STORESESSION done", flush=True); sys.exit(0)
