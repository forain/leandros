# lane/mmlock — 2026-10-19

Bug list items L1.3, L1.6 and L1.11 (artifacts/notes/openbugs-2026-10-19.md).

## L1.11 — `with_task_address_space` copies (evdev, DRM)

What actually happened (tested on main's kernel): the pid a device server is
handed is always the caller's own process (servers run synchronously in the
caller's context), so the root switch reloaded the same root, and a
not-yet-present destination (fresh anon page, untouched private file page)
faulted in normally: those cases worked. What was broken was the raw
`copy_nonoverlapping`: it was outside the fault-tolerant copy, so a bad
destination (PROT_NONE, read-only) killed the caller with SIGSEGV
(`[EXC] killing PID=.. (unresolvable user-pointer fault in kernel)`) instead
of failing with EFAULT. For a pid that really was another process the fault
would have been serviced against the CURRENT task's address space.

Fix: `with_task_address_space` is gone. `sched::copy_to_task` /
`copy_from_task` / `zero_task`: own address space → `uaccess` copy (faults
resolved as usual, EFAULT for bad pointers); another process → its page
tables via the HHDM under its `busy` flag, present pages only, VMA
protection checked, no root switch, no IRQ-masked window.

## L1.3 — /proc observers spinning on another process's `busy`

`with_process_as` (smaps, smaps_rollup, status, statm, cmdline, environ)
spun on the target's `busy` flag with IRQs masked, unbounded. Now
`acquire_foreign_as`: spin 50 µs, then park 1 ms at a time, give up on
SIGKILL or after 2 s (`[AS] observer gave up on the address space of pid N`;
the read then fails / omits memory lines). Falls back to the old spin where
sleeping is not allowed. The census also marks the flag held (lockwatch), so
`can_sleep()` is false under it.

Also: `can_sleep()` now checks a per-CPU count of held address-space flags.
lockwatch's single `HOLDER` slot per lock id is overwritten when a second
CPU takes another process's flag, so a holder could look sleepable.

The audit of every `busy` holder (kernel/ sched/ servers/ mm/) found none that
sleeps, touches user VAs, does file/port I/O or leaks the flag; so the
laptop's 50-min sshd wedge is not explained by a leaked flag in the current
tree. The observer side is what could turn a long hold into a wedged CPU, and
that is fixed. Long holds remain (see Open).

## L1.6 — address-space teardown with IRQs masked

`Drop for AddressSpace` freed every page in one IRQ-masked run. It now calls a
reschedule hook (`sched::kthread::cond_resched`, registered at init; no-op
where sleeping is not allowed) every 512 freed pages. The last reference is
normally dropped by the exiting thread itself (task context, no locks).

## Diagnostics

`/proc/sysstat` first line: `# mm late_tick_max_ms N teardown_points N
observer_parked N observer_gaveup N foreign_copies N`. `late_tick_max_ms` is
the longest timer-interrupt delay a busy (not idle) CPU saw since the last
read (reset on read) — the kernel's own measure of an IRQ-masked stretch.

## Tests (aarch64, HVF) — `memtest mmlock` (also in the default memtest run)

- `evdev_copy_unfaulted`: EVIOCGID into untouched anon pages (incl. page
  straddle) and an untouched private file page; PROT_NONE / PROT_READ
  destinations in a fork child must give EFAULT. main: FAIL (child killed by
  SIGSEGV, status 11, both cases). lane: PASS.
- `proc_reader_vs_busy`: 3 s of smaps/smaps_rollup/status reads of a process
  churning 64 MiB mmap/touch/munmap: ~31k reads, 0 empty, worst 10–24 ms
  (main: PASS too, worst ~2 ms — holds there are short; the test guards the
  no-failure / bounded-latency contract).
- `teardown_irq_latency`: victim touches half of free RAM (12 GiB guest:
  5.9 GiB) and exits while 8 spinners compete. Hook disabled:
  `late_tick_max_ms=590 / 280` (= the whole teardown) FAIL. Lane:
  see below.
  Lane: `late_tick_max_ms=0 / 0`, worst spinner gap 50–60 ms, PASS
  (exit-to-reap 0.66–1.4 s: the teardown now shares its CPU with the
  spinners). `# mm ... teardown_points 5942 observer_parked 40
  observer_gaveup 0 foreign_copies 0` — the park path ran; no device copy
  ever targeted another process.
- aarch64 regress (runtests --suite regress): all green except
  nettag2test under host load; re-run alone under the qemu lock: 9/9 PASS.
  Full `memtest`: RC=0.

## Both-arch batch notes

All changes are arch-independent Rust (sched, mm, vfs, evdev, drm). The x86
kernel-mode user-fault path (`arch/x86_64/src/idt.rs` ~650) uses the same
`uaccess::fixup`, so the EFAULT path is the same; x86 not run in the lane.

## Open

- Long `busy` holds still exist, each one IRQ-masked: `munmap`/`mremap` grow
  of a huge range, `mincore` over a 64 GiB reservation (per-page VMA scan),
  fork's `clone_as` under the global `COW_LOCK` (its shootdown waits out
  `ACK_SPINS` against CPUs spinning on `COW_LOCK`), and the smaps census
  itself (`pageref::get` per resident page). Holders cannot yield while other
  CPUs spin on the flag; making those preemptible needs a sleeping AS lock.
- Eager file mmap (kernel/src/syscall.rs ~3026): the VFS read fills
  `phys_to_virt(phys)` after `busy` is dropped; a sibling's munmap meanwhile
  frees the block under the write (from the audit; not verified).
- The 50-min laptop sshd outage is not reproduced; with this lane a reader
  can no longer wedge on a busy flag, and gives up loudly after 2 s.
