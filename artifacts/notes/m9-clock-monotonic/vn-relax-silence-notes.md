# Unexplained: Mesa never emitted `vn_relax`'s "stuck in ... wait" during the vktest TCG hang

Recorded 2026-08-06, during the root-cause of the `vktest`-under-TCG hang
(fixed by giving CLOCK_MONOTONIC sub-tick resolution — see the
`arch-timers.patch` / `clock-gettime.patch` wave). **No longer reproducible**,
because the deadlock that produced the long wait is gone. Written down because
a thread parked inside a single sleep call, rather than iterating, would be a
second and independent bug — and this project has been bitten by exactly that
shape before (`nanosleep` truncating sub-tick sleeps to zero, fixed `fb398c7`).

## What was expected

While the ring was deadlocked the guest was waiting in
`vn_ring_wait_seqno` (Mesa `src/virtio/vulkan/vn_ring.c`), which loops on
`vn_relax()` (`src/virtio/vulkan/vn_common.c`). That function logs
**unconditionally** — no debug flag, no env var:

```c
   if (unlikely(*iter % (1 << warn_order) == 0)) {
      vn_log(instance, "stuck in %s wait with iter at %d", state->reason_str, *iter);
      ...
      if (vn_watchdog_timeout(watchdog) && !VN_DEBUG(NO_ABORT)) {
         vn_log(instance, "aborting on expired ring alive status at iter %d", *iter);
         abort();
      }
   }
```

For `VN_RELAX_REASON_RING_SEQNO` the profile is
`base_sleep_us = 160, busy_wait_order = 8, warn_order = 12, abort_order = 16`:
iterations 1..255 call `thrd_yield()`, everything after that calls
`os_time_sleep()`, and the first warning lands at iteration 4096.

`os_time_sleep()` is `clock_nanosleep(CLOCK_MONOTONIC, 0, ...)`, and this
kernel's `sys_nanosleep` rounds any sub-tick request **up to one 10 ms tick**.
So iteration 4096 should have been reached after roughly
`3841 x 10 ms ~= 38 s`, and again every ~40 s after that.

## What was observed

Nothing. Across three separate reproductions:

| run | hang duration | Mesa output |
|---|---|---|
| `vkhang.py` (aarch64/TCG, prior session) | 2402 s | none |
| `vkprobe1.py` (x86_64/TCG, console stderr) | 370 s | none |
| `vkprobe3.py` (x86_64/TCG, `vktest > /tmp/vk1.log 2>&1 &`) | 350 s | none |

The redirected run is the strongest: stderr is unbuffered in musl, so a
`vn_log` write would have landed in the file immediately, after the last
flushed stdout line. The file ends at `[PASS] vkCreateInstance -> VK_SUCCESS`.
Expected in that window: roughly 8 warning lines. Observed: zero. No `abort()`
either, which `vn_relax` would have reached at `abort_order` eventually.

The guest was demonstrably alive and idle, not crashed: `info registers -a`
showed all four vCPUs with `HLT=1`, Ctrl-C returned to the shell, and the ring
control words were byte-identical at t=200 s and t=350 s
(`head=0x25C tail=0x2BC status=0x5`).

## The two candidate explanations

1. **The waiting thread was parked inside one call and never iterated.**
   The only two syscalls in the relax loop are `sched_yield` (iterations
   1..255) and `clock_nanosleep` (after that). If either can block forever,
   `*iter` never reaches 4096 and nothing is ever logged. This is the one
   worth chasing: `sched_yield` is `yield_now()`, and `clock_nanosleep`
   goes through `sleep_ticks_from` ->
   `sched::block_on_poll_prepare_until(deadline)` — the poll wait-channel,
   which is shared with poll/epoll waiters and has a recorded history of
   deadline-wake races (see the comment above the poll-deadline tick service
   in `sched/src/lib.rs`, and the M7 stranding it describes).

2. **Mesa's guest-side `mesa_log` never reaches stderr in this ICD build.**
   Host-side `virgl_render_server` messages *do* appear on the serial log
   (`vkr: submit_cmd: vn_dispatch_command failed` shows up during `venustest`),
   but that is the renderer's stderr in a different process, not the guest's.
   The ICD is built in an Alpine container
   (`leandros-artifacts/venus-lane/build-venus-icd-alpine.sh`); its meson
   options were not checked.

Distinguishing them is cheap and needs no Venus at all: a ~20-line guest
program that calls `nanosleep(160 us)` in a loop and prints an iteration
counter would settle (1) on its own, and a program that calls a Mesa entry
point known to log would settle (2).

## Why it does not block the clock fix

The deadlock is now measured, explained and fixed independently of this: the
host ring thread was idle with 96 bytes of a well-formed venus command
unconsumed at `head`, and Mesa's notify throttle had suppressed the wake-up
because `clock_gettime` advanced in 10 ms steps. Whatever the guest thread was
doing while it waited, it was waiting for a reply that was never going to
come. With the fix, `vktest` completes in 6-10 s on both TCG arches, so the
wait never gets long enough to reach a relax warning either way.
