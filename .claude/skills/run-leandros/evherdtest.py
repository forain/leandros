#!/usr/bin/env python3
"""evherdtest.py -- input on one evdev device must not wake waiters on another.

Runs `/bin/evtest2 herd <ms>` in the running VM (same LEANDROS_RUN_ID) and,
for as long as it runs, moves the virtio tablet through QMP. The guest parks
in epoll_wait on the keyboard (event0) and counts its own poll-channel wakes
(`/proc/sysstat` `w` line, last column); before the evdev wake was keyed by
device, every tablet frame woke it. Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 evherdtest.py [window_ms]
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402


def main():
    window_ms = int(sys.argv[1]) if len(sys.argv) > 1 else 4000
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    box = {}

    def guest():
        box["r"] = driver.serial_run(f"/bin/evtest2 herd {window_ms}",
                                     timeout=60 + window_ms / 1000, arch=arch)

    t = threading.Thread(target=guest, daemon=True)
    t.start()
    session = driver._qmp_open()
    sent = 0
    try:
        # ~15 frames/s: 3 events per frame stays inside the 256-event client
        # queue for the whole window, so no SYN_DROPPED hides frames.
        while t.is_alive():
            x = 4000 + (sent % 2) * 8000
            ev = [{"type": "abs", "data": {"axis": "x", "value": x}},
                  {"type": "abs", "data": {"axis": "y", "value": x}}]
            r = driver._qmp_command(session, "input-send-event", {"events": ev})
            if r is None or "return" not in r:
                print(f"evherd_qmp: FAIL {r!r}")
                break
            sent += 1
            time.sleep(1 / 15)
    finally:
        session.close()
    t.join()
    r = box.get("r") or {}
    out = r.get("output", "")
    print(out.strip())
    print(f"evherd_frames_sent={sent}")
    ok = r.get("rc") == 0 and "FAIL" not in out and "herd_no_cross_device_wake: PASS" in out
    print(f"evherd: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
