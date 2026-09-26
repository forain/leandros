# lane/cowtlb: targeted TLB shootdowns, one flush per CoW promotion (2026-09-25)

Branch `lane/cowtlb` from `origin/integ-wave-0924` (439106b). Commits:
`95d4ec5` adds the `[TLBSTAT]` instrumentation, `46a7279` is the fix, `40a651b` gates the print to periods with activity.
Machines: desktop x86_64 **KVM** with Venus/Zink (`--venus`, text-login plus `start-cosmic-leandros`), desktop x86_64 **TCG** (fix build only), Mac aarch64 **HVF** (`--no-gpu`, serial tests).
Scratch: desktop `/run/media/forain/samsung970pro512/cowtlb-tmp/` (`cowbench.py`, logs, PPMs). The Mac copy is `/tmp/cowtlb/`.

## Premise check on KVM (base = 95d4ec5, the instrumentation only)
The premise holds on KVM, not only on TCG. Figures are for the first 20 s of boot plus session start. The 3 runs were consistent.
- About 8 k shootdowns. Every one was an all-CPU broadcast, **about 24 k IPIs**.
- Summed initiator ack wait was **9–11 s**. 35 % of shootdowns timed out after 200 k spins, about 2 ms each. The cause: targets were spinning with IRQs masked (a sibling thread on the address space's `busy` lock, or RUN_QUEUE).
- A CoW copy promotion averaged **3.5–4 ms** (max 60 ms). execve argument prefault averaged 20 ms (max 152 ms).
- Big file mmaps: `[MMAP-BIG]` costs 0–31 µs on KVM, so the lazymmap fix holds. Desktop visible 5.5 s after launch (VNC distinct-colour poll).

## Fix
- **x86_64** (`arch/x86_64/src/paging.rs`, `idt.rs`, `smp.rs`): `LOADED_ROOT[cpu]` is published at every CR3 write (Dekker ordering against the PTE-store + fence). The shootdown IPI goes only to CPUs that have the flushed root loaded. For a single-threaded process that means none, just a local `invlpg`. Acks use per-CPU flush generations, so there is no global TLB_LOCK. The AS-`busy` spinners, the TrackedMutex (RUN_QUEUE etc.) spinners and waiting initiators all service pending flushes themselves. The ICR write is IRQ-safe.
- **aarch64** (`arch/aarch64/src/paging.rs`): each page is flushed with a broadcast `tlbi vaae1is` + `dsb ish`, falling back to `vmalle1is` above 16 pages. No IPI is sent. The in-place RO→RW upgrade adds a local `tlbi vaale1`.
- **mm** (`vmm.rs`, `cow.rs`, `paging.rs`): a copy promotion is break-before-make with **one** flush. The shared-frame refcount is dropped only after that flush. munmap, mprotect and brk flush only their range; fork flushes only the parent's root.
- **kernel/syscall.rs**: `prefault_user_ro` handles execve argv/envp and path prefaults. Pages the kernel only reads are no longer CoW-copied.
- `sched/src/lockwatch.rs`: the contended `lock()` now spins on try_lock + service.

## After (fix)
| | base | fix |
|---|---|---|
| KVM session: IPIs | ~24 k | 1.9–2.0 k |
| KVM ack wait | 9–11 s | 32–66 ms |
| KVM ack timeouts | ~2–3 k | 12–28 |
| KVM CoW copy avg | 3.5–4 ms | 9–18 µs |
| KVM exec_pre max | 65–152 ms | 51–96 µs |
| KVM desktop up after launch | 5.5 s (x3) | 2.4–3.5 s (5 of 6 runs) |
| x86 TCG session (fix only) | lazymmap: up to 8.8 s exec_pre on Mac TCG | exec_pre max 322 µs, 0 WDOG, desktop up 9.9 s |
| HVF CoW copy avg | 2.7 µs | 2.3 µs (37.7 k → 20.6 k flushes, fork cost unchanged at ~4 ms) |
`[WDOG]`: 0 everywhere, in both base and fix.

## Tests (fix)
- x86_64 KVM: forktest 11/11 (cow_socket_fork 0 bad), memtest, exectest, killmt 50 (all 11 PASS), drmsmoke, pthreadtest, racetest and smpwaketest: all rc=0.
- aarch64 HVF: the same set is rc=0, run 4 times (the last run on 40a651b).
- The COSMIC session was not measured on aarch64 (the Mac has no GPU). The desktop disk filled up (see below), so no aarch64 build could be made there.

## Pre-existing, not this lane
brush on aarch64 HVF sometimes never prints its prompt after `pthreadtest` that follows `drmsmoke`. It sits in epoll_pwait with no deadline until the next input byte. **Base does it too: 6 of 24** (fix: about 1 of 12). Most likely a poll-wakeup bug (the polltimer area).

## Notes
- The desktop's samsung970pro512 disk hit **100 %** (ENOSPC in mkfs). I removed my own images and target, which leaves about 8.5 GB free. The x86 images in my desktop worktree are gone; rebuild them before reusing it.
- Remaining KVM timeouts (about 20 per session, about 1.7 ms each) are targets inside long IRQ-masked syscalls. Bounded, as before.
- Shared files touched: `mm/src/vmm.rs`, `mm/src/cow.rs`, `sched/src/lib.rs`, `sched/src/lockwatch.rs`, `kernel/src/syscall.rs`.
