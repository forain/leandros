# lane/forkcow — lazy copy-on-write fork (2026-09-24/25)

Branch `lane/forkcow` @ `52e7cf4` (feature commit `6836cdf` + merge of `origin/integ-wave-0924` @ `0fa195f`).

## What was eager, and why
`mm/src/cow.rs::clone_as` copied **every writable private VMA** (heap, stacks, .data/.bss, RW
mmaps) into the child at fork time (e9510cb). Read-only private VMAs were already CoW, MAP_SHARED
VMAs are refcounted and shared, and device VMAs (`file_cap == usize::MAX`) are aliased. The eager
copy worked around lost writes in brush/tokio. Two things actually caused those lost writes:
1. stale writable TLB entries in sibling threads. The stop-the-world quiesce plus the final
   shootdown already close this.
2. kernel stores through the HHDM (`write_user_buf`: wait status, signal frames, sigprocmask,
   itimers, read() via console, ...), which bypass the read-only PTE.

## Changes
- `cow.rs`: writable private VMAs take the normal CoW path, with no copy at fork.
- `vmm.rs`: `unshare_cow_page()`. `write_user_buf` now takes `&mut self` and unshares before
  each store. `prefault_range` unshares writable CoW pages, so "prefaulted ⇒ no later fault" still
  holds. A copy-promotion now does break-before-make (unmap + shootdown before mapping the new
  frame).
- **aarch64 `exception_asm.s`: removed the `exc_el1_sync_capture` diagnostic stub.** It used
  PAR_EL1 as scratch for x1. PAR_EL1 does not round-trip arbitrary values on hardware (RES0/RES1
  fields). Under HVF every recoverable EL1 fault therefore resumed with a corrupted x1. TCG was
  exact, so the bug stayed hidden. With lazy CoW, EL1 write faults on present pages became common.
  The symptom was a unix-socket recv that returned 64 bytes and stored them nowhere, which showed
  up as the COSMIC panel/applet respawn loop. Proven with a kernel-buffer bounce. The bug predates
  this lane.
- Callers of `write_user_buf` switched to `with_*_address_space_mut` (syscall.rs, sched/lib.rs,
  servers/vfs, tty, evdev).
- `[FORK] tgid= shared_pages= private_rw_pages= copied_pages= clone_us=` is printed for forks of
  address spaces over 1 MiB.
- forktest: added cow_isolation, cow_kernel_write, cow_threads_fork, cow_socket_fork (4 modes +
  pipe) and cow_fork_cost.
- Eager-tail freeing: my own loop was dropped in favour of greeterleak's `free_eager_tail`, which
  frees the tail once, at split_at and at clone_as.

## Numbers
| | before (eager) | after (lazy) |
|---|---|---|
| cosmic-comp fork, aarch64 HVF | 53,776 pages copied (210 MiB), 90.3 ms | 0 copied, 71,392 shared, 12.7 ms |
| forktest 128 MiB resident fork, aarch64 HVF | avg 51 ms | avg 4.6–7.4 ms |
| forktest 128 MiB, x86_64 TCG | not measured | avg 388 ms (TCG) |

## Tests
- aarch64 HVF: forktest 11/11 PASS (cow_socket_fork 0 bad over ~460 forks). Greeter up. A full
  `start-cosmic-leandros` session came up (panel, dock, wallpaper) with stable pids and no applet
  respawn.
- x86_64 **TCG** (Mac): forktest 11/11 PASS. Greeter up (screenshot). The full session was not
  run on x86.
- Before the PAR fix: cow_socket_fork failed on HVF only (it passed on aarch64 TCG), and the
  session showed CosmicAppletTime/PanelButton restarting over and over.

## Open
- The first x86 TCG forktest run to the serial console stopped printing after 10 forks. A rerun
  redirected to a file passed completely. Most likely serial output was lost after the driver
  timed out, not a kernel bug, but not proven.
- The eager baseline of cow_socket_fork passed on HVF only because nothing faulted at EL1 on
  present pages. Any EL1 demand-paging fault was already exposed to the x1 corruption before this
  lane.
