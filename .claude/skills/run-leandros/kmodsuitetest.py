#!/usr/bin/env python3
"""kmodsuitetest - real kmodctl load/unload of the drivers that build as modules (lane kmodsuite).

The default image has every driver built in, so this test first makes a private variant of the tree
(reflink copy next to it), writes CONFIG_MODULES=y + VIRTIO_NET/NVME/IWLWIFI/BRCMF=m into the copy's .config and
builds it (KMOD_EXTRA=sdhci adds the Pi-only SDHCI module as a load-only fixture). The default build and the
images of the tree under test are never touched. Then, in a QEMU guest with an extra NVMe disk:
  virtio_net  3 unload/load cycles: eth0 gone after rmmod, back after load, DHCP + ping of the gateway each time
  nvme        load, read a known block of the QEMU nvme disk, rmmod refused (EBUSY) while an fd is open,
              rmmod after close, 3 cycles
  iwlwifi / brcmf / sdhci   load-only smoke without a device: load, listed with refcount 0, rmmod, gone (x2)
  whole run   no panic / oops / data abort / Zombie in the console log
Usage: kmodsuitetest.py [arch]   (default $LEANDROS_TEST_ARCH)
"""
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
rid = os.environ.get("LEANDROS_RUN_ID", "x").replace("/", "_")
VAR = f"{REPO}-kmodvar-{arch}-{os.getpid()}"
MAGIC = b"KMODSUITE-NVME-BLOCK1-OK"
fails = []


def check(name, ok, detail=""):
    print(f"  {'ok  ' if ok else 'FAIL'} {name} {detail}", flush=True)
    if not ok:
        fails.append(name)


def build():
    r = subprocess.run(["cp", "-a", "--reflink=auto", REPO, VAR], capture_output=True, text=True)
    if r.returncode:
        print("FAIL: cp of tree: " + r.stderr[-300:])
        return False
    with open(os.path.join(VAR, ".config"), "w") as f:
        f.write("CONFIG_MODULES=y\nCONFIG_VIRTIO_NET=m\nCONFIG_NVME=m\nCONFIG_IWLWIFI=m\nCONFIG_BRCMF=m\n")
    env = dict(os.environ, KMOD_ONLY="1", KMOD_EXTRA="sdhci" if arch == "aarch64" else "", KMOD_BOARD="raspi4b" if arch == "aarch64" else "", LEANDROS_RUN_ID=f"{rid}-var")
    env.pop("KMOD_TEST", None)
    with open(os.path.join(VAR, "kmodsuite-build.log"), "w") as lg:
        r = subprocess.run(["./scripts/build-all.sh", "--arch", arch], cwd=VAR, env=env, stdout=lg,
                           stderr=subprocess.STDOUT)
    if r.returncode:
        print(f"FAIL: variant build rc={r.returncode}")
        print(subprocess.run(["tail", "-n", "25", os.path.join(VAR, "kmodsuite-build.log")],
                             capture_output=True, text=True).stdout)
        return False
    return True


