#!/usr/bin/env python3
"""rootstricttest.py [aarch64|x86_64] [a|b|c|d ...]: a named root= is never replaced by another disk
(lane strictroot).

Boots the QEMU boot image (ESP on virtio, a VALID f2fs root on virtio vdb, the historical fallback)
with `root=PARTUUID=<guid>` naming a GPT partition on a USB stick (qemu-xhci + usb-storage):
  a  the named stick is present: root mounts from it (sdX), not vdb
  b  the stick is absent, `rootwait=6 root.fail=reboot`: vdb is NOT mounted, the console says what
     was wanted, and init restarts the machine (a second "Init (PID 1) starting" in one QEMU)
  c  the stick is hot-added (QMP) ~10 s after init started waiting: root mounts from it
  d  two sticks, the decoy first (bigger partition, other PARTUUID): only the named one is root
First it runs the cmdline parser unit tests (userland/init/src/rootspec.rs) with rustc --test.
Red on a build without the strict root policy: b (vdb is mounted), c and d usually pass anyway.
Usage: LEANDROS_RUN_ID=<lane> python3 rootstricttest.py [aarch64|x86_64] [a b c d]
"""
import imgclone
import json, os, re, shutil, subprocess, sys, time, uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
MIB = 1024 * 1024
INIT_LINE = "LeandrOS Init (PID 1) starting"


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True, timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def serial(rid):
    _, log = drv("log", timeout=30)
    try:
        log += open(f"/tmp/leandros-{rid}-serial-full.log", errors="replace").read()
    except OSError:
        pass
    return log


def copy_sparse(src, dst, dst_off):
    """Copy the data extents of src into dst at dst_off (holes stay holes)."""
    with open(src, "rb") as f, open(dst, "r+b") as g:
        pos = 0
        end = os.fstat(f.fileno()).st_size
        while pos < end:
            try:
                d = f.seek(pos, os.SEEK_DATA)
            except OSError:
                break
            h = f.seek(d, os.SEEK_HOLE)
            f.seek(d)
            g.seek(dst_off + d)
            left = h - d
            while left:
                b = f.read(min(left, 8 * MIB))
                if not b:
                    break
                g.write(b)
                left -= len(b)
            pos = h


def make_stick(arch, path, guid, extra_mib=0):
    """GPT image with one partition holding a copy of the root filesystem image."""
    root = os.path.join(REPO, f"f2fs-data0-{arch}.img")
    size = os.path.getsize(root)
    total = size + (2 + extra_mib) * MIB
    with open(path, "wb") as f:
        f.truncate(total)
    subprocess.run(["sgdisk", "-n", "1:2048:0", "-t", "1:8300", "-u", f"1:{guid}", path],
                   check=True, capture_output=True)
    copy_sparse(root, path, MIB)


def unit_tests(tmp):
    exe = os.path.join(tmp, "rootspec_test")
    src = os.path.join(REPO, "userland", "init", "src", "rootspec.rs")
    p = subprocess.run(["rustc", "--edition", "2021", "--test", src, "-o", exe], capture_output=True, text=True)
    if p.returncode != 0:
        return False, p.stderr[-400:]
    p = subprocess.run([exe], capture_output=True, text=True)
    return p.returncode == 0, p.stdout[-300:]


class T:
    def __init__(self, arch, tmp):
        self.arch, self.tmp, self.fails = arch, tmp, []
        self.base = os.environ.get("LEANDROS_RUN_ID", "x")

    def check(self, name, cond, detail=""):
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" ({detail})" if detail and not cond else ""))
        if not cond:
            self.fails.append(name)

    def boot_env(self, case, cmdline, sticks):
        """Env for one boot: cloned boot image with `cmdline`; sticks = [(img, id)] on one xhci."""
        rid = f"{self.base}-rs{case}-{os.getpid()}"  # fresh logs per run
        img = os.path.join(self.tmp, f"boot-{case}.img")
        imgclone.clone_file(os.path.join(REPO, f"leandros-limine-{self.arch}.img"), img)
        subprocess.run(["bash", os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, cmdline],
                       check=True, capture_output=True, cwd=REPO)
        env = dict(os.environ, LEANDROS_RUN_ID=rid, LEANDROS_DISK_IMAGE=img)
        extra = "-device qemu-xhci,id=xhs"
        for i, (st, sid) in enumerate(sticks):
            extra += (f" -drive if=none,id=d{sid},format=raw,file={st}"
                      f" -device usb-storage,bus=xhs.0,port={i + 1},drive=d{sid},id={sid}")
        env["LEANDROS_QEMU_EXTRA"] = extra
        return rid, env

    def root_dev(self, env):
        rc, out = drv("run", "grep ' / ' /proc/mounts", "30", timeout=90, env=env)
        m = re.search(r"/dev/(\w+) / ", out)
        return m.group(1) if m else None

    def up(self, arch, env):
        drv("stop", timeout=60, env=env)
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            return False, out[-300:]
        return True, ""

    def up_async(self, arch, env):
        drv("stop", timeout=60, env=env)
        return subprocess.Popen(DRV + ["start", arch], env=env, cwd=REPO, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True)

    def login(self, env):
        rc, out = drv("login", "root", "root", "240", timeout=400, env=env)
        return rc == 0, out[-300:]

    def wait_log(self, rid, pred, secs):
        t = time.time()
        while time.time() - t < secs:
            log = serial(rid)
            if pred(log):
                return True, log
            time.sleep(2)
        return False, serial(rid)


