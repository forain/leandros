#!/usr/bin/env python3
"""vfstagtest.py -- blocked VFS waiters park narrow and wake on their event.

Runs `/bin/vfstag --input` in the running VM (same LEANDROS_RUN_ID) over one
held serial connection. The guest checks by itself, for flock / F_SETLKW
waits, signalfd and inotify reads, always-ready fds in a poll set,
/dev/pipewire, and a blocking DRM card read, that the waiter parks with a
mask that is not all ones, that 1 s of unrelated AF_UNIX/pipe traffic does
not wake it, and that its real event does (a lock release by unlock, close
and exit; kill(SIGUSR1); a pipe write; a page-flip event). Then it parks a
blocking read of the tablet (/dev/input/event1) and prints "VT_WANT_INPUT":
this script moves the tablet through QMP until "VT_INPUT_DONE".
Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 vfstagtest.py
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

CASES = ["flock_wait", "setlkw_wait", "flock_collide", "signalfd_read", "inotify_read",
         "static_poll", "pipewire_poll", "drm_read", "evdev_read"]


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    box = {}

    def guest():
        box["r"] = driver.serial_run("/bin/vfstag --input", timeout=180, arch=arch, ser=ser)

    start = len(ser.buf)
    t = threading.Thread(target=guest, daemon=True)
    t.start()
    moves = 0
    notes = []
    session = None
    try:
        while t.is_alive():
            tail = ser.buf[start:]
            if b"VT_WANT_INPUT" in tail and b"VT_INPUT_DONE" not in tail:
                if session is None:
                    time.sleep(0.3)
                    session = driver._qmp_open()
                x = 4000 + (moves % 2) * 8000
                ev = [{"type": "abs", "data": {"axis": "x", "value": x}},
                      {"type": "abs", "data": {"axis": "y", "value": x}}]
                r = driver._qmp_command(session, "input-send-event", {"events": ev})
                if r is None or "return" not in r:
                    notes.append(f"vfstag_qmp: FAIL {r!r}")
                    break
                moves += 1
                time.sleep(0.2)
                continue
            time.sleep(0.05)
        t.join()
    finally:
        if session is not None:
            session.close()
        ser.close()
    r = box.get("r") or {}
    out = r.get("output", "")
    print(out.strip())
    for n in notes:
        print(n)
    print(f"vfstag_tablet_moves={moves}")
    missing = [c for c in CASES if f"{c}: PASS" not in out]
    # /dev/pipewire exists only with a sound device; its SKIP is not a failure.
    if "pipewire_poll: SKIP" in out:
        missing.remove("pipewire_poll")
    ok = r.get("rc") == 0 and "FAIL" not in out and not notes and not missing
    if missing:
        print(f"vfstagtest: missing PASS for {' '.join(missing)}")
    print(f"vfstagtest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