def main():
    if not build():
        return 1
    nv = os.path.join(VAR, "kmodsuite-nvme.img")
    with open(nv, "wb") as f:
        f.truncate(4 << 20)
        f.seek(512)
        f.write(MAGIC)
    # The farm points the driver at the images of the tree under test through env: boot the variant's own instead.
    env = dict(os.environ, LEANDROS_RUN_ID=f"{rid}-var",
               LEANDROS_DISK_IMAGE=os.path.join(VAR, f"leandros-limine-{arch}.img"),
               LEANDROS_ROOT_IMAGE=os.path.join(VAR, f"f2fs-data0-{arch}.img"),
               LEANDROS_VARS_FD=os.path.join(VAR, f"{arch}_vars.fd"),
               LEANDROS_QEMU_EXTRA=f"-drive if=none,id=kmnv,format=raw,file={nv} -device nvme,drive=kmnv,serial=kmodsuite")
    drv = [sys.executable, os.path.join(VAR, ".claude/skills/run-leandros/driver.py")]

    def d(*a, timeout=300):
        p = subprocess.run(drv + list(a), capture_output=True, text=True, timeout=timeout, env=env, cwd=VAR)
        return p.returncode, p.stdout + p.stderr

    def run(cmd, to=120):
        o = d("run", cmd, str(to), timeout=to + 60)[1]
        # kernel [FORK]/diag lines land in the middle of the serial capture and glue onto the next echo: drop them
        # before matching, and keep the cleaned text in the log so a failure can be read.
        o = "\n".join(l for l in o.splitlines() if not re.match(r"^\[\s*\d+\.\d+\]", l))
        o = re.sub(r"(?<=\d)(?=(RM_RC|LD_RC|BUSY_RC)=)", "\n", o)
        print("    | " + o.strip().replace("\n", " / ")[-400:], flush=True)
        return o

    try:
        rc, out = d("start", arch, timeout=300)
        if rc:
            print("FAIL: boot\n" + out[-600:])
            return 1
        rc, out = d("login", "root", "root", "180", timeout=400)
        if rc:
            print("FAIL: login\n" + out[-600:])
            return 1
        # ---- virtio_net x3
        run("kmodctl load virtio_net >/dev/null 2>&1; sleep 1")
        for i in range(1, 4):
            o = run("kmodctl rmmod virtio_net; echo RM_RC=$?; for j in 1 2 3 4 5 6; do grep -q 'eth0:' /proc/net/dev || break; sleep 1; done; "
                    "grep -c 'eth0:' /proc/net/dev; grep -c '^virtio_net ' /proc/modules; echo GONE_END")
            check(f"virtio_net rmmod #{i}", "RM_RC=0" in o, o.strip()[-120:])
            check(f"virtio_net #{i} eth0 gone after unload", re.search(r"^0\s*$", o, re.M) is not None)
            o = run("kmodctl load virtio_net; echo LD_RC=$?; for j in $(seq 1 30); do grep -q 'eth0:' /proc/net/dev && break; sleep 1; done; "
                    "for j in $(seq 1 20); do ping -c 1 -W 2 10.0.2.2 >/dev/null 2>&1 && echo PING_OK && break; sleep 1; done; echo NET_END", 150)
            check(f"virtio_net load #{i}", "LD_RC=0" in o, o.strip()[-120:])
            check(f"virtio_net #{i} eth0 back and gateway pingable", "PING_OK" in o)
        # ---- nvme
        o = run("grep '^nvme ' /proc/modules; echo BASE_END")
        o = run("kmodctl rmmod nvme >/dev/null 2>&1; kmodctl load nvme; echo LD_RC=$?; for j in $(seq 1 10); do [ -e /dev/nvme0n1 ] && break; sleep 1; done; "
                "dd if=/dev/nvme0n1 bs=512 skip=1 count=1 2>/dev/null | head -c %d; echo; echo NVME_END" % len(MAGIC))
        check("nvme load", "LD_RC=0" in o)
        run("grep '^nvme ' /proc/modules; echo AFTERREAD_END")
        check("nvme known block 1 read back", MAGIC.decode() in o)
        o = run("exec 3</dev/nvme0n1; kmodctl rmmod nvme; echo BUSY_RC=$?; cat /proc/modules | grep '^nvme '; exec 3<&-; "
                "dd if=/dev/nvme0n1 bs=512 skip=1 count=1 2>/dev/null | head -c 5; echo; echo BUSY_END")
        check("nvme rmmod refused while an fd is open", re.search(r"BUSY_RC=[1-9]", o) is not None, o.strip()[-160:])
        check("nvme still Live after the refusal", re.search(r"^nvme .*Live", o, re.M) is not None)
        for i in range(1, 4):
            o = run("kmodctl rmmod nvme; echo RM_RC=$?; ls /dev/nvme0n1 2>&1 | head -n 1; kmodctl load nvme; echo LD_RC=$?; "
                    "for j in $(seq 1 10); do [ -e /dev/nvme0n1 ] && break; sleep 1; done; "
                    "dd if=/dev/nvme0n1 bs=512 skip=1 count=1 2>/dev/null | head -c %d; echo; echo CY_END" % len(MAGIC))
            check(f"nvme cycle #{i} unload", "RM_RC=0" in o, o.strip()[-120:])
            check(f"nvme cycle #{i} node gone then block 1 readable", ("No such" in o or "cannot access" in o) and MAGIC.decode() in o)
        # ---- hardware-less modules: load-only smoke
        for mod in ("iwlwifi", "brcmf") + (("sdhci",) if arch == "aarch64" else ()):
            for i in (1, 2):
                o = run(f"kmodctl load {mod}; echo LD_RC=$?; grep -c '^{mod} ' /proc/modules; echo SM_END")
                # Without the device the init probes and answers ENODEV (or, if it stays loaded, must be rmmod-able
                # with refcount 0). Either way nothing may be left half-registered: not listed, loadable again.
                if "LD_RC=0" in o:
                    o2 = run(f"grep '^{mod} ' /proc/modules; kmodctl rmmod {mod}; echo RM_RC=$?; grep -c '^{mod} ' /proc/modules; echo SM2_END")
                    ref = re.search(rf"^{mod} \d+ (\d+) .*Live", o2, re.M)
                    check(f"{mod} smoke #{i}: loaded, refcount 0, unloaded", "RM_RC=0" in o2 and ref is not None and ref.group(1) == "0"
                          and re.search(r"^0\s*$", o2, re.M) is not None, o2.strip()[-160:])
                else:
                    check(f"{mod} smoke #{i}: no device -> ENODEV and nothing left listed",
                          "No such device" in o and re.search(r"^0\s*$", o, re.M) is not None, o.strip()[-200:])
        log = d("log", timeout=60)[1]
        bad = [l for l in log.splitlines() if re.search(r"panic|PANIC|Oops|Data abort|ZOMBIE|STALE REACHED", l)]
        check("no panic / oops / data abort / Zombie in the console log", not bad, "; ".join(bad[:3]))
    finally:
        try:
            d("stop", timeout=60)
        except subprocess.TimeoutExpired:
            pass
        shutil.rmtree(VAR, ignore_errors=True)
    print(("FAIL kmodsuite_" + arch + ": " + ", ".join(fails)) if fails else f"PASS kmodsuite_{arch}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
