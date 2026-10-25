#!/usr/bin/env python3
"""trialwatchtest.py [aarch64|x86_64] [--keep] -- the netwatch trial watch.

End to end through the real one-shot path, on a copy of the boot image:
  setup  boot (unmanaged), then `remote.py deploy-kernel --netwatch 120
         --append 'netwatch.reach=40 netwatch.every=5' --reboot` against the
         VM: espctl installs the same build as the `trial` slot (and the
         flashed entry as `good`), the VM reboots into the trial, and the
         console shows `[netwatch] trial watch: reach=40 s`.
  blip   `set_link net0 off` for 20 s, then on: the trial must NOT reboot
         (no `unreachable` line, no new `[netwatch] armed`) for 60 s more.
  loss   `set_link net0 off` until the console shows `[netwatch] <gw>
         unreachable for N s ... rebooting to the fallback entry` (N >= 40),
         link back on, and the next boot (same QEMU process) must be the
         good slot: `espctl status` says booted_slot=good and no
         `[netwatch] armed` is printed by it.
  promote a second trial with `netwatch.trial_max=1` is deployed and promoted
         at once: `[netwatch] disarmed (promoted)` must appear and the VM
         must still be up 90 s later (trial_max would have fired at 60 s).
Exit 0 iff every case PASSes. Run id `<caller run id>-trialwatch`.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

import imgclone

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
REMOTE = os.path.join(REPO, "scripts", "remote.py")
ARMED = "[netwatch] armed"


def main(argv):
    arch = next((a for a in argv if a in ("aarch64", "x86_64")), os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    keep = "--keep" in argv
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-trialwatch" if caller else "trialwatch"
    os.environ["LEANDROS_RUN_ID"] = run_id
    os.environ.pop("LEANDROS_NO_REBOOT", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def log():
        try:
            with open(driver.SERIAL_FULL_LOG, "rb") as f:
                return f.read().decode(errors="replace")
        except FileNotFoundError:
            return ""

    def wait_for(pred, secs, step=1.0):
        t0 = time.time()
        while time.time() - t0 < secs:
            v = pred()
            if v:
                return v
            time.sleep(step)
        return None

    tmp = tempfile.mkdtemp(prefix="trialwatch-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    subprocess.run(["cp", "-c", src, img], check=False)
    if not os.path.exists(img):
        imgclone.clone_file(src, img)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img, LEANDROS_REMOTE=f"qemu:{run_id}")
    os.environ["LEANDROS_DISK_IMAGE"] = img

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)

    def remote(*args, timeout=600):
        p = subprocess.run([sys.executable, REMOTE, *args], env=env, capture_output=True, text=True,
                           timeout=timeout)
        return p.returncode, p.stdout + p.stderr

    try:
        stop()
        subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            env=env, capture_output=True, text=True, timeout=1200)
        pid = driver._qemu_pid()
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        # seed: promote a first kernel so a `good` slot exists, as on the laptop
        for _ in range(3):
            rc, out = remote("deploy-kernel", "--arch", arch, "--netwatch", "0", "--reboot", "--promote", timeout=1500)
            if rc == 0 or "NEW KERNEL" in out:
                break
            time.sleep(10)
        subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(180 * scale))],
                       env=env, capture_output=True, text=True, timeout=1200)
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        rc, st = remote("espctl", "status", timeout=120)
        print(f"seed rc={rc} {st.strip()[-200:]!r}", flush=True)
        for _ in range(3):  # sshd may still be settling right after the first login
            rc, out = remote("deploy-kernel", "--arch", arch, "--netwatch", "120",
                             "--append", "netwatch.reach=40 netwatch.every=5", "--reboot", timeout=1500)
            if rc == 0 or "NEW KERNEL" in out:
                break
            print(f"deploy retry: {out.strip().splitlines()[-1][:120]!r}", flush=True)
            time.sleep(10)
        text = log()
        trial = "NEW KERNEL RUNNING: trial" in out and "[netwatch] trial watch: reach=40 s" in text
        verdict("setup", lg.returncode == 0 and rc == 0 and trial,
                f"deploy rc={rc} trial={trial}" + ("" if trial else f" ({out[-400:]!r})"))
        if not results["setup"]:
            return 1
        wait_for(lambda: "[NET] DHCP configured" in log()[log().rfind(ARMED):], 60 * scale)
        time.sleep(15)

        # blip: 20 s off must not fire
        armed0 = log().count(ARMED)
        driver._monitor_send("set_link net0 off")
        time.sleep(20)
        driver._monitor_send("set_link net0 on")
        time.sleep(60 * scale)
        text = log()
        noticed = "not answering" in text
        quiet = "unreachable for" not in text and text.count(ARMED) == armed0
        verdict("blip", quiet and driver._qemu_pid() == pid,
                f"quiet={quiet} noticed={noticed} answers_again={'answers again' in text}")

        # loss: stays off -> reboot after >= 40 s, next boot is good
        wait_for(lambda: "[NET] DHCP configured" in log()[-4000:] or "answers again" in log(), 30 * scale)
        t1 = time.time()
        driver._monitor_send("set_link net0 off")
        pat = r"\[netwatch\] (\S+) unreachable for (\d+) s[^\r\n]*rebooting to the fallback entry"
        m = wait_for(lambda: re.search(pat, log()), 150 * scale, 0.5)
        elapsed = time.time() - t1
        driver._monitor_send("set_link net0 on")
        booted = wait_for(lambda: log().count("Starting init") > text.count("Starting init")
                          or log().count("Welcome") > text.count("Welcome"), 180 * scale)
        lg2 = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(180 * scale))],
                             env=env, capture_output=True, text=True, timeout=1200)
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        rc, st = remote("espctl", "status", timeout=120)
        slot = re.search(r"booted_slot=(\S+)", st)
        slot = slot.group(1) if slot else "?"
        after = log()[m.end():] if m else ""
        verdict("loss", bool(m) and int(m.group(2)) >= 40 and slot == "good" and 40 - 5 - 1 <= elapsed <= 40 + 2 * 5 + 2 and ARMED not in after
                and driver._qemu_pid() == pid,
                (f"'{m.group(0)[:90]}' after {elapsed:.0f} s; " if m else "no unreachable line; ")
                + f"reboot at {elapsed:.0f} s (want 34..52: counted from the last answered probe, up to every=5 s before the cut); next boot slot={slot} armed_again={ARMED in after} booted={bool(booted)} login={lg2.returncode}")

        # promote: a promoted trial must never reboot itself (trial_max=1 min would)
        armed1 = log().count(ARMED)
        rc, out = remote("deploy-kernel", "--arch", arch, "--netwatch", "120", "--trial-max", "1",
                         "--reboot", "--promote", timeout=1500)
        dis = wait_for(lambda: "[netwatch] disarmed (promoted)" in log(), 30 * scale)
        t2 = time.time()
        time.sleep(max(0, 95 - (time.time() - t2)))
        text = log()
        fired = "trial not promoted within" in text
        rc2, st = remote("espctl", "status", timeout=120)
        verdict("promote", rc == 0 and bool(dis) and not fired and text.count(ARMED) == armed1 + 1
                and "booted_slot=trial" in st and driver._qemu_pid() == pid,
                f"deploy rc={rc} disarmed={bool(dis)} trial_max_fired={fired} armed={text.count(ARMED) - armed1}"
                f" status_rc={rc2}")
    finally:
        if not keep:
            stop()
            shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"trialwatchtest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
