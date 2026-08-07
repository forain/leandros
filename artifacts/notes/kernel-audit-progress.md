# Kernel Readiness Audit — Progress Checkpoint

Host-only read-only lane. Repo /Users/forain/code/leandros is READ-ONLY.
Deliverable: ~/code/leandros-artifacts/notes/kernel-readiness-audit.md

## Step log

- [TASK1 done] mmap/mprotect: sys_mmap syscall.rs:1404 rejects RWX (W^X); sys_mprotect->vmm.rs:995 mprotect() splits VMAs (split_at 761) + remaps backed pages with new flags (1030-1055) + tlb_shootdown_all. RW->RX SUPPORTED both arches. x86 NX correct (paging.rs:391). aarch64 translate_flags:180 correct BUT: SCTLR_EL1.UCI never set (grep empty) => EL0 dc cvau/ic ivau TRAP (EC 0x18, exception.rs:142 handler does NOT decode 0x18 -> process killed). arch_flush_cache_range (lib.rs:14) is dead (no callers) AND incomplete (dc cvau only, no ic ivau). No cacheflush syscall. => aarch64 NEEDS-FIX; x86 READY.
- [TASK2 done] Scheduler idle correctly halts (hlt/wfi sched/lib.rs:1448/1450). CULPRIT = servers/init/src/lib.rs:2601 event_loop() unconditional yield_now spin (PID1 always Ready -> one core 100%). Secondary busy-spins: net_blocking_op syscall.rs:5830, sys_read_sock 3649, sys_read_vfs 3675, futex_wait timed 80. block_on_port(795) + poll/epoll/nanosleep/stdin-read all already three-phase-block (OK).
- [TASK3 done] Image DYNAMICALLY sized to 2x content (mkfs...populated.py:551-553). NO fixed size constant. Current f2fs-data0: aarch64 1.14GB, x86 1.17GB. llvmpipe deps: aarch64 147MB, x86 162MB (libLLVM.so.19.1 alone ~148MB). +deps -> each image auto-grows ~+300MB. Integration action = ADD .so to usr_lib_files list (mirror GL set 375-414), no constant. NAT/SIT headroom enormous (465K nids / 28K segs=56GB cap).

- [DELIVERABLE WRITTEN] ~/code/leandros-artifacts/notes/kernel-readiness-audit.md complete. STOP.

- [started] Env survey done. kernel/src is flat: entry_aarch64.s, entry_x86_64.s, init.rs, main.rs, mem.rs, syscall.rs, x86_64_start.rs.
  - sys_mmap = kernel/src/syscall.rs:1388 ; sys_mprotect = kernel/src/syscall.rs:2506
  - servers/: drm evdev f2fs init libc-shim net pipewire proc tty vfs xattr
  - sched/ crate exists (sched/src)
  - llvmpipe deps confirmed: deps-<arch>/ has libLLVM.so.19.1, libstdc++.so.6.0.33, libxml2, libzstd, liblzma, libgcc_s
