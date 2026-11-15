"""Nitro-like x86_64 platform (lane/nitrox86, 2026-11-14): no PIT, no HPET, no
i8042, no display device at all (serial console only), as on AWS EC2 Nitro.
Boots with LEANDROS_NITRO=1 (q35,pit=off,hpet=off,i8042=off, no VGA/GPU),
requires a login prompt on serial, a [TSC] line whose frequency came from a
stated source (never the 1 GHz last-resort guess), and a clock that keeps
real time (clockdrift ratio within 5 %).
Usage: LEANDROS_RUN_ID=<lane> python3 nitrotest.py
"""
import os, re, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
LOG = f"/tmp/leandros{'-' + RUN_ID if RUN_ID else ''}-serial-full.log"


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def main():
    env = dict(os.environ)
    env["LEANDROS_NITRO"] = "1"
    ok = True
    drv("stop", timeout=120)
    try:
        rc, out = drv("start", "x86_64", timeout=400, env=env)
        log = open(LOG, "rb").read().decode("utf-8", "replace") if os.path.exists(LOG) else ""
        if rc != 0:
            print("FAIL: no login on serial without PIT/HPET/i8042/display\n" + out[-500:])
            print("\n".join(log.splitlines()[-6:]))
            return 1
        tsc = [l for l in log.splitlines() if "[TSC]" in l]
        print(tsc[-1] if tsc else "no [TSC] line")
        if not tsc:
            ok = False; print("FAIL: no [TSC] line")
        elif "(none)" in tsc[-1]:
            ok = False; print("FAIL: TSC frequency is the last-resort guess")
        r = subprocess.run([sys.executable, os.path.join(HERE, "clockdrift.py"), "10", "nitro"],
                           capture_output=True, text=True, timeout=120, env=env, cwd=REPO)
        line = (r.stdout + r.stderr).strip().splitlines()[-1:] or [""]
        print(line[0])
        m = re.search(r"ratio=([0-9.]+)", line[0])
        if not m or abs(float(m.group(1)) - 1.0) > 0.05:
            ok = False; print("FAIL: guest clock drifts > 5 % from the host")
    finally:
        drv("stop", timeout=60)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
