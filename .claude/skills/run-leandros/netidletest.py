"""netidletest.py [aarch64|x86_64] [--keep] -- idle wake bound for the in-kernel net daemon.

Boots normally, logs in, waits for the DHCP lease, lets the system settle, then samples the daemon's
pass counter (`netd: passes N` in /proc/net/nic) across a 20 s guest-side sleep with no network traffic.
  idle: daemon passes/s must stay below 10 (the old fixed 10 ms idle cadence gave ~100/s).
Also reports netctl_wakes/s. Exit 0 iff PASS.  Run id `<caller run id>-netidle`.
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
BOUND = 10.0
WINDOW = 20


def main(argv):
    arch = "x86_64" if "x86_64" in argv else ("aarch64" if "aarch64" in argv else os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    keep = "--keep" in argv
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    os.environ["LEANDROS_RUN_ID"] = f"{caller}-netidle" if caller else "netidle"
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")

    def sh(cmd, t=30):
        return driver.serial_run(cmd, timeout=t * scale, arch=arch).get("output", "")

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    ok, detail = False, "no sample"
    try:
        stop()
        subprocess.run([sys.executable, DRV, "start", arch], capture_output=True, text=True, timeout=900)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            capture_output=True, text=True, timeout=1200)
        import time
        t0 = time.time()
        while time.time() - t0 < 120 * scale and "dhcp: bound" not in sh("cat /proc/net/nic"):
            time.sleep(2)
        time.sleep(10)
        probe = "read u r < /proc/uptime; echo NI $u; cat /proc/net/nic; echo WS-BEGIN; cat /proc/wakestat | grep -v '^#'; echo WS-END"
        out = sh(f"{probe}; sleep {WINDOW}; {probe}", WINDOW + 60)
        s = [(float(m.group(1)), int(m.group(2)), int(m.group(3)))
             for m in re.finditer(r"^NI (\d+\.\d+)\n(?:.*\n)*?netd: passes (\d+) .*?netctl_wakes (\d+)", out, re.M)]
        if lg.returncode == 0 and len(s) == 2 and s[1][0] > s[0][0]:
            dt = s[1][0] - s[0][0]
            rate, nrate = (s[1][1] - s[0][1]) / dt, (s[1][2] - s[0][2]) / dt
            ok = rate < BOUND
            why = re.findall(r"^netd_why: .*$", out, re.M)
            print("netd_why before/after: " + " | ".join(why), flush=True)
            irqs = [int(x) for x in re.findall(r"virtio_rx_irqs (\d+)", out)]
            if len(irqs) == 2:
                print(f"virtio rx irqs during window: {irqs[1] - irqs[0]}", flush=True)
            detail = f"daemon passes {rate:.1f}/s (bound {BOUND:.0f}/s), netctl_wakes {nrate:.2f}/s over {dt:.1f} s"
        else:
            detail = f"login rc={lg.returncode} samples={s} out={out[-200:]!r}"
        print(f"idle: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
    finally:
        if not keep:
            stop()
    print(f"netidletest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
