#!/usr/bin/env python3
"""kvmqemutest.py -- unmodified QEMU runs a guest with `-accel kvm` INSIDE LeandrOS (kvm-plan step 7a).

aarch64 under TCG with virtualization=on (LEANDROS_VIRT=1), kernel built with CONFIG_KVM=y.
In the guest: `apk add qemu-system-aarch64 busybox-static tar cpio gzip` (needs the slirp network the
other apk suites use), the Alpine linux-virt kernel (`apk fetch`, the EFI zboot image is unpacked to
a plain arm64 Image), a tiny busybox initramfs that prints a marker and powers off, then

  qemu-system-aarch64 -accel kvm -M virt,gic-version=3,its=off -cpu host -nographic -kernel Image -initrd ...

PASS = the nested guest's Linux boot log reaches the initramfs marker line and QEMU exits through
PSCI SYSTEM_OFF. Opt-in (not in the default regress); SKIPs when the accelerator is not TCG.
Usage: LEANDROS_RUN_ID=<lane> python3 kvmqemutest.py
"""
import base64, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
import imgclone  # noqa: E402
REPO = driver.REPO_ROOT
DRV = [sys.executable, os.path.join(HERE, "driver.py")]

SETUP = r'''
set -e
mkdir -p /root/n; cd /root/n
apk update >/dev/null 2>&1 || true
apk add qemu-system-aarch64 busybox-static tar cpio gzip 2>&1 | tail -n 1
apk fetch --stdout linux-virt > lv.apk 2>/dev/null
tar -xzf lv.apk boot/vmlinuz-virt
f=boot/vmlinuz-virt
off=$(od -An -tu4 -j8 -N4 $f | tr -d ' '); sz=$(od -An -tu4 -j12 -N4 $f | tr -d ' ')
tail -c +$((off+1)) $f | head -c $sz | gunzip > Image
rm -rf rf; mkdir -p rf/bin rf/proc rf/sys rf/dev
cp /bin/busybox.static rf/bin/busybox
for a in sh mount echo uname cat poweroff sleep; do ln -sf busybox rf/bin/$a; done
cat > rf/init <<'EOT'
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
echo "NESTED-GUEST: init started on $(uname -m) kernel $(uname -r)"
/bin/busybox poweroff -f
EOT
chmod +x rf/init
(cd rf && printf "init\nbin\nproc\nsys\ndev\nbin/busybox\nbin/sh\nbin/mount\nbin/echo\nbin/uname\nbin/cat\nbin/poweroff\nbin/sleep\n" | cpio -o -H newc 2>/dev/null | gzip > ../initrd.gz)
ls -la Image initrd.gz
echo SETUP_DONE
'''

RUNQ = ("cd /root/n; timeout 600 qemu-system-aarch64 -accel kvm -M virt,gic-version=3,its=off -cpu host -m 256 -smp 1 "
        "-nographic -no-reboot -kernel Image -initrd initrd.gz -append 'console=ttyAMA0 rdinit=/init ignore_loglevel' > q.log 2>&1; "
        "echo QEMU_RC=$?; grep -a -c . q.log; grep -a 'Booting Linux\\|Linux version\\|NESTED-GUEST\\|GICv3\\|arch_timer\\|rror\\|nable' q.log | tail -n 14")


def drv(*args, timeout=300, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True, timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    os.environ["LEANDROS_VIRT"] = "1"
    if not driver._virt_on(driver._accel_flags("aarch64", "uefi")):
        print("kvmqemutest: SKIP (accelerated host: no nested EL2)")
        return 0
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="kvmqemu-")
    env = dict(os.environ, LEANDROS_VIRT="1", LEANDROS_RUN_ID=f"{caller}-kvmqemu" if caller else "kvmqemu")
    img, root = os.path.join(tmp, "boot.img"), os.path.join(tmp, "root.img")
    for src, dst in ((os.path.join(REPO, "leandros-limine-aarch64.img"), img),
                     (os.path.join(REPO, "f2fs-data0-aarch64.img"), root)):
        if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    env.update(LEANDROS_DISK_IMAGE=img, LEANDROS_ROOT_IMAGE=root)
    fails = []

    def check(name, cond, detail=""):
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" ({detail})" if detail and not cond else ""))
        if not cond:
            fails.append(name)

    try:
        rc, out = drv("start", "aarch64", "uefi", env=env)
        check("guest boots with virtualization=on", rc == 0, out[-300:])
        if rc == 0:
            time.sleep(20)
            drv("login", "root", "root", timeout=90, env=env)
            b = base64.b64encode(SETUP.encode()).decode()
            rc, out = drv("run", f"echo {b} | base64 -d > /root/kvmqemu-setup.sh; sh /root/kvmqemu-setup.sh 2>&1 | tail -n 6", "1500", "aarch64", timeout=1700, env=env)
            check("qemu + Alpine kernel + initramfs staged in the guest", "SETUP_DONE" in out, out[-500:])
            if "SETUP_DONE" in out:
                rc, out = drv("run", RUNQ, "900", "aarch64", timeout=1000, env=env)
                print(out[-1800:])
                check("QEMU -accel kvm starts (no KVM ioctl failure)", "KVM_" not in out and "Unexpected error" not in out, out[-400:])
                check("nested guest Linux boots", "Linux version" in out or "Booting Linux" in out, "no kernel banner in q.log")
                check("nested guest reaches the initramfs", "NESTED-GUEST: init started on aarch64" in out, "marker missing")
                check("QEMU exits via PSCI SYSTEM_OFF", "QEMU_RC=0" in out, "rc != 0")
    finally:
        try: drv("stop", timeout=60, env=env)
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"kvmqemutest: {'PASS' if not fails else 'FAIL ' + ', '.join(fails)}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
