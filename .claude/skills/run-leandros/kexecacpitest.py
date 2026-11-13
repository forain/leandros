#!/usr/bin/env python3
"""kexecacpitest.py -- ACPI tables in firmware-Reserved RAM above 2 GiB survive kexec (x86_64).

Own boot: OVMF q35 with max-ram-below-4g=3G and 6 GiB, which puts OVMF's ACPI
tables above 2 GiB. gen0 loads a kernel with kexec.test=acpiresvd, so the
next kernel's memory map carries those tables as raw type 8 (plain Reserved
for the direct-boot kernel: not in the HHDM). Before the fix gen1 faults or
finds no MADT (1 CPU); after, gen1 logs "mapped N ACPI page(s)" and has all
CPUs. KEXECACPI_NOFIX=1 adds direct.acpimap=0 (the guard must FAIL).
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ["LEANDROS_QEMU_MEM"] = "6G"
os.environ["LEANDROS_QEMU_EXTRA"] = "-machine q35,max-ram-below-4g=3G"
import driver  # noqa: E402

REPO = driver.REPO_ROOT
PUSH = [sys.executable, os.path.join(REPO, "scripts/pi-serial-push.py"), "--port", "unix:" + driver.SERIAL_SOCK]
ENV = dict(os.environ, LEANDROS_PI_SERIAL="unix:" + driver.SERIAL_SOCK, LEANDROS_PI_LOGIN="root:root")
KX = "/bin/kexec -l /boot/kexec --initrd=/boot/kexec-initrd"
NOFIX = " direct.acpimap=0" if os.environ.get("KEXECACPI_NOFIX") == "1" else ""
DRV = [sys.executable, os.path.join(HERE, "driver.py")]


def drv(*a, timeout=300):
    p = subprocess.run(DRV + list(a), capture_output=True, text=True, timeout=timeout, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def guest(cmd, timeout=120):
    r = subprocess.run(PUSH + ["--timeout", str(timeout), "--cmd", cmd], capture_output=True, text=True, env=ENV)
    return r.returncode, r.stdout + r.stderr


def main():
    if "x86_64" not in (sys.argv[1] if len(sys.argv) > 1 else "x86_64"):
        print("RESULT: SKIP (x86_64 only)")
        return 0
    fails = []

    def check(name, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {name} {detail}", flush=True)
        if not ok:
            fails.append(name)

    try:
        rc, out = drv("start", "x86_64")
        check("boot gen0", rc == 0, out[-200:])
        rc, out = drv("login", "root", "root", "180", timeout=400)
        check("login gen0", rc == 0, out[-200:])
        rc, out = guest("grep -c '^processor' /proc/cpuinfo")
        ncpu = out.split()[-1] if out.split() else "?"
        rc, out = guest(f"{KX} --reuse-cmdline --append='kexec.gen=1 kexec.test=acpiresvd{NOFIX}'")
        check("gen0 load", rc == 0, out.strip()[-80:])
        m = os.path.getsize(driver.SERIAL_FULL_LOG)
        r = subprocess.run(PUSH + ["--timeout", "240", "--kexec-exec", "kexec.gen=1"], capture_output=True, text=True, env=ENV)
        check("gen0 -> gen1", r.returncode == 0, (r.stdout + r.stderr).strip()[-120:])
        with open(driver.SERIAL_FULL_LOG, "rb") as f:
            f.seek(m)
            lg = f.read().decode("latin-1")
        mp = re.search(r"direct HHDM: mapped (0x)?([0-9a-fA-F]+) ACPI page", lg)
        check("gen1 mapped ACPI pages", bool(mp) and int(mp.group(2), 16 if mp.group(1) else 10) > 0, mp.group(0) if mp else "no line")
        rc, out = guest("grep -c '^processor' /proc/cpuinfo")
        check(f"gen1 has {ncpu} CPUs", out.split()[-1:] == [ncpu], out.strip()[-40:])
    finally:
        try:
            drv("stop", timeout=60)
        except Exception:
            pass
    print("RESULT:", "PASS" if not fails else "FAIL " + ", ".join(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
