"""PCI devices behind PCI-to-PCI bridges (lane/dtbres, 2026-10-23).
Boots with a PCIe root port -> PCI-PCI bridge -> virtio-rng-pci chain added
to QEMU. Until 2026-10-23 aarch64 enumerated bus 0 only (and mapped only the
bus-0 ECAM slice), so the rng behind two bridges was never found. Checks the
serial log for the device on a non-zero bus and that boot reaches login.
Also checks the DTB reserved-range summary line is printed, and (no Limine
DTB) that the RAM DTB scan ran before it, i.e. before the allocator.
Usage: LEANDROS_RUN_ID=<lane> python3 pcibridgetest.py [aarch64|x86_64]
"""
import os, re, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
LOG = f"/tmp/leandros{'-' + RUN_ID if RUN_ID else ''}-serial-full.log"


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = (
        "-device pcie-root-port,id=brrp,chassis=21,slot=21 "
        "-device pci-bridge,id=brpb,bus=brrp,chassis_nr=22 "
        "-device virtio-rng-pci,bus=brpb,addr=0x5")
    ok = True
    drv("stop", timeout=120)  # runtests leaves its own VM up; this test boots its own
    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        log = open(LOG, "rb").read().decode("utf-8", "replace")
        found = re.findall(r"\[PCI\] Found dev 0x00001AF4:0x0000(?:1005|1044) on bus 0x0*([0-9A-F]+)", log)
        bridges = re.findall(r"\[PCI\] (?:numbering )?bridge -> bus 0x0*([0-9A-F]+)", log)
        print(f"bridges -> buses {bridges}; virtio-rng on buses {found}")
        if not found:
            ok = False; print("FAIL: virtio-rng behind the bridges not enumerated")
        elif int(found[-1], 16) < 2:
            ok = False; print("FAIL: virtio-rng expected two bridges deep (bus >= 2)")
        rsv = [l[l.index("[early] dtb reserved:"):] for l in log.splitlines() if "[early] dtb reserved:" in l]
        print("\n".join(rsv[:8]) or "no dtb reserved line")
        if arch == "aarch64" and not rsv:
            ok = False; print("FAIL: no '[early] dtb reserved:' summary at boot")
        # lane/dtblate: when Limine hands over no DTB, the RAM scan must run
        # before the allocator is seeded, i.e. before the reservation summary.
        if arch == "aarch64" and "DTB not found via Limine" in log:
            i_scan = log.find("[early] dtb ram scan:")
            i_rsv = log.find("[early] dtb reserved:")
            i_mm = log.find("DTB not found via Limine")
            print(log[i_scan:log.find("\n", i_scan)] if i_scan >= 0 else "no dtb ram scan line")
            if not (0 <= i_scan < i_rsv < i_mm):
                ok = False; print("FAIL: RAM DTB scan did not run before the early reservation step")
    finally:
        drv("stop", timeout=60)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
