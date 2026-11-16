#!/usr/bin/env python3
"""f2fscrashtest.py [aarch64|x86_64] [--iters N] [--mode mix|kill|crashpt] [--seed S] [--crashpt N] [--wseed N] [--cmdline "k=v ..."] [--keep] [--linux]

Crash consistency of f2fs (lane f2fsync), on a fresh 64 MiB volume attached
as an extra disk (run id `<caller>-f2fscr`). Every iteration:

  workload  `mount LABEL=f2fscr-test /mnt/cr`, then `f2fstest crash
            /mnt/cr/w<i> <seed>` (appends, in-place rewrites, new files,
            rename-replace, mkdir/rmdir; every fsync it claims is printed
            after it returned), until the machine dies:
              kill     QEMU is SIGKILLed after a random 4-20 s;
              crashpt  the boot carries `f2fs.crashpt=<random>`: the volume's
                       writes stop (and fail) after that many blocks, as at
                       a power cut in the middle of whatever was being
                       written (a checkpoint, its journal, its home writes,
                       a roll-forward record); QEMU is killed once the
                       workload saw its first failed fsync;
              mix      alternate (default).
  fsck      on the image as the crash left it: scripts/fsck-f2fs.py
            --strict-links (the newest valid checkpoint with its journal must
            be consistent).
  recover   next boot: mount (journal replay + roll-forward), `f2fstest
            crashscan`, compare with the claims: every append file at least
            as long as its last fsync and all pattern; every rewritten block
            a gen that was written there and not older than the last fsynced
            one; every fsynced new file present (unless unlinked later) and
            whole; every r<k> a whole version; then umount and fsck again
            with --strict-leaks.
Exit 0 iff every iteration passes.

--linux (lane f2fsp2): the whole run uses `f2fs.lxchain=1` (Linux-format fsync
node chains) and, between the crash and the LeandrOS recovery boot, the crashed
image is handed to Linux on the box (scripts/f2fslx/interop.py push/fetch/box;
hold F2FSINTEROP_LOCK=$SP/lock.sh): `fsck.f2fs --dry-run` of the crashed image
(informational), a rw loop mount (Linux's own roll-forward recovery, dmesg
recover_* lines logged), umount, `fsck.f2fs --dry-run` of the result (must be
rc 0), and the recovered image is fetched back and checked by the usual
crashscan against the claims (every fsynced file's content and size).
Re-run one failure with the params printed per iteration
(`--wseed W --crashpt C --mode crashpt|kill --iters 1`).
"""
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LABEL = "f2fscr-test"

caller = os.environ.get("LEANDROS_RUN_ID", "")
os.environ["LEANDROS_RUN_ID"] = f"{caller}-f2fscr" if caller else "f2fscr"
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
    fsck.full = r.stdout + r.stderr
    return r.returncode == 0, "\n    ".join(tail[-8:])


def keep_failed(out, img, tag, seed, params, arch):
    """Keep the image as the failure left it plus the full fsck output; at most 2 sets."""
    kd = os.path.join(out, "failed")
    os.makedirs(kd, exist_ok=True)
    sets = sorted({f.rsplit(".", 1)[0] for f in os.listdir(kd)}, key=lambda n: os.path.getmtime(os.path.join(kd, n + ".txt")))
    for old in sets[:max(0, len(sets) - 1)]:
        for ext in (".img", ".txt"):
            if os.path.exists(os.path.join(kd, old + ext)):
                os.unlink(os.path.join(kd, old + ext))
    base = os.path.join(kd, f"{arch}-seed{seed}-{tag}")
    subprocess.run(["cp", "-c", img, base + ".img"], check=False, stderr=subprocess.DEVNULL)
    if not os.path.exists(base + ".img"):
        shutil.copyfile(img, base + ".img")
    with open(base + ".txt", "w") as f:
        f.write(f"seed {seed} {tag} params: {params}\n" + getattr(fsck, "full", ""))
    print(f"    kept {base}.img (+ .txt)", flush=True)


