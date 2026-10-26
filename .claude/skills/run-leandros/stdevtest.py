"""Every mount must report its own st_dev (lane/stdev, 2026-10-26, openbugs #90).

Boots with two extra virtio-blk disks cloned from ONE f2fs image, mounts
both, and checks:
  * the two mount roots (same inode number) report different st_dev, and
    each matches the backing device's major:minor (as Linux reports it);
  * /proc/self/mountinfo prints that same major:minor for each mount;
  * `cp -a /mnt/sda/f /mnt/sdb/f` works (both files get the same inode
    number; with a shared st_dev coreutils refused "are the same file");
  * the tmpfs roots /tmp and /dev/shm differ from each other and from /.
Until 2026-10-26 the VFS hardcoded st_dev = 1 for every filesystem.

Usage: LEANDROS_RUN_ID=<lane> python3 stdevtest.py [aarch64|x86_64]
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


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    rid = os.environ.get("LEANDROS_RUN_ID", "x")
    tmp = os.path.join(REPO, f"stdev-{rid}")
    os.makedirs(tmp, exist_ok=True)
    base = os.path.join(tmp, "base.img")
    subprocess.run([sys.executable, MKFS, "create", base, "--size", "64M"],
                   check=True, capture_output=True, cwd=REPO)
    extra = []
    for i in range(2):
        img = os.path.join(tmp, f"d{i}.img")
        if subprocess.run(["cp", "-c", base, img], capture_output=True).returncode != 0:
            shutil.copyfile(base, img)
        extra.append(f"-drive if=none,id=sd{i},format=raw,file={img} "
                     f"-device virtio-blk-pci,drive=sd{i},disable-legacy=on")
    env = dict(os.environ)
    env["LEANDROS_QEMU_EXTRA"] = " ".join(extra)
    da, db = "vd" + chr(ord("a") + BASE_DISKS), "vd" + chr(ord("a") + BASE_DISKS + 1)
    fails = []

    def check(name, cond, detail=""):
        print(f"{'ok  ' if cond else 'FAIL'} {name} {detail}".rstrip())
        if not cond: fails.append(name)

    try:
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
        if rc != 0:
            print("FAIL: login\n" + out[-800:]); return 1
        script = (f"mkdir -p /mnt/sda /mnt/sdb && mount -t f2fs /dev/{da} /mnt/sda && "
                  f"mount -t f2fs /dev/{db} /mnt/sdb && echo MNT_OK; "
                  f"echo R $(stat -c '%d %i' /mnt/sda) $(stat -c '%d %i' /mnt/sdb); "
                  f"echo N $(stat -c '%t %T' /dev/{da}) $(stat -c '%t %T' /dev/{db}); "
                  "echo one > /mnt/sda/f; echo two > /mnt/sdb/f; "
                  "echo F $(stat -c '%d %i' /mnt/sda/f) $(stat -c '%d %i' /mnt/sdb/f); "
                  "cp -a /mnt/sda/f /mnt/sdb/f && echo CP_OK; echo C $(cat /mnt/sdb/f); "
                  "exec 3</mnt/sda/f; echo D $(stat -L -c '%d' /proc/self/fd/3); exec 3<&-; "
                  "echo T $(stat -c '%d' / /tmp /dev/shm); "
                  "grep -E ' /mnt/sd[ab] ' /proc/self/mountinfo | sed 's/^/MI /'; "
                  "umount /mnt/sda; umount /mnt/sdb; echo DONE")
        rc, out = drv("run", script, "120", timeout=200)
        g = lambda tag: (re.search(rf"^{tag} (.*)$", out, re.M) or [None, ""])[1].split()
        r, n, f, t = g("R"), g("N"), g("F"), g("T")
        check("mount_both", "MNT_OK" in out)
        check("roots_same_ino", len(r) == 4 and r[1] == r[3], str(r))
        check("roots_distinct_dev", len(r) == 4 and r[0] != r[2], str(r))
        if len(n) == 4 and len(r) == 4:
            mk = lambda a, b: ((int(a, 16) & 0xfff) << 8) | (int(b, 16) & 0xff) | ((int(b, 16) & ~0xff) << 12)
            check("dev_is_block_devt", int(r[0]) == mk(n[0], n[1]) and int(r[2]) == mk(n[2], n[3]),
                  f"st_dev {r[0]},{r[2]} vs rdev {n}")
        check("files_distinct_dev", len(f) == 4 and f[0] != f[2], str(f))
        check("cp_a_between_mounts", "CP_OK" in out and g("C") == ["one"], str(g("C")))
        check("fstat_dev_matches", len(r) == 4 and g("D") == [r[0]], f"{g('D')} vs {r[:1]}")
        check("tmpfs_distinct_dev", len(t) == 3 and len(set(t)) == 3, str(t))
        mi = re.findall(r"^MI \d+ \d+ (\d+):(\d+) / (/mnt/sd[ab]) ", out, re.M)
        want = {}
        if len(r) == 4:
            for d, mp in ((int(r[0]), "/mnt/sda"), (int(r[2]), "/mnt/sdb")):
                want[mp] = (str((d >> 8) & 0xfff), str((d & 0xff) | ((d >> 12) & ~0xff)))
        got = {mp: (a, b) for a, b, mp in mi}
        check("mountinfo_agrees", len(got) == 2 and got == want, f"{got} vs {want}")
        if fails: print(out.strip()[-900:])
    finally:
        try: drv("stop", timeout=60)
        except subprocess.TimeoutExpired: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print("PASS stdev" if not fails else f"FAIL stdev {fails}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main())
