# lane/uptrfault: kernel faults on lazy / bad user pages (2026-09-27)

Branch `lane/uptrfault` from `origin/integ-wave-0924` (b7f48ccf), head `295d47c9`, pushed.
Machines: Mac only. aarch64 ran on HVF; x86_64 ran on TCG. The desktop and the laptop were not used.

## Outcome: FIXED

The premise that the in-kernel fault path does not handle lazy pages was **partly refuted**. Valid lazy
anon, file, CoW and deep-stack pages were already resolved from kernel mode, through
`exc_el1_sync_handler` and `fault_common`, into `handle_page_fault`. uptrtest's valid-buffer cases
passed on base aarch64. The real defects are listed below.

1. **Unresolvable user faults under a spinlock.** On aarch64 the task was killed with the lock still
   held. On x86 the CPU halted with the lock still held. The laptop wedge fits this exactly: x86 halted
   in `UnixRing::write` with `UNIX_CONNS` held, and three CPUs spun on the lock, which is the QEMU
   300 %. Deterministic repro: `recv()` into a PROT_NONE page.
   - Base aarch64: `[EXC] killing PID` in recvfrom, then cpu0 wedged, because exit teardown re-takes
     `UNIX_CONNS`.
   - Base x86: `*** KERNEL EXCEPTION *** Vector 0x0E` in recvfrom, then `[WDOG]`.
   - The same fault on a pipe read into a read-only page leaked `PIPE_RINGS`.
2. **x86 APs ran with CR0.WP=0.** The SIPI trampoline sets only PE and PG. Kernel stores into
   copy-on-write pages went into the parent's frame, and stores into PROT_READ pages succeeded. Base
   x86 failed 10 cow cases (recv, recvmsg, scm, socketpair, pipe2 and others) with the parent's memory
   modified.
3. **pread64/pwrite64 had no prefault.** A lazy private-file page reached f2fs under `F2FS_MOUNTS`, and
   the fault re-entered the filesystem. This is a deterministic deadlock on both arches (`[WDOG]`
   pid in syscall 0x44/0x12).
4. **readv() on a socket returned EBADF.** It went to the VFS.

## Changes
- `sched/src/uaccess.rs` (new): `copy_raw` is an asm copy (`rep movsb` on x86, a word/byte loop on
  aarch64) with a fixup landing pad. On top of it sit `copy_{from,to}_user`, `read_user`,
  `write_user` and `prefault`.
- `arch/x86_64/src/idt.rs`: the uaccess fixup, and a kill (SIGSEGV) instead of `hlt` for other kernel
  faults on user addresses.
- `arch/x86_64/src/lib.rs`: CR0.WP on every CPU.
- `arch/aarch64/src/exception{.rs,_asm.s}`: the fixup, which rewrites the saved ELR in the frame.
- `servers/net/src/lib.rs`:
  - The unix rings are fault-tolerant: they commit only copied bytes, undo a half-copied datagram, and
    return EFAULT.
  - Buffers are prefaulted before `UNIX_CONNS` in send, recv, sendmsg and recvmsg.
  - socketpair writes `sv` after dropping `SOCK_TABLES`, and TCP/UDP/ICMP recv copy out after dropping
    the stack lock.
  - Every msghdr, iov, cmsg, sockaddr and sockopt access is fault-tolerant.
- `servers/vfs/src/lib.rs`: pipe rings and tmpfs `vmo_copy_*` are fault-tolerant.
- `kernel/src/syscall.rs`:
  - prefault in pread64/pwrite64, pipe2, FIONREAD and the TTY ioctl fallback.
  - epoll_ctl reads the event before taking `EPOLL_INSTANCES`.
  - Socket route for readv.
- `userland/uptrtest` (new): 26 syscalls × {anon, file, cow, stack} plus 7 EFAULT cases and
  `after_efault`. Also added to `userland/Cargo.toml`, `build-userland.sh` and `mkfs-f2fs-populated.py`.

## Numbers
- Base: uptrtest never finishes on either arch.
  - aarch64: 64 PASS, then a pwrite hang. Run alone, `uptrtest e` wedges at efault_recv_protnone.
  - x86: 51 PASS, 18 FAIL (10 of them cow corruption), then a pwrite hang. `uptrtest e` gives a
    kernel #PF halt.
  - Some base FAILs were test-side and were fixed in the test: getsockname expectation, clock_gettime
    alignment, and the aarch64 binary still used SO_TYPE.
- Fix: uptrtest **112/112 RC=0 on both arches**.
- Fix: memtest, forktest, sigtest, polltest, exectest, `killmt 50`, drmsmoke and uptrtest all **RC=0**
  on aarch64 HVF and on x86_64 TCG.

## Open
- The Mac virgl greeter crash-loops with an EL0 NULL+0x224 fault. It is **identical on base**: pre-existing, not this lane.
- Still direct user accesses under locks, left resolvable-only:
  - vfs: RamFile/DevZero/FIONREAD under `FD_TABLES`, and getdents under `FD_TABLES`.
  - pty under `PTYS`, tty termios under `CONSOLE_TERMIOS`.
  - pipewire under `STATE` (non-faulting accessor).
  - A bad pointer there still kills the caller with the lock held. The fix is the same pattern:
    `copy_raw`.
- Unvalidated pointers: ioctl `arg` (block/DRM/tty), getdents64/getcwd, which validate only 1 byte,
  mount, memfd_create name, and sigaction.
- Each EFAULT still prints `[PF] handle_user_page_fault returned false`.
- Shared files touched: `kernel/src/syscall.rs`, `servers/vfs/src/lib.rs`, `sched/src/lib.rs` (one
  line), and the arch fault handlers.
