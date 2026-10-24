#!/usr/bin/env python3
"""evgkeytest.py -- EVIOCGKEY reports held buttons/keys (L11.6).

Runs `/bin/evtest2 held` while QMP holds BTN_LEFT, releases it, holds/releases
KEY_A, then holds BTN_LEFT across a >256-event motion burst (queue overflow,
SYN_DROPPED). Pass = evtest2 prints no FAIL.

Input is sent only once the guest prints its "waiting" line (nodes open and
drained) - a fixed delay raced the serial shell: a missed prompt costs
_type_line a 10 s echo timeout, the first press/release/key landed before
evtest2 opened the nodes, and the run failed held_btn_bit_cleared +
held_key_seen_down (56 s instead of 22 s). EVGKEY_GUEST_PREFIX (e.g.
"sleep 12; ") delays the guest to exercise that path.

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
    ser = driver._Serial()
    start = len(ser.buf)
    cmd = os.environ.get("EVGKEY_GUEST_PREFIX", "") + "/bin/evtest2 held"

    def guest():
        box["r"] = driver.serial_run(cmd, timeout=150, arch=arch, ser=ser)

    def wait_marker(marker, secs):
        # The marker on a line of its own: the typed command line never
        # contains it.
        end = time.time() + secs
        while t.is_alive() and time.time() < end:
            if marker in ser.buf[start:]:
                return True
            time.sleep(0.05)
        return False

    t = threading.Thread(target=guest, daemon=True)
    t.start()
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

    notes = []
    try:
        if not wait_marker(b"evtest2 held: waiting", 90):
            notes.append("evgkey_guest_ready: FAIL (no waiting line)")
        time.sleep(0.3)
        send([btn(True)]); time.sleep(1.5)
        send([btn(False)]); time.sleep(1.5)
        send([key(True)]); time.sleep(1.5)
        send([key(False)]); time.sleep(1.5)
        send([btn(True)])
        # Flood only once evtest2 has stopped reading (its 6 s sleep).
        if not wait_marker(b"evtest2 held: overflow phase", 30):
            notes.append("evgkey_overflow_phase: FAIL (no overflow line)")
        time.sleep(0.3)
        for i in range(300):
            send(absev(2000 + (i * 61) % 28000))
        t.join(60)
        send([btn(False)])
    finally:
        s.close()
        t.join(5)
        ser.close()
    out = (box.get("r") or {}).get("output", "")
    print(out.strip()[-900:])
    for n in notes:
        print(n)
    bad = ("FAIL" in out) or ("evtest2 held done" not in out) or bool(notes)
    print("evgkey", "FAIL" if bad else "PASS")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
