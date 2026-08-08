# The input path never starved under load. The console did, and it took the tick with it.

2026-08-08, Linux box (`forain@172.16.158.150`), x86_64/KVM, QEMU 11.0.1.

## What was believed

TODO.md item 6 recorded, from `artifacts/m14_rate.py` on an idle guest with no
compositor: delivery falling monotonically with injection rate — 2.00, 1.12,
0.91, 0.85 evdev events per injected move at 2, 10, 30, 60 moves/s — with zero
QMP rejections. It read that as "a column that falls as rate rises is buffer
exhaustion", localised it to the 32-descriptor virtio-input eventq, and named
two suspects: the 100 Hz drain not keeping up, and the missing volatile
accesses/barriers in `drivers/src/virtio_keyboard.rs`.

The ladder reproduced byte-for-byte: 40 / 112 / 272 / 512 events, identical to
the recorded run. So the phenomenon was real. The reading of it was not.

## What the counters said

A gated `[VQSTAT]` census (`drivers/src/virtio_keyboard.rs`, `VQ_STATS`) added
polls attempted, polls skipped on `try_lock`, events drained, largest single
drain, and the low-water mark of `avail.idx - used.idx`. On the same boot as the
ladder:

```
t=  12.00 polls=  1200 skips=0 drained=    8 maxb= 4 minfree=28 starve=0 aidx=  40 uidx=   8 notify=  2
t=  14.00 polls=  1400 skips=0 drained=   40 maxb=32 minfree= 0 starve=1 aidx=  72 uidx=  40 notify=  3
t=  16.00 polls=  1600 skips=0 drained=   40 maxb=32 minfree= 0 starve=1 aidx=  72 uidx=  40 notify=  3
...
t=  42.00 polls=  4200 skips=0 drained=  904 maxb=32 minfree= 0 starve=3 aidx= 932 uidx= 904 notify=205
t=  44.00 polls=  4400 skips=0 drained=  936 maxb=32 minfree= 0 starve=4 aidx= 968 uidx= 936 notify=206
```

- **`skips` is 0 for the entire run.** `try_lock` contention on `VIRTIO_INPUTS`
  is not a factor and never was: `poll_events()` is called only from the
  `cpu == 0` arm of `on_tick` on both arches, so no AP ever contends it.
- **`maxb` is 32 and `notify` moves by exactly +1 after each burst.** Every
  drain but the last of a rung took exactly 4 events — one move's
  `ABS_X, SYN, ABS_Y, SYN` — and then one single poll took the whole 32-buffer
  ring at once. That shape is not a drain falling behind; it is a drain that
  stopped and restarted.
- The number of 4-event drains per rung was 2 / 20 / 60 / 120 against 20 / 100 /
  300 / 600 moves injected. At 10, 30 and 60 moves/s that is **exactly two
  seconds of perfect delivery** and then nothing. The delivered *count* was
  flat; only the delivered *fraction* fell, because the denominator grew.

`polls` tracking ticks at exactly 200 per 200 ticks proves nothing about wall
time, and this is the trap: `TICK_COUNT` and `VQ_POLLS` are incremented in the
same handler, so if the handler stops, both stop and their ratio stays 1:1. The
run's tick clock advanced 52 s across ~88 s of wall time.

## Where the loss actually was

QEMU's own `virtio_input_queue_full` trace, counted host-side, gives the exact
number of frames the device dropped for want of a posted buffer:

```
-trace enable=virtio_input_queue_full,file=...
```

The ladder produced **1572** of them. Injected frames = 2040 QMP commands
(one `EV_ABS` + one `EV_SYN` each); delivered = 936 events = 468 frames;
2040 − 468 = **1572, exact**. So every lost event is a host-side
`virtqueue_pop() == NULL` and nothing is lost anywhere else in the path.

## What decides it

Three identical 60 moves/s, 10 s sweeps in **one boot**, differing only in who
is draining QEMU's serial chardev (`artifacts/m15_serial_stall.py`):

| phase | serial consumer | frames | queue_full | delivered |
|---|---|---|---|---|
| PARKED | connected, never reads | 1200 | 1086 | **9.5%** |
| DRAINED | connected, reads throughout | 1200 | 0 | **100.0%** |
| ABSENT | not connected at all | 1200 | 0 | **100.0%** |

Same kernel, same rate, same boot. **The input path delivers 100% at 60
moves/s — 240 events/s against a 32-descriptor ring — whenever the console is
not back-pressured.** There is no load-dependent loss to explain.

## The mechanism

`arch/x86_64/src/lib.rs::putc` polled LSR bit 5 for the transmit holding
register with no bound; `arch/aarch64/src/uart.rs::putc` polled PL011 `FR.TXFF`
the same way. QEMU's 16550 withholds `LSR.THRE` for exactly as long as its
chardev back end refuses the byte — `hw/char/serial.c:serial_xmit` installs a
`G_IO_OUT` watch on `EAGAIN` and returns *without* setting THRE:

```c
int rc = qemu_chr_fe_write(&s->chr, &s->tsr, 1);
if ((rc == 0 || (rc == -1 && errno == EAGAIN)) && s->tsr_retry < MAX_XMIT_RETRY) {
    s->watch_tag = qemu_chr_fe_add_watch(&s->chr, G_IO_OUT | G_IO_HUP, serial_watch_cb, s);
    if (s->watch_tag > 0) { s->tsr_retry++; return; }
}
```

