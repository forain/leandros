#!/usr/bin/env python3
"""f2fsckpttest.py [aarch64|x86_64] [--n N] [--count-only|--cut-only]

Atomic small writes on f2fs (lane f2fsckpt3), on a fresh 64 MiB volume attached as
an extra disk (run id `<caller>-f2fsckpt`).

  count  `f2fstest cfgpat` N atomic writes of each pattern (plain: temp file +
         fsync + rename + fsync(dir); cfg: cosmic-config's mkdir + create + fsync
         + rename + fsync(tmpdir) + fsync(dir) + rmdir); checkpoints taken are
         /proc/f2fsstat cp_total deltas. Gate: <= CP_MAX[pattern] * N + 4 (the
         slack covers an age/pressure checkpoint), so the cost cannot silently grow.
  cut    for each pattern and stage 1 (after the file's fsync), 2 (after the
         rename), 3 (after the last fsync returned): generation 1 is durable, the
         guest writes generation 2 and the machine is SIGKILLed at the stage.
         fsck of the crashed image must pass; after recovery c0 must be a whole
         generation 1 or 2 (stage 3: generation 2) and fsck must pass again.
Exit 0 iff everything passes."""
import os
import hostclean
import re
import shutil
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LABEL = "f2fsckpt-test"
# Checkpoints per atomic write that the implementation may take (measured 2026-11-21).
CP_MAX = {"plain": float(os.environ.get("F2FSCKPT_PLAIN_MAX", "1")), "cfg": float(os.environ.get("F2FSCKPT_CFG_MAX", "2"))}

caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2fsckpt" if caller else "f2fsckpt"
os.environ.pop("LEANDROS_USB_STICK", None)
sys.path.insert(0, HERE)
import driver  # noqa: E402


def relabel(img):
    name = LABEL.encode("utf-16-le")
    with open(img, "r+b") as f:
        for base in (0, 4096):
            f.seek(base + 1024 + 124)
            f.write(name + bytes(1024 - len(name)))


def fsck(img, strict_leaks):
    args = [sys.executable, os.path.join(REPO, "scripts", "fsck-f2fs.py"), img, "--strict-links"]
    if strict_leaks:
        args.append("--strict-leaks")
    r = subprocess.run(args, capture_output=True, text=True, timeout=600)
    tail = [l for l in r.stdout.splitlines() if l.startswith(("ERROR", "warning", "checkpoint", "fsck", "note"))]
    return r.returncode == 0, "\n    ".join(tail[-8:])