def linux_recover(img, out, it):
    """Hand the crashed image to Linux on the box; returns (ok, detail). The image is
    replaced by Linux's recovered + unmounted result."""
    sys.path.insert(0, os.path.join(REPO, "scripts", "f2fslx"))
    import interop
    interop.LOCK = os.environ.get("F2FSINTEROP_LOCK", interop.LOCK)
    rd = f"/tmp/f2fscrlx-{os.getpid()}-{it}"
    try:
        interop.box(f"mkdir -p {rd}")
        if not interop.push(img, f"{rd}/c.img"):
            return False, "push failed"
        rc, o = interop.box(f"""cd {rd}; cp --sparse=always c.img f0.img; timeout 300 fsck.f2fs --dry-run f0.img </dev/null >f0.txt 2>&1; echo FSCK0_RC=$?
mkdir -p m; echo LXMARK_{os.getpid()}_{it} | sudo tee /dev/kmsg >/dev/null
if sudo mount -o loop c.img m; then echo LX_MOUNT_OK; sync; sudo umount m && echo LX_UMOUNT_OK; else echo LX_MOUNT_FAIL; fi
sudo dmesg | sed -n '/LXMARK_{os.getpid()}_{it}/,$p' > dm.txt; grep -E 'recover_|f2fs.*(error|corrupt|invalid|Failed)' dm.txt | head -6
timeout 300 fsck.f2fs --dry-run c.img </dev/null >f1.txt 2>&1; echo FSCK1_RC=$?; grep -E '^\[FSCK\]|Fail|\[ASSERT\]' f1.txt | head -4""", 900)
        print("    linux:", " | ".join(l.strip()[:110] for l in o.splitlines() if l.strip())[-420:], flush=True)
        with open(os.path.join(out, f"linux-{it}.txt"), "w") as f:
            f.write(o)
        if "LX_MOUNT_OK" not in o or "LX_UMOUNT_OK" not in o:
            return False, "Linux mount/umount failed"
        if "FSCK1_RC=0" not in o:
            return False, "fsck.f2fs after Linux recovery rc != 0"
        if not interop.fetch(f"{rd}/c.img", img):
            return False, "fetch failed"
        return True, ""
    finally:
        interop.box(f"rm -rf {rd}")


def unsplice(s):
    """Drop kernel log lines ("[IRQ] ...", "[F2FS] ...") from guest output; the
    console is shared, so one can land inside a claim line ("U 6" + log + "99")
    and the line used to be lost, giving a false "fsynced but missing"."""
    tag = r"\[[A-Z][A-Z0-9_]*\][^\n]*\n"
    junk = r"(?:\x1b\[[0-9;?]*[A-Za-z]|[ \t\r\[\]0-9.])*"
    s = re.sub(r"(?m)^" + junk + tag, "", s)
    return re.sub(r"(?:\[ *[0-9.]+\] ?)?" + tag, "", s)


def selftest():
    for a, b in (("U 6[F2FS] x\n99\n", "U 699"), ("N 6[   31.2] [IRQ] y\n99 4096\n", "N 699 4096"),
                 ("[IRQ] z\nN 5 7\n", "N 5 7")):
        assert b in unsplice(a), (a, unsplice(a))
    print("selftest: PASS")


