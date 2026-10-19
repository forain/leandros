#!/usr/bin/env python3
"""pollnarrowtest.py -- blocked /dev/netctl and console readers park narrow.

Runs `/bin/pollnarrow --input` in the running VM (same LEANDROS_RUN_ID) over
one held serial connection. The guest checks by itself that a thread blocked
in read(/dev/netctl) and one blocked in read(0) on the console park with a
narrow poll mask, that 1 s of unrelated AF_UNIX/pipe traffic does not wake
them, and that a snapshot request / a cursor-position report does. Then it
asks for real console input: on "PN_WANT_KEY" this script presses `k` on the
virtio keyboard through QMP, on "PN_WANT_SERIAL" it sends `s` over the UART,
and the guest checks that each one reached the parked reader.
Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 pollnarrowtest.py
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402


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
        box["r"] = driver.serial_run("/bin/pollnarrow --input", timeout=120, arch=arch, ser=ser)

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
            if not sent_key and b"PN_WANT_KEY" in tail:
                time.sleep(0.3)
                sent_key = True
                if not qmp_key("k"):
                    notes.append("pollnarrow_qmp: FAIL send-key")
            if not sent_serial and b"PN_WANT_SERIAL" in tail:
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
    print(f"pollnarrow_input_sent: key={int(sent_key)} serial={int(sent_serial)}")
    ok = (r.get("rc") == 0 and "FAIL" not in out and not notes
          and "netctl_read_narrow: PASS" in out
          and "console_read_narrow: PASS" in out
          and "console_read_input: PASS" in out)
    print(f"pollnarrowtest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
