#!/usr/bin/env python3
"""espresettest.py [aarch64|x86_64] -- a one-shot trial kernel is booted at
most once even when it never gets to undo the default itself (openbugs #80,
lanes espdur/espdur2). On a copy of the boot image:
  setup  promote a first kernel (good slot), then deploy the same build as
         the one-shot `trial` with `espctl.fault_boot=1`: in the trial boot
         `espctl boot`'s reset of the default fails as if the ESP had
         answered EIO (console: `one-shot reset FAILED`).
  fault  `system_reset` (a power cut); the next boot must be the good slot.
         Limine consumed the one-shot (chain form, conf.rs), so the failed
         userland reset does not matter.
  hang   deploy a trial with `leandros.testhang=1` (the kernel hangs before
         userland: nothing on the medium can change), reboot into it, cut
         the power once it hangs; the next boot must be the good slot and
         `espctl boot` must report the trial as not reaching userland.
         Before the chain form the trial stayed the default and booted
         again -- the laptop's 2026-10-25 symptom.
Exit 0 iff every case PASSes. Run id `<caller run id>-espreset`.
"""
import os, re, subprocess, sys, tempfile, time
import hostclean
import imgclone

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
REMOTE = os.path.join(REPO, "scripts", "remote.py")


def main(argv):
    arch = next((a for a in argv if a in ("aarch64", "x86_64")), os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-espreset" if caller else "espreset"
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

    tmp = tempfile.mkdtemp(prefix="espreset-")

    hostclean.track(tmp)
    img = os.path.join(tmp, f"boot-{arch}.img")
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    imgclone.clone_file(src, img)
    imgclone.ensure_remote_key(img)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img, LEANDROS_REMOTE=f"qemu:{run_id}")
    os.environ["LEANDROS_DISK_IMAGE"] = img

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)

    def remote(*args, timeout=600):
        p = subprocess.run([sys.executable, REMOTE, *args], env=env, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout + p.stderr

    def deploy(*extra, reboot=True):
        out = ""
        for _ in range(3):
            rc, out = remote("deploy-kernel", "--arch", arch, "--netwatch", "0", *(["--reboot"] if reboot else []), *extra, timeout=1500)
            if not reboot:
                if rc == 0:
                    return rc, out
                time.sleep(10)
                continue
            if rc == 0 or "NEW KERNEL" in out:
                return rc, out
            time.sleep(10)
        return rc, out

    try:
        stop()
        subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                       env=env, capture_output=True, text=True, timeout=1200)
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        deploy("--promote")
        subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(180 * scale))],
                       env=env, capture_output=True, text=True, timeout=1200)
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        mark = len(log())
        rc, out = deploy("--append", "espctl.fault_boot=1")
        failed = wait_for(lambda: "one-shot reset FAILED" in log()[mark:], 120 * scale)
        verdict("setup", "NEW KERNEL RUNNING: trial" in out and bool(failed),
                f"deploy rc={rc} injected={bool(failed)}" + ("" if failed else f" ({out[-300:]!r})"))
        if not results["setup"]:
            return 1
        time.sleep(10 * scale)
        cut = len(log())
        driver._monitor_send("system_reset")
        m = wait_for(lambda: re.search(r"espctl: booted (\S+) kernel", log()[cut:]), 400 * scale)
        slot = m.group(1) if m else None
        verdict("fault", slot == "good", f"next boot: {slot}")

        subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(180 * scale))],
                       env=env, capture_output=True, text=True, timeout=1200)
        wait_for(lambda: remote("status", timeout=60)[0] == 0, 180 * scale, 5)
        rc, out = deploy("--append", "leandros.testhang=1", reboot=False)
        mark = len(log())
        remote("reboot", "--no-wait", timeout=120)
        hung = wait_for(lambda: "leandros.testhang=1: hanging before userland" in log()[mark:], 400 * scale)
        if not hung:
            verdict("hang", False, f"trial never hung (deploy rc={rc} {out[-200:]!r})")
            return 1
        time.sleep(5)
        cut = len(log())
        driver._monitor_send("system_reset")
        m = wait_for(lambda: re.search(r"espctl: booted (\S+) kernel", log()[cut:]), 400 * scale)
        slot = m.group(1) if m else None
        noted = wait_for(lambda: "did not reach userland" in log()[cut:], 60 * scale)
        verdict("hang", slot == "good" and bool(noted), f"next boot: {slot} disarm-noted={bool(noted)}")
    finally:
        stop()
    return 0 if results and all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main, sys.argv[1:]))
