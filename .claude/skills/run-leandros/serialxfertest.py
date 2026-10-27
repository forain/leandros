#!/usr/bin/env python3
"""serialxfertest.py -- serial-console file push + kexec (scripts/pi-serial-*).

Both ends are real code: scripts/pi-serial-push.py on the host talks to the
guest shell over QEMU's serial socket, /bin/serrx receives. Cases:
  1. bootstrap: a fresh receiver path is typed in as base64 (head | base64 -d)
     and then used for a 64 KiB random file;
  2. the 5.6 MB kernel + initrd pushed into /boot/pi/trial, kexec'd with
     --reuse-cmdline + extras, and the new kernel's /proc/cmdline checked.
The VM ends up in the kexec'd kernel: run this test alone (runtests serialxfer).
"""
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

REPO = driver.REPO_ROOT
PUSH = [sys.executable, os.path.join(REPO, "scripts/pi-serial-push.py"), "--port", "unix:" + driver.SERIAL_SOCK]


def run(cmd, env=None, timeout=1800):
    t0 = time.time()
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=timeout)
    tail = (r.stdout + r.stderr).replace("\r", "\n").strip().splitlines()[-4:]
    print(f"  [{time.time() - t0:.0f}s rc={r.returncode}] " + " | ".join(tail), flush=True)
    return r.returncode


def main():
    fails = []
    blob = os.path.join("/tmp", f"serialxfer-{os.getpid()}.bin")
    with open(blob, "wb") as f:
        f.write(os.urandom(65536))
    print("case bootstrap+push 64 KiB", flush=True)
    if run(PUSH + ["--serrx", "/tmp/serrx-boot", blob, "/tmp/serialxfer.bin"]) != 0:
        fails.append("bootstrap push")
    os.unlink(blob)
    print("case kexec flow (kernel-direct + initrd -> /boot/pi/trial)", flush=True)
    env = dict(os.environ, LEANDROS_PI_SERIAL="unix:" + driver.SERIAL_SOCK, LEANDROS_PI_LOGIN="root:root",
               LEANDROS_SERRX="/tmp/serrx-boot")
    if run([os.path.join(REPO, "scripts/pi-serial-kexec.sh"),
            os.path.join(REPO, "target/final-aarch64/kernel-direct"),
            os.path.join(REPO, "initrd-aarch64.cpio"), "serialxfer=1"], env=env) != 0:
        fails.append("kexec flow")
    print("RESULT:", "PASS" if not fails else "FAIL " + ", ".join(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
