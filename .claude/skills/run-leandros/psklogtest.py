#!/usr/bin/env python3
"""psklogtest - the Wi-Fi PSK given on the kernel command line never reaches a log.

Boots a copy of the image with `wifi.ssid=PskLeakNet wifi.psk=<FAKE>` (a fake,
test-only passphrase) and asserts that FAKE does not appear in:
  serial      the whole serial log of the boot (= what the persistent klog
              partition records: every console byte is copied there)
  klog        `espctl klog` in the guest (the persistent log, when present)
  proc_user   /proc/cmdline read by the non-root user `leandro`
              (must show `wifi.psk=<redacted len=N>` instead)
Positive control: root's /proc/cmdline still has the real value (kexec
--reuse-cmdline depends on it), so the knob really was on the line.

Usage: psklogtest.py [aarch64|x86_64]    (REPO=<checkout> to test another tree)
"""
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
import imgclone  # noqa: E402

FAKE = "testpsk-0123456789"
CMDLINE = f"wifi.ssid=PskLeakNet wifi.psk={FAKE}"


def main(argv):
    arch = argv[0] if argv and argv[0] in ("aarch64", "x86_64") else "aarch64"
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-psklog" if caller else "psklog"
    tmp = tempfile.mkdtemp(prefix="psklog-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if subprocess.run(["cp", "-c", src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, CMDLINE], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"psklogtest: could not set the command line: {r.stdout[-300:]}{r.stderr[-300:]}")
        return 1
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root})
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    import driver  # noqa: E402  (scoped to this run id)
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"psklog_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    try:
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"psklogtest: boot failed: {r.stdout[-300:]}{r.stderr[-300:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("psklogtest: login failed")
            return 1
        time.sleep(5)  # let the Wi-Fi/netctl startup lines land
        with open(driver.SERIAL_FULL_LOG, "rb") as f:
            boot_log = f.read().decode(errors="replace")  # boot + login, before any command of ours
        _, out = sh("cat /proc/cmdline")
        verdict("proc_root_control", FAKE in out, "(root sees the real knob)" if FAKE in out else "(knob missing: test invalid)")
        _, out = sh("su leandro -c 'id -u; cat /proc/cmdline'")
        ok = FAKE not in out and "wifi.psk=<redacted len=18>" in out and "1000" in out
        verdict("proc_user", ok, "(non-root /proc/cmdline redacted)" if ok else "(non-root sees the PSK or no redaction marker)")
        sh("espctl klog > /tmp/psklog-k.txt 2>&1; true", 90)
        def count(c):
            _, o = sh(c, 30)
            return next((w for w in reversed(o.split()) if w.isdigit()), "?")
        kl = [count("wc -c < /tmp/psklog-k.txt"), count("grep 'kernel cmdline' /tmp/psklog-k.txt | wc -l"),
              count("grep -- '" + FAKE + "' /tmp/psklog-k.txt | wc -l")]
        verdict("klog", kl[2] == "0", f"(espctl klog: {kl[0]} bytes, cmdline lines {kl[1]}, fake hits {kl[2]})")
        hits = [l for l in boot_log.splitlines() if FAKE in l]
        verdict("serial", not hits, f"({len(hits)} boot serial line(s) with the fake PSK"
                + (f", first: {hits[0][:60].replace(FAKE, '<FAKE>')!r})" if hits else ")"))
    finally:
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
    bad = [k for k, v in results.items() if not v]
    print(f"psklogtest: {len(results) - len(bad)}/{len(results)} passed" + (f"; FAILED {bad}" if bad else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
