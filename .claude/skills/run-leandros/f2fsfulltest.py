#!/usr/bin/env python3
"""f2fsfulltest.py [aarch64|x86_64] [--iters N] [--seed S]

Near-full volume + metadata churn (lane f2fsfull, L4.11). On a fresh 64 MiB
f2fs volume `f2fstest fullchurn` fills it to ~98%, then churns small files
(create+fsync / rename / unlink) until ENOSPC has been seen 20+ times; every
failure must be ENOSPC. Iteration 0 runs to completion (no crash) and checks
space is returned; later iterations carry `f2fs.crashpt=<random>` and are
killed at the first failure, then: fsck of the crashed image, remount,
`f2fstest fullscan`, every fsynced file (ACK k) not being deleted must be
present and whole, no file may be torn; umount, fsck --strict-leaks.
Exit 0 iff all pass.
"""
import os, random, re, shutil, signal, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2ffull" if caller else "f2ffull"
os.environ.pop("LEANDROS_USB_STICK", None)
sys.path.insert(0, HERE)
import driver  # noqa: E402
import f2fscrashtest as cr  # noqa: E402  (fsck, relabel; shares LABEL)


def main(argv):
    arch, iters, seed = "aarch64", 4, int(time.time())
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"): arch = a
        elif a == "--iters": iters = int(argv[i + 1]); i += 1
        elif a == "--seed": seed = int(argv[i + 1]); i += 1
        else: sys.exit(__doc__)
        i += 1
    rnd = random.Random(seed)
    print(f"f2fsfulltest: seed {seed}, {iters} crash iterations", flush=True)
    out = f"/tmp/f2ffull-{arch}{('-' + caller) if caller else ''}"
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "full.img")
    if os.path.exists(img): os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    cr.relabel(img)
    pci = "virtio-blk-pci,drive=crdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (f" -drive if=none,id=crdisk,format=raw,file={img} -device {pci}").strip()
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    boot_copy = os.path.join(out, f"boot-{arch}.img")
    fails = []

    def boot(cmdline):
        if os.path.exists(boot_copy): os.unlink(boot_copy)
        if cmdline:
            shutil.copyfile(src, boot_copy)
            subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot_copy, cmdline],
                           check=True, stdout=subprocess.DEVNULL)
            os.environ["LEANDROS_DISK_IMAGE"] = boot_copy
        else:
            os.environ.pop("LEANDROS_DISK_IMAGE", None)
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=180):
        return driver.serial_run(c, timeout=t, arch=arch)["output"]

    def hard_kill():
        pid = driver._qemu_pid()
        if pid:
            os.kill(pid, signal.SIGKILL); time.sleep(1)
        driver.cmd_stop()

    def mount():
        return "MOUNT_OK" in run(f"mkdir -p /mnt/cr && mount -t f2fs LABEL={cr.LABEL} /mnt/cr && echo MOUNT_OK", 120)

    def bad_lines(log):
        return [l for l in log.splitlines() if "F2FS" in l and ("in place" in l or "refused" in l or "emergency" in l)]

    prev = None
    for it in range(iters + 2):
        crash = it > 0 and it <= iters
        cmdline = f"f2fs.crashpt={rnd.randint(200, 30000)} f2fs.crashpt_mount=2" if crash else None
        boot(cmdline)
        if not mount():
            fails.append(f"iter {it}: mount failed"); hard_kill(); break
        if prev is not None:
            pd, acked, dels, kmax = prev
            if os.environ.get("F2FSFULL_KEEP"):
                try:
                    lg = open(driver.SERIAL_FULL_LOG, errors="replace").read()
                    with open(os.path.join(out, f"recovery-iter{it - 1}.log"), "w") as f:
                        f.write("\n".join(l for l in lg.splitlines() if "[F2FS]" in l))
                except OSError:
                    pass
            scan = run(f"/bin/f2fstest fullscan {pd} {kmax + 1}", 300)
            present = {}
            for m in re.finditer(r"^P ([st])(\d+) (OK|BAD)", scan, re.M):
                present.setdefault(int(m.group(2)), []).append((m.group(1), m.group(3)))
            errs = []
            if "SCAN_DONE" not in scan: errs.append("scan did not finish")
            for k, v in present.items():
                if any(x[1] == "BAD" for x in v): errs.append(f"k{k} torn/garbled")
            for k in acked - dels:
                if k not in present: errs.append(f"k{k} fsynced but missing")
            print(f"iter {it - 1}: recovered: {'PASS' if not errs else 'FAIL'} ({len(acked)} acked, {len(present)} present)", flush=True)
            for e in errs[:10]: print("    ", e, flush=True)
            if errs: fails.append(f"iter {it - 1}: {len(errs)} recovery errors")
            # fsck the recovered tree BEFORE deleting it: dentry damage in the test
            # directory is invisible once `rm -rf` has dropped it.
            run("umount /mnt/cr && sync && echo UMOUNT_OK", 120)
            ok, detail = cr.fsck(img, strict_leaks=False)
            print(f"iter {it - 1}: fsck of recovered tree: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
            if not ok: fails.append(f"iter {it - 1}: fsck of recovered tree")
            if not mount(): fails.append("remount for cleanup failed"); hard_kill(); break
            run(f"rm -rf {pd}", 300)
            run("umount /mnt/cr && sync && echo UMOUNT_OK", 120)
            ok, detail = cr.fsck(img, strict_leaks=True)
            print(f"iter {it - 1}: fsck after recovery: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
            if not ok: fails.append(f"iter {it - 1}: fsck after recovery")
            if it > iters: driver.cmd_stop(); break
            if not mount(): fails.append("remount failed"); hard_kill(); break
        d = f"/mnt/cr/w{it}"
        wseed = rnd.randint(1, 10 ** 9)
        ser = driver._Serial()
        try:
            ser.pump(0.2)
            driver._type_line(ser, f"/bin/f2fstest fullchurn {d} {wseed} 3000", 10.0 * driver.wait_scale(arch))
            start = len(ser.buf)
            ser.pump(600 * driver.wait_scale(arch), until=lambda b: b"FULL_DONE" in b[start:] or
                     (crash and b"FAIL " in b[start:]))
            ser.pump(1.5)
            text = driver._lrc_clean(ser.buf[start:], True)
        finally:
            ser.close()
        if not crash:
            m = re.search(r"FULL_DONE enospc=(\d+) ops=(\d+) (PASS|FAIL)", text)
            print(f"iter {it}: no-crash fullchurn: {m.group(0) if m else 'NO FULL_DONE'}", flush=True)
            if not m: print("    tail:", repr(text[-400:]), flush=True)
            for l in text.splitlines():
                if l.startswith("FAIL") or l.startswith("FILLED"): print("   ", l[:120], flush=True)
            if not m or m.group(3) != "PASS" or int(m.group(1)) < 20:
                fails.append("no-crash fullchurn failed")
            lg = run("dmesg 2>/dev/null | grep -E 'F2FS' | grep -E 'in place|refused|emergency' | tail -5", 30)
            for l in bad_lines(lg): print("  log:", l.strip()[:160], flush=True)
            run("umount /mnt/cr && sync && echo UMOUNT_OK", 120)
            ok, detail = cr.fsck(img, strict_leaks=True)
            print(f"iter {it}: fsck: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
            if not ok: fails.append("fsck after no-crash run")
            driver.cmd_stop()
            prev = None
            continue
        hard_kill()
        if os.environ.get("F2FSFULL_KEEP"):  # keep each crashed image for offline fsck/debugging
            subprocess.run(["cp", "-c", img, os.path.join(out, f"crashed-iter{it}.img")])
        acked = {int(x) for x in re.findall(r"^ACK (\d+)", text, re.M)}
        dels = {int(x) for x in re.findall(r"^DEL (\d+)", text, re.M)}
        kmax = max(acked | {0})
        print(f"iter {it}: {cmdline}: {len(acked)} acked, {len(dels)} deleted, "
              f"{'full' if 'FILLED' in text else 'still filling'}", flush=True)
        ok, detail = cr.fsck(img, strict_leaks=False)
        print(f"iter {it}: fsck of crashed image: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
        if not ok: fails.append(f"iter {it}: fsck of crashed image")
        prev = (d, acked, dels, kmax)
    if os.path.exists(boot_copy): os.unlink(boot_copy)
    print(f"f2fsfulltest: {'PASS' if not fails else 'FAIL'} {fails}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
