#!/usr/bin/env python3
"""f2fsnidtest.py [aarch64|x86_64] [--keep] [--cmdline "f2fs.nidscan=0"]
(--cmdline f2fs.nidscan=0 = the old allocator: must FAIL)

Stale checkpoint next_free_nid (lane f2fsnid). A fresh 64 MiB volume is filled
with 400 files (manifest of sha256 sums on the volume), unmounted; the host
then rewrites the newest checkpoint's next_free_nid to 100, far BELOW the live
nids (what a block-copied live volume / in-place NAT writeback leaves). Second
boot: mount cleanly, create 3000 symlinks + 300 files, `sha256sum -c` the
manifest: every pre-existing file must be unchanged (before the fix, new
symlinks reused live nids and overwrote them). Then fsck-f2fs.py. Exit 0 iff PASS.
"""
import os
import hostclean
import shutil
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = os.path.join(HERE, "driver.py")
LABEL = "f2fsnid-test"
sys.path.insert(0, HERE)
import importlib.util
import f2fsgctest as gc


def load_fsck():
    sys.path.insert(0, os.path.join(REPO, "scripts"))
    spec = importlib.util.spec_from_file_location("fsckf2fs", os.path.join(REPO, "scripts", "fsck-f2fs.py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def patch_cp(img, value):
    fk = load_fsck()
    v = fk.Vol(img)
    with open(img, "r+b") as f:
        f.seek(v.cp_addr * 4096)
        cp = bytearray(f.read(4096))
        old = struct.unpack_from("<I", cp, 152)[0]
        struct.pack_into("<I", cp, 152, value)
        if struct.unpack_from("<I", cp, 164)[0] == 4092:
            crc = fk.linux_crc(bytes(cp[:4092]))
        else:
            crc = zlib.crc32(bytes(cp[:4092]))
        struct.pack_into("<I", cp, 4092, crc)
        f.seek(v.cp_addr * 4096)
        f.write(cp)
    return old


def cursor_state(img):
    """(checkpoint next_free_nid, highest live NAT nid) of the newest checkpoint."""
    fk = load_fsck()
    v = fk.Vol(img)
    cpb = v.blk(v.cp_addr)
    cur = struct.unpack_from("<I", cpb, 152)[0]
    top = 0
    for b in range(v.seg_cnt_nat // 2 * v.bps):
        blk = v.blk(v.nat_blk + b)
        for i in range(NAT_PER_BLK_T):
            if struct.unpack_from("<I", blk, i * 9 + 5)[0] != 0:
                top = b * NAT_PER_BLK_T + i
    return cur, top


NAT_PER_BLK_T = 455


def main(argv):
    cmdline = None
    if "--cmdline" in argv:
        k = argv.index("--cmdline"); cmdline = argv[k + 1]; del argv[k:k + 2]
    arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    keep = False
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
        else:
            sys.exit(__doc__)
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    gc.stop_caller_vm(caller)
    run_id = f"{caller}-f2fsnid" if caller else "f2fsnid"
    out = os.environ.get("F2FSNID_OUT", f"/tmp/f2fsnid-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    hostclean.track(out, glob="/tmp/f2fsnid-*")
    img = os.path.join(out, "nid.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    gc.LABEL = LABEL
    gc.relabel(img)
    env = dict(os.environ, LEANDROS_RUN_ID=run_id)
    pci = "virtio-blk-pci,drive=niddisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    env["LEANDROS_QEMU_EXTRA"] = (env.get("LEANDROS_QEMU_EXTRA", "") +
                                  f" -drive if=none,id=niddisk,format=raw,file={img} -device {pci}").strip()
    if cmdline is not None:
        bc = os.path.join(out, f"boot-{arch}.img")
        src = os.path.join(REPO, f"leandros-limine-{arch}.img")
        subprocess.run(["cp", "-c", src, bc], check=False)
        if not os.path.exists(bc):
            shutil.copyfile(src, bc)
        subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), bc, cmdline], check=True)
        env["LEANDROS_DISK_IMAGE"] = bc
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def drv(*a, timeout=600):
        return subprocess.run([sys.executable, DRV, *a], env=env, capture_output=True, text=True, timeout=timeout)

    def run(cmd, timeout):
        r = drv("run", cmd, str(timeout), arch, timeout=timeout + 120)
        print(f"--- {cmd[:60]}\n" + "\n".join(r.stdout.strip().splitlines()[-6:]), flush=True)
        return r.returncode == 0, r.stdout

    def boot():
        drv("stop", timeout=60)
        for attempt in (1, 2):
            r = drv("start", arch, timeout=600)
            if r.returncode == 0:
                break
            # surface QEMU's own error (image lock, bad drive, ...) instead of a bare "boot FAIL"
            print(f"[boot-start-failed attempt {attempt}] rc={r.returncode}\n"
                  + "\n".join((r.stdout + r.stderr).strip().splitlines()[-15:]), flush=True)
            drv("stop", timeout=60)
        if r.returncode != 0 or \
           drv("login", "root", "root", "180", timeout=400).returncode != 0:
            verdict("boot", False)
            return False
        return True

    M = f"mount -t f2fs LABEL={LABEL} /mnt/gc"
    try:
        if not boot():
            return 1
        ok, _ = run(f"mkdir -p /mnt/gc && {M} && mkdir /mnt/gc/t && cd /mnt/gc/t && "
                    "i=0; while [ $i -lt 400 ]; do i=$((i+1)); "
                    "echo \"file $i $((i*7919))\" > f$i; done; "
                    "cd /mnt/gc && sha256sum t/* > MANIFEST && sync && umount /mnt/gc", 600)
        verdict("populate", ok)
        cur, top = cursor_state(img)
        verdict("cursor-after-umount", cur > top, f"(cp.next_free_nid {cur}, max live nid {top})")
        # Installer pattern: create more files, sync, remount read-only, then
        # the VM is killed with no umount (live block copy).
        ok, _ = run(f"{M} && cd /mnt/gc && mkdir u && cd u && i=0; while [ $i -lt 200 ]; do i=$((i+1)); "
                    "echo r$i > r$i; done; cd /mnt/gc && sync && mount -o remount,ro /mnt/gc && echo REMOUNTED", 600)
        verdict("populate-ro", ok and "REMOUNTED" in _)
    finally:
        drv("stop", timeout=120)
    cur, top = cursor_state(img)
    verdict("cursor-after-remount-ro", cur > top, f"(cp.next_free_nid {cur}, max live nid {top})")
    old = patch_cp(img, 100)
    print(f"patched checkpoint next_free_nid {old} -> 100")
    try:
        if not boot():
            return 1
        ok, o = run(f"{M} && cd /mnt/gc && mkdir s && cd s && i=0; while [ $i -lt 3000 ]; do i=$((i+1)); "
                    "ln -s /bin/busybox l$i; done; i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo n$i > n$i; done; "
                    "cd /mnt/gc && sync && sha256sum -c MANIFEST > /tmp/sum.txt 2>&1; echo SUMRC=$?; "
                    "grep -c FAILED /tmp/sum.txt; ls s | wc -l; umount /mnt/gc", 1800)
        verdict("hashes", "SUMRC=0" in o)
        lg = drv("log", timeout=60).stdout
        for line in lg.splitlines():
            if "next_free_nid" in line:
                print("  log:", line.strip()[:200])
    finally:
        if not keep:
            drv("stop", timeout=120)
    fk = subprocess.run([sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img],
                        capture_output=True, text=True, timeout=600)
    print(fk.stdout[-1500:].rstrip())
    verdict("fsck", fk.returncode == 0)
    allok = all(results.values())
    print(f"f2fsnidtest: {'PASS' if allok else 'FAIL'}")
    return 0 if allok else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main, sys.argv[1:]))
