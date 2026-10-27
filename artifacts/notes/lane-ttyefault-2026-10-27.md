# lane/ttyefault — 2026-10-27 — FIXED

Bug (from lane piserialxfer): `read(0, buf, n)` on the console returned EFAULT when
`buf` had never been touched.

## Root cause
The console read fast path (`sys_read_impl`, console branch) copied its kernel buffer
out with `AddressSpace::write_user_buf`, which only writes pages already present
(it translates with `virt_to_phys` and gives up on `None`). Unlike every other read
route (VFS, socket) this path did no prefault. The failing shape is a destination in
an absent **file-backed** page — a `.data` page of the exec image, or the `.data`/`.bss`
boundary page where serrx's buffers sat. A page-aligned pure `.bss` buffer happened to work.

## Fix
- Console read now copies with the fault-tolerant uaccess copy
  (`sched::uaccess::copy_to_user`, through the new `copy_to_user_bytes` helper): absent
  pages (demand-zero, file-backed, CoW) are faulted in like the task's own store, and
  only truly invalid ranges give EFAULT. No lock is held at that point.
- The same helper replaces `write_user_buf` at the one-line current-task sites:
  waitid infop, times, getgroups, sigprocmask old set (two sites), sigpending.
- `write_user_buf` itself now populates an absent anonymous page (safe under the AS
  lock, as `prefault_range` already does), which covers the server-side helpers
  (tty/vfs/evdev) for anonymous buffers. Absent file-backed pages there still rely on
  the caller's prefault (sys_read/VFS and socket routes already prefault).

## Test
`/bin/uaccesstest` (userland/uaccesstest, in the default regress): console (via a CPR
query, the kernel answers into the input queue), pipe, readv, write-from, file, socket,
pty and mmap-anon reads into untouched `.bss` and `.data` buffers.
Unfixed kernel (aarch64): `console_read_data: FAIL - returned -1 errno 14 (Bad address)`,
all others PASS. Fixed: 12/12 PASS (ptytest, sigtest green).

## Open
- Server-side `write_user_buf` into an absent file-backed page (e.g. an ioctl reply
  into an untouched initialised struct) is still EFAULT unless the caller prefaults;
  not reproduced, not chased.
