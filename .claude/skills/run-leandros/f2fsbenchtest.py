#!/usr/bin/env python3
"""f2fsbenchtest.py [aarch64|x86_64] [--n N] [--cmdline "k=v ..."] [--size 64M] [--keep]

fsync microbenchmark + mount time on a fresh f2fs volume (lane f2fslx). Boots
once with --cmdline (e.g. `f2fs.lxchain=1`), mounts the volume (timed with
`date +%s%N`), runs `f2fstest fsyncbench` (N rounds of append+fsync and
create+write+fsync), umounts, mounts again (timed) and prints
`RESULT mount_ms=.. remount_ms=.. us_per_fsync=..`. Exit 0 iff the bench ran.
"""
import os
import re
import imgclone
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



def stop_caller_vm(caller):
    """Under `runtests host:` the caller's VM still holds the write lock on the
    root image, so this test's own QEMU would exit at once ("Failed to get
    write lock", seen as "shell prompt not seen"). Stop it first."""
    if caller:
        subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "driver.py"), "stop"],
                       env=dict(os.environ, LEANDROS_RUN_ID=caller), capture_output=True, timeout=120)


def main(argv):
    arch, factor, cmdline, size, keep = os.environ.get("LEANDROS_TEST_ARCH", "aarch64"), 300, None, "64M", False
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--n":
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
    stop_caller_vm(caller)
    run_id = f"{caller}-f2fsbench" if caller else "f2fsbench"
    out = os.environ.get("F2FSBENCH_OUT", f"/tmp/f2fsbench-{arch}{('-' + caller) if caller else ''}")
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
            imgclone.clone_file(src, boot_copy)
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
        MT = "s=$(date +%s%N); mount -t f2fs LABEL={l} /mnt/gc; e=$(date +%s%N); echo MOUNT_NS $((e-s))"
        ok, o = run("mkdir -p /mnt/gc; " + MT.format(l=LABEL) + "; mkdir -p /mnt/gc/t", 120)
        m1 = re.search(r"MOUNT_NS (\d+)", o)
        verdict("mount", ok and bool(m1))
        if ok:
            ok, o = run(f"/bin/f2fstest fsyncbench /mnt/gc/t {factor}", 3000)
            m2 = re.search(r"us_per_fsync=(\d+)", o)
            verdict("bench", ok and bool(m2))
            ok2, o = run("umount /mnt/gc; " + MT.format(l=LABEL), 300)
            m3 = re.search(r"MOUNT_NS (\d+)", o)
            run("umount /mnt/gc", 120)
            if m1 and m2 and m3:
                print(f"RESULT cmdline={cmdline!r} arch={arch} n={factor} mount_ms={int(m1.group(1))//1000000} "
                      f"remount_ms={int(m3.group(1))//1000000} us_per_fsync={m2.group(1)}", flush=True)
        lg = drv("log", timeout=60).stdout
        for line in lg.splitlines():
            if "[F2FS]" in line and ("SSR" in line or "space" in line or "SIT" in line or "out of" in line):
                print("  log:", line.strip()[:200])
    finally:
        if not keep:
            drv("stop", timeout=120)
    allok = all(results.values()) and "bench" in results
    print(f"f2fsbenchtest: {'PASS' if allok else 'FAIL'}")
    return 0 if allok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
