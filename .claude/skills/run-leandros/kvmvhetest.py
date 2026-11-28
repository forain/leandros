#!/usr/bin/env python3
"""kvmvhetest.py -- the kernel stays at EL2 with VHE under virtualization=on (lane kvm-vhe, aarch64).

kvm-plan.md section 6 item 1: boot with QEMU `virt,virtualization=on` (EL2 + VHE,
LEANDROS_VIRT=1 in driver.py, -cpu cortex-a76) on BOTH entry paths and check the
boot line "[el] running at EL2 (VHE)":
  1. Limine (UEFI) path: Limine base revision 6 enters at EL2 with E2H=1 and the MMU
     on; also runs the `elcheck` ktest (CurrentEL, HCR E2H/TGE, host tick = INTID 26).
  2. direct path (-kernel ELF, MMU off at EL2): the entry stub itself sets E2H/TGE.
Needs a kernel built with CONFIG_KVM=y (cargo feature drv-kvm, default OFF):
  ./scripts/build-all.sh with KVM=y in .config. A default kernel prints no [el]
  line and the test FAILs with that hint. SKIPs under HVF/KVM (no nested EL2).
Not part of the default regress. Usage: LEANDROS_RUN_ID=<lane> python3 kvmvhetest.py
"""
import os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
import imgclone  # noqa: E402
REPO = driver.REPO_ROOT
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
LINE = "[el] running at EL2 (VHE)"


def drv(*args, timeout=300, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True, timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    os.environ["LEANDROS_VIRT"] = "1"
    if not driver._virt_on(driver._accel_flags("aarch64", "uefi")):
        print("kvmvhetest: SKIP (accelerated host: no nested EL2, A76 has no FEAT_NV)")
        return 0
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="kvmvhe-")
    fails = []

    def check(name, cond, detail=""):
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" ({detail})" if detail and not cond else ""))
        if not cond:
            fails.append(name)

    def boot(tag, mode, ktest):
        env = dict(os.environ, LEANDROS_VIRT="1", LEANDROS_RUN_ID=f"{caller}-kvmvhe{tag}" if caller else f"kvmvhe{tag}")
        args = ["start", "aarch64", mode]
        if mode == "uefi":
            img = os.path.join(tmp, f"boot-{tag}.img")
            root = os.path.join(tmp, f"root-{tag}.img")
            for src, dst in ((os.path.join(REPO, "leandros-limine-aarch64.img"), img),
                             (os.path.join(REPO, "f2fs-data0-aarch64.img"), root)):
                if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
                    imgclone.clone_file(src, dst)
            if ktest:
                r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, "ktest=elcheck"],
                                   capture_output=True, text=True)
                if r.returncode != 0:
                    return None, r.stdout + r.stderr
            env.update(LEANDROS_DISK_IMAGE=img, LEANDROS_ROOT_IMAGE=root)
        rc, out = drv(*args, env=env)
        if rc != 0:
            return None, out[-300:]
        try:
            deadline = time.time() + 240
            log = ""
            while time.time() < deadline:
                _, log = drv("log", timeout=60, env=env)
                if LINE in log or "[el] running at EL" in log:
                    if not ktest or "elcheck PASS" in log or "elcheck FAIL" in log:
                        break
                time.sleep(5)
            return log, ""
        finally:
            try: drv("stop", timeout=60, env=env)
            except Exception: pass

    try:
        for tag, mode, kt in (("a", "uefi", True), ("b", "direct", False)):
            log, err = boot(tag, mode, kt)
            check(f"{mode}: boots with virtualization=on", log is not None, err)
            if log is None:
                continue
            check(f"{mode}: boot line '{LINE}'", LINE in log,
                  "no [el] line: kernel not built with CONFIG_KVM=y?" if "[el]" not in log else "wrong EL line")
            if kt:
                check(f"{mode}: ktest elcheck PASS", "elcheck PASS" in log, "see [KTEST] elcheck lines")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"kvmvhetest: {'PASS' if not fails else 'FAIL ' + ', '.join(fails)}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
