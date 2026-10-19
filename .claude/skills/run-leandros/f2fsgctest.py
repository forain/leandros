#!/usr/bin/env python3
"""f2fsgctest.py [aarch64|x86_64] [--factor N] [--cmdline "k=v ..."] [--size 64M] [--keep]

Space reuse on a small f2fs volume (lane f2fsgc), in its own boot (run id
`<caller>-f2fsgc`):

  mkfs     a fresh 64 MiB volume (scripts/mkfs-f2fs-home.py, relabelled
           `f2fsgc-test` so fstab's /home label stays unique), attached as an
           extra virtio disk
  churn    `mount LABEL=f2fsgc-test /mnt/gc`, then `f2fstest churn /mnt/gc/t N`:
           writes N (default 8) x the capacity through interleaved
           create/delete/shrink/rewrite with live data <= 65 %; every write must
           succeed and every file verifies
  remount  umount, mount again, `f2fstest verify /mnt/gc/t`, umount
  fsck     after QEMU exits: scripts/fsck-f2fs.py --churn /t --strict-leaks on
           the image (structure, SIT vs. reachable blocks, every file's content)

--cmdline boots a copy of the boot image with that kernel command line (e.g.
`f2fs.ssr=0`, the pre-f2fsgc allocator, which must FAIL churn with ENOSPC).
Exit 0 iff all PASS.
"""
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = os.path.join(HERE, "driver.py")
LABEL = "f2fsgc-test"


def relabel(img):
    name = LABEL.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))


def main(argv):
    arch, factor, cmdline, size, keep = "aarch64", 8, None, "64M", False
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--factor":
            factor = int(argv[i + 1]); i += 1
        elif a == "--cmdline":
            cmdline = argv[i + 1]; i += 1
        elif a == "--size":
            size = argv[i + 1]; i += 1
        elif a == "--keep":
            keep = True
        else:
            sys.exit(__doc__)
        i += 1
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-f2fsgc" if caller else "f2fsgc"
    out = os.environ.get("F2FSGC_OUT", f"/tmp/f2fsgc-{arch}")
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "gc.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", size], check=True, stdout=subprocess.DEVNULL)
    relabel(img)

    env = dict(os.environ, LEANDROS_RUN_ID=run_id)
    pci = "virtio-blk-pci,drive=gcdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    extra = f"-drive if=none,id=gcdisk,format=raw,file={img} -device {pci}"
    env["LEANDROS_QEMU_EXTRA"] = (env.get("LEANDROS_QEMU_EXTRA", "") + " " + extra).strip()
    boot_copy = None
    if cmdline is not None:
        boot_copy = os.path.join(out, f"boot-{arch}.img")
        src = os.path.join(REPO, f"leandros-limine-{arch}.img")
        subprocess.run(["cp", "-c", src, boot_copy], check=False)
        if not os.path.exists(boot_copy):
            shutil.copyfile(src, boot_copy)
        subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot_copy, cmdline], check=True)
        env["LEANDROS_DISK_IMAGE"] = boot_copy

    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def drv(*args, timeout=600):
        return subprocess.run([sys.executable, DRV, *args], env=env, capture_output=True, text=True,
                              timeout=timeout)

    def run(cmd, timeout):
        r = drv("run", cmd, str(timeout), arch, timeout=timeout + 120)
        tail = "\n".join(r.stdout.strip().splitlines()[-12:])
        print(f"--- {cmd}\n{tail}", flush=True)
        return r.returncode == 0, r.stdout

    drv("stop", timeout=60)
    st = drv("start", arch, timeout=600)
    if st.returncode != 0:
        print(st.stdout[-2000:], st.stderr[-2000:])
        verdict("boot", False)
        return 1
    try:
        if drv("login", "root", "root", "180", timeout=400).returncode != 0:
            verdict("boot", False, "(no root login)")
            return 1
        ok, _ = run(f"mkdir -p /mnt/gc && mount -t f2fs LABEL={LABEL} /mnt/gc && mkdir -p /mnt/gc/t", 60)
        verdict("mount", ok)
        if ok:
            ok, o = run(f"/bin/f2fstest churn /mnt/gc/t {factor}", 3000)
            verdict("churn", ok and "churn: PASS" in o)
            ok2, o = run(f"umount /mnt/gc && mount -t f2fs LABEL={LABEL} /mnt/gc && "
                         f"/bin/f2fstest verify /mnt/gc/t", 600)
            verdict("remount", ok2 and "verify: PASS" in o)
            ok3, _ = run("umount /mnt/gc", 120)
            verdict("umount", ok3)
        lg = drv("log", timeout=60).stdout
        for line in lg.splitlines():
            if "[F2FS]" in line and ("SSR" in line or "space" in line or "SIT" in line or "out of" in line):
                print("  log:", line.strip()[:200])
    finally:
        if not keep:
            drv("stop", timeout=120)
    if not keep:
        fk = subprocess.run([sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img,
                             "--churn", "/t", "--strict-leaks"], capture_output=True, text=True, timeout=600)
        print(fk.stdout[-3000:].rstrip())
        verdict("fsck", fk.returncode == 0)
        if boot_copy and os.path.exists(boot_copy):
            os.unlink(boot_copy)
    allok = all(results.values()) and "churn" in results
    print(f"f2fsgctest: {'PASS' if allok else 'FAIL'}")
    return 0 if allok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