def case_a(t, g):
    st = os.path.join(t.tmp, "a.img"); make_stick(t.arch, st, g)
    rid, env = t.boot_env("a", f"root=PARTUUID={g} rootwait=60", [(st, "usba")])
    try:
        ok, why = t.up(t.arch, env); t.check("a: boot", ok, why)
        ok, why = t.login(env) if ok else (False, "")
        t.check("a: login", ok, why)
        if ok:
            dev = t.root_dev(env)
            t.check("a: / is on the USB stick", bool(dev and dev.startswith("sd")), f"root on {dev}")
        log = serial(rid)
        t.check("a: vdb not mounted as root", "falling back to /dev/vdb" not in log)
    finally:
        drv("stop", timeout=60, env=env)


def case_b(t, g):
    rid, env = t.boot_env("b", f"root=PARTUUID={g} rootwait=6 root.fail=reboot", [])
    try:
        proc = t.up_async(t.arch, env)   # `start` waits for a login prompt that never comes
        ok, log = t.wait_log(rid, lambda l: l.count(INIT_LINE) >= 2, 200)
        t.check("b: machine restarted (init ran twice)", ok, f"init lines: {log.count(INIT_LINE)}")
        t.check("b: vdb never mounted", "F2FS mounted successfully" not in log and "falling back to /dev/vdb" not in log)
        t.check("b: console names the wanted root", f"root=PARTUUID={g}" in log, "\n".join(l for l in log.splitlines() if "root" in l.lower())[-600:])
        t.check("b: root.fail=reboot message", "root.fail=reboot: restarting" in log)
    finally:
        proc.kill()
        drv("stop", timeout=60, env=env)


def case_c(t, g):
    st = os.path.join(t.tmp, "c.img"); make_stick(t.arch, st, g)
    rid, env = t.boot_env("c", f"root=PARTUUID={g} rootwait=90", [])
    try:
        proc = t.up_async(t.arch, env)
        ok, log = t.wait_log(rid, lambda l: "Waiting for the root device root=" in l, 150)
        t.check("c: init waits for the named root", ok)
        time.sleep(10)
        rc1, _ = drv("qmp", "blockdev-add", json.dumps({"driver": "raw", "node-name": "hotc",
                     "file": {"driver": "file", "filename": st}}), timeout=60, env=env)
        rc2, o2 = drv("qmp", "device_add", json.dumps({"driver": "usb-storage", "bus": "xhs.0",
                      "drive": "hotc", "id": "usbhot"}), timeout=60, env=env)
        t.check("c: hot-add accepted", rc1 == 0 and rc2 == 0 and "error" not in o2.lower(), o2[-200:])
        try:
            out = proc.communicate(timeout=300)[0]
        except subprocess.TimeoutExpired:
            proc.kill(); out = "start timed out"
        t.check("c: start returns once the stick is added", proc.returncode == 0, out[-200:])
        ok, why = t.login(env); t.check("c: login after hot-add", ok, why)
        if ok:
            dev = t.root_dev(env)
            t.check("c: / is on the hot-added stick", bool(dev and dev.startswith("sd")), f"root on {dev}")
        log = serial(rid)
        t.check("c: no fallback", "falling back to /dev/vdb" not in log)
    finally:
        drv("stop", timeout=60, env=env)


def case_d(t, g):
    decoy = os.path.join(t.tmp, "d-decoy.img"); make_stick(t.arch, decoy, str(uuid.uuid4()), extra_mib=64)
    st = os.path.join(t.tmp, "d-named.img"); make_stick(t.arch, st, g)
    rid, env = t.boot_env("d", f"root=PARTUUID={g} rootwait=60", [(decoy, "usbdecoy"), (st, "usbnamed")])
    try:
        ok, why = t.up(t.arch, env); t.check("d: boot", ok, why)
        ok, why = t.login(env) if ok else (False, "")
        t.check("d: login", ok, why)
        if ok:
            dev = t.root_dev(env)
            t.check("d: / is on a USB stick", bool(dev and dev.startswith("sd")), f"root on {dev}")
            # the decoy's partition is 64 MiB bigger than the named one: root must be the smaller sd?1
            _, out = drv("run", "cat /proc/partitions", "30", timeout=90, env=env)
            parts = {m.group(2): int(m.group(1)) for m in re.finditer(r"^\s*\d+\s+\d+\s+(\d+)\s+(sd\w*1)$", out, re.M)}
            rootsize = parts.get(dev) if dev else None
            t.check("d: root partition is the named (smaller) one",
                    len(parts) == 2 and rootsize == min(parts.values()) and len(set(parts.values())) == 2,
                    f"root {dev}, partitions {parts}")
    finally:
        drv("stop", timeout=60, env=env)


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
    cases = []
    for a in sys.argv[1:]:
        if a in ("aarch64", "x86_64"):
            arch = a
        else:
            cases.append(a)
    cases = cases or ["a", "b", "c", "d"]
    tmp = os.path.join(REPO, f"rootstrict-{os.environ.get('LEANDROS_RUN_ID', 'x')}")
    os.makedirs(tmp, exist_ok=True)
    t = T(arch, tmp)
    try:
        ok, out = unit_tests(tmp)
        t.check("parser unit tests", ok, out)
        for c in cases:
            {"a": case_a, "b": case_b, "c": case_c, "d": case_d}[c](t, str(uuid.uuid4()))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"rootstricttest {arch}: {'PASS' if not t.fails else 'FAIL ' + ', '.join(t.fails)}")
    return 1 if t.fails else 0


if __name__ == "__main__":
    sys.exit(main())
