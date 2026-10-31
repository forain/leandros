#!/usr/bin/env python3
"""kexecfdttest.py -- the kexec FDT block is returned to the map (lane kexecfdt, aarch64).

Each LeandrOS kexec FDT leaves its own 16 KiB block (FDT + AP park page) out of
/memory. Before the fix the next kernel never took it back, so holes piled up
along a kexec chain and `kexec -l` rejected images reaching past one.

Chain gen0 -> gen1 -> gen2 -> gen3 -> gen4 over the serial console, with the
debug knob kexec.fdt_low=1 (FDT right above the image footprint):
  gen1 loads gen2 with the FDT low (X1); gen3 then loads an image whose last
  PT_LOAD memsz is patched to end past X1 (rc must be 0) and kexecs into it.
Asserts "[KEXEC] boot FDT block ... returned to the map" in gen1..gen4.
KEXECFDT_NOFIX=1 adds kexec.keep_fdt_hole=1 to gen1/gen2 (the guard must FAIL).
The VM ends in the kexec'd kernel: run alone (runtests kexecfdt).
"""
import os
import re
import struct
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

REPO = driver.REPO_ROOT
PUSH = [sys.executable, os.path.join(REPO, "scripts/pi-serial-push.py"), "--port", "unix:" + driver.SERIAL_SOCK]
ENV = dict(os.environ, LEANDROS_PI_SERIAL="unix:" + driver.SERIAL_SOCK, LEANDROS_PI_LOGIN="root:root")
KEEP = " kexec.keep_fdt_hole=1" if os.environ.get("KEXECFDT_NOFIX") == "1" else ""
KX = "/bin/kexec -l /boot/kexec --initrd=/boot/kexec-initrd"


def guest(cmd, timeout=120):
    r = subprocess.run(PUSH + ["--timeout", str(timeout), "--cmd", cmd], capture_output=True, text=True, env=ENV)
    return r.returncode, (r.stdout + r.stderr)


def kexec_exec(extra):
    r = subprocess.run(PUSH + ["--timeout", "240", "--kexec-exec", extra], capture_output=True, text=True, env=ENV)
    return r.returncode


def log_from(mark):
    with open(driver.SERIAL_FULL_LOG, "rb") as f:
        f.seek(mark)
        return f.read().decode("latin-1")


def mark():
    return os.path.getsize(driver.SERIAL_FULL_LOG)


def last_pt_load(path):
    d = open(path, "rb").read()
    phoff, phentsize, phnum = struct.unpack_from("<Q", d, 32)[0], *struct.unpack_from("<HH", d, 54)
    best = None
    for i in range(phnum):
        o = phoff + i * phentsize
        if struct.unpack_from("<I", d, o)[0] != 1:
            continue
        paddr, memsz = struct.unpack_from("<QQ", d, o + 24)[0], struct.unpack_from("<Q", d, o + 40)[0]
        if memsz and (best is None or paddr + memsz > best[1] + best[2]):
            best = (o + 40, paddr, memsz)
    return best  # (file offset of p_memsz, paddr, memsz)


