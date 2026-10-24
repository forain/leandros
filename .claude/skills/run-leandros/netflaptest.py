#!/usr/bin/env python3
"""netflaptest.py [aarch64|x86_64] [--keep] -- QMP `set_link` flap guard for the netctl event path.

Boots normally, waits for the DHCP lease, runs `netctl watch --no-snapshot` in the guest with every
event stamped with the guest uptime, then:
  link down  (monitor `set_link net0 off`): a carrier-off event must reach netctl within 1 s;
  link up    (monitor `set_link net0 on`) : carrier-on, and the DHCP lease (`dhcp: bound` in
             /proc/net/nic) must be back within a few seconds (<= 10 s).
Exit 0 iff both PASS.  Run id `<caller run id>-netflap`.
"""
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")


def main(argv):
    arch = "x86_64" if "x86_64" in argv else ("aarch64" if "aarch64" in argv else os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    keep = "--keep" in argv
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    os.environ["LEANDROS_RUN_ID"] = f"{caller}-netflap" if caller else "netflap"
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    res = {}

    def verdict(name, ok, detail=""):
        res[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(cmd, t=30):
        return driver.serial_run(cmd, timeout=t * scale, arch=arch).get("output", "")

    def uptime():
        m = re.search(r"^(\d+\.\d+)", sh("cat /proc/uptime"), re.M)
        return float(m.group(1)) if m else -1.0

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    try:
        stop()
        subprocess.run([sys.executable, DRV, "start", arch], capture_output=True, text=True, timeout=900)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            capture_output=True, text=True, timeout=1200)
        t0 = time.time()
        while time.time() - t0 < 120 * scale and "dhcp: bound" not in sh("cat /proc/net/nic"):
            time.sleep(2)
        import threading
        marks = {}

        def flap():
            time.sleep(4)
            marks["off"] = time.time()
            driver._monitor_send("set_link net0 off")
            time.sleep(8)
            marks["on"] = time.time()
            driver._monitor_send("set_link net0 on")

        th = threading.Thread(target=flap)
        t_start = time.time()
        th.start()
        out = sh("read u r < /proc/uptime; echo U0 $u; netctl watch --no-snapshot --for 24 | "
                 "while read l; do read u r < /proc/uptime; echo \"S $u $l\"; done", 90)
        th.join()
        m = re.search(r"^U0 (\d+\.\d+)", out, re.M)
        u0 = float(m.group(1)) if m else -1.0
        evs = [(float(x.group(1)) - u0, x.group(2)) for x in re.finditer(r"^S (\d+\.\d+) (.*)$", out, re.M)]
        t_off, t_on = marks["off"] - t_start, marks["on"] - t_start
        down = [e for e in evs if t_off - 0.5 <= e[0] and re.search(r"carrier|link|down", e[1], re.I)]
        dd = down[0][0] - t_off if down else None
        verdict("down", lg.returncode == 0 and dd is not None and dd <= 1.5,
                f"first event {dd:.2f} s after set_link off: {down[0][1][:70]}" if down else f"no event; evs={evs[:6]}")
        up = [e for e in evs if e[0] >= t_on - 0.5]
        carrier = [e for e in up if re.search(r"carrier", e[1])]
        bound = [e for e in up if re.search(r"ipv4 bound", e[1])]
        dc = (carrier[0][0] - t_on) if carrier else None
        db = (bound[0][0] - t_on) if bound else None
        verdict("up", bool(carrier) and bool(bound) and db <= 10 * scale,
                f"carrier event {dc:.2f} s, lease bound event {db:.2f} s after set_link on (latency-offset +-0.3)"
                if bound and carrier else f"missing carrier/bound event; evs={[e[1][:60] for e in evs]}")
    finally:
        if not keep:
            stop()
    ok = bool(res) and all(res.values())
    print(f"netflaptest: {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in res.items()))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