A socket chardev with no client returns `len` and never blocks (this is the
already-recorded "QEMU serial drops output w/o client"); a socket chardev with a
client that has stopped reading blocks. `putc` is reached **from IRQ context** —
the 0.5 Hz `[EVSTAT]`/`[VQSTAT]` census runs from the timer tick, via
`poll_deadline_tick`. So a parked serial reader wedged CPU 0 inside the timer
IRQ handler. `TICK_COUNT` froze, `sched::timer_tick_irq` never ran, and
`virtio_keyboard::poll_events` never ran — which is why the eventq emptied, why
the drain that eventually followed took exactly the full ring (32), and why the
next rung recovered as soon as the harness resumed pumping.

Everything in the ladder falls out of that: ~2 s of a live guest per rung
(injection starts, console fills, guest wedges), a constant +32 flush on unwedge,
and a fraction that falls only because the denominator rises.

## The fix

Both `putc` implementations now wait against a **cycle-counter deadline**, not
an iteration count, and latch `TX_WEDGED` when the deadline expires so a
back-pressured console costs one probe per byte instead of one full deadline per
byte. Console output may be lost; an interrupt handler may not be stalled.

An iteration bound is not sufficient and the intermediate measurement says so:
10 000 LSR reads is ~10 ms against a real UART but ~100 ms against an emulated
one, because each `in al, dx` is an exit to host userspace. That version moved
PARKED from 9.5% to only 27.3%.

`virtio_keyboard.rs` also got the volatile accesses and barriers it was missing
on `used.idx` and on the `avail.idx` publish. **That was not the cause here** —
the x86_64 disassembly of the old code is faithful (`mov %cx,0x4(%r12,%rax,2)`
immediately followed by `incw 0x2(%r12)`, correctly ordered under TSO) — but it
is a real defect on aarch64, where nothing orders those two stores.

## Before / after, `artifacts/m14_rate.py` unmodified

| rate/s | moves | qmp_ok | qmp_rej | before ev/move | after ev/move |
|---|---|---|---|---|---|
| 2 | 20 | 40 | 0 | 2.00 | **4.00** |
| 10 | 100 | 200 | 0 | 1.12 | **4.00** |
| 30 | 300 | 600 | 0 | 0.91 | **4.00** |
| 60 | 600 | 1200 | 0 | 0.85 | **4.00** |

Host-side `virtio_input_queue_full` over the whole ladder: **1572 → 0.**
Guard `m15_serial_stall`: PARKED 9.5% → **100.0%**, controls 100% throughout.

## Two things TODO.md records incorrectly

1. **"`drop+0` on every evdev sample, which exonerates the ring" is false.**
   `drop` reaches **680** in the very serial log that sentence was written from.
   The evdev ring is `MAX_EVENTS = 256` (`servers/evdev/src/lib.rs:44`), depth
   pins at 256 from the 30/s rung onward because nothing is reading the node,
   and it overwrites from there. It happens not to affect the ladder's
   arithmetic — `push` is counted before the ring is consulted — but the ring
   was not exonerated, it was saturated.
2. **"the loss is in the virtqueue handoff" is false.** The handoff is lossless
   at every rate tested.

## Reproducing

```
export LEANDROS_QEMU_EXTRA="-qmp unix:/tmp/leandros-qmp.sock,server=on,wait=off \
  -trace enable=virtio_input_queue_full,file=/tmp/vq-trace.log"
export M15_TRACE=/tmp/vq-trace.log
python3 .claude/skills/run-leandros/driver.py start x86_64 --venus
python3 .claude/skills/run-leandros/driver.py login root root
python3 artifacts/m15_serial_stall.py /tmp/m15      # guard
python3 artifacts/m14_rate.py        /tmp/m14       # ladder
```

`x-query-virtio-queue-status` was tried as a host-side witness and is a dead end
here: QMP returns its fields dash-named, and reading it live against a running
QEMU produced a silent all-`None` row that looked exactly like a device with no
queue. The trace count is the reliable host-side metric.


## Regression, x86_64, fresh images, `vfstest` run once per image

`m13_suite.py` on this box is **not** trustworthy on x86_64 and was not before
this change. Every row's exit status lands in the next row's window — vfstest's
36 subtests come back 16 in its own window and 20 in the next — so the harness
reports `NO EXIT STATUS READ BACK` for most tests. Widening every budget to
700 s reproduces it identically, so nothing is timing out. The cause is a
separate pre-existing defect (`console-loss-preexisting.md`): x86_64 console
writes have no flow control, so the bytes were never sent. The recorded
2026-08-07 run in `artifacts/notes/m13-cosmic-config/` shows the same shape.

What can be read back, against the baselines in TODO.md:

| test | `failures = N` | baseline | verdict |
|---|---|---|---|
| vfstest | 0 (36 PASS across two windows) | 36/0 | clean |
| drmsmoke | 0 | 29/0 | clean |
| scmtest | 0 | 32/0 | clean |
| forktest | 0 | 3/0 | clean |
| polltest | 0 | 6/0 | clean |
| sigtest | 0 | 6/0 | clean |
| memtest | 0 | 4/0 | clean |
| waittest | **1** (`wait_on_process_group`) | 4/0 **or** 3/1 | known flake, within baseline |
| venustest | — | 108/0 **under `--venus` only** | not run: `m13_suite.py` boots plain `uefi` |
| wakepolltest, epolltest, timertest, f2fstest | not read back | — | harness, not the kernel |

No test named a FAIL other than `waittest: wait_on_process_group`, which
TODO.md records as acceptable on either arch in either direction.

**aarch64 was not run here.** Both `putc` changes are symmetric and the aarch64
tree builds, but the aarch64 counterpart of every measurement above is
unverified from this box and is the Mac lane's to confirm. Note that the
console-loss defect is x86_64-only — aarch64's `write_byte` already routes
through `arch_serial_putc`.
