# lane/blkfault (2026-10-19/20)

Items: L1.12, L4.8, L4.6. Tested on aarch64 release (HVF).

## L1.12 truncate vs MAP_SHARED: FIXED (971d60e5)
- tmpfs/memfd shared faults now look the frame up with the address space unlocked
  (`mm::vmm::vmo_page`, hook `vfs::vmo_fault_frame`), then `install_vmo_fault`
  re-validates against the global `BACKING_SHRINK_SEQ`. Every truncate/hole-punch zap
  bumps that sequence first (`sched::zap_backing_all`), so a fault that read old
  bytes and raced the zap retries instead of mapping a stale page.
- open(O_TRUNC) of a mapped f2fs file zaps its mappings like ftruncate(0)
  (`kernel/src/syscall.rs` vfs_open_resolved).
- Test: memtest `truncate_followups` (in regress).

## L4.8 dead virtio-blk: FIXED (1c467883)
- `drivers/src/virtio_blk.rs` wait_for_completion: bounded at 30 s; every 256 polls
  it checks device_status (all-ones or NEEDS_RESET/FAILED fails at once) and opens an
  IRQ window. On failure the disk is marked dead, the stuck descriptors and DMA pages
  are never reused, and later requests fail fast with EIO.
- The first version gated the IRQ window on `preempt_disabled()`. aarch64 syscalls
  run IRQ-masked with preemption enabled, so the window never opened:
  `[WDOG] cpu1 took no timer tick for ~22 s ... /bin/dd`. It now wraps the window in
  preempt_disable/preempt_enable_no_resched whenever IRQs are masked and pid != 0.
  Without the pid check, boot-time reads hang (the same gate nvme's IoWindow uses).
- Test: `.claude/skills/run-leandros/blkhangtest.py aarch64` (~3 min, not in regress).
  Result: PASS. dd gets EIO after 31 s, the system stays alive, the second read fails
  fast, the dead line is logged, and there is no WDOG line.
- Why the test did not hang before: blkdebug's `break` fires only on
  BLKDBG events, which a format driver emits on its file child. With blkdebug as the
  top node no event fires. The fix is `driver=raw,file.driver=blkdebug,...`.
- Pitfall: a request suspended in blkdebug blocks QEMU's quit. The test now resumes
  `r1` before `stop` and SIGKILLs its own QEMU (pidfile) if it is still alive.
  A stale QEMU holding the image lock looks like "FAIL: boot" with an empty serial.

## L4.6 coarse dirty tracking: OPEN (design limit)
Writeback leaves shared file PTEs writable, so a page written again after writeback
is not re-dirtied precisely. Fixing it needs a write-protect walk after writeback
(like zap_backing_all) plus a re-dirtying write fault. The cost is redundant
writeback, not data loss. See docs/kernel/memory.md:189 and TODO.md:215.

## Possible follow-up
nvme's IoWindow opens the IRQ window only for `masked && pid != 0 && !can_sleep`;
it already disables preemption itself, so it does not have the virtio-blk bug.
The page-fault (SIGBUS) path of a dead disk was not exercised end to end: the
test covers the syscall read (EIO). The fault path uses the same wait.

## Final regress (2026-10-20, rebased on main c286fd17): INCONCLUSIVE
`runtests.py aarch64 bf-reg --suite regress` gave "boots failed: 1/1": both login attempts
saw an empty serial, and every test said "cannot connect to serial socket". The same build
booted and logged in as root under blkhangtest a few minutes earlier, so this looks
environmental, but it is not proven. Before merging 1c467883, run regress once more. If it
is red, merge 971d60e5 (L1.12) alone. Regress was 41/41 green before the rebase (with an
earlier L4.8 version that had no IRQ-window change).
