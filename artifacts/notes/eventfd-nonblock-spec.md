# eventfd EFD_NONBLOCK kernel-gap — landing spec

## IMPORTANT: the fix appears already in progress, uncommitted, in the working tree

This repo lane is read-only, but `git status`/`git diff` show the tree the other
agent owns currently has **uncommitted changes across 7 files**, including
`kernel/src/syscall.rs` and `servers/vfs/src/lib.rs`, that implement almost exactly
this fix already (see "Current uncommitted state" below). Treat the rest of this
document as verification + gap-check against that in-flight diff, not a
from-scratch spec — whoever lands this should `git diff` first to avoid duplicating
work.

## Background

`sys_eventfd2` previously discarded the `flags` argument entirely
(`fn sys_eventfd2(initval: usize, _flags: usize)`), so `EFD_NONBLOCK` set at
creation was silently dropped. `servers/vfs/src/lib.rs`'s `handle_eventfd` matched
(`flags: 0` hardcoded on the created `FdEntry`). A prior wave worked around this
shim-side in commit `0bed5ad` (`ports/input-stack/shims/libseat/libseat.c`) by
making `libseat_dispatch` never call `read()` on the idle connection eventfd at
all, because the read would otherwise EAGAIN once (correctly, from the VFS
counter check) and then **yield-spin forever** in the kernel's generic blocking
read loop, since the kernel had no record that the fd was meant to be
non-blocking. Flagged as an "M5 watch" because cosmic-comp/calloop create their
own eventfds and would hit the same wedge without a per-caller shim workaround.

## Flag values (confirmed)

- `EFD_NONBLOCK == O_NONBLOCK == 0o4000` (octal 4000, i.e. bit 0x800).
- `EFD_CLOEXEC == O_CLOEXEC == 0x8_0000`.
- Both are literally the same bit patterns used for pipes/timerfd/signalfd/sockets
  elsewhere in this codebase — no new constant is needed; `O_NONBLOCK_FL: u32 =
  0o4000` and `O_CLOEXEC: u32 = 0x8_0000` already exist module-level in
  `servers/vfs/src/lib.rs` (the net side has its own `O_NONBLOCK` check via
  `NET_GETFL`/bit `0x800`, see `kernel/src/syscall.rs:4562` `net_fd_nonblock`,
  same numeric bit).

## Current committed (HEAD) state — the gap

- `kernel/src/syscall.rs` (HEAD, committed): `sys_eventfd2` signature is
  `fn sys_eventfd2(initval: usize, _flags: usize) -> isize` (flags parameter
  named `_flags`, unused) and the VFS message is built as
  `make_vfs_msg(vfs::VFS_EVENTFD, &[initval as u64])` — flags never sent.
- `servers/vfs/src/lib.rs` (HEAD, committed): `fn handle_eventfd(pid: u32,
  initval: u64) -> Message` takes no flags parameter; the created `FdEntry` is
  hardcoded `flags: 0`.
- Dispatch: `VFS_EVENTFD => handle_eventfd(caller_pid, arg(msg,0) as u64)`
  (1-arg call, matching the 1-arg handler).

## Existing conventions this fix must match (confirmed by reading the code)

1. **Storage**: `FdEntry.flags: u32` is the single per-fd flag word already used
   for `O_CLOEXEC` (bit 0x80000) and `O_NONBLOCK` (bit 0o4000/0x800) uniformly
   across pipe/tmpfile/timerfd/signalfd/etc. `pub fn fd_nonblock(pid, fd) ->
   bool` (`servers/vfs/src/lib.rs:652`) reads exactly this bit generically for
   *any* fd kind — it is not per-VnodeKind logic.
2. **Read-blocking loop**: the *generic* fallback arm of `sys_read` in
   `kernel/src/syscall.rs` (around line 3692-3707, the `_ => { ... }` catch-all
   after the epoll/socket-specific arms) already does exactly the required
   thing for any VFS fd, eventfd included:
   ```rust
   let nonblock = vfs::fd_nonblock(pid, fd);
   loop {
       let n = vfs_reply_val(&vfs::handle(&msg, pid));
       if n != -11 || nonblock { return n; }   // -11 == EAGAIN
       if interrupted() { return -4; }          // EINTR
       irq_window();
       yield_now("sys_read_vfs");
   }
   ```
   This means **no new read-path code is needed** — once the eventfd's
   `FdEntry.flags` correctly carries `O_NONBLOCK` from creation, this existing
   loop already returns EAGAIN immediately without yield-looping (the exact
   "must NOT yield-loop" requirement). The bug is purely that the flag was
   never stored, not that the consumer of the flag is missing.
