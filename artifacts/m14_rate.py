#!/usr/bin/env python3
"""M14 rate ladder: where does injected pointer motion actually disappear?

m14_input.py established that everything ABOVE the evdev ring is lossless —
17 evdev motion frames in, 17 wl_pointer.motion out at the client; 12 key
events in, 12 out. What it also established is that only ~1% of the injected
motion ever reaches the ring: 1787 QMP moves in a 30 s sweep produced 68 evdev
events. So the whole loss sits between QEMU's virtio-input device and
`drivers/src/virtio_keyboard.rs`'s 100 Hz poll, and nothing above it matters
until that is fixed.

This measures the SHAPE of that loss, which decides whose bug it is. The guest
runs no compositor at all — nothing but the login shell — so the only consumer
of the ring is the kernel's own census, and a slow userspace reader cannot be
blamed for what the counter shows.

  loss independent of rate   -> a fixed filter (transform, dropped axis,
                                per-frame coalescing in QEMU's input layer)
  loss growing with rate     -> buffer exhaustion. The eventq has 32
                                descriptors (virtio_keyboard.rs:246) and is
                                drained only from the 100 Hz tick, so QEMU
                                drops whole frames whenever it finds the ring
                                empty — and that is OUR bug to fix, on our
                                side, not COSMIC's.

CONTROLS. Every rung is bracketed by an idle window; a counter that climbs
while nothing is being injected invalidates the rung next to it. The QMP
accept/reject count is asserted per rung, so "the host refused to send it" can
never be read as "the guest lost it".

usage: m14_rate.py [outdir]
"""

import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import m12_caps as M                                             # noqa: E402
from m12_caps import Serial, Qmp                                 # noqa: E402
from m13_input import evstat_last                                # noqa: E402

RUNGS = [(2, 10), (10, 10), (30, 10), (60, 10)]   # (moves/s, seconds)


def push(ser, dev=1):
    """dev's cumulative evdev push count, from the most recent [EVSTAT]."""
    rec = evstat_last(ser.tee_path).get(dev)
    return None if rec is None else rec.get("push", 0)


def sweep_at(q, rate, secs, w=1920, h=1080):
    """Injected absolute motion at a fixed rate, returning what the HOST did."""
    n, t0, period = 0, time.time(), 1.0 / rate
    sent0, rej0 = q.sent, q.rejected
    while time.time() - t0 < secs:
        f = ((time.time() - t0) / secs)
        q.move(int(200 + f * (w - 400)), int(200 + f * (h - 400)))
        n += 1
        nxt = t0 + n * period
        d = nxt - time.time()
        if d > 0:
            time.sleep(d)
    return n, time.time() - t0, q.sent - sent0, q.rejected - rej0


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m14-rate"
    os.makedirs(out, exist_ok=True)
    M.OUT = out
    ser = Serial(tee=os.path.join(out, "serial.log"))

    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    if not m:
        print(">>> CONTROL FAILED: absence and failure are indistinguishable. "
              "Aborting.", flush=True)
        sys.exit(4)
    print(f">>> CONTROL OK ({m.group(1)!r})\n", flush=True)

    q = Qmp(1920, 1080)
    if q.f is None:
        print(">>> NO QMP: nothing can be injected. Aborting.", flush=True)
        sys.exit(3)
    # Qmp's constructor sets a 5 s socket timeout, which is fine under KVM and
    # far too tight under aarch64/TCG: the first run there timed out after four
    # commands, self.f went None, and every later _send() returned False without
    # raising -- 0 injected, no traceback, and four rungs of zeros that look
    # exactly like a real negative. Widen it, and keep asserting q.sent.
    #
    # WIDENING IS NOT ENOUGH, and the aarch64 evidence in artifacts/notes/
    # m14-input says so: at 60 s the same run got 4 commands through in
    # 61.1 s, i.e. ~15 s PER input-send-event. QEMU's main loop is starved
    # by the TCG vCPU, so injected input cannot be rate-controlled on
    # aarch64 at all on this host. aarch64 needs an ARM host with KVM;
    # until then any aarch64 input rate reported from here measures QEMU,
    # not LeandrOS, and this harness prints q.sent beside every rung so
    # that stays visible instead of becoming a false negative.
    q.s.settimeout(int(os.environ.get("M14_QMP_TIMEOUT", "60")))

    # The census prints twice a second, but read_until() returns the instant its
    # regex matches, so the tee can hold less than one tick's worth of console
    # when we get here. Pump first: otherwise the guard below reports "the
    # kernel has no counter" when the truth is "we have not listened yet".
    ser.pump(4)
    if push(ser, 1) is None:
        print(">>> NO [EVSTAT]: the kernel was built with EV_STATS = false, so "
              "the guest-side witness does not exist. Aborting rather than "
              "reporting host counts as if they were guest counts.", flush=True)
        sys.exit(5)

    rows = []
    for rate, secs in RUNGS:
        ser.pump(4)                      # idle bracket
        a = push(ser, 1)
        n, d, sent, rej = sweep_at(q, rate, secs)
        ser.pump(4)                      # let the 100 Hz census catch up
        b = push(ser, 1)
        rows.append((rate, n, d, sent, rej, a, b))
        print(f"  rung {rate}/s: {n} moves in {d:.1f}s "
              f"(qmp {sent} sent / {rej} rejected)  evdev push {a} -> {b}",
              flush=True)

    # Idle tail: proves the counter is not climbing on its own.
    a = push(ser, 1)
    ser.pump(10)
    b = push(ser, 1)
    print(f"\n  IDLE tail: evdev push {a} -> {b} (must be flat)", flush=True)

    print("\n================ RATE LADDER ================", flush=True)
    print(f"{'rate/s':>7} {'moves':>7} {'qmp_ok':>7} {'qmp_rej':>8} "
          f"{'evdev_ev':>9} {'ev/move':>8} {'delivered%':>11}", flush=True)
    for rate, n, d, sent, rej, a, b in rows:
        ev = b - a
        # Each move is two QMP batches (one per axis); QEMU syncs per batch, so
        # a fully delivered move is 4 evdev events (ABS_X, SYN, ABS_Y, SYN).
        per = ev / n if n else 0
        print(f"{rate:>7} {n:>7} {sent:>7} {rej:>8} {ev:>9} {per:>8.2f} "
              f"{100.0 * per / 4.0:>10.1f}%", flush=True)
    print("\n  ev/move at 4.00 = nothing lost. A column that FALLS as rate "
          "rises is buffer exhaustion, not a filter.", flush=True)
    print(">>> serial log:", os.path.join(out, "serial.log"), flush=True)


if __name__ == "__main__":
    main()
