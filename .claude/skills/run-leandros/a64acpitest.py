#!/usr/bin/env python3
"""a64acpitest.py [--keep] -- aarch64 ACPI platform path (lane a64acpi).

Boots the UEFI/Limine image with `acpi=force` (the firmware's DTB is ignored)
and checks the platform came from ACPI tables, not from QEMU-virt constants:

  madt     `[ACPI] MADT gicd=0x8000000 ... cpus=4` and GICv3 redistributors
  spcr     `[ACPI] SPCR uart=0x9000000 pl011`
  gtdt     `[ACPI] GTDT virt=0x1b`
  mcfg     `[ACPI] MCFG segments=...`
  fadt     `[ACPI] FADT PSCI conduit`
  smp      4 CPUs online (/proc/cpuinfo)
  pci      root disk found through MCFG ECAM (/dev/vda exists)
Exit 0 iff all PASS. Fail-first: before lane a64acpi none of the [ACPI] lines
exist and `acpi=force` is an unknown knob.
"""
import os
import imgclone
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")


def main(argv):
    keep = "--keep" in argv
    arch = "aarch64"
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="a64acpi-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, "acpi=force"], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"a64acpitest: could not set the command line: {r.stdout}{r.stderr}")
        return 1
    run_id = f"{caller}-a64acpi" if caller else "a64acpi"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root,
                "LEANDROS_NET": "user", "LEANDROS_SMP": "4"})
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"a64acpi_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    log = ""
    try:
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"a64acpitest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        booted = r.returncode == 0 and lg.returncode == 0
        try:
            log = open(driver.SERIAL_FULL_LOG, errors="replace").read()
        except OSError:
            log = ""

        def line(pat):
            m = re.search(pat, log, re.I)
            return m.group(0) if m else ""

        l = line(r"\[ACPI\] MADT gicd=0x(?:0x)?0*8000000.*")
        verdict("madt", bool(l) and re.search(r"cpus=0x0*4\b", l) is not None, l[:100])
        l = line(r"\[ACPI\] SPCR uart=0x(?:0x)?0*9000000 pl011.*")
        verdict("spcr", bool(l), l[:100])
        l = line(r"\[ACPI\] GTDT virt=0x0*1b.*")
        verdict("gtdt", bool(l), l[:100])
        l = line(r"\[ACPI\] MCFG segments=.*")
        verdict("mcfg", bool(l), l[:100])
        l = line(r"\[ACPI\] FADT PSCI conduit \w+")
        verdict("fadt", bool(l), l[:100])
        if booted:
            _, out = sh("grep -c ^processor /proc/cpuinfo", 30)
            verdict("smp", out.strip().splitlines()[-1:] == ["4"], out.strip()[-20:])
            rc, out = sh("test -b /dev/vda && echo vda-ok", 30)
            verdict("pci", "vda-ok" in out, out.strip()[-30:])
        else:
            verdict("smp", False, "no login")
            verdict("pci", False, "no login")
    finally:
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/a64acpi-{arch}-serial.log")
        except Exception:  # noqa: BLE001
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"a64acpitest: {'PASS' if ok else 'FAIL'} {sum(results.values())}/{len(results)} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