3. **fcntl(F_SETFL, O_NONBLOCK) after creation**: also already works today,
   with **no change needed**. `sys_fcntl` (`kernel/src/syscall.rs:4568`) only
   special-cases fd 0-2, epoll fds, and net-socket fds; anything else
   (including an eventfd) falls through to the generic
   `make_vfs_msg(vfs::VFS_FCNTL, ...)` path (line 4620), which lands in
   `handle_fcntl` (`servers/vfs/src/lib.rs:3615`). `F_SETFL` there is fully
   generic: `tbl.fds[fd].flags = (tbl.fds[fd].flags & O_CLOEXEC) | arg as u32`
   (line 3662) — it doesn't switch on `VnodeKind` at all, so it already updates
   an eventfd's stored flags correctly. **Confirmed: F_SETFL on an eventfd
   already works regardless of this fix; only EFD_NONBLOCK-at-creation was
   broken.**
4. **Read EAGAIN semantics** (already correct, unaffected by this fix):
   `VnodeKind::EventFd` read handler (`servers/vfs/src/lib.rs` ~line 2753):
   `if val == 0 { return err_reply(-11); }` — counter==0 → EAGAIN, always
   returned by the VFS regardless of the fd's nonblock flag (blocking
   emulation happens one layer up, in the kernel's yield-loop per point 2).
