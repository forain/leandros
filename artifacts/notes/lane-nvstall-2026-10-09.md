# lane/nvstall — 2026-10-09 — tick stalls, slow COSMIC frames, greeter on autotest boots

Branch `lane/nvstall`, based on main `de7fd3e` (lane/nvzink merged). Not
pushed. Input: `nvrm-boot6.log` (real PC, Kaby Lake 4C/4T, RTX 3070, USB 2.0
stick root), the first boot with COSMIC actually running on the 3070.

## 1. The 61 `[WDOG] ... took no timer tick for ~2 s` reports

### Root cause

**`F2FS_MOUNTS` — one global IRQ-off spinlock held across synchronous USB
block I/O — plus an I/O storm that COSMIC's session start causes on a
slow stick.** Not the GPU path.

Evidence from boot6:

- Zero reports in boots 2-5 (same kernel paths for nvktest/nvkmstest, no
  COSMIC). In boot6 every report falls inside COSMIC process start-up: the
  greeter (60-150 s) and the session plus its panel applets (224-471 s).
  They stop when the session has finished loading.
- `preempt_disable=0` in every report: the stalled CPU was **not** the one
  doing USB I/O (an `usb_storage::IoWindow` holder has preemption disabled
  and keeps taking ticks through its poll loop's `irq_window`s). It was a
  waiter.
- "not in a syscall" = page faults of lazily mapped binaries/libraries
  (fault reads go to `f2fs::pread_ino_by_port`, which takes `F2FS_MOUNTS`);
  "last syscall" 0/2/3/4/5/9/0x59/0x3b = read/open/close/stat/fstat/mmap/
  readlink/execve, all f2fs paths. No `spinning for <lock>` field: the lock
  was not one of the lockwatch-tracked ones.
- Several CPUs stall at once (cpu0+cpu1+cpu2, cpu3 reporting) — a queue on
  one lock, not a per-CPU timer failure.
- `[MMAP-SLOW] kind=file-lazy ... total_ms=19946 map_ms=0 io_ms=0`: a 20 s
  mmap that spent nothing in its timed phases — it was spinning in
  `inode_by_port` / `pin_inode` for `F2FS_MOUNTS`. `kind=file-copy len=0xBED
  io_ms=3728`: 3.7 s to read 3 KiB = lock wait.
- The storm: dozens of `[MMAP-SLOW] kind=file-copy flags=1` (MAP_SHARED,
  read-only) of 0.2-0.7 MB files on fd 5/6/13/27/29, from every panel applet
  (cosmic-panel-button x4, cosmic-applet-*, cosmic-osd, cosmic-files-applet,
  ...). That is fontdb (cosmic-text) mapping **every font file** with memmap2
  in every COSMIC process. A read-only MAP_SHARED f2fs mapping took the eager
  path: the whole file read at mmap time, privately, per process — on a USB
  2.0 stick at a few MB/s, with every other CPU queued behind it.
- `spin::Mutex` is unfair: a waiter could lose the race again and again
  (the 12 s and 20 s cases).

Consequences beyond the log noise: when the stalled CPU was the BSP
(cpu0 is in 18 of 61 reports), every 100 Hz tick hook stopped with it —
`drm_tick` (flip events, sync-file signalling), USB HID polling (mouse,
keyboard), audio, poll deadlines. The compositor got no flip events and no
input for seconds at a time. nvktest's `vkCreateComputePipelines` went from
157 ms (boot5) to 3013 ms (its page faults queued behind the greeter's).

### Fix

1. `sched::iolock::IoMutex` (new, `sched/src/iolock.rs`): a **ticket lock**
   (FIFO, no starvation) whose IRQ-masked waiters open an `irq_window` every
   1024 spins under `preempt_disable` (tick yes, task switch no — the same
   contract as the USB/virtio poll windows), service TLB-shootdown requests,
   and publish themselves to lockwatch as `F2FS_MOUNTS` (new id `L_F2FS`).
   Callers with IRQs enabled run preempt-disabled from ticket to release (a
   descheduled ticket holder would stall the whole queue). Waits >= 500 ms
   print one `[LOCKWAIT]` line (rate-limited to 1/s) naming the waiter, its
   syscall, queue depth, average wait and max hold. `F2FS_MOUNTS` uses it.
2. `usb_storage::UsbDisk::lock`'s masked spin gets the same windows + TLB
   service.
3. **Read-only MAP_SHARED f2fs mappings are demand-paged** like MAP_PRIVATE
   ones (`kernel/src/syscall.rs`, `sys_mmap_inner`): only the pages touched
   are read, and they are shared through the page cache across processes.
   The eager copy was already a private snapshot, so nothing is lost; a
   writable MAP_SHARED f2fs mapping keeps the eager path. This removes most
   of the storm (fontdb reads a few table headers per face).

### New diagnostics (cheap, always on)

- **NMI stall probe (x86_64)**: when the watchdog reports a stalled CPU it
  sends it an NMI (`arch_send_nmi`); the NMI handler (`exc_nmi`, which used
  to be fatal for every NMI) records RIP/RSP/RFLAGS/CS and up to 8
  frame-pointer return addresses into atomics; the observer prints
  `[WDOG]   cpuN NMI sample: kernel RIP=0x... RSP=0x... IF=0 bt: 0x... ...`.
  Unrequested NMIs stay fatal. Resolve addresses with
  `llvm-addr2line -fe target/build-x86_64-standard/.../kernel <addr>`.
- `[WDOG] ... spinning for F2FS_MOUNTS held by cpuN` now that the lock is
  tracked; `[LOCKWAIT] F2FS_MOUNTS: ...` for long-but-not-deaf waits.
- `wdog.selftest=1` (kernel command line, opt-in): cpu1 stalls with IRQs
  masked at its 3000th tick (~30 s) until the watchdog has reported it (at
  most 20 s), to show what a report and an NMI sample look like on that
  machine. The RIP is exact; the backtrace is best-effort (frame-pointer
  walk from the NMI frame; inlined frames and IRQ entry stubs blur it).

## 2. Slow greeter/desktop frame rate

The flip-event path, fence waits, implicit sync and the present were
re-read. What the log does show:

- the greeter presented only 45 frames per buffer (90 total) while it was
  up during the I/O storm; the session's 1000 presents averaged 132 us and
  the producer waits totalled 0 ms: **the present itself is not the cost**;
- the EXEC rate of the compositor's channel (seq numbers in sampled
  `[nvuapi] EXEC` lines) reached ~130-200 EXEC/s in bursts, ~6 EXECs per
  present;
- the BSP stalls above froze the flip events, input polling and sync-file
  signalling for seconds during the storm (224-471 s, most of what the user
  saw after logging in).

Changes:

1. **Synthetic 60 Hz vblank off the BSP tick** (`drm_device_interface.rs`):
   under a KMS backend every unfenced flip event (backend present, or a
   commit that moved no pixels) is delivered by a high-resolution one-shot
   timer at max(now, last delivery + 16.67 ms) instead of by `drm_tick`
   (up to 10 ms late, max one per tick, and nothing at all while the BSP was
   deaf). The tick still delivers after `FLIP_FALLBACK_NS` (20 ms) as a
   backstop. virtio paths unchanged.
2. **`[nvkms] frames` line every 5 s while presenting** (uapi_kms.rs):
   presents/s, active fps (gaps < 250 ms), longest gap, present avg/max,
   producer-wait n/avg/max, vblank events + queue->deliver latency,
   event->next-commit latency (= the compositor's render + CPU fence waits),
   fence waits (EXEC/VM_BIND in-fences, KMS producer wait, SYNCOBJ_WAIT —
   smithay's render-fence wait) n/avg/max/slept, GPU interrupt hints, ISR
   and EXEC deltas, pending sync files. Its doc comment says how to read it.

## 3. Greeter started on an `nvrm.vktest=1` boot

init starts greetd (pid 10) at 0.8 s; the gate lived only in `/bin/gpu-env`,
inside the greeter. Now **init** keeps the graphical login off when the
command line has `nvrm.vktest=1` without `nvrm.desktop=1` (or has
`nvrm.desktop=0`), printing `graphical login off: nvrm.vktest=1 ...`.
gpu-env's own check no longer depends on `[ -r /proc/cmdline ]`.
`nvkmstest`: SET_MASTER EBUSY (a compositor is master) turns dumb / gem /
vk / atomic / console into **SKIP** with that reason instead of FAIL.

## 4. Verification (Mac: aarch64 HVF, x86_64 TCG)

| check | result |
|---|---|
| `./scripts/build-all.sh` (both arches, release, default) | ok (also restored after the stick build) |
| `cargo check -p kernel --features nvidia` (x86_64) | clean, no new warnings in touched files |
| `runtests --suite regress` aarch64 / x86_64 | **15/15 / 15/15** (twice: before and after the selftest change) |
| `ffsession.py aarch64 --nofirefox` (virgl COSMIC desktop) | greeter 0.7 s, desktop 0.8 s, terminal 0.7 s |
| x86_64 NVIDIA stick (`nvidia-test-stick.sh --image`, `--cmdline wdog.selftest=1`), `--usb-stick --no-gpu` | `graphical login off: nvrm.vktest=1 ...`; no greeter; nvktest/nvgltest exit 3 (no NVIDIA GPU), nvkmstest 4 PASS 2 SKIP, `nvk-autotest: done` |
| watchdog selftest on that boot | `[WDOG] cpu1 took no timer tick for ~2 s ... local_ticks=3000 (seen from cpu0)` then `[WDOG]   cpu1 NMI sample: kernel RIP=FFFFFFFF80200D30 ... IF=0 bt: ...`; addr2line: RIP = `sched::wdog_selftest_stall` |

Not testable in QEMU: the 60 Hz pacing and `[nvkms] frames` (need the
NVIDIA KMS backend), the USB-stick contention itself (virtio/QEMU USB is
fast; no `[LOCKWAIT]` appeared in any QEMU run, as expected), nvkmstest's
master-busy SKIP (needs a compositor on a nouveau card0).

## 5. What the next PC boot should show

- No `[WDOG] ... took no timer tick` during COSMIC start. If one appears it
  carries an NMI sample (RIP + backtrace) and, if it is the f2fs lock,
  `spinning for F2FS_MOUNTS held by cpuN`.
- Possibly some `[LOCKWAIT] F2FS_MOUNTS` lines during session start (slow
  stick), with the CPU still ticking; far fewer `[MMAP-SLOW] kind=file-copy
  flags=1` lines (read-only shared maps are now `file-lazy`).
- On the autotest boot (vktest=1): `graphical login off: nvrm.vktest=1`, no
  greeter, nvkmstest 6/6 again.
- With `--cmdline nvrm.desktop=1` (or vktest=0): `[nvkms] frames` lines;
  expect `queue->deliver` <= 16.6 ms and active fps near 60 while moving
  the mouse. If not: `event->next commit` ~ frame time => compositor-side
  (render or fence waits; compare `fence waits` and `GPU irq hints`).