def main(argv):
    arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    n, do_count, do_cut = 100, True, True
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--n":
            n = int(argv[i + 1]); i += 1
        elif a == "--count-only":
            do_cut = False
        elif a == "--cut-only":
            do_count = False
        else:
            sys.exit(__doc__)
        i += 1
    if caller:
        subprocess.run([sys.executable, os.path.join(HERE, "driver.py"), "stop"],
                       env=dict(os.environ, LEANDROS_RUN_ID=caller), capture_output=True, timeout=120)
    out = os.environ.get("F2FSCKPT_OUT", f"/tmp/f2fsckpt-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    hostclean.track(out, glob="/tmp/f2fsckpt-*")
    img = os.path.join(out, "ckpt.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    pci = "virtio-blk-pci,drive=ckdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (os.environ.get("LEANDROS_QEMU_EXTRA", "") +
                                         f" -drive if=none,id=ckdisk,format=raw,file={img} -device {pci}").strip()
    fails = []

    def boot():
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=180):
        return driver.serial_run(c, timeout=t, arch=arch)["output"]

    def hard_kill():
        pid = driver._qemu_pid()
        if pid:
            os.kill(pid, signal.SIGKILL)
            time.sleep(1)
        driver.cmd_stop()

    def mount():
        o = run(f"mkdir -p /mnt/ck && mount -t f2fs LABEL={LABEL} /mnt/ck && echo MOUNT_OK", 120)
        if "MOUNT_OK" not in o:
            fails.append(f"mount failed: {o[-300:]}")
            return False
        return True

    driver.cmd_stop()
    if do_count:
        boot()
        run('echo WARM_$((1+1))', 120)
        if mount():
            cpt = ("cpt() { while read k v; do [ \"$k\" = cp_total ] && echo $v; done < /proc/f2fsstat; }; "
                   "sleep 3; sync; a=$(cpt); echo STAT0; cat /proc/f2fsstat; echo STAT1; {cmd}; b=$(cpt); echo STAT2; cat /proc/f2fsstat; echo STAT3; echo CPDELTA $((b-a))")
            for mode in ("plain", "cfg"):
                o = run(cpt.replace("{cmd}", f"/bin/f2fstest cfgpat /mnt/ck/{mode} {n} {mode}"), 1200)
                m, t = re.search(r"CPDELTA (\d+)", o), re.search(r"total_us=(\d+)", o)
                def snap(a, b):
                    d = {}
                    for l in o[o.rfind(a):o.rfind(b)].splitlines():
                        w = l.split()
                        if len(w) == 2 and w[1].isdigit(): d[w[0]] = int(w[1])
                    return d
                s0, s2 = snap("STAT0", "STAT1"), snap("STAT2", "STAT3")
                print(f"  stat deltas {mode}: " + " ".join(f"{k}={s2[k]-s0.get(k,0)}" for k in s2 if s2[k] != s0.get(k, 0)), flush=True)
                if not (m and t):
                    fails.append(f"{mode}: no result: {o[-300:]}")
                    continue
                cps, us = int(m.group(1)), int(t.group(1))
                lim = CP_MAX[mode] * n + 4
                ok = cps <= lim
                print(f"count {mode}: {n} atomic writes, {cps} checkpoints ({cps / n:.2f}/write, limit {lim:.0f}), "
                      f"{us / 1000:.0f} ms total ({us / n / 1000:.1f} ms/write) {arch}: {'PASS' if ok else 'FAIL'}", flush=True)
                if not ok:
                    fails.append(f"{mode}: {cps} checkpoints for {n} writes (limit {lim:.0f})")
            run("umount /mnt/ck && echo UMOUNT_OK", 120)
        driver.cmd_stop()
    if do_cut:
        cases = [(m, s) for m in ("plain", "cfg") for s in (1, 2, 3)]
        prev = None
        for k in range(len(cases) + 1):
            boot()
            if not mount():
                hard_kill(); break
            if prev:
                pm, ps, pd = prev
                o = run(f"/bin/f2fstest cfgscan {pd}", 120)
                m = re.search(r"CFGSCAN gen=(\w+) ok=(\d)", o)
                good = bool(m) and m.group(2) == "1" and m.group(1) in (("2",) if ps == 3 else ("1", "2"))
                print(f"cut {pm} stage {ps}: recovered {m.group(0) if m else 'no scan line: ' + o[-200:]}: "
                      f"{'PASS' if good else 'FAIL'}", flush=True)
                if not good:
                    fails.append(f"cut {pm} stage {ps}: {m.group(0) if m else 'no scan line'}")
                run("umount /mnt/ck && sync && echo UMOUNT_OK", 120)
                ok, detail = fsck(img, strict_leaks=False)
                print(f"cut {pm} stage {ps}: fsck after recovery: {'PASS' if ok else 'FAIL'}\n    {detail}", flush=True)
                if not ok:
                    fails.append(f"cut {pm} stage {ps}: fsck after recovery")
            if k == len(cases):
                driver.cmd_stop(); break
            if prev:
                if "MOUNT_OK" not in run(f"mount -t f2fs LABEL={LABEL} /mnt/ck && echo MOUNT_OK", 120):
                    fails.append("remount failed"); hard_kill(); break
            mode, stage = cases[k]
            d = f"/mnt/ck/cut{k}"
            ser = driver._Serial()
            try:
                ser.pump(0.2)
                start = len(ser.buf)
                driver._type_line(ser, f"/bin/f2fstest cfgcut {d} {mode} {stage}", 10.0 * driver.wait_scale(arch))
                ser.pump(240, until=lambda b: re.search(rb"CUTAT \d", bytes(b[start:])) is not None or b"CFGCUT_FAIL" in b[start:])
                ser.pump(1.0)
                text = bytes(ser.buf[start:]).decode("latin1")
            finally:
                ser.close()
            hard_kill()
            if f"CUTAT {stage}" not in text:
                fails.append(f"cut {mode} stage {stage}: workload did not reach the stage: {text[-1500:]!r}")
                break
            ok, detail = fsck(img, strict_leaks=False)
            print(f"cut {mode} stage {stage}: fsck of the crashed image: {'PASS' if ok else 'FAIL'}\n    {detail}", flush=True)
            if not ok:
                fails.append(f"cut {mode} stage {stage}: fsck of the crashed image")
            prev = (mode, stage, d)
    print(f"f2fsckpttest: {'PASS' if not fails else 'FAIL'} {fails}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main, sys.argv[1:]))
