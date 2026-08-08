#!/usr/bin/env python3
"""Guard: a serial consumer that stops reading must not cost the guest its input.

WHAT THIS EXISTS TO CATCH. `arch::putc` polls the UART transmitter for room and
used to poll it without a bound. It runs in IRQ context — the 0.5 Hz [EVSTAT] /
[VQSTAT] census reaches it straight from the timer tick — and QEMU's 16550
withholds LSR.THRE for as long as its chardev back end refuses a byte
(`hw/char/serial.c:serial_xmit` installs a G_IO_OUT watch on EAGAIN and returns
without setting THRE). So a host that holds the serial socket open and stops
reading it parked the timer IRQ handler forever: TICK_COUNT stopped, the
scheduler tick stopped, and `virtio_keyboard::poll_events` stopped draining the
eventq, whereupon QEMU dropped every arriving input frame for want of a posted
buffer. It takes about a second to provoke — QEMU writes one byte per sendmsg
and AF_UNIX charges ~768 B of skb per byte, so the default 208 KiB socket buffer
fills after ~280 bytes of console output.

This is a HOST-SIDE measurement on purpose. The loss metric is QEMU's own
`virtio_input_queue_full` trace, counted on the host, so it needs no guest
output — an instrument that had to print would be throttled by the very stall
it is measuring, which is how this defect stayed hidden behind a rate ladder for
two lanes.

REQUIRES `EV_STATS = true` (kernel/src/syscall.rs), which is now committed off.
The guard works by having the timer IRQ print: with the census off nothing
reaches `putc` from IRQ context, so a parked serial reader has nothing to
back-pressure and PARKED reads 100% on a broken kernel too. Turn it on before
running this, and off again before committing.

THREE PHASES, one boot, identical 60 moves/s injection, differing only in who is
draining the serial chardev:

  PARKED   a client is connected and never reads   <- the case under test
  DRAINED  a client is connected and reads throughout
  ABSENT   no client at all (QEMU discards guest output, never back-pressures)

DRAINED and ABSENT are the controls: they must deliver everything on any kernel,
fixed or broken, so a run where they fail is a broken run and says nothing about
PARKED. PARKED is the guard. Restore the unbounded spin in `arch::putc` and
PARKED collapses to ~10% while both controls stay at 100%.

Requires: -qmp unix:/tmp/leandros-qmp.sock and
          -trace enable=virtio_input_queue_full,file=$M15_TRACE

usage: m15_serial_stall.py [outdir]
"""

import os
import re
import select
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from m12_caps import Qmp, Serial                                # noqa: E402

TRACE = os.environ.get("M15_TRACE", "/tmp/vq-trace.log")
SERIAL_SOCK = "/tmp/leandros-serial.sock"

RATE, SECS = 60, 10
PASS_FRACTION = 0.90        # PARKED must deliver at least this much


def queue_full():
    """How many input frames QEMU has dropped for want of a posted buffer.
    QEMU's log trace backend fflushes per event, so a live read is exact."""
    try:
        with open(TRACE, "rb") as f:
            return sum(1 for _ in f)
    except OSError:
        return None


class Reader:
    """A serial client that is connected either way, and either reads or does
    not. `drain=False` is not 'no client' — the socket stays open, which is what
    lets QEMU's chardev back-pressure the guest."""

    def __init__(self, drain):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.drain, self.stop, self.n = drain, False, 0
        self.t = threading.Thread(target=self._loop, daemon=True)
        self.t.start()

    def _loop(self):
        while not self.stop:
            if not self.drain:
                time.sleep(0.05)
                continue
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    c = self.s.recv(65536)
                except (BlockingIOError, OSError):
                    continue
                if not c:
                    return
                self.n += len(c)

    def close(self):
        self.stop = True
        self.t.join(2)
        try:
            self.s.close()
        except OSError:
            pass


