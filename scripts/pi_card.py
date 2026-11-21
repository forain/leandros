"""remote.py pi-card: update the Pi 5 CARD's kernel through firmware tryboot.

Verbs: status | setup | stage | try | commit | abort | rollback (all take
--dry-run). The card side is `espctl pi ...` (userland/espctl/src/picard.rs);
this file is the Mac-side gate: nothing reaches the card unless the build is a
self-contained rpi5 direct-boot kernel that the Pi has already kexec-verified.

Secrets: the kernel command line carries the Wi-Fi PSK. Nothing here prints it,
or any cmdline*.txt; the trial marker is checked with grep -qw on the Pi.
"""
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

SLOT_KERNEL = 32 << 20
SLOT_INITRD = 16 << 20
GOOD_DIR = "/boot/pi/good"
STAGE_DIR = "/boot/pi/card-stage"


def pi_hosts():
    """Addresses to try after a reboot: LEANDROS_PI_HOST, a comma-separated list
    (e.g. wired,wifi) when the Pi can come back on another interface."""
    hosts = [h.strip() for h in os.environ.get("LEANDROS_PI_HOST", "").split(",") if h.strip()]
    if not hosts:
        sys.exit("pi_card: set LEANDROS_PI_HOST=<ip>[,<alt-ip>] (the Pi's address after a reboot)")
    return hosts
MARKER = "leandros.tryboot=1"


class Refuse(Exception):
    pass


def sha256(b):
    return hashlib.sha256(b).hexdigest()


def cpio_names(data):
    """Entry names of a newc (070701/070702) archive."""
    names, o = [], 0
    while o + 110 <= len(data):
        if data[o:o + 6] not in (b"070701", b"070702"):
            raise Refuse("initrd is not a newc cpio archive")
        fs = int(data[o + 54:o + 62], 16)
        ns = int(data[o + 94:o + 102], 16)
        name = data[o + 110:o + 110 + ns - 1].decode(errors="replace")
        if name == "TRAILER!!!":
            break
        names.append(name)
        o = (o + 110 + ns + 3) & ~3
        o = (o + fs + 3) & ~3
    return names


def check_self_contained(initrd, kconfig_path):
    """A card kernel must not need modules: no lib/modules in the initrd, no =m
    in the build's kconfig, and the kconfig must be an rpi5 one."""
    if any(n.lstrip("./").startswith("lib/modules") for n in cpio_names(initrd)):
        raise Refuse("the initrd carries lib/modules: a card kernel must be self-contained (no =m)")
    try:
        cfg = open(kconfig_path).read()
    except OSError:
        raise Refuse(f"{kconfig_path} missing: cannot prove the build has no =m modules")
    if re.search(r"^CONFIG_\w+=m\b", cfg, re.M):
        raise Refuse("the kernel build has =m options: a card kernel must be self-contained")
    if not re.search(r"^CONFIG_\w*RPI5\w*=y\b", cfg, re.M | re.I):
        raise Refuse(f"{kconfig_path} is not an rpi5 configuration")


def build_tag(elf):
    """BUILD_TAG (<rev>[-dirty]-YYYYMMDDTHHMMZ, build-all.sh) embedded in the ELF."""
    found = re.findall(rb"[0-9a-f]{7,12}(?:-dirty)?-\d{8}T\d{4}Z", elf)
    if not found:
        raise Refuse("no BUILD_TAG found in the kernel ELF (build with build-all.sh)")
    return max(set(found), key=found.count).decode()


def objcopy_binary(elf_path):
    """`llvm-objcopy -O binary` of the ELF, the way build-all.sh makes kernel-direct.bin."""
    tool = shutil.which("llvm-objcopy") or shutil.which("rust-objcopy")
    if not tool:
        try:
            sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True).stdout.strip()
            for root, _d, files in os.walk(os.path.join(sysroot, "lib", "rustlib")):
                if "llvm-objcopy" in files:
                    tool = os.path.join(root, "llvm-objcopy")
                    break
        except OSError:
            pass
    if not tool:
        raise Refuse("no llvm-objcopy found to re-derive kernel-direct.bin")
    with tempfile.TemporaryDirectory() as d:
        out = os.path.join(d, "k.bin")
        subprocess.run([tool, "-O", "binary", elf_path, out], check=True)
        return open(out, "rb").read()


def gates_stage(elf_path, bin_path, initrd_path, kconfig_path, good, unverified=False,
                check_elf=None, bid=None, objcopy=objcopy_binary):
    """All Mac-side stage gates. `good` = {'kernel': sha, 'initrd': sha, 'tag': tag|None}
    read from the Pi. Returns the staging facts."""
    if check_elf:
        check_elf(elf_path)                                    # 1
    elf = open(elf_path, "rb").read()
    kbin = open(bin_path, "rb").read()
    if kbin != objcopy(elf_path):                              # 2
        raise Refuse(f"{bin_path} differs from llvm-objcopy -O binary of {elf_path}")
    initrd = open(initrd_path, "rb").read()
    if bid:
        bid(elf_path, initrd_path)                             # 3
    check_self_contained(initrd, kconfig_path)                 # 4
    tag = build_tag(elf)                                       # 5
    if good.get("tag") and tag == good["tag"]:
        raise Refuse(f"BUILD_TAG {tag} equals the good slot's: nothing new to try")
    if not unverified and sha256(elf) != good["kernel"]:       # 6
        raise Refuse(f"the kernel ELF is not {GOOD_DIR}/kernel on the Pi: only a kexec-verified kernel "
                     f"may reach the card (pi-deploy + pi-promote first, or --unverified)")
    if sha256(initrd) != good["initrd"]:                       # M6
        raise Refuse(f"the initrd is not {GOOD_DIR}/initrd on the Pi")
    if len(kbin) > SLOT_KERNEL or len(initrd) > SLOT_INITRD:   # 7
        raise Refuse("kernel or initrd larger than the card slot")
    return {"tag": tag, "ksha": sha256(kbin), "isha": sha256(initrd), "klen": len(kbin), "ilen": len(initrd)}


