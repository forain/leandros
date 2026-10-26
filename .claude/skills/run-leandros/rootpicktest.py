#!/usr/bin/env python3
"""The root filesystem must not depend on virtio probe order (lane/rootpick,
2026-10-26, openbugs #93).

Boots with one extra 64M f2fs disk added as a MODERN virtio-blk device
(disable-legacy=on). On x86_64 the base disks are transitional (legacy
0x1001) and the kernel probes modern (0x1042) devices first, so the extra
becomes vda and every base disk shifts by one. Before root=LABEL= the kernel
guessed "/dev/root -> vdb" (= the boot ESP here) and the boot failed; the
QEMU boot image now passes root=LABEL=leandros-root. Checks:
  * login works and /bin/login exists on the mounted root;
  * the kernel log resolves /dev/root to the root image's disk (vdc on
    x86_64 where the extra is vda; vdb on aarch64 where it comes last);
  * the kernel log shows the root= hint, not the no-hint warning.
To see it fail the old way, point LEANDROS_DISK_IMAGE at a boot image copy
whose limine.conf has no cmdline line.
Usage: LEANDROS_RUN_ID=<lane> python3 rootpicktest.py [aarch64|x86_64]
"""
import os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MKFS = os.path.join(REPO, "scripts", "mkfs-f2fs-home-lx.py")


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    rid = os.environ.get("LEANDROS_RUN_ID", "x")
    tmp = os.path.join(REPO, f"rootpick-{rid}")
    os.makedirs(tmp, exist_ok=True)
    img = os.path.join(tmp, "extra.img")
    subprocess.run([sys.executable, MKFS, "create", img, "--size", "64M"],
                   check=True, capture_output=True, cwd=REPO)
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (f"-drive if=none,id=rp0,format=raw,file={img} "
                                  f"-device virtio-blk-pci,drive=rp0,disable-legacy=on")
    want = "vdc" if arch == "x86_64" else "vdb"
    fails = []

    def check(name, cond, detail=""):
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" ({detail})" if detail and not cond else ""))
        if not cond:
            fails.append(name)

    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        check("boot", rc == 0, out[-300:])
        if rc == 0:
            rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
            check("login", rc == 0, out[-300:])
        if rc == 0:
            rc, out = drv("run", "test -x /bin/login && echo ROOT_OK", "60", timeout=120)
            check("root has /bin/login", "ROOT_OK" in out, out[-200:])
        _, log = drv("log", timeout=60)
        m = re.search(r"/dev/root -> (\w+)", log)
        got = m.group(1) if m else None
        check(f"/dev/root -> {want}", got == want, f"got {got}")
        check("root= hint on the cmdline", "root=LABEL=leandros-root" in log)
        check("no no-hint warning", "no root= on the kernel command line" not in log)
    finally:
        try: drv("stop", timeout=60)
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"rootpicktest {arch}: {'PASS' if not fails else 'FAIL ' + ', '.join(fails)}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
