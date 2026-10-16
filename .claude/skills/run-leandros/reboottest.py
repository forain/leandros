#!/usr/bin/env python3
"""reboottest.py [aarch64|x86_64] [--keep] [--cmdline-note] -- reboot(2) and
poweroff in QEMU, through the device-quiesce path (lane rebootq).

In its own boot (run id `<caller run id>-reboot`, `reboot` without one):

  boot1     boot + root login on the serial console
  reboot    `reboot`: the kernel logs its plan (`[REBOOT] restart: quiesce=1
            ...`), the audio step, the disk flush and the xHCI/bus-master
            phase, then resets (x86_64: 0xCF9 full reset by default; aarch64:
            PSCI SYSTEM_RESET); QEMU must NOT exit
  boot2     a second login prompt appears in the same QEMU process; root
            login works and /proc/uptime is small (a fresh boot)
  poweroff  `poweroff`: `[REBOOT] power-off: ...` then QEMU exits by itself

Exit 0 iff all PASS. QEMU is started without -no-reboot (driver.py's
default for UEFI boots), so a guest reset stays inside one QEMU process.
"""
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")


def main(argv):
    arch, keep = "aarch64", False
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    run_id = f"{caller}-reboot" if caller else "reboot"
    env = dict(os.environ, LEANDROS_RUN_ID=run_id)
    env.pop("LEANDROS_NO_REBOOT", None)
    os.environ.update({"LEANDROS_RUN_ID": run_id})
    os.environ.pop("LEANDROS_NO_REBOOT", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (scoped to the reboot run id)
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def login():
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            env=env, capture_output=True, text=True, timeout=1200)
        return lg.returncode == 0

    def log_since(mark):
        try:
            with open(driver.SERIAL_FULL_LOG, "rb") as f:
                f.seek(mark)
                return f.read().decode(errors="replace")
        except FileNotFoundError:
            return ""

    def log_size():
        try:
            return os.path.getsize(driver.SERIAL_FULL_LOG)
        except FileNotFoundError:
            return 0

    try:
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        ok = r.returncode == 0 and login()
        verdict("boot1", ok, f"{time.time() - t0:.0f} s")
        if not ok:
            print(r.stdout[-400:], r.stderr[-400:])
            return 1
        pid1 = driver._qemu_pid()

        mark = log_size()
        try:
            driver.serial_run("reboot", timeout=20 * scale, arch=arch)
        except Exception:  # noqa: BLE001  (the shell dies with the system)
            pass
        # Wait for the reset line and the next boot's banner.
        t1 = time.time()
        text = ""
        while time.time() - t1 < 120 * scale:
            text = log_since(mark)
            if "Restarting system" in text and re.search(r"login:", text.split("Restarting system", 1)[1]):
                break
            if driver._qemu_pid() is None:
                break
            time.sleep(2)
        plan = re.search(r"\[REBOOT\] restart: [^\r\n]*", text)
        audio = re.search(r"\[REBOOT\] audio: [^\r\n]*", text)
        p1 = re.search(r"\[REBOOT\] phase 1 done[^\r\n]*", text)
        how = re.search(r"reboot: (0xCF9[^\r\n]*|ACPI RESET_REG|8042 pulse|triple fault)", text)
        alive = driver._qemu_pid() is not None and driver._qemu_pid() == pid1
        verdict("reboot", bool(plan and audio and p1 and alive),
                "; ".join(m.group(0).strip() for m in (plan, audio, p1, how) if m) + ("" if alive else " QEMU EXITED"))
        if not alive:
            return 1
        ok = login()
        up = None
        if ok:
            rr = driver.serial_run("cat /proc/uptime", timeout=30 * scale, arch=arch)
            m = re.search(r"^(\d+)\.", rr.get("output", ""), re.M)
            up = int(m.group(1)) if m else None
        verdict("boot2", ok and up is not None and up < 600 * scale, f"uptime {up}")

        mark = log_size()
        try:
            driver.serial_run("poweroff", timeout=20 * scale, arch=arch)
        except Exception:  # noqa: BLE001
            pass
        t2 = time.time()
        while time.time() - t2 < 90 * scale and driver._qemu_pid() is not None:
            time.sleep(1)
        text = log_since(mark)
        po = re.search(r"\[REBOOT\] power-off: [^\r\n]*", text)
        verdict("poweroff", driver._qemu_pid() is None and po is not None,
                (po.group(0).strip() if po else "no [REBOOT] power-off line") +
                ("" if driver._qemu_pid() is None else " QEMU still running"))
    finally:
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
    ok = bool(results) and all(results.values())
    print(f"reboottest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
