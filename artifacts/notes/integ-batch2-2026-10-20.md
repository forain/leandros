# integ/batch2 — main da51b45e, 2026-10-20

Release build OK on both arches. When both arches run in parallel, each needs its own LEANDROS_RUN_ID.
runtests' default id `runtests` is global under /tmp. On the first attempt the two arches
collided (they shared sockets), so that attempt was discarded.

## aarch64 (HVF)
- regress: everything passes except pollnarrowtest, nettag2test and kparkstest, which
  failed under load (1 stray wake each). Re-run alone under the qemu lock: all three PASS.
- net-e2e PASS. f2fscrashtest PASS (6 iterations, mix mode).

## x86_64 (TCG)
- net-e2e PASS. f2fscrashtest PASS.
- regress: the only failure is syscalltest `uaccess_fs`. Given a "straddle" pointer
  (2 mapped 'a' bytes, then an unmapped page), memfd_create, the getxattr name and the
  mount target return success instead of EFAULT. Reproduced in 4/4 runs.
- Evidence: a temporary kernel print in copy_user_str showed the kernel reading 0x04
  at m+pg-2 (n=2, no fault). User mode reads 'a' across the whole page (new check in
  make_bad), and a forked child gets SIGSEGV on the unmapped page (new probe). So during
  the syscall the kernel reads this user page through a STALE translation (an old frame),
  not through the PTE that user mode sees.
- Suspected cause (unverified): a stale x86 TLB entry. Since lane/vmreuse2 (fe5126c6),
  VAs recur identically across processes. Two ways an old translation could stay live:
  (1) a freed PML4 frame is reused as the next fork's root and the context switch skips
  the CR3 write because the root looks unchanged; (2) a lazy/CoW PTE installed on
  another CPU gets no shootdown. aarch64 (ASIDs, broadcast TLBI) is not affected.
  Merges: the test came in with lane/uaccess (67c52d8e, tested on aarch64 only). The
  underlying bug most likely surfaced with lane/vmreuse2 (fe5126c6). Not bisected
  (stopped on context budget).
- Next steps: check where the context switch skips the CR3 write and the x86
  fault-install/CoW flush paths. Bisect fe5126c6 against its parent by running
  syscalltest on x86.

## Commits
- syscalltest: make_bad now checks that the mapped page survives the munmap of its
  neighbour, and that both unmapped pages fault from user mode (test hardening; both
  checks pass on x86).
