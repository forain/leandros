#!/usr/bin/env python3
"""evgkeytest.py -- EVIOCGKEY reports held buttons/keys (L11.6).

Runs `/bin/evtest2 held` while QMP holds BTN_LEFT, releases it, holds/releases
KEY_A, then holds BTN_LEFT across a >256-event motion burst (queue overflow,
SYN_DROPPED). Pass = evtest2 prints no FAIL.

    LEANDROS_RUN_ID=x python3 evgkeytest.py
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
        box["r"] = driver.serial_run("/bin/evtest2 held", timeout=90, arch=arch)

    t = threading.Thread(target=guest, daemon=True)
    t.start()
    time.sleep(float(os.environ.get("EVGKEY_PRE", "10")))
    s = driver._qmp_open()

    def send(evs):
        driver._qmp_command(s, "input-send-event", {"events": evs})

    def btn(down):
        return {"type": "btn", "data": {"button": "left", "down": down}}

    def key(down):
        return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": "a"}}}

    def absev(x):
        return [{"type": "abs", "data": {"axis": "x", "value": x}},
                {"type": "abs", "data": {"axis": "y", "value": x}}]

    try:
        send([btn(True)]); time.sleep(1.5)
        send([btn(False)]); time.sleep(1.5)
        send([key(True)]); time.sleep(1.5)
        send([key(False)]); time.sleep(1.5)
        send([btn(True)]); time.sleep(1.0)
        for i in range(300):
            send(absev(2000 + (i * 61) % 28000))
        t.join(60)
        send([btn(False)])
    finally:
        s.close()
    out = (box.get("r") or {}).get("output", "")
    print(out.strip()[-900:])
    bad = ("FAIL" in out) or ("evtest2 held done" not in out)
    print("evgkey", "FAIL" if bad else "PASS")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