class Claims:
    def __init__(self):
        self.a = {}            # k -> last fsynced size
        self.o_allowed = {}    # (k, b) -> set of gens written there (0 = setup)
        self.o_sync = {}       # (k, b) -> last fsynced gen
        self.n = {}            # id -> size (fsynced)
        self.unlinked = set()
        self.t = {}            # (k, g) -> size
        self.max_gen = 0       # newest rewrite gen whose W line arrived
        self.inflight = 0      # blocks of a newer gen (its W line was lost with the kill)
        self.min_gen = None    # oldest rewrite gen whose W line arrived (gens start at 1)
        self.uncaptured = 0    # blocks of a gen older than the capture's first W line
        self.setup = False

    def parse(self, text):
        for raw in text.splitlines():
            m = re.match(r"^\s*(?:\[\s*\d+\.\d+\]\s*)?(A|W|O|N|U|T|R|SETUP)\b(.*)$", raw)  # a kernel log prefix can land in front of a workload line
            if not m:
                continue
            tag, rest = m.group(1), m.group(2).split()
            try:
                v = [int(x) for x in rest]
            except ValueError:
                continue
            if tag == "SETUP":
                self.setup = True
            elif tag == "A" and len(v) == 2:
                self.a[v[0]] = max(self.a.get(v[0], 0), v[1])
            elif tag == "W" and len(v) == 4:
                self.max_gen = max(self.max_gen, v[1])
                self.min_gen = v[1] if self.min_gen is None else min(self.min_gen, v[1])
                for b in range(v[2], v[2] + v[3]):
                    self.o_allowed.setdefault((v[0], b), {0}).add(v[1])
            elif tag == "O" and len(v) == 4:
                for b in range(v[2], v[2] + v[3]):
                    self.o_sync[(v[0], b)] = max(self.o_sync.get((v[0], b), 0), v[1])
            elif tag == "N" and len(v) == 2:
                self.n[v[0]] = v[1]
            elif tag == "U" and len(v) == 1:
                self.unlinked.add(v[0])
            elif tag == "T" and len(v) == 3:
                self.t[(v[0], v[1])] = v[2]

    def check(self, scan):
        errs = []
        seen_n = set()
        sa = {}
        for raw in scan.splitlines():
            p = raw.split()
            if not p:
                continue
            tag, v = p[0], p[1:]
            try:
                v = [int(x) for x in v]
            except ValueError:
                continue
            if tag == "SA":
                sa[v[0]] = v[1]
            elif tag == "SA_BAD":
                errs.append(f"a{v[0]}: content wrong at byte {v[2]} (size {v[1]})")
            elif tag in ("SO_MISSING", "SO_SIZE") and not (self.setup or self.min_gen is not None):
                # The crash hit setup before its final fsync (no SETUP line,
                # no rewrite): an o file need not exist or be whole yet.
                pass
            elif tag in ("SO_BAD", "SO_MISSING", "SO_SIZE"):
                errs.append(f"o: {raw.strip()}")
            elif tag == "SO":
                k, b, g = v
                allowed = self.o_allowed.get((k, b), {0})
                if g not in allowed and g > self.max_gen:
                    # The rewrite after the last W line that reached the host:
                    # the kill lost its line on the serial port, not the write.
                    self.inflight += 1
                elif self.min_gen is not None and 0 < g < self.min_gen:
                    # The serial capture began after the workload's first W
                    # lines (seen: it started at W gen 13 while the workload
                    # numbers rewrites from 1), so where gen g went is unknown.
                    self.uncaptured += 1
                elif g not in allowed:
                    errs.append(f"o{k} block {b}: gen {g} was never written there (newest W gen {self.max_gen})")
                elif g < self.o_sync.get((k, b), 0):
                    errs.append(f"o{k} block {b}: gen {g} older than fsynced gen {self.o_sync[(k, b)]}")
            elif tag in ("SN", "SN_BAD"):
                i, size, g = v
                seen_n.add(i)
                if tag == "SN_BAD" and not (size == 0 and i not in self.n):
                    errs.append(f"n{i}: content wrong (size {size})")
                elif i in self.n and size != self.n[i]:
                    errs.append(f"n{i}: size {size} != fsynced {self.n[i]}")
            elif tag in ("SR", "SR_BAD"):
                k, size, g = v
                if tag == "SR_BAD" or self.t.get((k, g)) != size:
                    errs.append(f"r{k}: not a whole version (size {size}, gen {g})")
        for k, size in self.a.items():
            got = sa.get(k)
            if got is None or got == 2 ** 64 - 1:
                errs.append(f"a{k}: missing (fsynced {size})")
            elif got < size:
                errs.append(f"a{k}: size {got} < fsynced {size}")
        for i in self.n:
            if i not in self.unlinked and i not in seen_n:
                errs.append(f"n{i}: fsynced but missing")
        return errs


