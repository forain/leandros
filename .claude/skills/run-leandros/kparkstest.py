#!/usr/bin/env python3
"""kparkstest.py -- console poll/epoll, empty poll sets and VT_WAITACTIVE park narrow.

Runs `/bin/kparks --input` in the running VM (same LEANDROS_RUN_ID) over one
held serial connection. The guest checks by itself that a poll() and an
epoll_wait() on the console fd 0 park with a narrow mask, take ~0 wakes from
1 s of unrelated AF_UNIX/pipe traffic and wake on a cursor-position report;
that poll(NULL, 0, 100), a poll of negative fds and select(0, ...) sleep
~100 ms with 0 poll wakes (and a signal still ends poll(NULL, 0, -1)); and
that VT_WAITACTIVE parks narrow and returns right after a VT switch made by
another process. For real console input: on "KP_WANT_KEY" this script presses
`k` on the virtio keyboard through QMP, on "KP_WANT_SERIAL" it sends `s` over
the UART, and the guest checks that each one woke both parked waiters.
Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 kparkstest.py
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

CASES = ["console_poll_narrow", "console_poll_input", "empty_poll_sleep", "vt_waitactive"]


def qmp_key(key):
    session = driver._qmp_open()
    try:
        r = driver._qmp_command(session, "send-key",
                                {"keys": [{"type": "qcode", "data": key}], "hold-time": 50})
        return r is not None and "return" in r
    finally:
        session.close()


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    box = {}

    def guest():
        box["r"] = driver.serial_run("/bin/kparks --input", timeout=120, arch=arch, ser=ser)

    start = len(ser.buf)
    t = threading.Thread(target=guest, daemon=True)
    t.start()
    sent_key = sent_serial = False
    notes = []
    try:
        while t.is_alive():
            tail = ser.buf[start:]
            # The marker on a line of its own: the typed command line never
            # contains it.
            if not sent_key and b"KP_WANT_KEY" in tail:
                time.sleep(0.3)
                sent_key = True
                if not qmp_key("k"):
                    notes.append("kparks_qmp: FAIL send-key")
            if not sent_serial and b"KP_WANT_SERIAL" in tail:
                time.sleep(0.3)
                sent_serial = True
                ser.send(b"s")
            time.sleep(0.05)
        t.join()
    finally:
        ser.close()
    r = box.get("r") or {}
    out = r.get("output", "")
    print(out.strip())
    for n in notes:
        print(n)
    print(f"kparks_input_sent: key={int(sent_key)} serial={int(sent_serial)}")
    missing = [c for c in CASES if f"{c}: PASS" not in out]
    ok = r.get("rc") == 0 and "FAIL" not in out and not notes and not missing
    if missing:
        print(f"kparkstest: missing PASS for {' '.join(missing)}")
    print(f"kparkstest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
