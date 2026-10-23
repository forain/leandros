#!/usr/bin/env python3
"""evbtntest.py -- do BTN_LEFT events batched with pointer motion reach evdev?

Runs `/bin/evtest2 btn` in the running VM while QMP sends N clicks, each as
[abs x, abs y, btn down] then (a few ms later) [abs x, abs y, btn up], at
the given gap (ms). Prints sent vs received counts.

    LEANDROS_RUN_ID=x python3 evbtntest.py [clicks] [gap_ms]
"""
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402


def main():
    clicks = int(sys.argv[1]) if len(sys.argv) > 1 else 100
    gap = float(sys.argv[2]) / 1000 if len(sys.argv) > 2 else 0.02
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    box = {}

    def guest():
        box["r"] = driver.serial_run("/bin/evtest2 btn", timeout=60, arch=arch)

    t = threading.Thread(target=guest, daemon=True)
    t.start()
    time.sleep(float(os.environ.get("EVBTN_PRE", "10")))
    session = driver._qmp_open()

    def send(evs):
        r = driver._qmp_command(session, "input-send-event", {"events": evs})
        return r is not None and "return" in r

    def absev(x):
        return [{"type": "abs", "data": {"axis": "x", "value": x}},
                {"type": "abs", "data": {"axis": "y", "value": x}}]

    def btn(down):
        return {"type": "btn", "data": {"button": "left", "down": down}}

    ok = 0
    t0 = time.time()
    try:
        for i in range(clicks):
            x = 3000 + (i * 977) % 20000
            ok += send(absev(x) + [btn(True)])
            time.sleep(gap)
            ok += send(absev(x + 50) + [btn(False)])
            time.sleep(gap)
    finally:
        session.close()
    el = time.time() - t0
    t.join()
    r = box.get("r") or {}
    print(r.get("output", "").strip()[-600:])
    print(f"evbtn_sent_clicks={clicks} qmp_ok={ok}/{2*clicks} elapsed={el:.1f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
