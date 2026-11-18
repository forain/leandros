#!/usr/bin/env python3
"""storesession.py <arch> <outdir> [--gpu virgl|venus] [--keep]

cosmic-store against the PackageKit facade (leandros-sysbus packagekit):
boot --virgl, serial root login, greeter login as leandro, wait for
cosmic-panel, start `cosmic-store` in the session, wait ~60 s, then check
(1) cosmic-store still alive and no panic in /tmp/store.log, (2) the
leandros-sysbus packagekit service exists and the session log shows a
get-packages transaction (fallback: BackendName == apk over busctl),
(3) /var/lib/swcatalog/xml holds alpinelinux-*-community.xml.gz.
Prints PASS/FAIL per check and `STORESESSION pass=N fail=M`.
Reuses the helpers and conventions of gstsession.py.
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "storesession")
os.environ.setdefault("LEANDROS_VNC_BASE", "5944")
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import gstsession as g  # noqa: E402

driver, drv, log, key, typ, shot = g.driver, g.drv, g.log, g.key, g.typ, g.shot


def main():
    arch, out = sys.argv[1], sys.argv[2]
    A = sys.argv[3:]
    gpu = A[A.index("--gpu") + 1] if "--gpu" in A else "virgl"
    os.makedirs(out, exist_ok=True)
    scale = driver.wait_scale(arch)
    steps = {}
    log(f"storesession arch={arch} gpu={gpu} scale={scale}")
    for what, src, var in (("boot", f"leandros-limine-{arch}.img", "LEANDROS_DISK_IMAGE"),
                           ("root", f"f2fs-data0-{arch}.img", "LEANDROS_ROOT_IMAGE")):
        img = g.clone_path(arch, what)
        try:
            os.unlink(img)
        except OSError:
            pass
        src = os.path.join(g.REPO, src)
        if subprocess.run(["cp", "-c", src, img], capture_output=True).returncode != 0:
            subprocess.run(["cp", src, img], check=True)
        os.environ[var] = img
    if drv("start", arch, f"--{gpu}", t=900) != 0:
        drv("stop", t=60)
        sys.exit("boot failed")
    if drv("login", "root", "root", str(int(60 * scale)), t=900) != 0:
        sys.exit("serial login failed")
    ser = driver._Serial()

    def sh(c, t=60):
        return driver.serial_run(c, timeout=t * scale, arch=arch, ser=ser)["output"]

    argv = 'cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\\0" "\\n"'

    def have(name):
        return any(os.path.basename(l.strip()).startswith(name) for l in sh(argv, 90).splitlines())

    def wait_for(name, pred, timeout):
        t = time.time()
        while time.time() - t < timeout * scale:
            if pred():
                steps[name] = round(time.time() - t, 1)
                log(f"{name}: after {steps[name]} s")
                return True
            ser.pump(4)
        steps[name] = None
        log(f"{name}: NOT ready after {timeout * scale:.0f} s")
        return False

    wait_for("greeter", lambda: have("cosmic-greeter"), 150)
    ser.pump(4 * scale)
    typ("leandro")
    key("ret")
    wait_for("panel", lambda: have("cosmic-panel"), 150)
    ser.pump(10 * scale)
    sh("P=$(for d in /proc/[0-9]*; do [ \"$(cat $d/comm 2>/dev/null)\" = cosmic-panel ] && echo ${d#/proc/}; done | head -1); "
       "tr '\\0' '\\n' < /proc/$P/environ | grep -E '^(XDG_RUNTIME_DIR|HOME|USER|DBUS_SESSION|XDG_SESSION|LANG)' > /tmp/env.txt", 60)
    ENV = ("set -a; . /tmp/env.txt 2>/dev/null; set +a; export WAYLAND_DISPLAY=wayland-1; unset WAYLAND_SOCKET; "
           "export PATH=$PATH:/usr/bin; ")
    log("start: " + sh(ENV + "(cosmic-store > /tmp/store.log 2>&1 & echo $! > /tmp/store.pid); true", 30))
    ser.pump(60 * scale)

    PID = ('for p in $(ls /proc | grep -E "^[0-9]+$"); do c=$(cat /proc/$p/comm 2>/dev/null); '
           '[ "$c" = cosmic-store ] && echo $p; done | head -n 1')
    SYSBUS = ('for p in $(ls /proc | grep -E "^[0-9]+$"); do [ "$(cat /proc/$p/comm 2>/dev/null)" = leandros-sysbus ] '
              '&& tr "\\0" " " < /proc/$p/cmdline | grep -q packagekit && echo $p; done | head -n 1')
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        steps[name] = ok
        log(f"{'PASS' if ok else 'FAIL'} {name} {detail}".rstrip())

    r = sh("P=$(" + PID + "); echo ALIVE=${P:-none}-END; grep -ci panicked /tmp/store.log; tail -n 3 /tmp/store.log | cut -c1-160", 60)
    m = re.search(r"ALIVE=(\w+)-END\s+(\d+)", r)
    check("store_alive", bool(m) and m.group(1) != "none", r[-160:].replace("\n", "|") if not m or m.group(1) == "none" else "")
    check("store_no_panic", bool(m) and m.group(2) == "0")

    pk = sh("echo PKPID=$(" + SYSBUS + ")-END", 60)
    mp = re.search(r"PKPID=(\d*)-END", pk)
    check("packagekit_service", bool(mp) and mp.group(1) != "")
    slog = g.session_logs(sh)
    served = bool(re.search(r"leandros-sysbus\[packagekit\]: tx \d+ role=get-packages.*result=ok", slog))
    detail = "log"
    if not served:
        b = sh(g_env_busctl(ENV), 60)
        served = "apk" in b
        detail = "busctl BackendName: " + b[-80:].replace("\n", "|")
    check("packagekit_get_packages", served, detail)

    cat = sh("ls /var/lib/swcatalog/xml 2>&1", 30)
    check("appstream_staged", bool(re.search(r"alpinelinux-.*-community\.xml\.gz", cat)), cat[-100:].replace("\n", "|"))

    npass, nfail = sum(results), len(results) - sum(results)
    if nfail:
        shot(f"{out}/store-fail.ppm")
    print(f"STORESESSION pass={npass} fail={nfail}", flush=True)
    json.dump(steps, open(f"{out}/steps.json", "w"), indent=1)
    open(f"{out}/serial-live.log", "w").write(ser.text() if hasattr(ser, "text") else "")
    if "--keep" not in A:
        drv("stop", t=60)
    sys.exit(1 if nfail else 0)


def g_env_busctl(env):
    return (env + "busctl --system call org.freedesktop.PackageKit /org/freedesktop/PackageKit "
            "org.freedesktop.DBus.Properties Get ss org.freedesktop.PackageKit BackendName 2>&1")


if __name__ == "__main__":
    main()
