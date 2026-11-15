#!/usr/bin/env python3
"""uartgaptest.py -- console output must not starve serial RX (openbugs #101).

Console writes run with IRQs masked. On the Pi 5 a long one (brush redrawing
its line, a framebuffer scroll + full-screen blit) outlasted the PL011's
32-byte RX FIFO (~2.8 ms at 115200) and a fast-typed line lost ~25 bytes.
QEMU never overruns (the chardev back-pressures), so this asserts on the
kernel's own measurement instead: /proc/sysstat `# uart txgap_us N` is the
longest stretch of any console write without an RX service point. While
the guest floods the console (thousands of lines, so the framebuffer
console scrolls), the host pastes a long line at full speed.

    LEANDROS_RUN_ID=x python3 uartgaptest.py
"""
import os
import re
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

LIMIT_US = 1500  # half the FIFO's 2.8 ms
PASTE = b"#" + bytes((ord("a") + i % 26) for i in range(199)) + b"\r"


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    fails = []
    try:
        box = {}

        def guest():
            box["r"] = driver.serial_run("i=0; while [ $i -lt 3000 ]; do echo flood line $i; i=$((i+1)); done", timeout=120, arch=arch, ser=ser)

        t = threading.Thread(target=guest, daemon=True)
        start = len(ser.buf)
        t.start()
        end = time.time() + 60
        while t.is_alive() and time.time() < end and b"flood line 200" not in ser.buf[start:]:
            time.sleep(0.02)
        ser.send(PASTE)
        t.join(110)
        r = driver.serial_run("cat /proc/sysstat", timeout=30, arch=arch, ser=ser)
        out = r.get("output", "")
        m = re.search(r"# uart txgap_us (\d+) irqlate_us (\d+) rescued (\d+) overruns (\d+)", out)
        # txgap_us is wall time between UART FIFO writes: under TCG the host
        # scheduler/translation stalls dominate it (aarch64 TCG box 1656us, Mac
        # 10856us, both with 0 overruns), so only KVM guests are held
        # to the FIFO budget; every guest must still lose no byte (overruns 0).
        tcg = driver.accel_kind(arch) != "kvm"  # HVF on a shared Mac is as noisy as TCG (9347us seen)
        ok = m is not None and 0 < int(m.group(1)) and (tcg or int(m.group(1)) <= LIMIT_US) and m.group(4) == "0"
        print(f"uartgap_txgap_bounded: {'PASS' if ok else 'FAIL'} "
              f"({m.group(0) if m else 'no # uart line: ' + out.strip()[-200:]}, limit {'none (not KVM)' if tcg else LIMIT_US})")
        if not ok:
            fails.append("txgap")
    finally:
        ser.close()
    print("uartgap", "FAIL" if fails else "PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
