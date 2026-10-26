#!/usr/bin/env python3
"""Many virtio-blk data disks (lane/blkmany, 2026-10-25, openbugs #83).

Boots with N (default 12) extra virtio-blk disks on top of the default
config's three (vda root, vdb, vdc home), each a small f2fs volume, then
checks every one shows up in /dev and mounts (by device
path, one at a time). Until 2026-10-25 virtio_blk::probe_all() stopped at 8
devices without a word, so with 8 extras the last three never appeared.

Usage: LEANDROS_RUN_ID=<lane> python3 blkmanytest.py [aarch64|x86_64] [N]
"""
import os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MKFS = os.path.join(REPO, "scripts", "mkfs-f2fs-home-lx.py")
BASE_DISKS = 3  # vda, vdb, vdc in the default config


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def vdname(i):
    return "vd" + chr(ord("a") + i)


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    rid = os.environ.get("LEANDROS_RUN_ID", "x")
    tmp = os.path.join(REPO, f"blkmany-{rid}")
    os.makedirs(tmp, exist_ok=True)
    base = os.path.join(tmp, "base.img")
    subprocess.run([sys.executable, MKFS, "create", base, "--size", "64M"],
                   check=True, capture_output=True, cwd=REPO)
    extra = []
    for i in range(n):
        img = os.path.join(tmp, f"d{i}.img")
        if subprocess.run(["cp", "-c", base, img], capture_output=True).returncode != 0:
            shutil.copyfile(base, img)
        extra.append(f"-drive if=none,id=bm{i},format=raw,file={img} "
                     f"-device virtio-blk-pci,drive=bm{i},disable-legacy=on")
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = " ".join(extra)
    # x86's base disks are transitional (legacy 0x1001) and the kernel probes
    # modern (0x1042) virtio-blk first, so there these disable-legacy=on
    # extras are vda.. and the base disks follow them. The root is found by
    # root=LABEL= either way (lane/rootpick); only the names differ.
    first = 0 if arch == "x86_64" else BASE_DISKS
    names = [vdname(first + i) for i in range(n)]
    ok = True
    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        rc, out = drv("run", "ls /dev | tr '\\n' ' '", "30")
        parts = set(re.findall(r"\b(vd[a-z]+)\b", out))
        missing = [d for d in names if d not in parts]
        print(f"/dev: {len(parts)} vd nodes; missing {missing or 'none'}")
        if missing: ok = False
        script = ("mkdir -p /mnt/bm; for d in " + " ".join(names) +
                  "; do if mount -t f2fs -o ro /dev/$d /mnt/bm; then echo MOK_$d; umount /mnt/bm; "
                  "else echo MFAIL_$d; fi; done; echo MDONE")
        rc, out = drv("run", script, "240", timeout=300)
        good = re.findall(r"MOK_(vd[a-z]+)", out)
        bad = [d for d in names if d not in good]
        print(f"mounted {len(good)}/{n}; failed {bad or 'none'}")
        if bad or "MDONE" not in out:
            ok = False; print(out.strip()[-400:])
        _, log = drv("log", timeout=30)
        hit = [l for l in log.splitlines() if "VIRTIO-BLK" in l and ("limit" in l or "skip" in l)]
        if hit: print("\n".join(hit[-3:]))
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"PASS blkmany N={n}" if ok else f"FAIL blkmany N={n}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