def pi_status_cmd(pi_dir="/boot/pi"):
    """pi-status shell command: the kernel cmdline is redacted to length + sha8 + marker."""
    return ("uname -v; "
            "echo \"cmdline: $(wc -c < /proc/cmdline | tr -d ' ') bytes, sha256 $(sha256sum /proc/cmdline | cut -c1-8), "
            f"tryboot-marker=$(grep -qw {MARKER} /proc/cmdline && echo yes || echo no)\"; "
            "echo \"tryboot: $(grep -vq state=off /proc/tryboot 2>/dev/null && cat /proc/tryboot || echo off)\"; "
            "echo booted slot: $(cat /tmp/.pi-booted-slot 2>/dev/null || echo card); "
            f"for s in trial good prev; do echo \"$s: $(cat {pi_dir}/$s/info 2>/dev/null || echo -)\"; done")


def parse_good(status_text):
    m = re.search(r"^s\d state=good\s+tag=(\S+)", status_text, re.M)
    return m.group(1) if m else None


def fetch_good(t):
    out = t.out(f"sha256sum {GOOD_DIR}/kernel {GOOD_DIR}/initrd").split()
    if len(out) < 4:
        raise Refuse(f"{GOOD_DIR}/kernel or initrd missing on the Pi")
    tag = None
    p = t.run("espctl pi status", check=False)
    if p is not None and p.returncode == 0:
        tag = parse_good(p.stdout.decode(errors="replace"))
    return {"kernel": out[0], "initrd": out[2], "tag": tag}


def _wait_back(R, t, timeout, expect_tag):
    """After a tryboot reset: wired first, then Wi-Fi. Returns (host, uname) or None."""
    t0 = time.time()
    time.sleep(5)
    while time.time() - t0 < timeout:
        for h in pi_hosts():
            tt = R.Target(f"pi:{h}")
            if R.probe(tt, "true") == 0:
                return tt, tt.out("uname -v").strip()
        time.sleep(3)
    return None


def main(t, R, a):
    verb = a.verb
    dry = ["--dry-run"] if a.dry_run else []
    esp = lambda s: " ".join(["espctl pi", s] + dry)
    try:
        if verb == "status":
            print(t.out("espctl pi status").rstrip())
        elif verb == "setup":
            extra = " --accept-root-sector-risk" if a.accept_root_sector_risk else ""
            print(t.out(esp("setup" + extra)).rstrip())
        elif verb == "stage":
            elf = a.kernel or os.path.join(R.REPO, "target/final-aarch64/kernel-direct")
            kbin = a.kernel_bin or elf + ".bin"
            initrd = a.initrd or os.path.join(R.REPO, "initrd-aarch64.cpio")
            kconf = a.kconfig or os.path.join(R.REPO, "target/kconfig-aarch64/.config")
            good = fetch_good(t)
            f = gates_stage(elf, kbin, initrd, kconf, good, a.unverified, check_elf=R.pi_check_elf, bid=R.bid_check)
            print(f"gates passed: tag {f['tag']}, kernel {f['klen']} B, initrd {f['ilen']} B")
            cmd = (f"espctl pi stage auto --kernel {STAGE_DIR}/kernel --initrd {STAGE_DIR}/initrd --tag {f['tag']} "
                   f"--sha256 {f['ksha']} --initrd-sha256 {f['isha']}")
            if a.dry_run:
                print(f"[dry-run] would upload {kbin} and {initrd} to {STAGE_DIR} and run: {cmd} --dry-run")
                return 0
            t.run(f"mkdir -p {STAGE_DIR}")
            R.upload(t, [(kbin, f"{STAGE_DIR}/kernel", None), (initrd, f"{STAGE_DIR}/initrd", None)])
            print(t.out(cmd).rstrip())
        elif verb == "try":
            before = t.out("uname -v").strip()
            if a.dry_run:
                print(t.out(esp("try")).rstrip())
                return 0
            print(f"requesting the tryboot reset on {t.label()} (was: {before})")
            t.run(esp("try"), check=False, timeout=30, master=False)
            t.close_master()
            r = _wait_back(R, t, a.timeout, None)
            if r is None:
                print("NOTHING ANSWERS: power-cycle the Pi; the flag is one-shot, so the card boots config.txt (the good slot).", file=sys.stderr)
                return 2
            tt, after = r
            took = tt.run(f"grep -qw {MARKER} /proc/cmdline || grep -qw leandros.tryboot=noprefix /proc/cmdline", check=False)
            if took is not None and took.returncode == 0:
                print(f"TRY OK on {tt.host}: {after} (trial marker present). `pi-card commit` to keep it, `pi-card abort` to go back.")
                return 0
            print(f"TRY DID NOT TAKE on {tt.host}: {after} (no trial marker): the firmware did not read tryboot.txt, or the trial reset back.", file=sys.stderr)
            return 3
        elif verb == "commit":
            print(t.out(esp("commit")).rstrip())
        elif verb == "abort":
            print(t.out(esp("abort")).rstrip() if a.dry_run else "")
            if not a.dry_run:
                t.run(esp("abort"), check=False, timeout=30, master=False)
        elif verb == "rollback":
            print(t.out(esp("rollback")).rstrip())
    except Refuse as e:
        R.die(f"pi-card {verb}: refused: {e}")
    return 0
