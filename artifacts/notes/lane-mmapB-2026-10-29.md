# lane mmapB — lazy MAP_SHARED of memfd/tmpfs (2026-10-29)

Plan: Lane B of the mmapcost design. Status: FIXED (aarch64); x86_64 boot checked once.

## What changed
- `sys_mmap` (kernel/src/syscall.rs): MAP_SHARED of a tmpfs/memfd now only creates the VMA
  (`AddressSpace::map_shared_vmo_lazy`) after `vfs::vmo_lazy_prepare` (promote to VMO, assign id,
  no frame touched). Dmabuf/borrowed objects and ranges past EOF keep the old eager path.
- Pages are found by the existing `FaultPlan::Vmo` path (`vmo_fault_frame`, AS unlocked), so the
  TMP_FILES/TMP_VMOS locks are never held across a range and never nest under AS-busy.
- Object lifetime: the fd is usually closed right after mmap, so an object could die with
  unfaulted pages. VMAs now count themselves on the object (`vmo_retain`/`vmo_release` hooks:
  set_vmo, fork clone, split, unmap, address-space drop). An object destroyed while mapped moves to
  `TMP_ZOMBIES` and is served from there; the last VMA frees it.
- recvmsg (servers/net): prefault only the bytes queued at entry; a copy that faults with nothing
  delivered retries once with the full RING_MAX window (no spurious EFAULT if data arrived late).
- Test: `shmtest memfd_lazy` (two mappings + pwrite/pread, kernel copy into an untouched page,
  close(fd) then touch, forked child write). Bench: `shmtest bench`.

## Numbers (aarch64, HVF, 4 MiB memfd, ns/iter, 500 iters)
| case | before | after |
|---|---|---|
| memfd untouched mmap / munmap | 65223 / 51113 | 2554 / 2202 |
| memfd touched (mmap+touch+munmap) | 65707+10353+51171 = 127k | 2760+1169932+43623 = 1.22M |
| anon touched (reference) | 3096+1344503+122625 | 3116+1346538+125876 |

Touched memfd is now first-touch dominated like anon (0.9x anon, target <= 1.5x). Every page costs
one fault (~1.1 us); fault-around (16 pages) is NOT implemented and would cut the touched case.

## Open
- Fault-around for VMO faults (one TMP_VMOS lock round trip per page today).
- x86_64 siblings-busy bench (needs the KVM box; root cause 1 is lane mmapA).
