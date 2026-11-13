#!/usr/bin/env python3
"""kexecblktest.py -- M6 gate (kmod W2-3): a kexec'd kernel mounts root with VIRTIO_BLK=m.

The kernel is built CONFIG_MODULES=y CONFIG_VIRTIO_BLK=m (scripts/kmod/kexecblktest.sh), root is on
virtio-blk, so BOTH kernels only see the disk once the virtio_blk module (modules/BOOT) is loaded
from the initrd. gen0 kexecs into the same kernel + initrd; the serial login that follows only works
if gen1 found and mounted root. Asserts the module load line appears in both generations.
The VM ends in the kexec'd kernel: run alone (runtests kexecblk).
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

REPO = driver.REPO_ROOT
PUSH = [sys.executable, os.path.join(REPO, "scripts/pi-serial-push.py"), "--port", "unix:" + driver.SERIAL_SOCK]
ENV = dict(os.environ, LEANDROS_PI_SERIAL="unix:" + driver.SERIAL_SOCK, LEANDROS_PI_LOGIN="root:root")
KX = "/bin/kexec -l /boot/kexec --initrd=/boot/kexec-initrd"


def guest(cmd, timeout=120):
    r = subprocess.run(PUSH + ["--timeout", str(timeout), "--cmd", cmd], capture_output=True, text=True, env=ENV)
    return r.returncode, (r.stdout + r.stderr)


def kexec_exec(extra):
    r = subprocess.run(PUSH + ["--timeout", "240", "--kexec-exec", extra], capture_output=True, text=True, env=ENV)
    return r.returncode


def log_from(mark):
    with open(driver.SERIAL_FULL_LOG, "rb") as f:
        f.seek(mark)
        return f.read().decode("latin-1")


def main():
    fails = []

    def check(name, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {name} {detail}", flush=True)
        if not ok:
            fails.append(name)

    rc, out = guest("cat /proc/modules")
    if "virtio_blk" not in out:
        # The stock build has VIRTIO_BLK=y; this gate only means something on the =m kernel that
        # scripts/kmod/kexecblktest.sh builds. Do not report a stock-kernel run as a failure.
        print("RESULT: SKIP (virtio_blk is built in; run scripts/kmod/kexecblktest.sh for the =m kernel)")
        return 0
    check("gen0 virtio_blk is a module", True)
    rc, out = guest(f"{KX} --reuse-cmdline --append='kexec.gen=1'")
    check("gen0 load", rc == 0, out.strip()[-80:])
    m = os.path.getsize(driver.SERIAL_FULL_LOG)
    check("gen0 -> gen1", kexec_exec("kexec.gen=1") == 0)
    lg = log_from(m)
    check("gen1 loaded virtio_blk from the initrd", re.search(r"\[kmod\] (modules\.load )?virtio_blk loaded", lg) is not None)
    rc, out = guest("cat /proc/modules; grep ' / ' /proc/mounts; echo GEN1ROOT")
    check("gen1 root mounted (shell works)", rc == 0 and "GEN1ROOT" in out, out.strip()[-120:])
    check("gen1 lists virtio_blk", "virtio_blk" in out)
    print("RESULT:", "PASS" if not fails else "FAIL " + ", ".join(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
