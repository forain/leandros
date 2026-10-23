#!/usr/bin/env python3
"""evclocktest.py -- EVIOCSCLOCKID must change the clock events are stamped in.

Runs `/bin/evtest2 clock` in the running VM (same LEANDROS_RUN_ID) while the
virtio tablet is moved through QMP. Exit status 0 iff every clock matched.

    LEANDROS_RUN_ID=x python3 evclocktest.py
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    box = {}

    def guest():
        box["r"] = driver.serial_run("/bin/evtest2 clock", timeout=60, arch=arch)

    t = threading.Thread(target=guest, daemon=True)
    t.start()
    session = driver._qmp_open()
    sent = 0
    try:
        while t.is_alive():
            x = 4000 + (sent % 2) * 8000
            ev = [{"type": "abs", "data": {"axis": "x", "value": x}},
                  {"type": "abs", "data": {"axis": "y", "value": x}}]
            r = driver._qmp_command(session, "input-send-event", {"events": ev})
            if r is None or "return" not in r:
                print(f"evclock_qmp: FAIL {r!r}")
                break
            sent += 1
            time.sleep(1 / 15)
    finally:
        session.close()
    t.join()
    r = box.get("r") or {}
    out = r.get("output", "")
    print(out.strip())
    ok = r.get("rc") == 0 and "FAIL" not in out and "clock_realtime_stamp: PASS" in out
    print(f"evclock: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
