# lane/mmapA - x86_64 TLB shootdown off the address-space lock (2026-10-29)

Plan: designs/mmapcost-2026-10-29.md, Lane A. Base main 11fc1860.

## Changes
- arch/x86_64/src/paging.rs: `flush_remote` split into `flush_remote_start` (IPIs, no wait),
  `flush_remote_poll` (acks latch in the ticket), `flush_remote_wait` (bounded, returns false on
  timeout). New `arch_tlb_flush_range_start/_poll/_wait`; `arch_tlb_flush_range` = start + wait.
  aarch64 stubs return an always-done ticket (broadcast TLBI is complete on return).
- mm/src/paging.rs: `FlushTicket`, `tlb_flush_range_start`, `flush_poll`, `flush_wait`.
- mm/src/vmm.rs: `unmap_range`, `strip_range`, the zero-range path and the VMO-truncate path start the
  flush and hand frames / buddy blocks / device ranges / pruned page-table pages to `free_after_flush`:
  freed now if every ack is in, else queued on a global deferred list. `drain_deferred(wait)` runs from
  `unmap_chunked` (kernel/src/syscall.rs) after the AS lock is dropped. A timed-out wait never frees:
  the entry stays queued (counters `DEFERRED_FREES`, `DEFERRED_TIMEOUTS`), fixing the latent
  use-after-free on ACK_SPINS timeout for these paths.
- Ack service points: syscall_dispatch entry, copy_from_user, copy_to_user, uaccess::prefault
  call `tlb_service_pending()`.
- userland/mmapbench (relibc-linked, registered in userland/Cargo.toml, build-userland.sh,
  mkfs-f2fs-populated.py): `mmapbench [iters=2000]`.

## Numbers (box KVM x86_64, 2000 iters, 4 MiB, us/call)
| case | before mmap/munmap | after mmap/munmap |
| anon | 1 / 328 | 1 / 293 |
| memfd touched | 188 / 135 | 188 / 138 |
| memfd untouched | 188 / 135 | 187 / 137 |
| anon +3 busy | 9 / 343 | 6 / 308 |
| memfd +3 busy | 171 / 143 | 186 / 149 |
as_hold_max_us: 591/377 before, 316/513 after. TIMEOUTS not printed by the bench.

## Caveats
- The bench's busy siblings (pread of 1 MiB from tmpfs) are short syscalls, so it does NOT reproduce the
  laptop's multi-hundred-us ack waits (Firefox's siblings sit in recvmsg/ioctl); before ~= after here.
  anon munmap (~300 us) is dominated by freeing 1024 pages one at a time (batch free is the next item).
  The real proof is the laptop ffgpu4 re-run.
- Not converted (still wait synchronously under the lock, and free after a timed-out wait):
  mprotect/madvise (`tlb_flush_range` callers at the CoW/permission sites), mremap move, kmodwin, kva.
- Deferred entries are drained only by munmap; frames of a timed-out entry are held until the next munmap.