def main(argv):
    if "--selftest" in argv:
        return selftest()
    arch, iters, mode, seed, keep = os.environ.get("LEANDROS_TEST_ARCH", "aarch64"), 6, "mix", None, False
    extra_cmd = ""
    linux = False
    fixed_cp = fixed_ws = None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--iters":
            iters = int(argv[i + 1]); i += 1
        elif a == "--cmdline":
            extra_cmd = argv[i + 1]; i += 1
        elif a == "--mode":
            mode = argv[i + 1]; i += 1
        elif a == "--seed":
            seed = int(argv[i + 1]); i += 1
        elif a == "--crashpt":
            fixed_cp = int(argv[i + 1]); i += 1
        elif a == "--wseed":
            fixed_ws = int(argv[i + 1]); i += 1
        elif a == "--keep":
            keep = True
        elif a == "--linux":
            linux = True
        else:
            sys.exit(__doc__)
        i += 1
    if linux:
        extra_cmd = (extra_cmd + " f2fs.lxchain=1").strip()
    seed = seed if seed is not None else int(time.time())
    rnd = random.Random(seed)
    print(f"f2fscrashtest: seed {seed}, {iters} iterations, mode {mode}", flush=True)
    out = os.environ.get("F2FSCR_OUT", f"/tmp/f2fscr-{arch}{('-' + caller) if caller else ''}")
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "cr.img")
    if os.path.exists(img):
        os.unlink(img)
    subprocess.run([sys.executable, os.path.join(REPO, "scripts", "mkfs-f2fs-home.py"), "create", img,
                    "--size", "64M"], check=True, stdout=subprocess.DEVNULL)
    relabel(img)
    pci = "virtio-blk-pci,drive=crdisk" + (",disable-legacy=on" if arch == "aarch64" else "")
    os.environ["LEANDROS_QEMU_EXTRA"] = (os.environ.get("LEANDROS_QEMU_EXTRA", "") +
                                         f" -drive if=none,id=crdisk,format=raw,file={img} -device {pci}").strip()
    src = os.path.join(REPO, f"leandros-limine-{arch}.img")
    boot_copy = os.path.join(out, f"boot-{arch}.img")
    fails = []

    def boot(cmdline):
        if os.path.exists(boot_copy):
            os.unlink(boot_copy)
        if cmdline:
            subprocess.run(["cp", "-c", src, boot_copy], check=False, stderr=subprocess.DEVNULL)
            if not os.path.exists(boot_copy):
                shutil.copyfile(src, boot_copy)
            subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), boot_copy, cmdline],
                           check=True, stdout=subprocess.DEVNULL)
            os.environ["LEANDROS_DISK_IMAGE"] = boot_copy
        else:
            os.environ.pop("LEANDROS_DISK_IMAGE", None)
        driver.cmd_start(arch)
        driver.cmd_login("root", "root", timeout=int(90 * driver.wait_scale(arch)))

    def run(c, t=180):
        r = driver.serial_run(c, timeout=t, arch=arch)
        return r

    def hard_kill():
        pid = driver._qemu_pid()
        if pid:
            os.kill(pid, signal.SIGKILL)
            time.sleep(1)
        driver.cmd_stop()

    prev = None   # (dir, claims) to verify on the next boot
    for it in range(iters + 1):
        last = it == iters
        how = None
        cmdline = None
        if not last:
            how = mode if mode != "mix" else ("crashpt" if it % 2 else "kill")
            if how == "crashpt":
                cp = rnd.randint(50, 6000)
                cp = fixed_cp if fixed_cp is not None else cp
                cmdline = (f"f2fs.crashpt={cp} "
                           f"f2fs.crashpt_mount={2 if prev is not None else 1}")
        cmdline = " ".join(x for x in (cmdline, extra_cmd) if x) or None
        boot(cmdline)
        r = run(f"mkdir -p /mnt/cr && mount -t f2fs LABEL={LABEL} /mnt/cr && echo MOUNT_OK", 120)
        if "MOUNT_OK" not in r["output"]:
            fails.append(f"iter {it}: mount failed: {r['output'][-300:]}")
            hard_kill()
            break
        lg = driver.serial_run("dmesg 2>/dev/null | grep -E 'F2FS.*(roll-forward|journal|packs|crashpoint|emergency|in place|refused)' | tail -5",
                               timeout=30, arch=arch)["output"]
        for l in lg.splitlines():
            if "F2FS" in l:
                print("  log:", l.strip()[:160], flush=True)
        if prev is not None:
            pdir, claims, ppre = prev
            r = run(f"/bin/f2fstest crashscan {pdir}", 300)
            scan = r["output"]
            if "SCAN_DONE" not in scan:
                fails.append(f"iter {it - 1}: crashscan did not finish: {scan[-300:]}")
            else:
                errs = claims.check(scan)
                nclaims = len(claims.a) + len(claims.o_sync) + len(claims.n)
                extra = f"; {claims.inflight} blocks of the in-flight rewrite" if claims.inflight else ""
                if claims.uncaptured:
                    extra += (f"; {claims.uncaptured} blocks of a rewrite older than the capture's "
                              f"first W line (gen {claims.min_gen})")
                print(f"iter {it - 1}: recovered: {'PASS' if not errs else 'FAIL'} "
                      f"({nclaims} durability claims checked{extra})", flush=True)
                for e in errs[:12]:
                    print("    ", e, flush=True)
                if errs:
                    fails.append(f"iter {it - 1}: {len(errs)} recovery errors")
                    if ppre and os.path.exists(ppre):
                        keep_failed(out, ppre, f"it{it - 1}-prelinux", seed, "image before Linux recovery", arch)
            run(f"rm -rf {pdir}", 300)
            r = run("umount /mnt/cr && sync && echo UMOUNT_OK", 120)
            ok, detail = fsck(img, strict_leaks=True)
            print(f"iter {it - 1}: fsck after recovery + umount: {'PASS' if ok else 'FAIL'}\n    {detail}", flush=True)
            if not ok:
                fails.append(f"iter {it - 1}: fsck after recovery failed")
                keep_failed(out, img, f"it{it - 1}-recovered", seed, "see log", arch)
            if last:
                driver.cmd_stop()
                break
            r = run(f"mount -t f2fs LABEL={LABEL} /mnt/cr && echo MOUNT_OK", 120)
            if "MOUNT_OK" not in r["output"]:
                fails.append(f"iter {it}: remount failed")
                hard_kill()
                break
        if last:
            driver.cmd_stop()
            break
        # The workload, until the crash.
        d = f"/mnt/cr/w{it}"
        wseed = rnd.randint(1, 10 ** 9)
        wseed = fixed_ws if fixed_ws is not None else wseed
        print(f"iter {it}: params seed={seed} how={how} wseed={wseed} cmdline={cmdline!r}", flush=True)
        ser = driver._Serial()
        try:
            ser.pump(0.2)
            start = len(ser.buf)
            driver._type_line(ser, f"/bin/f2fstest crash {d} {wseed}", 10.0 * driver.wait_scale(arch))
            if how == "kill":
                secs = rnd.uniform(4, 20)
                ser.pump(secs)
            else:
                ser.pump(180, until=lambda b: b"FSYNC_FAIL" in b[start:] or b"WRITE_FAIL" in b[start:]
                         or b"CRASH " in b[start:])
            text = unsplice(driver._lrc_clean(ser.buf[start:], True))
            if os.environ.get("F2FSCR_OUT"):
                with open(os.path.join(os.environ["F2FSCR_OUT"], f"raw-{it}.bin"), "wb") as f:
                    f.write(bytes(ser.buf[max(0, start - 400):]))
        finally:
            ser.close()
        hard_kill()
        if os.environ.get("F2FSCR_OUT") or linux:
            with open(os.path.join(os.environ.get("F2FSCR_OUT") or out, f"work-{it}.txt"), "w") as f:
                f.write(text)
        claims = Claims()
        claims.parse(text)
        ops = text.count("\n")
        print(f"iter {it}: {how}{(' ' + cmdline) if cmdline else ''}: workload printed {ops} lines "
              f"(setup {'done' if claims.setup else 'NOT done'}; {len(claims.a)} append files, "
              f"{len(claims.o_sync)} fsynced blocks, {len(claims.n)} new files)", flush=True)
        if how == "crashpt" and "FSYNC_FAIL" not in text and "WRITE_FAIL" not in text:
            print("    note: the crash point was not reached within 180 s", flush=True)
        ok, detail = fsck(img, strict_leaks=False)
        print(f"iter {it}: fsck of the crashed image: {'PASS' if ok else 'FAIL'}\n    {detail}", flush=True)
        if not ok:
            fails.append(f"iter {it}: fsck of the crashed image failed")
            keep_failed(out, img, f"it{it}-crashed", seed, f"how={how} wseed={wseed} cmdline={cmdline!r}", arch)
        pre = None
        if linux:
            pre = os.path.join(out, f"pre-linux-{it}.img")
            subprocess.run(["cp", "-c", img, pre], check=False, stderr=subprocess.DEVNULL)
            if not os.path.exists(pre):
                shutil.copyfile(img, pre)
            lok, ldet = linux_recover(img, out, it)
            print(f"iter {it}: Linux recovery + fsck.f2fs: {'PASS' if lok else 'FAIL ' + ldet}", flush=True)
            if not lok:
                fails.append(f"iter {it}: Linux recovery: {ldet} (seed={seed} wseed={wseed} how={how} cmdline={cmdline!r})")
                keep_failed(out, img, f"it{it}-linux", seed, f"how={how} wseed={wseed} cmdline={cmdline!r}", arch)
                break
        prev = (d, claims, pre)
    if not keep and os.path.exists(boot_copy):
        os.unlink(boot_copy)
    print(f"f2fscrashtest: {'PASS' if not fails else 'FAIL'} {fails}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