def main():
    fails = []
    if "x86_64" in guest("uname -m")[1]:
        # x86_64 has no FDT; just prove the chain gen0 -> gen1 -> gen2 still works.
        rc, out = guest(f"{KX} --reuse-cmdline --append='kexec.gen=1'")
        print("  x86 load", rc, flush=True)
        r1 = kexec_exec("kexec.gen=1")
        rc, out = guest(f"{KX} --reuse-cmdline --append='kexec.gen=2'")
        r2 = kexec_exec("kexec.gen=2")
        print("RESULT:", "PASS" if (r1 == 0 and r2 == 0 and rc == 0) else "FAIL x86 chain")
        return 0 if (r1 == 0 and r2 == 0 and rc == 0) else 1

    def check(name, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {name} {detail}", flush=True)
        if not ok:
            fails.append(name)

    # gen0 -> gen1 (fdt_low on in gen1, so gen1's FDT for gen2 sits above its image).
    rc, out = guest(f"{KX} --reuse-cmdline --append='kexec.fdt_low=1 kexec.gen=1{KEEP}'")
    check("gen0 load", rc == 0, out.strip()[-80:])
    m = mark()
    check("gen0 -> gen1", kexec_exec("kexec.gen=1") == 0)
    # gen1 loads gen2 without --reuse-cmdline, so gen2 carries no knob but KEEP.
    m1 = mark()
    rc, out = guest(f"{KX} --append='kexec.gen=2{KEEP}'")
    lg = log_from(m1)
    mm = re.search(r"\[KEXEC\] loaded:.*?bootinfo=(0x[0-9a-fA-F]+)", lg)
    seg = re.findall(r"dest seg: (0x[0-9a-fA-F]+)-(0x[0-9a-fA-F]+)", lg)
    check("gen1 load", rc == 0 and mm and seg, out.strip()[-80:])
    if not (mm and seg):
        print("RESULT: FAIL " + ", ".join(fails))
        return 1
    x1 = int(mm.group(1), 16)
    seg_end = max(int(e, 16) for _, e in seg)
    print(f"  gen1: image ends {seg_end:#x}, FDT block X1={x1:#x}", flush=True)
    check("X1 above image", x1 >= seg_end)
    check("gen1 -> gen2", kexec_exec("kexec.gen=2") == 0)
    # gen2: plain load for gen3 (its own FDT is wherever the allocator puts it).
    rc, out = guest(f"{KX} --append='kexec.gen=3'")
    check("gen2 load", rc == 0, out.strip()[-80:])
    check("gen2 -> gen3", kexec_exec("kexec.gen=3") == 0)
    # gen3: the image reaching past X1.
    pt = last_pt_load(os.path.join(REPO, "target/final-aarch64/kernel-direct"))
    off, paddr, memsz = pt
    new_end = x1 + 0x4000
    check("patch fits", new_end > paddr + memsz, f"(image ends {paddr + memsz:#x}, wanted {new_end:#x})")
    hexb = "".join(f"\\x{b:02x}" for b in struct.pack("<Q", new_end - paddr))
    rc, out = guest(f"cp /boot/kexec /tmp/k.elf && printf '{hexb}' | dd of=/tmp/k.elf bs=1 seek={off} conv=notrunc 2>&1 | tail -n 1")
    check("patch image", rc == 0, out.strip()[-60:])
    m3 = mark()
    rc, out = guest(f"/bin/kexec -l /tmp/k.elf --initrd=/boot/kexec-initrd --append='kexec.gen=4 kexec.test_straggler=3'; echo KRC=$?")
    lg3 = log_from(m3)
    krc = re.search(r"KRC=(\d+)", out)
    check("gen3 kexec -l past X1 rc=0", bool(krc) and krc.group(1) == "0", out.strip()[-120:] + " ".join(re.findall(r"destination [^\n]*", lg3))[:100])
    if krc and krc.group(1) == "0":
        check("gen3 -> gen4 (patched image)", kexec_exec("kexec.gen=4") == 0)
    # lane pikexec: gen4 kexecs with kexec.test_straggler=3 (a CPU spinning in
    # uart::getc, IRQs masked, RX lock held). gen5 must come up with 4 CPUs.
    if krc and krc.group(1) == "0":
        rc, out = guest(f"{KX} --append='kexec.gen=5 kexec.test_straggler=3 kexec.test_straggler_mode=sleep'")
        check("gen4 load", rc == 0, out.strip()[-80:])
        check("gen4 -> gen5 (straggler CPU)", kexec_exec("kexec.gen=5") == 0)
        rc, out = guest("grep -c '^processor' /proc/cpuinfo")
        check("gen5 has 4 CPUs online", out.split()[-1:] == ["4"], out.strip()[-40:])
    # lane pikexec3: gen5 kexecs with a CPU looping msleep(1) under a spin::Mutex
    # (the brcmf radio loop); it must still park. gen6 must have 4 CPUs.
    if krc and krc.group(1) == "0":
        rc, out = guest(f"{KX} --append='kexec.gen=6'")
        check("gen5 load (sleep straggler)", rc == 0, out.strip()[-80:])
        check("gen5 -> gen6 (msleep straggler)", kexec_exec("kexec.gen=6") == 0)
        rc, out = guest("grep -c '^processor' /proc/cpuinfo")
        check("gen6 has 4 CPUs online", out.split()[-1:] == ["4"], out.strip()[-40:])
        check("sleep straggler path exercised", "looping msleep(1) holding a spin::Mutex" in log_from(0))
    full = log_from(0)
    check("straggler path exercised", "straggler spinning in uart::getc" in full)
    for bad in ("never reached the park page", "never came online", "still in the old kernel"):
        check("no '" + bad + "'", bad not in full)
    n = len(re.findall(r"\[KEXEC\] boot FDT block 0x[0-9a-fA-F]+-0x[0-9a-fA-F]+ returned to the map", full))
    check("FDT block returned in gen1..gen4", n >= 4 if not KEEP else True, f"(count {n})")
    print("RESULT:", "PASS" if not fails else "FAIL " + ", ".join(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
