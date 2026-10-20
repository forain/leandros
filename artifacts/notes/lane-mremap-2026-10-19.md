# lane/mremap — 2026-10-19

Bug list items L1.4, L1.5, L1.7, L1.8 (artifacts/notes/openbugs-2026-10-19.md).

## L1.4 — truncate under a live mapping: FIXED

Root cause: nothing took the pages back from the mappers. f2fs truncate dropped
the page-cache frames, tmpfs `free_from` dropped the VMO's references, and each
mapping kept its own reference: a detached frame that was no longer the file.
A tmpfs/memfd VMA's absent page also zero-filled privately.

- `AddressSpace::zap_backing(file_cap, vmo, from, to)` drops a VMA's resident
  pages whose backing bytes start in `[from, to)` (private CoW copies included,
  as Linux's truncate does; ELF BSS past the file extent is left alone).
- `sched::zap_backing_all` visits every process (no reverse map) with
  `with_address_space_mut`; only called when the object is known to be mapped.
- f2fs: `sys_ftruncate` (and `truncate(2)` through it) looks up the inode's
  registered cap (`mmap_cap_get`, no cap = nothing maps it) and zaps after the
  truncate, outside every filesystem lock. Refault past EOF = SIGBUS.
- tmpfs/memfd: VMAs now record the VMO they alias (`VmaRegion::vmo`, identity
  `serial << 16 | slot`, assigned at first `MAP_SHARED`); `handle_ftruncate`
  shrink and `FALLOC_FL_PUNCH_HOLE` zap after dropping the VFS locks; an absent
  page of such a VMA is `Fault::Bus`.

Open: `open(O_TRUNC)` does not zap; a fault whose unlocked read raced the
truncate can still install one stale page; a zapped memfd page stays SIGBUS
even after the file grows back (no VMO refault path).

## L1.5 — mremap: FIXED

Root cause: `sys_mremap` only moved shared mappings of unchanged size (or
growable shm/file); everything else was `mmap(new) + copy`, with
`MREMAP_FIXED`'s address passed to `sys_mmap` as a hint (no unmap), and a
growing memfd mapping copied into private memory.

Now: `mremap_grow` (in place, any kind) and `mremap_move` (moves PTEs for every
kind: private anon/file keep pages and CoW state, shared keeps sharing, eager
VMAs converted via `make_lazy`, whole device apertures move but never grow).
A memfd VMA's grown tail gets the VMO's own frames
(`vfs::vmo_acquire_frames_by_id`, taken before `busy`). `MREMAP_FIXED` unmaps
the target (EINVAL without MAYMOVE / overlapping), `MREMAP_DONTUNMAP`
(private anon) supported, EFAULT if the old range spans VMAs. `old_size == 0`
stays EINVAL.

## L1.7 — /proc/<pid>/maps empty, smaps unnamed: FIXED

Root cause: `maps` was a stub returning 0 bytes (VFS `gen_proc_self_content`;
also a kernel readlink stub), and neither VmaStat nor any registry carried a
name. `gen_proc_maps` now renders maps/smaps/smaps_rollup from a census copied
out under `busy`, then names outside it: file caps by the path they were first
mmapped/exec'd through (`FILE_NAMES` in syscall.rs, `mm::vmm::file_name` hook),
memfd as `/memfd:<name> (deleted)`, tmpfs by path, `[heap]`, `[stack]` (no
vDSO exists). Cap raised from 256 KiB (smaps was silently cut) to 16 MiB;
`maps` added to PROC_PID_FILES.

## L1.8 — eager stack / RssFile: ALREADY-FIXED

The stack has been demand-paged since 2026-09-26 (`map_lazy` + 4-page
prefault, `docs/kernel/memory.md`), and written private file pages count as
RssAnon since 543d02f0 (`written` bitmap). memtest `stack_demand_paged`
(84 KiB before, 4260 after a 4 MiB recursion) and `proc_rss_file_written`
pass. Only stale TODO/docs lines remained; removed. Residual: eager-copy
private mappings of non-f2fs files still count all pages RssFile.

## Evidence (aarch64, HVF)

`memtest mremap` (new subset): truncate_mapped_sigbus, mremap_linux_semantics,
proc_maps_names (240663 bytes, 6008 lines), mremap_preserves_data,
mremap_nomove, stack_demand_paged, proc_rss_file_written: all PASS.
Regress (aarch64): all green except one udptest `udp_mcast_loop` failure; udptest re-run 3/3 PASS (network flake, unrelated). After rebasing on c71ed0c6: memtest, vfstest, shmtest, scmtest, forktest, exectest, pcachetest PASS; full regress green except wifisimtest timing out under host load (re-run alone: PASS, 35.9 s). Pre-fix failure of the new cases was not re-run (the
old code paths are a 0-byte stub, a hint-only FIXED and no zap at all).