5. **Write semantics — a real, separate, minor gap** (NOT covered by the
   in-progress diff, low priority): the `VnodeKind::EventFd` write handler
   (`servers/vfs/src/lib.rs` ~line 2908) does
   `counters[slot] = counters[slot].saturating_add(addval)` with **no overflow
   check and no EAGAIN path**. Real eventfd semantics: a write that would push
   the counter to `u64::MAX` blocks (or EAGAIN's in nonblocking mode) until a
   read makes room. Given every caller in this codebase writes small
   increments (typically 1) to wake a poller, this is very unlikely to be hit
   in practice (would need ~2^64 unread increments) — flagging for
   completeness per the task brief, not recommending it block the M5 landing.

## What actually needs to land (2 files, matches the in-progress uncommitted diff)

### `servers/vfs/src/lib.rs`

- `handle_eventfd(pid: u32, initval: u64, flags: u32) -> Message` — add the
  `flags: u32` parameter.
- Compute `let stored = flags & (O_NONBLOCK_FL | O_CLOEXEC);` and use it as the
  new `FdEntry`'s `flags` field instead of the hardcoded `0`.
- Update the `dispatch()` call site:
  `VFS_EVENTFD => handle_eventfd(caller_pid, arg(msg,0) as u64, arg(msg,1) as u32)`.
- (The uncommitted diff also happens to carry the identical fix through for
  `handle_signalfd_create`/`handle_timerfd_create` in the same pass — those were
  the same class of bug for signalfd/timerfd and are already covered by other
  notes; no new action needed there beyond what's already staged.)

### `kernel/src/syscall.rs`

- `sys_eventfd2(initval: usize, flags: usize) -> isize` — rename `_flags` to
  `flags` (drop the underscore) and change the VFS message to
  `make_vfs_msg(vfs::VFS_EVENTFD, &[initval as u64, flags as u64])`.

No other file needs to change: `fd_nonblock`, the `sys_read` generic
blocking loop, and `handle_fcntl`'s `F_SETFL` are all already generic over
`FdEntry.flags` and require zero eventfd-specific code once the flag is stored
correctly at creation.

## Test recipe

Extend `userland/epolltest/src/main.rs` — it already has raw-syscall access to
`EVENTFD2` (`nr::EVENTFD2 = 290` x86_64 / `19` aarch64, declared at lines
58-69) via `extern "C" fn syscall(sysno: c_long, ...) -> c_long`, already
constructs a blocking eventfd for `test_timeout_accuracy` (line 281:
`syscall(nr::EVENTFD2, 0i64, 0i64)`), and follows a uniform
`unsafe fn test_xxx() -> bool { ...; report(name, condition) }` pattern (see
`test_signalfd_signo` at line 351 for the closest analog: raw syscall create +
direct `read()` + byte-level assertion on the return value).

Add a new `test_eventfd_nonblock()`:

```rust
unsafe fn test_eventfd_nonblock() -> bool {
    let name = b"eventfd_nonblock\0";

    // EFD_NONBLOCK == O_NONBLOCK == 0o4000 (same constant already defined at
    // the top of this file for pipe2).
    let efd = syscall(nr::EVENTFD2, 0i64, O_NONBLOCK as i64) as i32;
    if efd < 0 { return report(name, false); }

    // Counter starts at 0 -> read() on a nonblocking eventfd must return
    // EAGAIN (-11) IMMEDIATELY, not hang/yield-spin. Bound the wall-clock
    // cost as the actual regression signal: pre-fix this call never returns
    // (kernel yield-loops forever since fd_nonblock() was always false).
    let mut buf = [0u8; 8];
    let mut start: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut start);
    let n = read(efd, buf.as_mut_ptr(), 8);
    let mut end: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut end);
    let elapsed_ms = (end.tv_sec - start.tv_sec) * 1000
        + (end.tv_nsec - start.tv_nsec) / 1_000_000;

    // n == -11 (EAGAIN) and fast (<50ms is generous; a yield-spin bug would
    // either hang the whole test binary or take far longer).
    let empty_read_ok = n == -11 && elapsed_ms < 50;

    // Sanity: write 1, then a nonblocking read must succeed and return 8.
    let one: u64 = 1;
    let wn = write(efd, &one as *const u64 as *const u8, 8);
    let rn = read(efd, buf.as_mut_ptr(), 8);
    let val = u64::from_le_bytes(buf);

    close(efd);
    report(name, empty_read_ok && wn == 8 && rn == 8 && val == 1)
}
```

Wire it into `epoll_main`'s test list (bump the `write_summary(N, failures)`
count by 1) alongside the existing `test_signalfd_signo`/`test_inotify_never_fires`
calls.

Optional follow-up (F_SETFL path, since it's already believed to work and is
cheap to confirm): a second test creating the eventfd *without* EFD_NONBLOCK,
then `fcntl(efd, F_SETFL, O_NONBLOCK)`, then repeating the same empty-read
timing assertion — this exercises `handle_fcntl`'s generic `F_SETFL` arm rather
than the creation-time path, so it's a genuinely different code path worth
covering even though no code change is expected to be needed for it.

## Anchors (file:line, current working-tree state)

- `kernel/src/syscall.rs:1183` — `EVENTFD2 => sys_eventfd2(a0, a1),` dispatch
- `kernel/src/syscall.rs:6578-6591` (working tree, already touched) —
  `sys_eventfd2` body
- `servers/vfs/src/lib.rs:60` — `VFS_EVENTFD` message tag constant
- `servers/vfs/src/lib.rs:1012-1015` (working tree, already touched) —
  `O_NONBLOCK_FL` const definition
- `servers/vfs/src/lib.rs:652-661` — `pub fn fd_nonblock(pid, fd) -> bool`
  (the generic consumer; no change needed)
- `servers/vfs/src/lib.rs:2753-2764` — `VnodeKind::EventFd` read arm (EAGAIN on
  empty counter; no change needed)
- `servers/vfs/src/lib.rs:~2908` — `VnodeKind::EventFd` write arm (the
  saturating_add overflow-EAGAIN gap noted above; optional)
- `servers/vfs/src/lib.rs:3615-3662` — `handle_fcntl`, `F_GETFL`/`F_SETFL`
  generic arms (no change needed)
- `servers/vfs/src/lib.rs:4159-4178` (working tree, already touched) —
  `handle_eventfd`
- `kernel/src/syscall.rs:3692-3707` — generic `sys_read` blocking/EAGAIN loop
  (no change needed; this is what makes the fix "just work" once the flag is
  stored)
- `userland/epolltest/src/main.rs:58-69` — `nr::EVENTFD2` syscall-number
  constants per arch
- `userland/epolltest/src/main.rs:41` — `O_NONBLOCK: c_int = 0o4000` (reusable)
- `userland/epolltest/src/main.rs:276-307`, `:351-375` — existing test
  patterns to model the new `test_eventfd_nonblock` on
- `ports/input-stack/shims/libseat/libseat.c` (commit `0bed5ad`) — the
  shim-side workaround that becomes removable/simplifiable once this kernel
  fix lands (not required to remove it, but it's now redundant belt-and-braces
  for the specific idle-dispatch call it patches)
