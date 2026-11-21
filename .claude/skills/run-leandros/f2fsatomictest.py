"""A caller that may not sleep must never stall the f2fs mount table (lane f2fsatomic).

Reproduces the wifi-brcmf pattern: `ktest=spinfw` starts a kthread that, once a second, holds a
spin lock and reads /etc/brcmf.ctl through request_firmware (-> VFS_OPEN from atomic context).
The root disk's writes are throttled (LEANDROS_ROOTFS_BPS_WRITE, default 600000 B/s) and a
bulk write (F2FSATOMIC_MB MiB, default 100) runs while a probe loop times `cat`/`ls`.
Before the fix every such open counted as a writer, hit cp_throttle and ran a locked
checkpoint (56-200 s F2FS_MOUNTS holds). Gates: worst probe < 2 s, no `[F2FS] long hold`
>= 1000 ms in the serial log, no `[WDOG]` line.
Usage: LEANDROS_RUN_ID=<id> python3 f2fsatomictest.py [aarch64|x86_64]
"""
import os, re, shutil, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
LOG = f"/tmp/leandros{'-' + RUN_ID if RUN_ID else ''}-serial-full.log"
MB = int(os.environ.get("F2FSATOMIC_MB", "100"))
GATE_CS = int(os.environ.get("F2FSATOMIC_GATE_CS", "200"))
CMDLINE = "ktest=spinfw"

GUEST = (
    "rm -rf /root/atom; mkdir -p /root/atom; "
    f"(dd if=/dev/zero of=/root/atom/a bs=1M count={MB} 2>/dev/null; touch /root/atom/done) & "
    "worst=0; n=0; "
    "while [ ! -e /root/atom/done ]; do "
    "read a _ < /proc/uptime; cat /etc/hostname > /dev/null; ls /root > /dev/null; read b _ < /proc/uptime; "
    "d=$(( 10#${b/./} - 10#${a/./} )); [ $d -gt $worst ] && worst=$d; n=$((n+1)); sleep 1; done; "
    "echo WORST_CS=$worst PROBES=$n"
)


def drv(*args, timeout=200, env=None):
    try:
        p = subprocess.run(DRV + list(args), capture_output=True, text=True, timeout=timeout, env=env, cwd=REPO)
    except subprocess.TimeoutExpired:
        return 124, f"driver {args[0]} timed out after {timeout}s"
    return p.returncode, p.stdout + p.stderr


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"f2fsatomic-{RUN_ID or arch}")
    os.makedirs(out, exist_ok=True)
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(out, f"f2fsatomic-{arch}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run(["cp", "-c", src, img], stderr=subprocess.DEVNULL).returncode != 0:
        shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, CMDLINE],
                   check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img,
               LEANDROS_ROOTFS_BPS_WRITE=os.environ.get("LEANDROS_ROOTFS_BPS_WRITE", "600000"))
    ok = True
    drv("stop", timeout=120)
    try:
        rc, o = drv("start", arch, timeout=400, env=env)
        if rc != 0:
            print("FAIL: boot\n" + o[-800:]); ok = False
        elif drv("login", "root", "root", "240", timeout=300)[0] != 0:
            print("FAIL: root login"); ok = False
        else:
            rc, o = drv("run", GUEST, "1500", arch, env=env, timeout=1600)
            m = re.search(r"WORST_CS=(\d+) PROBES=(\d+)", o)
            if not m:
                ok = False; print("FAIL: no WORST_CS line\n" + o[-400:])
            else:
                worst, n = int(m.group(1)), int(m.group(2))
                print(f"worst probe {worst * 10} ms over {n} probes (gate {GATE_CS * 10} ms)")
                if worst >= GATE_CS or n < 3:
                    ok = False; print("FAIL: probe stall too long")
        log = open(LOG, "rb").read().decode("utf-8", "replace")
        holds = [int(x) for x in re.findall(r"\[F2FS\] long hold (\d+) ms", log)]
        print(f"long holds: {sorted(holds)[-5:]}  spinfw polls: {len(re.findall(r'spinfw: \d+ polls', log))}")
        if any(h >= 1000 for h in holds):
            ok = False; print("FAIL: a long hold >= 1 s")
        if "[WDOG]" in log:
            ok = False; print("FAIL: watchdog line in the serial log")
        if "spinfw: start" not in log:
            ok = False; print("FAIL: the spinfw knob never started")
    finally:
        drv("stop", timeout=60)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
