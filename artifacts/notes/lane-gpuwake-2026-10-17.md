# lane/gpuwake — 2026-10-17

## Problem (measured on the real laptop during YouTube)

All 8 CPUs ~98 % busy; `/proc/lockstat` RUN_QUEUE ~516k acq/s, 51 %
contended, ~2.75 CPUs pure spin; 49–61 runnable threads including idle
daemons. Cause: GPU fence wakes were system-wide poll herds
(`unblock_port_tagged(POLL_WAIT_CHANNEL, POLL_TAG_ALL)`), from

- `syncobj_install` -> `syncobj_wake` -> blocking `wake_poll()` per
  signalled syncobj per exec (i915 real path, NVIDIA, virtio);
- `gpu_fence_irq_hint` on every i915 retire (and the fake engine's submit),
  every NVIDIA non-stall IRQ;
- `syncobj_retire_fence` (virtio/v3d watermark);
- `syncobj_tick`: an unconditional herd every 10 ms tick while any syncobj
  waiter existed and a GPU backend (i915/NVIDIA) was registered.

## Fix (8827cc9d)

- `sched::POLL_TAG_SYNCOBJ` (`poll_class::SYNCOBJ`). Every syncobj/fence
  waiter parks with it (`syncobj_park`: SYNCOBJ_WAIT/TIMELINE_WAIT,
  `wait_fences` — i915 GEM_WAIT/exec in-fences, NVIDIA EXEC/KMS producer —
  and the v3d BO wait).
- All four wake sources use `syncobj_try_wake` ->
  `sched::try_wake_poll_exclusive(POLL_TAG_SYNCOBJ)`: non-blocking
  (`try_lock_spin` 50 µs; on failure the tag is queued with
  `request_poll_wake_tagged` and paid on the next tick), and EXCLUSIVE —
  broadcast-mask (`POLL_TAG_ALL`) parkers are skipped.
  Why that is sound: a broadcast mask means "I hold an interest I could not
  name"; no pollable object has its readiness published by a syncobj wake
  (sync files are eventfds signalled by `sync_file::tick` with their EVENTFD
  tag; DRM events use the open's DRM tag; dma-buf poll does not consult
  fences), and every park that does depend on syncobj state now carries the
  exact tag. Signals (broadcast tag) and per-task deadlines still reach
  syncobj waiters. Without "exclusive", each GPU wake still woke every
  broadcast-mask poller (measured: sshd on every submit in QEMU).
- `syncobj_tick`: per-tick herd removed; it only backstops a watermark
  advance (tagged). Backend-fence waiters already re-probe every 1 ms
  (`EXT_FENCE_REPROBE_NS`).
- `drm.syncobjherd=1`: restores the old broadcast parks/wakes and the
  per-tick herd — for A/B on hardware without rebuilding.
- Unchanged and already tagged: virtio VIRTGPU_WAIT (DRM FENCE tag), DRM
  event fd poll (per-open tag), sync_file poll (EVENTFD tag).

## Instrumentation

- `/proc/wakestat` (always on, cumulative since boot — diff two reads):
  `site calls tagged woken allmask` per wake call site (`file:line` of the
  `#[track_caller]` chain; e.g. `drivers/src/drm_device_interface.rs:35xx`
  are the syncobj sites; in e6d72d82 `sched/src/lib.rs:2459` is the
  deadline/timerfd service, `:2354` the deferred-wake service, `:1412` the
  signalfd `wake_poll()` in `deliver_signal`). `allmask` =
  woken tasks that had the broadcast mask.
- `sched.wakestat=1` adds `outcome <site> <tgid> <useful> <spurious>` (the
  former compile-time `gdwake`/`GD_STATS` census, now runtime; serial
  printing stays off, `GD_SERIAL`).
- `/proc/<pid>/stat` field 14 (utime) = thread-group CPU time in 1/100 s
  ticks (user+kernel, no split; stime stays 0) — `top`/`ps` can now rank.
- Not done: pcsample knob (optional item).

## Measurements (QEMU x86_64 TCG, fake i915 `i915.fake=2 i915.kms=1`, headless)

`i915test wakebench 1000` (e6d72d82): a forked child parks in
TIMELINE_WAIT(WAIT_FOR_SUBMIT) on point k while the parent submits a batch
signalling k, 1000 times, 2 ms apart — the laptop's i915 install+retire
pattern with a parked waiter. Same kernel, `drm.syncobjherd=1` vs default:

| during the bench               | legacy (herd) | gpuwake | Δ |
|--------------------------------|--------------:|--------:|---|
| GPU-site wakes: tasks woken    | 15 403 (≈11.4 per install wake) | 998 (= the waiter) | −94 % |
| of which broadcast-mask tasks  | 2 198 | 0 | |
| per-tick herd (syncobj_tick)   | 308 calls / 3 907 woken | 0 | |
| RUN_QUEUE acquisitions         | 130 635 | 54 233 | −58 % |
| RUN_QUEUE contended            | 36 976 | 10 200 | −72 % |
| RUN_QUEUE spin                 | 252 ms | 116 ms | −54 % |
| EPOLL acquisitions             | 21 943 | 3 459 | −84 % |
| FD_TABLE acquisitions          | 56 959 | 18 633 | −67 % |

Idle 10 s windows were identical between the two (27.4k RUN_QUEUE acq).
Only ~12 pollers are parked in this headless boot; the herd scales with
the parked-task count, ~600 on the laptop desktop, so the legacy cost there
is ~50x per wake and the gpuwake cost is unchanged (the waiter only).

virgl desktop (aarch64, COSMIC greeter on virgl): no syncobj waiters exist
(no GPU-UAPI backend), so no syncobj wakes either way — numbers identical;
it cannot show this herd. Venus does not work on the Mac.

## Tests (release builds)

- regress: x86_64 24/24 PASS, aarch64 24/24 PASS.
- drm suite (`--virgl`, `drm.syncfile=1`: SYNCOBJ_* incl.
  WAIT_WOKEN_BY_FORKED_SIGNAL, SYNC_FILE_*, V3D_WAIT_BO_BLOCKS_THEN_COMPLETES,
  EXECBUF out-fence): 72 PASS / 0 FAIL on both arches.
- fake i915 (`i915.fake=2 i915.kms=1`): `i915test -v` failures 0 and
  `i915test wakebench 2000` PASS on both arches.

## Laptop

Kernel built, NOT deployed: `LEANDROS_KERNEL_TAG=gpuwake-e6d72d82`,
`target/final-x86_64/kernel` of that build.

Expectations during YouTube: the GPU sites in `/proc/wakestat` should show
`woken ≈ calls` (one waiter each, allmask 0) instead of ~600 per call;
RUN_QUEUE acq/s should drop by most of the herd share (the 516k/s was
dominated by woken pollers re-running), runnable threads should fall to the
real workers, and HDA/video starvation should go. A/B without redeploying:
add `drm.syncobjherd=1` to the cmdline. Read `/proc/wakestat` twice ~10 s
apart; add `sched.wakestat=1` to see useful/spurious per (site, tgid).

## Follow-ups seen in the census (not fixed here)

- `servers/evdev/src/lib.rs:557` `wake_pollers()` is still a broadcast
  `try_wake_poll()` (≤100/s, burst mode): in QEMU each one woke ~12 tasks,
  ~all spurious. Touchpad motion on the laptop = up to 100 herds/s. The
  probe already reports per-device EVDEV tags; the producer could OR the
  pushed devices' tags (careful: console/tty readers of the keyboard).
- Signal delivery is a broadcast: `try_deliver_signal*` and the signalfd
  `wake_poll()` in `deliver_signal` (one herd per signal; a SIGCHLD/timer
  heavy workload pays it). A SIGNALFD tag per tgid would narrow the latter.
- Broadcast-mask parkers (sshd here) are woken by every other tagged wake;
  `allmask` per site shows how much that costs.