def sweep(q, rate=RATE, secs=SECS):
    n, t0 = 0, time.time()
    while time.time() - t0 < secs:
        f = (time.time() - t0) / secs
        q.move(int(200 + f * 1520), int(200 + f * 680))
        n += 1
        d = t0 + n / rate - time.time()
        if d > 0:
            time.sleep(d)
    return n


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m15-stall"
    os.makedirs(out, exist_ok=True)

    # POSITIVE CONTROL, first thing on every boot: if a command that cannot
    # possibly work does not report failing, this harness cannot tell absence
    # from failure and nothing below it may be believed.
    print("=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser = Serial(tee=os.path.join(out, "serial.log"))
    ser.send("nosuchbinary_xyz42")
    m, _ = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    if not m:
        print(">>> CONTROL FAILED: absence and failure are indistinguishable. "
              "Aborting.", flush=True)
        sys.exit(4)
    print(f">>> CONTROL OK ({m.group(1)!r})\n", flush=True)
    ser.s.close()
    time.sleep(1)

    if queue_full() is None:
        print(f">>> NO TRACE at {TRACE}: QEMU was launched without "
              f"-trace enable=virtio_input_queue_full, so the loss metric does "
              f"not exist. Aborting rather than reporting zero drops as a pass.",
              flush=True)
        sys.exit(5)

    q = Qmp(1920, 1080)
    if q.f is None:
        print(">>> NO QMP: nothing can be injected. Aborting.", flush=True)
        sys.exit(3)
    q.s.settimeout(int(os.environ.get("M15_QMP_TIMEOUT", "60")))

    rows = []
    for tag, mode in (("PARKED", "park"), ("DRAINED", "drain"), ("ABSENT", None)):
        r = Reader(mode == "drain") if mode else None
        time.sleep(1.0)
        a = queue_full()
        sent0, rej0 = q.sent, q.rejected
        n = sweep(q)
        time.sleep(2.0)
        lost = queue_full() - a
        read = r.n if r else -1
        if r:
            r.close()
        time.sleep(1.5)
        # One QMP command per axis; QEMU syncs per command, so each command is
        # one EV_ABS + one EV_SYN frame and each move is two frames.
        frames = 2 * n
        rows.append((tag, n, q.sent - sent0, q.rejected - rej0, frames, lost,
                     read))
        print("  %-8s moves=%4d qmp=%4d/%d frames=%5d queue_full=%5d "
              "delivered=%5.1f%%  serial_read=%d"
              % (tag, n, q.sent - sent0, q.rejected - rej0, frames, lost,
                 100.0 * (frames - lost) / frames, read), flush=True)

    print("\n============== SERIAL-STALL GUARD ==============", flush=True)
    ok = True
    d = {}
    for tag, n, sent, rej, frames, lost, read in rows:
        d[tag] = 100.0 * (frames - lost) / frames
        if rej:
            print(f">>> BROKEN RUN: {tag} had {rej} QMP rejections; the host "
                  f"refused to send what the guest is being blamed for losing.",
                  flush=True)
            ok = False
    for c in ("DRAINED", "ABSENT"):
        if d[c] < 99.9:
            print(f">>> CONTROL {c} delivered only {d[c]:.1f}%. A control that "
                  f"fails makes PARKED unreadable — this run proves nothing.",
                  flush=True)
            ok = False
    if not ok:
        sys.exit(2)
    verdict = "PASS" if d["PARKED"] >= 100.0 * PASS_FRACTION else "FAIL"
    print(f"  PARKED {d['PARKED']:.1f}%  (controls DRAINED {d['DRAINED']:.1f}% "
          f"ABSENT {d['ABSENT']:.1f}%)", flush=True)
    print(f"  serial_stall_input_loss: {verdict} "
          f"(threshold {100.0 * PASS_FRACTION:.0f}%)", flush=True)
    print(f"failures = {0 if verdict == 'PASS' else 1}", flush=True)
    sys.exit(0 if verdict == "PASS" else 1)


if __name__ == "__main__":
    main()
