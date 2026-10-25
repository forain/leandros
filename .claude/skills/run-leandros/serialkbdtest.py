#!/usr/bin/env python3
"""serialkbdtest.py -- serial-console bytes never reach event0 (openbugs L12 #67).

The serial line is a tty, not a keyboard. Two cases, one boot:

  default  `/bin/evtest2 serial 6000` watches /dev/input/event0 while the host
           types `#Ab*=1` on the serial console: no EV_KEY may arrive (it used
           to get the raw ASCII byte as a keycode with value 2 and no release:
           '*' read as a held KEY_LEFTSHIFT, '=' as KEY_F3), and no serial-kbd
           node exists.
  optin    the same with `on` (root's SERIALKBD_IOCTL on /dev/input/hotplug,
           the run-time twin of `evdev.serialkbd=1`): event0 still silent; the
           separate "serial-kbd" node gets US-layout keys with Shift around
           upper case and symbols, every press released, nothing held.

    LEANDROS_RUN_ID=x python3 serialkbdtest.py
"""
import os
import re
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

TYPED = b"#Ab*=1"
# Written independently of the kernel table: (KEY_*, shift) per byte.
KEYS = {ord("#"): (4, True), ord("A"): (30, True), ord("b"): (48, False),
        ord("*"): (9, True), ord("="): (13, False), ord("1"): (2, False)}


def expected():
    seq = []
    for b in TYPED:
        code, sh = KEYS[b]
        seq += (["+42"] if sh else []) + [f"+{code}", f"-{code}"] + (["-42"] if sh else [])
    return seq


def run_case(arch, ser, on):
    box = {}
    start = len(ser.buf)
    cmd = "/bin/evtest2 serial 6000" + (" on" if on else "")

    def guest():
        box["r"] = driver.serial_run(cmd, timeout=90, arch=arch, ser=ser)

    t = threading.Thread(target=guest, daemon=True)
    t.start()
    end = time.time() + 60
    while t.is_alive() and time.time() < end and b"evtest2 serial: waiting" not in ser.buf[start:]:
        time.sleep(0.05)
    time.sleep(0.3)
    ser.send(TYPED + b"\r")  # buffered by the tty; the shell reads a comment
    t.join(80)
    return (box.get("r") or {}).get("output", "")


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    fails = []
    try:
        out = run_case(arch, ser, False)
        m = re.search(r"event0_keys=(\d+)", out)
        ok = m is not None and m.group(1) == "0" and "node=none" in out
        print(f"serialkbd_default_event0_silent: {'PASS' if ok else 'FAIL'} "
              f"(event0_keys={m.group(1) if m else '?'}, node={'none' if 'node=none' in out else '?'})")
        if not ok:
            fails.append("default")
        out = run_case(arch, ser, True)
        m = re.search(r"event0_keys=(\d+) presses=(\d+) releases=(\d+) repeats=(\d+) held=(\d+)", out)
        toks = re.findall(r"(?<![\w=])([+\-*]\d+)(?= )", out.split("waiting node=", 1)[-1])
        exp = expected()
        ok0 = m is not None and m.group(1) == "0"
        okp = m is not None and m.group(2) == m.group(3) and m.group(4) == "0" and m.group(5) == "0"
        okc = toks[:len(exp)] == exp
        print(f"serialkbd_optin_event0_silent: {'PASS' if ok0 else 'FAIL'} (event0_keys={m.group(1) if m else '?'})")
        print(f"serialkbd_optin_press_release: {'PASS' if okp else 'FAIL'} ({m.group(0) if m else 'no summary'})")
        print(f"serialkbd_optin_codes: {'PASS' if okc else 'FAIL'} (got {' '.join(toks[:len(exp)])})")
        if not (ok0 and okp and okc):
            fails.append("optin")
            print(out.strip()[-600:])
    finally:
        ser.close()
    print("serialkbd", "FAIL" if fails else "PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
