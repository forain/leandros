#!/usr/bin/env python3
"""f2fscrashtest.py [aarch64|x86_64] [--iters N] [--mode mix|kill|crashpt] [--seed S] [--crashpt N] [--wseed N] [--cmdline "k=v ..."] [--keep]

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
            m = re.match(r"^\s*(A|W|O|N|U|T|R|SETUP)\b(.*)$", raw)
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
    arch, iters, mode, seed, keep = os.environ.get("LEANDROS_TEST_ARCH", "aarch64"), 6, "mix", None, False
    extra_cmd = ""
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
        else:
            sys.exit(__doc__)
        i += 1
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
            pdir, claims = prev
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
            text = driver._lrc_clean(ser.buf[start:], True)
            if os.environ.get("F2FSCR_OUT"):
                with open(os.path.join(os.environ["F2FSCR_OUT"], f"raw-{it}.bin"), "wb") as f:
                    f.write(bytes(ser.buf[max(0, start - 400):]))
        finally:
            ser.close()
        hard_kill()
        if os.environ.get("F2FSCR_OUT"):
            with open(os.path.join(os.environ["F2FSCR_OUT"], f"work-{it}.txt"), "w") as f:
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
        prev = (d, claims)
    if not keep and os.path.exists(boot_copy):
        os.unlink(boot_copy)
    print(f"f2fscrashtest: {'PASS' if not fails else 'FAIL'} {fails}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
